/// A visibility event must reach the mod while no focus poll or native frame
/// is required: otherwise a hidden mod cannot turn its own timers off.
#[test]
fn mods_see_native_visibility_without_waiting_for_focus_poll() {
    in_own_process("theme_api_visibility");
}

#[gpui::test]
fn theme_api_visibility(cx: &mut gpui::TestAppContext) {
    use serde_json::json;
    if !running_alone("theme_api_visibility") { return; }
    let source = r#"import { View, div } from "gpui-kit";
import { window, shared, setShared, nextCommand } from "herdr";
export default class extends View {
  init(_props, cx) {
    setShared("visible.initial", window().visible);
    cx.spawn(async () => {
      for (;;) {
        if (await nextCommand() === "window.visibility") {
          setShared("visible.events", [...(shared("visible.events") ?? []), window().visible]);
        }
      }
    });
  }
  render() {
    const visible = window().visible;
    if (visible !== this.last) {
      this.last = visible;
      setShared("visible.seen", [...(shared("visible.seen") ?? []), visible]);
    }
    return div();
  }
}"#;
    let layout = json!({ "id":"root", "type":"block", "content":{"mod":"visible-probe"} });
    let (view, cx) = open_window(cx, "visible-probe", source, layout);
    cx.run_until_parked();
    let seen = || crate::mods::shared("visible.seen").unwrap_or(json!([]));
    assert_eq!(seen(), json!([true]));
    assert_eq!(crate::mods::shared("visible.initial"), Some(json!(true)));
    // No advance_clock here: the 400 ms poll must not be the delivery path.
    cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    cx.run_until_parked();
    assert_eq!(seen(), json!([true, false]));
    assert_eq!(crate::mods::shared("visible.events"), Some(json!([false])));
    cx.simulate_visibility_change(gpui::WindowVisibility::Hidden);
    cx.run_until_parked();
    assert_eq!(seen(), json!([true, false]));
    assert_eq!(crate::mods::shared("visible.events"), Some(json!([false])));
    cx.simulate_visibility_change(gpui::WindowVisibility::Visible);
    cx.run_until_parked();
    assert_eq!(seen(), json!([true, false, true]));
    assert_eq!(crate::mods::shared("visible.events"), Some(json!([false, true])));
    cx.deactivate_window();
    cx.update(|window, cx| view.update(cx, |this, cx| this.poll(window, cx)));
    cx.run_until_parked();
    assert_eq!(seen(), json!([true, false, true]), "blur does not hide the window");
}

/// Both copies share one native cache, but their awaited callbacks must resume
/// with their own application generation so saved cx.notify() stays valid.
#[test]
fn json_file_promises_preserve_each_mod_copys_owner() {
    in_own_process("theme_api_json_owners");
}

#[gpui::test]
fn theme_api_json_owners(cx: &mut gpui::TestAppContext) {
    use serde_json::json;
    if !running_alone("theme_api_json_owners") { return; }
    // The real file worker wakes the executor from another OS thread.
    cx.executor().allow_parking();
    let file = crate::mods::app_dir().join("feed.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, r#"{"items":[{"id":1,"text":"owned"}]}"#).unwrap();
    let source = r#"import { View, div } from "gpui-kit";
import { props, readJsonFile, setShared } from "herdr";
export default class extends View {
  init(_props, cx) {
    const {path, id} = props();
    cx.spawn(async (cx) => {
      try {
        const first = await readJsonFile(path);
        cx.notify();
        const next = await readJsonFile(path, {hash: first.hash});
        cx.notify();
        setShared("json.owner." + id, [first.status, first.value.items[0].text, next.status, next.changed, next.value]);
      } catch (error) { setShared("json.owner." + id, String(error)); }
    });
  }
  render() { return div(); }
}"#;
    let layout = json!({"id":"root","type":"row","children":[
        {"id":"a","type":"block","content":{"mod":"json-probe","props":{"id":"a","path":file}}},
        {"id":"b","type":"block","content":{"mod":"json-probe","props":{"id":"b","path":file}}}
    ]});
    let (_view, cx) = open_window(cx, "json-probe", source, layout);
    for _ in 0..200 {
        cx.run_until_parked();
        if crate::mods::shared("json.owner.a").is_some() && crate::mods::shared("json.owner.b").is_some() { break; }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for id in ["a", "b"] {
        assert_eq!(crate::mods::shared(&format!("json.owner.{id}")), Some(json!(["ok", "owned", "ok", false, null])), "{id}");
    }
}
