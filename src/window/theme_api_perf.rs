/// Manual diagnostic, excluded from normal CI: this measures the real native
/// worker -> host value -> JS object delivery as well as parsing. It deliberately
/// reports observations rather than asserting a frame-time budget. The existing
/// in_own_process helper captures successful child output; inspect measurements
/// by invoking the guarded theme_api_performance leaf directly with its isolated
/// HERDR_APP_HOME and HERDR_TEST_WINDOW=theme_api_performance.
#[test]
#[ignore = "manual real-worker UI delivery diagnostic; inspect its JSON metrics"]
fn json_file_large_payload_ui_delivery_diagnostic() {
    in_own_process("theme_api_performance");
}

#[gpui::test]
fn theme_api_performance(cx: &mut gpui::TestAppContext) {
    use serde_json::json;
    use std::time::{Duration, Instant};

    if !running_alone("theme_api_performance") { return; }
    // This diagnostic intentionally mixes the test UI with real worker I/O.
    cx.executor().allow_parking();

    // Feed-shaped records rather than one giant string: the receiving VM must
    // actually allocate and expose the parsed array and its nested objects.
    let fixture = std::env::var_os("HERDR_JSON_PERF_FIXTURE").map(std::path::PathBuf::from);
    let (bytes, expected_items, expected_spaces, expected_agents) = if let Some(path) = &fixture {
        let bytes = std::fs::read(path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let items = value.get("items").and_then(|v| v.as_array()).map(Vec::len);
        let spaces = value.get("spaces").and_then(|v| v.as_array());
        let agents = spaces.map(|spaces| spaces.iter().map(|s| s.get("agents").and_then(|v| v.as_array()).map_or(0, Vec::len)).sum::<usize>());
        (bytes, items, spaces.map(Vec::len), agents)
    } else {
    let text = "A completed project message with a short summary and source context. ".repeat(3);
    let sample = json!({
        "id": 0, "kind": "message", "role": "assistant", "status": "completed",
        "at": "2026-11-07T12:34:56Z", "text": text,
        "details": { "project": "example", "elapsedMs": 1432, "tags": ["work", "update"] }
    });
    let target_bytes = std::env::var("HERDR_JSON_PERF_BYTES")
        .ok().and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(5 * 1024 * 1024 / 2).clamp(1024, 8 * 1024 * 1024);
    let count = target_bytes / (serde_json::to_vec(&sample).unwrap().len() + 1);
    let items: Vec<_> = (0..count).map(|index| {
        let mut item = sample.clone();
        item["id"] = json!(index);
        item
    }).collect();
    let bytes = serde_json::to_vec(&json!({ "items": items })).unwrap();
    (bytes, Some(count), None, None)
    };
    let file_bytes = bytes.len();
    let file = crate::mods::app_dir().join("large-feed.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, bytes).unwrap();

    let source = r#"import { View, div } from "gpui-kit";
import { props, readJsonFile, nextCommand, setShared } from "herdr";
export default class extends View {
  init(_props, cx) {
    const path = props().path;
    cx.spawn(async () => {
      setShared("json.perf.ready", true);
      for (;;) {
        if (await nextCommand() !== "bench.start") continue;
        try {
          const started = Date.now();
          const first = await readJsonFile(path);
          const firstResolved = Date.now();
          const rows = first.value && first.value.items;
          const firstSummary = {
            status: first.status, changed: first.changed,
            is_array: Array.isArray(rows),
            count: Array.isArray(rows) ? rows.length : null,
            last_id: Array.isArray(rows) && rows.length ? rows[rows.length - 1].id : null,
            spaces_count: Array.isArray(first.value?.spaces) ? first.value.spaces.length : null,
            agents_count: Array.isArray(first.value?.spaces) ? first.value.spaces.reduce((n, s) => n + (Array.isArray(s.agents) ? s.agents.length : 0), 0) : null,
            js_resolution_ms: firstResolved - started
          };
          // This small interim signal lets native code distinguish first payload
          // delivery from the subsequent conditional-hash round trip.
          setShared("json.perf.first", firstSummary);
          const unchangedStarted = Date.now();
          const unchanged = await readJsonFile(path, { hash: first.hash });
          const done = Date.now();
          setShared("json.perf.result", {
            first: firstSummary,
            unchanged: {
              status: unchanged.status, changed: unchanged.changed,
              value_is_null: unchanged.value === null,
              same_hash: unchanged.hash === first.hash,
              js_resolution_ms: done - unchangedStarted
            },
            js_roundtrip_ms: done - started
          });
        } catch (error) {
          setShared("json.perf.result", { error: String(error) });
        }
      }
    });
  }
  render() { return div(); }
}"#;
    let layout = json!({
        "id": "root", "type": "block",
        "content": { "mod": "json-perf-probe", "props": { "path": file } }
    });
    let (_view, cx) = open_window(cx, "json-perf-probe", source, layout);

    // Exclude runtime/module initialization, including registration of the
    // command waiter, from the measurement. No simulated clock advancement.
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if crate::mods::shared("json.perf.ready") == Some(json!(true)) { break; }
        assert!(Instant::now() < ready_deadline, "performance probe did not initialize");
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut baseline = Vec::with_capacity(20);
    for _ in 0..20 {
        let pump = Instant::now();
        cx.run_until_parked();
        baseline.push(pump.elapsed().as_micros());
        std::thread::sleep(Duration::from_millis(2));
    }
    baseline.sort_unstable();

    let started = Instant::now();
    assert!(crate::mods::send_command("json-perf-probe", "bench.start"), "one visible probe must receive the start command");
    let mut pumps = Vec::new();
    let mut first_elapsed_ms = None;
    let result = loop {
        let pump = Instant::now();
        cx.run_until_parked();
        pumps.push(pump.elapsed().as_micros());
        if first_elapsed_ms.is_none() && crate::mods::shared("json.perf.first").is_some() {
            first_elapsed_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
        }
        if let Some(result) = crate::mods::shared("json.perf.result") { break result; }
        if started.elapsed() >= Duration::from_secs(20) {
            break json!({
                "error": "real-worker performance diagnostic timed out after 20 seconds",
                "first": crate::mods::shared("json.perf.first")
            });
        }
        // Let the actual shared worker run; advancing the fake executor clock
        // would mix the 400 ms app poll and synthetic time into this diagnostic.
        std::thread::sleep(Duration::from_millis(2));
    };
    let elapsed_roundtrip_ms = started.elapsed().as_secs_f64() * 1000.0;
    pumps.sort_unstable();
    let p95_index = (pumps.len().saturating_sub(1) * 95) / 100;
    let metrics = json!({
        "diagnostic": "theme_api_large_json_ui_delivery",
        "file_bytes": file_bytes,
        "record_count": expected_items,
        "fixture": fixture.as_deref().and_then(|p| p.file_name()).and_then(|n| n.to_str()),
        "baseline_pump_us": baseline[baseline.len() / 2],
        "baseline_largest_pump_us": baseline.last(),
        "largest_pump_us": pumps.last(),
        "p95_pump_us": pumps[p95_index],
        "pump_samples": pumps.len(),
        "first_delivery_elapsed_ms": first_elapsed_ms,
        "elapsed_roundtrip_ms": elapsed_roundtrip_ms,
        "first": result.get("first"),
        "unchanged": result.get("unchanged"),
        "js_roundtrip_ms": result.get("js_roundtrip_ms"),
        "error": result.get("error")
    });
    eprintln!("{metrics}");
    assert!(result.get("error").is_none(), "{metrics}");
    assert_eq!(result["first"]["status"], "ok", "{metrics}");
    assert_eq!(result["first"]["changed"], true, "{metrics}");
    assert_eq!(result["first"]["is_array"], expected_items.is_some(), "{metrics}");
    assert_eq!(result["first"]["count"], json!(expected_items), "{metrics}");
    assert_eq!(result["first"]["last_id"], json!(expected_items.and_then(|n| n.checked_sub(1))), "{metrics}");
    assert_eq!(result["first"]["spaces_count"], json!(expected_spaces), "{metrics}");
    assert_eq!(result["first"]["agents_count"], json!(expected_agents), "{metrics}");
    assert_eq!(result["unchanged"]["status"], "ok", "{metrics}");
    assert_eq!(result["unchanged"]["changed"], false, "{metrics}");
    assert_eq!(result["unchanged"]["value_is_null"], true, "{metrics}");
    assert_eq!(result["unchanged"]["same_hash"], true, "{metrics}");
}
