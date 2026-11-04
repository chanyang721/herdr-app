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
    #[serde(default)]
    pub audit_only: bool,
    pub target: Target,
    pub roots: Vec<String>,
    pub expected: Vec<Identity>,
    stamp: Value,
    panes: Vec<(String,String)>,
}

/// Only an exact, complete batch receipt can produce the value accepted by `finish`.
pub(super) struct Completed {
    identities: Vec<Identity>,
    stamp: Value,
}

pub(super) enum Resolution { Closed(Completed), Cancelled }

/// Only the native RPC error kind carries the no-signal guarantee. Display
/// messages, timeout text and generic IO errors are never classified as safe.
pub(super) struct CallFailure { pub kind: Option<String>, pub message: String }

impl Plan {
    /// Transport is supplied by the caller; unit tests never reach a real core.
    pub fn execute(&self, context: &Value, call: impl FnOnce(Value) -> Result<Value, String>) -> Result<Completed, String> {
        validate_context(context)?;
        if self.audit_only { return Err("an audit plan cannot send a kill".into()); }
        if self.expected.is_empty() { return self.completed(vec![]); }
        let mut params = json!({"operation_id": self.operation_id, "roots": self.roots, "expected": self.expected});
        if !context.is_null() { params["context"] = context.clone(); }
        self.accept_reply(call(params)?)
    }

    pub fn execute_guarded(&self, context: &Value, call: impl FnOnce(Value) -> Result<Value,CallFailure>) -> Result<Resolution,String> {
        let mut failure=None;
        let result=self.execute(context,|params|call(params).map_err(|error| {
            let message=error.message.clone();failure=Some(error);message
        }));
        if failure.as_ref().and_then(|e|e.kind.as_deref())==Some("close_retry_safe") {
            return Ok(Resolution::Cancelled);
        }
        result.map(Resolution::Closed)
    }

    /// Recovery never resends a kill; only the original operation can be reconciled.
    pub fn reconcile(&self, call: impl FnOnce(Value) -> Result<Value, String>) -> Result<Resolution, String> {
        let reply=call(json!({"operation_id":self.operation_id,"roots":self.roots,"expected":self.expected}))?;
        if reply["operation_id"].as_str()==Some(self.operation_id.as_str()) && reply["ok"]==false
            && reply["state"]=="cancelled" && reply["no_signal"]==true && reply.get("terminated").is_none() {
            let mut ids:Vec<Identity>=serde_json::from_value(reply["expected"].clone()).map_err(|_|"invalid cancelled close receipt")?;
            ids.sort_by(|a,b|a.address.cmp(&b.address));
            if ids==self.expected { return Ok(Resolution::Cancelled); }
            return Err("cancelled close receipt does not match the complete operation".into());
        }
        self.accept_reply(reply).map(Resolution::Closed)
    }

    fn accept_reply(&self, reply: Value) -> Result<Completed, String> {
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
        let panes=model.tabs().flat_map(|t| t.root.terminals()).filter_map(|n| {
            let address=n.terminal()?;
            expected.iter().any(|i|i.address==address).then(||(n.id.clone(),address.to_owned()))
        }).collect();
        Ok(Self { operation_id, audit_only:false, target, roots, expected, stamp, panes })
    }

    /// Capture UI scope before a read-only query; never interpret an empty
    /// draft identity set as a mod-only close or permission to change the model.
    pub fn audit_draft(target: Target, model: &Model) -> Result<Self,String> {
        let roots=target.roots(model)?;
        if roots.is_empty() { return Err("no terminal to audit".into()); }
        if roots.iter().any(|a|a.is_empty()||a.contains('/')) { return Err("remote audit is not verified".into()); }
        let remote=match &target {
            Target::Space(id)=>model.space(id),
            Target::Tab(id)|Target::Pane(id)=>model.space_of(id),
            Target::Terminal(_)=>None,
        }.and_then(|space|space.machine.as_deref());
        if remote.is_some() { return Err("remote audit is not verified".into()); }
        let roots=roots.into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let stamp=target.stamp(model)?;
        let panes=model.tabs().flat_map(|t|t.root.terminals()).filter_map(|n| {
            let address=n.terminal()?;
            roots.iter().any(|a|a==address).then(||(n.id.clone(),address.to_owned()))
        }).collect();
        use std::hash::{BuildHasher,Hasher};
        let random=||std::collections::hash_map::RandomState::new().build_hasher().finish();
        Ok(Self {operation_id:format!("appclose{:016x}{:016x}",random(),random()),audit_only:true,
            target,roots,expected:vec![],stamp,panes})
    }

    pub fn audit_snapshot(&self, core_id: &str, snapshot: Value) -> Result<Self,String> {
        if !self.audit_only || core_id.is_empty() || snapshot["core_id"].as_str()!=Some(core_id) {
            return Err("natural-exit snapshot core identity mismatch".into());
        }
        let roots:Vec<String>=serde_json::from_value(snapshot["roots"].clone()).map_err(|_|"invalid audit roots")?;
        if roots!=self.roots { return Err("natural-exit snapshot changed roots".into()); }
        let mut expected:Vec<Identity>=serde_json::from_value(snapshot["expected"].clone()).map_err(|_|"invalid audit identity set")?;
        expected.sort_by(|a,b|a.address.cmp(&b.address));
        if expected.is_empty() || expected.windows(2).any(|p|p[0].address==p[1].address)
            || self.roots.iter().any(|a|!expected.iter().any(|i|&i.address==a)) {
            return Err("natural-exit snapshot has incomplete or duplicate identities".into());
        }
        if snapshot["operations"].as_array().is_some_and(|ops|ops.iter().any(|op|op["acknowledged"]!=true && op["cancelled"]!=true)) {
            return Err("an original unsettled operation must be reconciled first".into());
        }
        let targets=snapshot["targets"].as_array().ok_or("missing native actual Exit witnesses")?;
        if targets.len()!=expected.len() { return Err("natural-exit snapshot has partial witnesses".into()); }
        let mut witnessed=BTreeSet::new();
        for record in targets {
            let address=record["address"].as_str().ok_or("missing audit address")?;
            let identity=Identity::read(address,record)?;
            if !expected.contains(&identity) || !witnessed.insert(address)
                || record["native_state"]!="finished" || record["observed_exit"]!=true
                || record["input_generation"].as_u64().is_none()
                || record["program_started_ms"].as_u64().is_none_or(|t|t==0)
                || record["turn"].as_u64().is_none() {
                return Err("natural-exit snapshot is live, changed or lacks actual Exit evidence".into());
            }
        }
        let mut plan=self.clone();plan.expected=expected;Ok(plan)
    }

    pub fn reconcile_guarded(&self, call: impl FnOnce(Value)->Result<Value,CallFailure>) -> Result<Resolution,String> {
        let mut failure=None;
        let result=self.reconcile(|params|call(params).map_err(|error| {
            let message=error.message.clone();failure=Some(error);message
        }));
        // Audit-only operations cannot signal. Only a structured native safe
        // denial can retire them; uncertain original kill operations stay held.
        if self.audit_only && failure.as_ref().and_then(|e|e.kind.as_deref())==Some("close_retry_safe") {
            return Ok(Resolution::Cancelled);
        }
        result
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
        if self.audit_only && self.expected.is_empty() { return Err("unprepared audit draft has no completion authority".into()); }
        terminated.sort_by(|a,b| a.address.cmp(&b.address));
        if terminated != self.expected {
            return Err("close batch did not confirm exactly the prepared terminal identities".into());
        }
        Ok(Completed { identities: terminated, stamp: self.stamp.clone() })
    }

    pub fn represented(&self, model: &Model) -> bool {
        self.panes.iter().any(|(pane,address)|model.address_of(pane).as_deref()==Some(address.as_str()))
            || matches!(&self.target,Target::Terminal(_))
    }

    /// A recovered receipt may clear only the originally captured panes when a
    /// container changed. New content and moved/replaced pane identities survive.
    pub fn finish_reconciled(&self, model: &mut Model, result: Result<Completed,String>) -> Result<(),String> {
        let completed=result?;
        if completed.identities!=self.expected || completed.stamp!=self.stamp { return Err("foreign close receipt".into()); }
        if self.unchanged(model).is_ok() { return self.finish(model,Ok(completed)); }
        for (pane,address) in &self.panes {
            if model.address_of(pane).as_deref()==Some(address.as_str()) { model.remove_pane(pane); }
        }
        Ok(())
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

/// The capability/context is deliberately absent from durable app evidence.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved { core_id: String, plan: Plan }

fn valid_operation(id: &str) -> bool {
    id.strip_prefix("appclose").is_some_and(|s| s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit()))
}

pub(super) fn save_pending(dir: &std::path::Path, core_id: &str, plan: &Plan) -> Result<(), String> {
    use std::io::Write;
    if core_id.is_empty() || !valid_operation(&plan.operation_id) || plan.expected.is_empty() || plan.roots.is_empty() { return Err("missing close core/operation identity or prepared scope".into()); }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path=dir.join(format!("{}.json",plan.operation_id));
    let text=serde_json::to_vec(&Saved {core_id:core_id.into(),plan:plan.clone()}).map_err(|e|e.to_string())?;
    let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e|e.to_string())?;
    file.write_all(&text).and_then(|_| file.sync_all()).map_err(|e| e.to_string())
}

pub(super) fn load_pending(dir: &std::path::Path, core_id: &str) -> Result<BTreeMap<String,Plan>, String> {
    let mut plans=BTreeMap::new();
    let files=match std::fs::read_dir(dir) {
        Ok(files)=>files,
        Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(plans),
        Err(e)=>return Err(e.to_string()),
    };
    for entry in files {
        let path=entry.map_err(|e|e.to_string())?.path();
        if path.extension().and_then(|s|s.to_str()) != Some("json") { continue; }
        let saved:Saved=serde_json::from_slice(&std::fs::read(&path).map_err(|e|e.to_string())?).map_err(|e|format!("invalid retained close evidence: {e}"))?;
        if saved.plan.expected.is_empty() || saved.plan.roots.is_empty() { return Err(format!("unprepared retained close evidence: {}",path.display())); }
        if !valid_operation(&saved.plan.operation_id) || path.file_stem().and_then(|s|s.to_str())!=Some(&saved.plan.operation_id) {
            return Err("retained close operation identity mismatch".into());
        }
        if saved.core_id==core_id { plans.insert(saved.plan.operation_id.clone(),saved.plan); }
    }
    Ok(plans)
}

pub(super) fn remove_pending(dir: &std::path::Path, plan: &Plan) -> Result<(), String> {
    if !valid_operation(&plan.operation_id) { return Err("invalid close operation identity".into()); }
    std::fs::remove_file(dir.join(format!("{}.json",plan.operation_id))).map_err(|e|e.to_string())
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
    fn lost_ack_survives_restart_and_reconciles_without_live_capture_or_new_kill() {
        let (mut model,live)=fixture();
        let plan=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        let dir=std::env::temp_dir().join(&plan.operation_id);
        save_pending(&dir,"core-a",&plan).unwrap();
        let attempt=plan.execute(&Value::Null,|_|Err("terminated, but final ACK lost".into()));
        assert!(plan.finish(&mut model,attempt).is_err());
        assert!(Plan::capture(Target::Space("s".into()),&model,&HashMap::new()).is_err());
        let restored=load_pending(&dir,"core-a").unwrap().remove(&plan.operation_id).unwrap();
        assert!(load_pending(&dir,"other-core").unwrap().is_empty());
        // A new tab after failure must never be removed with the old space stamp.
        model.add_tab("s",Tab::new("later","keep","new-pane","t9"));
        for reply in [Err("helper unavailable".into()),Ok(json!({"ok":true,"operation_id":restored.operation_id,"terminated":restored.expected[..1]})),
            Ok(json!({"ok":true,"operation_id":"wrong-operation","terminated":restored.expected}))] {
            assert!(restored.finish_reconciled(&mut model,restored.reconcile(|_|reply).and_then(|r|match r {Resolution::Closed(c)=>Ok(c),Resolution::Cancelled=>Err("cancelled".into())})).is_err());
            assert!(model.tab("tab").is_some());
            assert_eq!(load_pending(&dir,"core-a").unwrap().len(),1);
        }
        let mut calls=0;
        let result=restored.reconcile(|params| {
            calls+=1;
            assert_eq!(params,json!({"operation_id":restored.operation_id,"roots":restored.roots,"expected":restored.expected}));
            Ok(json!({"ok":true,"operation_id":restored.operation_id,"terminated":restored.expected}))
        });
        let Resolution::Closed(receipt)=result.unwrap() else {panic!("unexpected cancellation")};
        restored.finish_reconciled(&mut model,Ok(receipt)).unwrap();
        assert_eq!(calls,1);
        assert!(model.tab("tab").is_none());
        assert_eq!(model.address_of("new-pane").as_deref(),Some("t9"));
        remove_pending(&dir,&restored).unwrap();
        assert!(load_pending(&dir,"core-a").unwrap().is_empty());
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn recovery_never_removes_a_replaced_pane_or_spared_child() {
        let (mut model,mut live)=fixture();
        live.get_mut("t2").unwrap()["labels"]=json!({"herdr.space":"s"});
        model.add_tab("s",Tab::new("kept","keep","kept-pane","t2"));
        let plan=Plan::capture(Target::Pane("p".into()),&model,&live).unwrap();
        assert_eq!(plan.expected.len(),1);
        model.remove_pane("p");
        model.add_tab("s",Tab::new("replacement","keep","p","t9"));
        plan.finish_reconciled(&mut model,plan.completed(plan.expected.clone())).unwrap();
        assert_eq!(model.address_of("p").as_deref(),Some("t9"));
        assert_eq!(model.address_of("kept-pane").as_deref(),Some("t2"));
    }

    #[test]
    fn only_an_exact_no_signal_cancellation_can_release_a_preflight_hold() {
        let (model,live)=fixture();
        let plan=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        let receipt=json!({"ok":false,"operation_id":plan.operation_id,"state":"cancelled","no_signal":true,"expected":plan.expected});
        assert!(matches!(plan.reconcile(|_|Ok(receipt.clone())),Ok(Resolution::Cancelled)));
        for key in ["operation_id","state","no_signal","expected"] {
            let mut bad=receipt.clone();bad.as_object_mut().unwrap().remove(key);
            assert!(plan.reconcile(|_|Ok(bad)).is_err());
        }
        let mut partial=receipt.clone();partial["expected"]=json!([plan.expected[0]]);
        assert!(plan.reconcile(|_|Ok(partial)).is_err());
        assert!(plan.reconcile(|_|Err("timeout is not cancellation".into())).is_err());
        assert!(model.space("s").is_some());
    }

    #[test]
    fn pre_ready_report_denial_allows_a_new_operation_without_reconcile_or_ui_cleanup() {
        let (mut model,live)=fixture();
        let plan=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        let dir=std::env::temp_dir().join(&plan.operation_id);
        save_pending(&dir,"core-a",&plan).unwrap();
        let result=plan.execute_guarded(&Value::Null,|params| {
            assert_eq!(params["operation_id"],plan.operation_id);
            Err(CallFailure {kind:Some("close_retry_safe".into()),message:"report requested before READY".into()})
        });
        assert!(matches!(result,Ok(Resolution::Cancelled)));
        assert!(model.space("s").is_some());
        remove_pending(&dir,&plan).unwrap();
        assert!(load_pending(&dir,"core-a").unwrap().is_empty());
        let next=Plan::capture(Target::Space("s".into()),&model,&live).unwrap();
        assert_ne!(next.operation_id,plan.operation_id);
        next.finish(&mut model,next.completed(next.expected.clone())).unwrap();
        assert!(model.space("s").is_none());
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn unknown_or_post_signal_errors_cannot_become_retry_safe_by_message_text() {
        let (model,live)=fixture();
        let plan=Plan::capture(Target::Pane("p".into()),&model,&live).unwrap();
        for kind in [None,Some("close_denied"),Some("timeout"),Some("cancelled"),Some("method_not_found")] {
            let result=plan.execute_guarded(&Value::Null,|_|Err(CallFailure {
                kind:kind.map(str::to_owned),message:"close_retry_safe no signal".into()
            }));
            assert!(result.is_err());
            assert!(model.tab("tab").is_some());
        }
    }

    fn audit_snapshot(plan:&Plan, live:&HashMap<String,Value>)->Value {
        let expected=live.iter().map(|(a,r)|Identity::read(a,r).unwrap()).collect::<Vec<_>>();
        json!({"core_id":"core-a","roots":plan.roots,"expected":expected,"operations":[],
            "targets":live.values().map(|r|json!({"address":r["address"],"pid":r["pid"],"created_ms":r["created_ms"],
                "input_generation":7,"program_started_ms":100,"turn":2,"native_state":"finished","observed_exit":true})).collect::<Vec<_>>()})
    }

    #[test]
    fn natural_exit_requires_actual_witness_and_audited_receipt_never_a_kill() {
        let (mut model,live)=fixture();
        let draft=Plan::audit_draft(Target::Space("s".into()),&model).unwrap();
        assert!(draft.expected.is_empty());
        assert!(draft.completed(vec![]).is_err());
        assert!(draft.execute_guarded(&Value::Null,|_|panic!("natural exit sent a kill")).is_err());
        let plan=draft.audit_snapshot("core-a",audit_snapshot(&draft,&live)).unwrap();
        assert!(plan.audit_only);
        assert!(plan.execute_guarded(&Value::Null,|_|panic!("prepared audit sent a kill")).is_err());
        let rejected=plan.reconcile_guarded(|_|Err(CallFailure{kind:Some("close_denied".into()),message:"missing live binding or acceptance".into()}));
        assert!(rejected.is_err());
        assert!(model.space("s").is_some());
        let result=plan.reconcile(|p| {
            assert_eq!(p["operation_id"],draft.operation_id);
            Ok(json!({"ok":true,"operation_id":plan.operation_id,"terminated":plan.expected}))
        }).unwrap();
        let Resolution::Closed(receipt)=result else {panic!("unexpected cancellation")};
        plan.finish_reconciled(&mut model,Ok(receipt)).unwrap();
        assert!(model.space("s").is_none());
    }

    #[test]
    fn live_unbound_unknown_changed_and_partial_natural_snapshots_preserve_everything() {
        let (model,live)=fixture();
        let draft=Plan::audit_draft(Target::Pane("p".into()),&model).unwrap();
        let good=audit_snapshot(&draft,&live);
        for (field,value) in [("native_state",json!("live")),("observed_exit",json!(false)),
            ("input_generation",Value::Null),("program_started_ms",json!(0)),("turn",Value::Null),("pid",json!(0))] {
            let mut bad=good.clone();bad["targets"][0][field]=value;
            assert!(draft.audit_snapshot("core-a",bad).is_err(),"{field}");
        }
        for field in ["expected","targets","roots","core_id"] {
            let mut bad=good.clone();bad.as_object_mut().unwrap().remove(field);
            assert!(draft.audit_snapshot("core-a",bad).is_err());
        }
        let mut partial=good.clone();partial["targets"].as_array_mut().unwrap().pop();
        assert!(draft.audit_snapshot("core-a",partial).is_err());
        let mut other_operation=good.clone();other_operation["operations"]=json!([{"operation_id":"earlier","acknowledged":false,"cancelled":false}]);
        assert!(draft.audit_snapshot("core-a",other_operation).is_err());
        assert!(model.tab("tab").is_some());
    }

    #[test]
    fn natural_audit_cancellation_does_not_release_an_uncertain_kill_operation() {
        let (model,live)=fixture();
        let draft=Plan::audit_draft(Target::Pane("p".into()),&model).unwrap();
        let plan=draft.audit_snapshot("core-a",audit_snapshot(&draft,&live)).unwrap();
        let safe=||Err(CallFailure{kind:Some("close_retry_safe".into()),message:"audit denied before READY".into()});
        assert!(matches!(plan.reconcile_guarded(|_|safe()),Ok(Resolution::Cancelled)));
        let kill=Plan::capture(Target::Pane("p".into()),&model,&live).unwrap();
        assert!(kill.reconcile_guarded(|_|safe()).is_err());
    }

    #[test]
    fn automatic_selection_policy_does_not_turn_manual_focus_into_a_structural_edit() {
        for name in ["pane.focus","pane.go","space.select","tab.select","agent.focus","terminal.show","layout.select"] {
            assert!(selection_command(name));
            assert!(!structural_command(name),"manual pending focus must remain available: {name}");
        }
        for name in ["tab.move","space.move","pane.swap","pane.close","terminal.park"] { assert!(structural_command(name)); }
        for name in ["terminal.paste","terminal.paste_image","app.quit","window.close"] {
            assert!(!selection_command(name));
        }
    }

    #[test]
    fn old_cores_are_refused_before_creating_an_unrecoverable_operation() {
        for hello in [json!({}),json!({"methods":["terminal.kill"]}),json!({"methods":["terminal.kill_batch"]})] {
            assert!(!supported(&hello));
        }
        assert!(supported(&json!({"methods":["terminal.kill_batch","terminal.close_reconcile"]})));
    }

    #[test]
    fn invalid_retained_evidence_is_not_silently_discarded() {
        let (model,live)=fixture();
        let plan=Plan::capture(Target::Pane("p".into()),&model,&live).unwrap();
        let dir=std::env::temp_dir().join(&plan.operation_id);
        save_pending(&dir,"core-a",&plan).unwrap();
        assert!(save_pending(&dir,"core-a",&plan).is_err());
        let path=dir.join(format!("{}.json",plan.operation_id));
        let text=std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("handoff"));
        std::fs::write(&path,"{unfinished").unwrap();
        assert!(load_pending(&dir,"core-a").is_err());
        std::fs::remove_file(path).unwrap();std::fs::remove_dir(dir).unwrap();
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
        let full=include_str!("../window.rs");
        assert!(full.contains("automatic close pending; selection is held"));
        assert!(full.contains("work.reconcile_guarded("));
        assert!(full.contains("close::save_pending("));
        assert!(full.contains("close::load_pending("));
        for name in ["space.select","tab.next","pane.focus","agent.focus","layout.select","terminal.show"] { assert!(selection_command(name)); }
        assert!(!selection_command("terminal.paste"));
        assert!(!selection_command("terminal.paste_image"));
        assert!(!selection_command("app.quit"));
        assert!(!selection_command("window.close"));
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
        | "pane.split" | "pane.close" | "pane.swap" | "terminal.open" | "terminal.kill" | "terminal.close_reconcile" | "terminal.keep" | "terminal.park"
        | "layout.set" | "layout.reset" | "layout.resize" | "layout.clone" | "layout.switch" | "app.reset")
}

pub(super) fn may_prune(completed: bool, held: bool) -> bool { completed && !held }

pub(super) fn selection_command(name: &str) -> bool {
    name.starts_with("space.") || name.starts_with("tab.") || name.starts_with("pane.") || name.starts_with("agent.") || name.starts_with("layout.")
        || matches!(name,"terminal.show"|"terminal.peek"|"terminal.search"|"mod.open"|"overlay.close"|"app.palette"|"app.settings"|"app.help")
}

pub(super) fn supported(hello: &Value) -> bool {
    ["terminal.kill_batch","terminal.close_reconcile"].iter().all(|method|
        hello["methods"].as_array().is_some_and(|methods|methods.iter().any(|m|m.as_str()==Some(method))))
}
