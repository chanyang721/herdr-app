# JSON snapshots and visible windows

`herdr.readJsonFile(path, {hash?, maxBytes?})` resolves with
`{status, hash, changed, value, errorKind?}`. Reads, SHA-256, serde parsing and
construction of the native result run on one shared background worker. All
mod copies and windows use that same cache. No JavaScript `JSON.parse` is
required. The ordinary HostModule Promise path retains its application owner;
await it inside the copy's own `cx.spawn` task before notifying that copy.

| status | Meaning | hash / value / changed |
| --- | --- | --- |
| `ok` | Current JSON snapshot | Content SHA256, parsed value, true |
| `ok` | Caller supplied the same hash | Same SHA256, null, false |
| `too_large` | File exceeds this call's cap, including growth while reading | null, null, false |
| `parse_error` | Invalid JSON or invalid UTF-8 | null, null, false |
| `read_error` | Missing/unreadable/non-regular file, or raced snapshot | null, null, false |

`read_error` adds `errorKind:"not_found"` for missing paths and `"io"` for
other read failures. This lets consumers clear a deliberately deleted optional
input without treating permission/transient failures as deletion. Raw OS error
strings and paths are not returned. Other statuses omit `errorKind`.

Omitted `hash` means return the value. A top-level JSON null is still a
successful changed value; inspect `status` and `changed`, not value alone.
`maxBytes` defaults to 8 MiB and accepts integers 1..64 MiB. It applies even
when another caller cached the file with a larger cap. Invalid argument types
throw before a file operation. Paths are local and used as supplied, as for
`readTextFile`; pass `home()`/`appHome()` explicitly rather than a `~` path.

An mtime/size/identity cache hit does not reread, hash or parse the file. A
matching caller hash does not construct or bridge the large value. Metadata
checks still happen on the worker; Promise scheduling/result delivery has
nonzero overhead, so “unchanged” does not mean zero system calls. Combining
this API with `watchFolder(parent)` avoids periodic reads entirely. A changed
value still must be bridged into the JS VM; measured UI delivery cost must be
distinguished from worker parse time.

The worker queue is bounded to 64 requests. The cache keeps at most 32 paths
and an estimated 32 MiB of parsed-tree allocation; larger individual results
can be delivered without being retained. The estimate is not an allocator
RSS limit. A producer that intentionally preserves every compared metadata
field may evade a metadata cache; use normal atomic replacement/mtime updates.
Snapshot checks reject a file that visibly changes while being read.

```js
import { readJsonFile, watchFolder, window, nextCommand } from "herdr";

// Run in this copy's own task; a shared JS watcher must not reuse another
// application's saved native cx/Entity when it notifies multiple copies.
async function reload(path) {
  const next = await readJsonFile(path, {hash: this.hash});
  if (next.status === "ok" && next.changed) {
    this.hash = next.hash;
    this.value = next.value;
  }
}
// In render: reading window() subscribes this mod to the window snapshot.
const moving = window().visible;
// The mod must suspend/resume its motion loop from this state explicitly.
// In this copy's existing nextCommand loop (do not create two competing loops):
const event = await nextCommand();
if (event === "window.visibility") this.setMoving(window().visible);
```

`window().visible` is independent of `window().active` (focus). A retained
native visibility observer publishes snapshot invalidation immediately,
including while no native frame is being presented. It does not wait for the
400 ms focus poll. Reading `window()` in a mod's render subscribes to these
updates using the existing live-reader mechanism. Invalidation alone does not
force JS render in a window the OS has stopped presenting. Therefore the same
transition also delivers `"window.visibility"` through each loaded copy's
existing `nextCommand()` channel, including retained hidden copies. Its own
task can reread `window().visible` and stop timers without a native frame.
Initial `window()` state is published before mod initialization. The API does
not cancel a mod's repeating timers automatically; the consumer owns its
motion policy. Integrate this event into one existing command receiver per
copy; multiple competing receivers split channel messages.

Visibility reflects GPUI's platform signal: minimized/hidden is false;
partially covered windows are visible. macOS reports full occlusion. Windows
currently reports visible/non-minimized, so full coverage by another window
is not detected. X11 full-occlusion reports depend on the window manager;
Wayland depends on compositor support for suspended state. These are platform
limitations, not a focus heuristic, and are not established by simulated tests.

## Development measurements

The isolated Linux diagnostic uses a real file worker and actual mod Promise
delivery. It reports observations, not compositor frame-time guarantees. In an
unoptimized test build (dependency optimization explicitly set to zero), a
2,640,405-byte feed with 6,808 records reached JS in 411 ms; the largest UI
executor pump took 142 ms (idle median 5 microseconds). The same-hash second
request returned null in 5 ms. Separately, a 3,000,001-byte cache fixture took
262 ms for its initial read/hash/parse; 1,000 unchanged metadata hits took
2.8 ms total and performed only one read, hash and parse across the sequence.

These measurements confirm the background/cache behavior and expose the
remaining changed-value bridge cost. They do not establish frame-drop-free
large-file delivery, optimized-build performance, or Windows/macOS behavior.
Run the ignored `json_file_large_payload_ui_delivery_diagnostic` for correctness;
to collect its metrics directly, run the guarded `theme_api_performance` test
with a scratch `HERDR_APP_HOME` and `HERDR_TEST_WINDOW=theme_api_performance`.
