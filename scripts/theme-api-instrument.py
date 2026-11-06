"""Generate timing-only overlays; applying them is restricted to a CI checkout.

Build/copy the clean application first. The instrumented test executable reports
worker read/hash/parse, worker HostValue construction and UI JS bridge separately.
Neither this script nor its overlays add a production API or optimize behavior.
"""
import argparse
import difflib
import hashlib
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if args.apply and not (
        os.environ.get("GITHUB_ACTIONS") == "true"
        and Path(os.environ.get("GITHUB_WORKSPACE", "")).resolve() == root
    ):
        parser.error("--apply requires the owned GitHub Actions checkout")
    replacements = {
        "src/json_files.rs": (
            "let result = cache.read(Path::new(&job.path), job.hash.as_deref(), job.max_bytes).map(ReadResult::into_host);",
            """let started = std::time::Instant::now();
                let snapshot = cache.read(Path::new(&job.path), job.hash.as_deref(), job.max_bytes);
                let read_hash_parse_us = started.elapsed().as_micros();
                let started = std::time::Instant::now();
                let result = snapshot.map(ReadResult::into_host);
                if std::env::var_os("HERDR_JSON_BENCH").is_some() {
                    eprintln!("json_worker read_hash_parse_us={} native_host_value_us={}", read_hash_parse_us, started.elapsed().as_micros());
                }""",
        ),
        "vendor/gpui-kit/crates/shell/src/engine/quickjs/host_modules.rs": (
            """    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        into_js(ctx, self.0)
    }""",
            """    fn into_js(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let diagnostic = std::env::var_os("HERDR_JSON_BENCH").is_some() && self.0.get("status").is_some();
        let started = std::time::Instant::now();
        let result = into_js(ctx, self.0);
        if diagnostic { eprintln!("json_ui_bridge_us={}", started.elapsed().as_micros()); }
        result
    }""",
        ),
    }
    # Validate both inputs before touching either source file.
    edits = []
    for relative, (old, new) in replacements.items():
        path = root / relative
        raw = path.read_bytes()
        original = raw.decode("utf-8").replace("\r\n", "\n")
        if original.count(old) != 1:
            raise RuntimeError(f"timing overlay input changed: {relative}")
        edits.append((relative, path, raw, original, original.replace(old, new)))
    args.out_dir.mkdir(parents=True, exist_ok=True)
    manifest = {
        "source_sha": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "purpose": "timing-only test instrumentation; clean app binary built before application",
        "applied": args.apply,
        "overlays": [],
    }
    for relative, path, raw, original, modified in edits:
        name = path.name
        (args.out_dir / name).write_text(modified, encoding="utf-8", newline="\n")
        (args.out_dir / (name + ".patch")).write_text("".join(difflib.unified_diff(
            original.splitlines(True), modified.splitlines(True), fromfile=relative, tofile="diagnostic/" + relative,
        )), encoding="utf-8", newline="\n")
        manifest["overlays"].append({
            "path": relative,
            "original_raw_sha256": hashlib.sha256(raw).hexdigest(),
            "original_lf_sha256": hashlib.sha256(original.encode()).hexdigest(),
            "diagnostic_sha256": hashlib.sha256(modified.encode()).hexdigest(),
        })
        if args.apply:
            path.write_text(modified, encoding="utf-8", newline="\n")
    (args.out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(json.dumps(manifest))


if __name__ == "__main__":
    main()
