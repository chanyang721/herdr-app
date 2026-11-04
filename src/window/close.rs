//! A close is a plan until the entire terminal batch has been accepted and ended.
//! This module never talks to a running core; snapshots and results are inputs.
use crate::model::{self, Model};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Target {
    Space(String),
    Tab(String),
    Pane(String),
    Terminal(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    pub address: String,
    pub pid: u64,
    pub created_ms: u64,
}

impl Identity {
    fn read(address: &str, record: &Value) -> Result<Self, String> {
        if address.is_empty() || record["address"].as_str() != Some(address) {
            return Err(format!("close identity missing or mismatched: {address}"));
        }
        // The first local adapter cannot certify an SSH core's batch/guard protocol.
        if address.contains('/') {
            return Err(format!("remote close is not verified: {address}"));
        }
        let pid = record["pid"].as_u64().filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .ok_or_else(|| format!("close identity has no process id: {address}"))?;
        let created_ms = record["created_ms"].as_u64().filter(|t| *t > 0)
            .ok_or_else(|| format!("close identity has no creation time: {address}"))?;
        Ok(Self { address: address.to_owned(), pid, created_ms })
    }
}

impl Target {
    pub fn roots(&self, model: &Model) -> Result<Vec<String>, String> {
        match self {
            Self::Space(id) => model.space(id).map(|s| s.tabs.iter().flat_map(|t| model::addresses(&t.root)).collect())
                .ok_or_else(|| format!("no space {id:?}")),
            Self::Tab(id) => model.tab(id).map(|t| model::addresses(&t.root)).ok_or_else(|| format!("no tab {id:?}")),
            Self::Pane(id) => model.tab_of(id).and_then(|t| t.root.find(id)).map(model::addresses)
                .ok_or_else(|| format!("no pane {id:?}")),
            Self::Terminal(address) => Ok(vec![address.clone()]),
        }
    }

    fn stamp(&self, model: &Model) -> Result<Value, String> {
        match self {
            Self::Space(id) => model.space(id).map(|s| json!([s.id, s.machine, s.name, s.folder,
                s.tabs.iter().map(|t| json!([t.id, t.name, t.root])).collect::<Vec<_>>()]))
                .ok_or_else(|| format!("no space {id:?}")),
            Self::Tab(id) => model.tab(id).map(|t| json!([t.id, t.name, t.root])).ok_or_else(|| format!("no tab {id:?}")),
            Self::Pane(id) => model.tab_of(id).and_then(|t| t.root.find(id)).map(|n| json!(n))
                .ok_or_else(|| format!("no pane {id:?}")),
            Self::Terminal(address) => Ok(json!(address)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub operation_id: String,
    pub target: Target,
    pub roots: Vec<String>,
    pub expected: Vec<Identity>,
    stamp: Value,
}

/// Only an exact, complete batch receipt can produce the value accepted by `finish`.
pub(super) struct Completed {
    identities: Vec<Identity>,
    stamp: Value,
}

impl Plan {
    /// Transport is supplied by the caller; unit tests never reach a real core.
    pub fn execute(&self, context: &Value, call: impl FnOnce(Value) -> Result<Value, String>) -> Result<Completed, String> {
        validate_context(context)?;
        if self.expected.is_empty() { return self.completed(vec![]); }
        let mut params = json!({"operation_id": self.operation_id, "roots": self.roots, "expected": self.expected});
        if !context.is_null() { params["context"] = context.clone(); }
        let reply = call(params)?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true)
            || reply.get("operation_id").and_then(Value::as_str) != Some(self.operation_id.as_str()) {
            return Err("guarded close batch did not succeed".into());
        }
        let terminated = serde_json::from_value(reply.get("terminated").cloned().unwrap_or(Value::Null))
            .map_err(|_| "guarded close batch returned an invalid receipt".to_owned())?;
        self.completed(terminated)
    }
    pub fn capture(target: Target, model: &Model, live: &HashMap<String, Value>) -> Result<Self, String> {
        let remote = match &target {
            Target::Space(id) => model.space(id),
            Target::Tab(id) | Target::Pane(id) => model.space_of(id),
            Target::Terminal(_) => None,
        }.and_then(|space| space.machine.as_deref());
        if let Some(machine) = remote {
            return Err(format!("remote close is not verified: {machine}"));
        }
        let stamp = target.stamp(model)?;
        let roots = target.roots(model)?.into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let mut doomed = roots.iter().cloned().collect::<BTreeSet<_>>();
        loop {
            let next = live.iter().filter(|(address, record)| {
                !doomed.contains(*address)
                    && record["parent"].as_str().is_some_and(|parent| doomed.contains(parent))
                    && !record["labels"]["herdr.space"].as_str().is_some_and(|space| !space.is_empty())
            }).map(|(address, _)| address.clone()).collect::<Vec<_>>();
            if next.is_empty() { break; }
            doomed.extend(next);
        }
        let expected = doomed.iter().map(|address| {
            let record = live.get(address).ok_or_else(|| format!("close lookup missing: {address}"))?;
            Identity::read(address, record)
        }).collect::<Result<Vec<_>, _>>()?;
        use std::hash::{BuildHasher, Hasher};
        let random = || std::collections::hash_map::RandomState::new().build_hasher().finish();
        let operation_id = format!("appclose{:016x}{:016x}", random(), random());
        Ok(Self { operation_id, target, roots, expected, stamp })
    }

    /// Even a core success cannot delete a container that acquired other content meanwhile.
    pub fn unchanged(&self, model: &Model) -> Result<(), String> {
        if self.target.stamp(model)? != self.stamp {
            return Err("close target changed while the batch was pending".into());
        }
        Ok(())
    }

    pub fn verify_identities(&self, fresh: &HashMap<String, Value>) -> Result<(), String> {
        // Container roots are already resolved; verify closure independently of the UI model.
        let mut wanted = BTreeMap::new();
        for identity in &self.expected {
            let record = fresh.get(&identity.address).ok_or_else(|| format!("close lookup missing: {}", identity.address))?;
            let actual = Identity::read(&identity.address, record)?;
            if actual != *identity { return Err(format!("close identity changed: {}", identity.address)); }
            wanted.insert(identity.address.as_str(), identity);
        }
        for (address, record) in fresh {
            if record["parent"].as_str().is_some_and(|parent| wanted.contains_key(parent))
                && !record["labels"]["herdr.space"].as_str().is_some_and(|space| !space.is_empty())
                && !wanted.contains_key(address.as_str())
            { return Err(format!("close acquired an unverified child: {address}")); }
        }
        Ok(())
    }

    pub fn completed(&self, mut terminated: Vec<Identity>) -> Result<Completed, String> {
        terminated.sort_by(|a,b| a.address.cmp(&b.address));
        if terminated != self.expected {
            return Err("close batch did not confirm exactly the prepared terminal identities".into());
        }
        Ok(Completed { identities: terminated, stamp: self.stamp.clone() })
    }

    /// The only model mutation in the close protocol. Errors leave even layout/focus intact.
    pub fn finish(&self, model: &mut Model, result: Result<Completed, String>) -> Result<(), String> {
        let completed = result?;
        if completed.identities != self.expected || completed.stamp != self.stamp {
            return Err("close receipt belongs to another plan".into());
        }
        self.unchanged(model)?;
        match &self.target {
            Target::Space(id) => { model.remove_space(id); }
            Target::Tab(id) => { model.remove_tab(id); }
            Target::Pane(id) => { model.remove_pane(id); }
            Target::Terminal(_) => {
                for identity in &self.expected {
                    if let Some(pane) = model.pane_of(&identity.address) { model.remove_pane(&pane); }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Space, Tab};

    fn fixture() -> (Model, HashMap<String, Value>) {
        let mut model = Model::default();
        model.spaces.push(Space { id:"s".into(), name:"test".into(), folder:"".into(), machine:None,
            tabs:vec![Tab::new("tab", "work", "p", "t1")], active:Some("tab".into()) });
        model.active = Some("s".into());
        let live = [("t1",1,None),("t2",2,Some("t1")),("t3",3,Some("t2"))].into_iter()
            .map(|(address,pid,parent)| (address.into(), json!({"address":address,"pid":pid,"created_ms":100+pid,"parent":parent,"labels":{}}))).collect();
        (model,live)
    }

    #[test]
    fn plans_all_descendants_before_any_model_change() {
        let (model,live)=fixture();
        for target in [Target::Space("s".into()),Target::Tab("tab".into()),Target::Pane("p".into()),Target::Terminal("t1".into())] {
            let plan=Plan::capture(target,&model,&live).unwrap();
            assert_eq!(plan.expected.iter().map(|i|i.address.as_str()).collect::<Vec<_>>(),["t1","t2","t3"]);
            assert!(model.space("s").is_some());
        }
    }

    #[test]
    fn any_refusal_preserves_space_tab_pane_and_focus() {
        let (mut model,live)=fixture();
        for target in [Target::Space("s".into()),Target::Tab("tab".into()),Target::Pane("p".into()),Target::Terminal("t1".into())] {
            let plan=Plan::capture(target,&model,&live).unwrap();
            let before=plan.target.stamp(&model).unwrap();
            assert!(plan.finish(&mut model,Err("gate/core denied".into())).is_err());
            assert_eq!(plan.target.stamp(&model).unwrap(),before);
            assert_eq!(model.active.as_deref(),Some("s"));
        }
    }

    #[test]
    fn lookup_failures_changed_process_and_new_child_are_rejected() {
        let (model,live)=fixture();
        let plan=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        for field in ["pid","created_ms"] {
            let mut fresh=live.clone();fresh.get_mut("t3").unwrap()[field]=json!(900);
            assert!(plan.verify_identities(&fresh).is_err());
        }
        let mut fresh=live.clone();fresh.remove("t2");assert!(plan.verify_identities(&fresh).is_err());
        let mut fresh=live.clone();fresh.insert("t4".into(),json!({"address":"t4","pid":4,"created_ms":104,"parent":"t3"}));
        assert!(plan.verify_identities(&fresh).is_err());
        let mut missing=live.clone();missing.get_mut("t3").unwrap()["pid"]=Value::Null;
        assert!(Plan::capture(Target::Space("s".into()),&model,&missing).is_err());
    }

    #[test]
    fn kept_children_are_spared_and_unverified_remote_roots_refused() {
        let (model,mut live)=fixture();
        live.get_mut("t2").unwrap()["labels"]=json!({"herdr.space":"owned"});
        assert_eq!(Plan::capture(Target::Space("s".into()),&model,&live).unwrap().expected.len(),1);
        live.insert("remote/t1".into(),json!({"address":"remote/t1","pid":1,"created_ms":100}));
        assert!(Plan::capture(Target::Terminal("remote/t1".into()),&model,&live).is_err());
    }

    #[test]
    fn a_changed_container_cannot_be_removed_by_an_older_success() {
        let (mut model,live)=fixture();
        let plan=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        model.add_tab("s",Tab::new("later","new work","new-pane","t4"));
        assert!(plan.finish(&mut model,plan.completed(plan.expected.clone())).is_err());
        assert!(model.tab("tab").is_some());
        assert!(model.tab("later").is_some());
    }

    #[test]
    fn success_applies_only_the_prepared_close_target() {
        let (mut model,live)=fixture();
        model.add_tab("s",Tab::new("other","keep","other-pane","t4"));
        let plan=Plan::capture(Target::Tab("tab".into()),&model,&live).unwrap();
        plan.finish(&mut model,plan.completed(plan.expected.clone())).unwrap();
        assert!(model.tab("tab").is_none());
        assert!(model.tab("other").is_some());
    }

    #[test]
    fn partial_duplicate_extra_and_stale_batch_receipts_never_change_the_layout() {
        let (mut model,live)=fixture();
        let plan=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        for ids in [plan.expected[..2].to_vec(), vec![plan.expected[0].clone();3],
            [plan.expected.clone(),vec![Identity{address:"t9".into(),pid:9,created_ms:109}]].concat()] {
            assert!(plan.finish(&mut model,plan.completed(ids)).is_err());
            assert!(model.space("s").is_some());
        }
        let other=Plan::capture(Target::Tab("tab".into()),&model,&live).unwrap();
        assert!(other.finish(&mut model,plan.completed(plan.expected.clone())).is_err());
        assert!(model.tab("tab").is_some());
    }

    #[test]
    fn rpc_refusal_malformed_partial_and_timeout_keep_every_target_unchanged() {
        for target in [Target::Space("s".into()),Target::Tab("tab".into()),Target::Pane("p".into()),Target::Terminal("t1".into())] {
            for reply in [Err("unsupported/timeout/EOF".into()),Ok(json!({"ok":false})),Ok(json!({"ok":true})),
                Ok(json!({"ok":true,"terminated":[{"address":"t1","pid":1,"created_ms":101}]}))] {
                let (mut model,live)=fixture();
                let plan=Plan::capture(target.clone(),&model,&live).unwrap();
                let before=plan.target.stamp(&model).unwrap();
                let mut calls=0;
                let result=plan.execute(&Value::Null,|params| {
                    calls+=1;
                    assert_eq!(params["expected"].as_array().unwrap().len(),3);
                    assert!(model.tab("tab").is_some(),"model intact during the RPC");
                    reply
                });
                assert!(plan.finish(&mut model,result).is_err());
                assert_eq!(calls,1,"one batch only, never single-kill fallback");
                assert_eq!(plan.target.stamp(&model).unwrap(),before);
            }
        }
    }

    #[test]
    fn rpc_preserves_automatic_context_and_commits_only_an_exact_success() {
        let (mut model,live)=fixture();
        let plan=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        let result=plan.execute(&json!({"mode":"automatic","handoff":"one-use-test-capability"}),|params| {
            assert_eq!(params["context"],json!({"mode":"automatic","handoff":"one-use-test-capability"}));
            assert_eq!(params["roots"],json!(["t1"]));
            Ok(json!({"ok":true,"operation_id":plan.operation_id,"terminated":plan.expected}))
        });
        plan.finish(&mut model,result).unwrap();
        assert!(model.space("s").is_none());
        for context in [json!({"mode":"automatic"}),json!({"mode":"automatic","handoff":""}),json!({}),json!({"mode":"manual"}),json!({"mode":"automatic","force":true})] {
            assert!(plan.execute(&context,|_|panic!("invalid context reached the core")).is_err());
        }
    }

    #[test]
    fn operation_identity_survives_persistence_and_cannot_accept_another_receipt() {
        let (model,live)=fixture();
        let plan=Plan::capture(Target::Pane("p".into()),&model,&live).unwrap();
        let restored:Plan=serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert_eq!(restored.operation_id,plan.operation_id);
        assert_eq!(restored.expected,plan.expected);
        assert_ne!(plan.operation_id,Plan::capture(Target::Pane("p".into()),&model,&live).unwrap().operation_id);
        for op in [Value::Null,json!("another-operation")] {
            assert!(restored.execute(&Value::Null,|params| {
                assert_eq!(params["operation_id"],plan.operation_id);
                Ok(json!({"ok":true,"operation_id":op,"terminated":plan.expected}))
            }).is_err());
        }
    }

    #[test]
    fn command_and_shutdown_paths_cannot_bypass_the_batch_boundary() {
        let source=include_str!("../window.rs").split("    fn dispatch(").nth(1).unwrap();
        for name in ["space.close","tab.close","pane.close","terminal.kill"] {
            let key=format!("\"{name}\" => {{");
            let branch=source.split(&key).nth(1).unwrap().split("\n            \"").next().unwrap();
            assert!(branch.contains("self.begin_close("),"{name} lacks batch dispatch");
            assert!(!branch.contains("self.model.remove_"),"{name} mutates before approval");
        }
        assert!(!source.contains("fn kill(&mut self"));
        let mods=include_str!("../mods.rs");
        assert!(!mods.contains("core.call_timeout(&name, args"),"shutdown direct-kill bypass");
        assert!(mods.contains("terminal.kill is unavailable during shutdown"));
        assert!(!structural_command("app.quit"));
        assert!(!structural_command("window.close"));
        for name in ["space.close","tab.close","pane.close","terminal.kill","pane.split","terminal.open"] {
            assert!(structural_command(name));
        }
    }

    #[test]
    fn observed_exit_and_partial_or_denied_batches_are_not_prune_permission() {
        assert!(!may_prune(false,false));
        assert!(!may_prune(false,true));
        assert!(!may_prune(true,true));
        assert!(may_prune(true,false));
    }
}

pub(super) fn validate_context(context: &Value) -> Result<(), String> {
    crate::commands::validate_close_context(context)
}

pub(super) fn structural_command(name: &str) -> bool {
    matches!(name, "space.new" | "space.close" | "space.rename" | "space.move" | "tab.new" | "tab.close" | "tab.rename" | "tab.move"
        | "pane.split" | "pane.close" | "pane.swap" | "terminal.open" | "terminal.kill" | "terminal.keep" | "terminal.park"
        | "layout.set" | "layout.reset" | "layout.resize" | "layout.clone" | "layout.switch" | "app.reset")
}

pub(super) fn may_prune(completed: bool, held: bool) -> bool { completed && !held }
