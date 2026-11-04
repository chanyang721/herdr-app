# Local fork close adapter: proposed batch boundary

Status: proposal for agreement with the core and lease CLI owners. The app
dispatcher is not wired until this boundary is agreed. No upstream API change
or installed binary deployment is included in this preparation.

The app source is based on release `v0.1.13` (`93b097f`). Its existing
`space.close`, `tab.close`, `pane.close` and `terminal.kill` must retain the
complete model until one guarded batch has succeeded. Window close and app quit
remain available: they do not themselves end terminals.

## Proposed RPC

One native core request, provisionally `terminal.kill_batch`:

```json
{
  "roots": ["t7"],
  "expected": [
    {"address": "t7", "pid": 700, "created_ms": 100007},
    {"address": "t8", "pid": 800, "created_ms": 100008}
  ]
}
```

`roots` are the explicitly selected terminals. `expected` is the complete exact
closure of roots and their unkept descendants in the app's observed snapshot.
A child with a nonempty `herdr.space` label and its subtree are spared unless
explicitly selected as a root. Address, PID and creation time are mandatory.
The first implementation refuses every remote address/container.

The app currently observes PID and creation time. The core owner must specify
whether caller-supplied input generation is required and, if so, the exact
`terminal.get`/`terminal.list` field and snapshot API that supplies it. An
omitted generation must not be silently treated as proof that input is unchanged.

Proposed success shape (final field names still require agreement):

```json
{
  "ok": true,
  "terminated": [
    {"address": "t7", "pid": 700, "created_ms": 100007},
    {"address": "t8", "pid": 800, "created_ms": 100008}
  ]
}
```

No missing, duplicate, extra or changed identity may count as success. A
partial result, timeout, unsupported method, missing lookup or guard refusal
leaves the model unchanged. There is no fallback to individual kill requests.

## Core and lease responsibilities

1. Resolve and freeze the complete batch, including descendants, and reject any
   difference from the caller's expected identities before ending anything.
2. Acquire the lease CLI's dispatch lock and receive READY for the complete
   batch. Verify reporting, review, acceptance and learning under that lease.
3. Immediately recheck every identity, input generation and child membership.
   Any failure rejects the whole batch, with no per-terminal success prefix.
4. Hold the lease/pipe through the core's completion or cancellation ACK. The
   lock must not be dropped between gate approval and core revalidation/kill.
5. Route direct single-terminal kill through the same guard. Unknown or remote
   implementations fail closed. Define timeout, EOF, cancellation and error
   responses explicitly so callers never mistake a lost reply for success.

## App integration points

- Capture an immutable close plan before model mutation and return a pending
  command response while the batch runs off the UI thread.
- Block conflicting structural edits while the operation is pending and reject
  an obsolete target/core generation before committing its UI change.
- Suppress target pruning in `terminal_ended` and `check_closed` until the batch
  completion has been verified; a stream ending is not a batch approval.
- Only an exact completion receipt can construct the value accepted by the
  close plan's model-commit method.
- `mods.rs` FINAL_KILLS bypasses the normal window dispatcher during shutdown.
  It must use the same guarded batch boundary or refuse the request; it must
  never fall back to unguarded single-terminal kill on an older core.
- Ordinary mod-only pane removal has no terminal batch. Remote container edits
  remain refused in this first local implementation.

## Verification

Pure tests use the real app model, layout and mirror code. Prepared cases cover
all target kinds, complete descendants, missing process identity, PID/creation
TOCTOU, newly added children, spared kept children, unverified remote roots,
core/gate refusal preserving model and focus, changed container content,
successful close scope, and partial/duplicate/extra/foreign completion receipts.
The dispatcher, RPC fake and automatic-pruning tests are still to be connected
after the endpoint and lease contract are agreed.
