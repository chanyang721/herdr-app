# Local fork guarded close v1

Contract agreed on 2026-11-05. This adapter is source-only until integration and
review pass. No release or installed application/core update is part of this work.
The native base is app v0.1.13 (93b097f).

See [installation order and compatibility checks](close-guard-install.md).
The core/helper must be reviewed and applied before the matching app.

## RPC and complete target identity

The app sends one `terminal.kill_batch` request:

```json
{"operation_id":"appclose<unique-id>","roots":["t7"],"expected":[{"address":"t7","pid":700,"created_ms":100007}],"context":{"mode":"automatic","handoff":"<one-use local capability>"}}
```

`expected` is the exact set of selected roots and their unkept descendants.
A child with a nonempty `herdr.space` label, and its subtree, are spared unless
explicitly selected as a root. Every address, PID and creation time is mandatory.
The core owns fresh closure and input-generation verification. Unknown, duplicate,
missing-child, identity-mismatched and remote sets are refused as a whole. The
batch endpoint requires its configured external guard; no guard means denial.
There is no fallback to a single-terminal kill.

The app allocates the operation ID before sending the batch. Only
`{ok:true,operation_id:<same operation>,terminated:[exact expected identities]}` counts as success. The app
checks the complete receipt, the connection generation and unchanged close-target
structure before removing anything. Errors, partial results, timeouts and unknown
completion retain the session model and its focus. Conflicting structural edits
and pane drags are held while a close is pending. Commands initially return
`{pending:true,operation:"guarded_close"}`; this is not a completion receipt.

## Automatic context

`pane.close` accepts optional `close_context:{mode:"automatic",handoff:string}` and passes that
object unchanged as RPC `context`. The handoff is a one-use local capability
consumed by the helper; it is never interpreted by the app or recorded in history. Unknown context fields/modes are refused. Internal
pane-to-tab-to-space escalation retains context and never silently becomes a
manual close. Automatic escalation cannot bypass last-space confirmation.
The helper preserves automatic timing, focus, pinning, last-pane and auto_close
requirements; omission is an explicit/manual request. Core transports context as
a generic value and does not implement policy itself.

## Lease handshake

The core is configured with an argv array and frozen local machine identity;
arguments are passed without shell interpolation. The helper command is
`py -3.12 <checkout>/runtime/harness.py close-guard`. The harness contract is
recorded in its `docs/close-guard-v1.md` (implementation b077547).

- Core sends version 1, operation_id, roots, expected, complete:true and the
  native target snapshot, including machine_id and input_generation.
- Helper acquires dispatch.lock, validates reporting/review/acceptance/learning
  and fresh runtime facts, and sends ready/allow:true with the same operation_id,
  a random token and exact identities. It retains the lock.
- Core rechecks the complete closure, identities, input generations and pending
  input/spawn work before the first signal, then fences new work on the batch.
- Core sends committed only after every target is observed ended, or cancelled
  only before any termination signal. Partial signaling uses unclear/EOF.
  Both include the matching operation_id and token.
- Helper independently verifies completion and sends closed/allow:false plus
  the same operation_id, then exits 0. Only closed plus exit 0 is a final ACK.
  Denied, unclear, cancelled, EOF, timeout or nonzero exit are not success.

JSONL lines are bounded to 256 KiB. READY and outcome deadlines are 120 s, final
ACK 30 s; the app RPC wait is bounded to 300 s. No reusable standalone token is
accepted. Direct kill, remote forwarding, automatic parent cleanup and core
shutdown are core-side enforcement responsibilities.

## Recovery after an uncertain result

Before any batch RPC, the app durably writes the immutable plan, caller operation
ID, core ID and original pane/address pairs to `close-pending/<operation>.json`.
No automatic handoff capability is stored. Invalid evidence blocks new closes
rather than being silently discarded. Evidence is retained even after successful
UI cleanup so a failed layout write or app crash cannot lose the recovery key.

The app command `terminal.close_reconcile {operation_id}` also recovers an
operation whose original container moved or vanished. Closing the same original
target again uses only `terminal.close_reconcile` with
its original operation_id, roots and expected identities. It never retransmits a
kill. Only an exact operation-bound receipt allows cleanup. Core/helper must
verify the original journal, actual Exit witnesses and completed audit again.
If the container changed, cleanup removes only original pane/address pairs still
present and preserves new or replaced content. Timeouts, partial receipts, live
targets and unknown records retain the model, holds and operation evidence.
Automatic pending requests hold local focus and selection, while typing/pasting
into the already focused terminal remains available during preparation. Core
input generations invalidate stale approvals and the native READY/commit fence
refuses input only at that boundary; helper report requests use this same rule.

Natural exits use a separate signal-free audit. Existing exit/missing-pane events
request a read-only terminal.close_snapshot with the original roots. The app
checks the actual core ID, complete unique identities, input generations and
all-ended actual Exit witnesses, then durably seals the original UI plan before
calling terminal.close_reconcile with a new audit-only operation. Only the helper
can approve the pre-exit binding and current report/review/acceptance/learning;
the snapshot grants no authority. A pending original operation must use its
original reconciliation instead. Old/unbound/not-found records remain visible.
The app submits one audit per observed address per app run; after reporting
is completed, an explicit close can retry. A read-only snapshot that races actual
Exit recording can be queried again by the existing missing-pane check after a
30-second cooldown; it never submits a kill or silently releases a hold. A live/mixed closure is refused without
changing any pane. An audit plan can never call terminal.kill_batch. A definite denial before READY carries native RPC `error.data.kind=close_retry_safe`.
The app reads that structured kind from the response to its original batch call
and retires its pending plan without any reconciliation attempt or UI cleanup.
The helper can send a missing-report request during preparation: native input
is fenced only from READY/commit, and earlier input changes invalidate approval.
Unknown/EOF/timeout/post-signal errors are never inferred safe from message text.
For recovery of an acknowledged pre-signal cancellation, only an exact operation/identity-bound response
with `ok:false,state:"cancelled",no_signal:true,expected:[exact original]` retires
the pending evidence and releases holds. It preserves the entire model and
requires a new close request. An arbitrary error or timeout never releases holds.

## UI bypass protection and limits

`terminal_ended` and `check_closed` do not prune a session on exit, --rm or missing
record observation alone. Pending/failed batches retain holds. Composition
terminal blocks are also removed only after verified completion. An unverified
natural exit stays visible; observation is not represented as authorized closure.
Closing a mod-only local pane has no terminal kill set.

Shutdown hooks have no retained window transaction and refuse terminal.kill
instead of sending a direct core call. App quit and window close themselves stay
available and preserve sessions. A failure does not restore an externally killed
OS process; it preserves UI/evidence rather than inventing successful termination.

The core must also fence/revalidate policy-relevant input/focus/protection changes
between READY and commitment. Full core/helper integration and observed runtime
termination need independent verification; pure app tests do not establish that
external contract or authorize installation.

## Validation

The real commands/model/layout/mirror/close modules are exercised by pure tests
with a fake RPC transport. Cases cover all target kinds; exact descendants; kept
children; missing identities; PID/start/new-child races; remote denial; full UI
model preservation on refusal/timeout/malformed/partial receipts; foreign and
duplicate receipts; changed target structure; exact success; automatic context;
shutdown/direct-kill bypass checks; and unverified exit pruning. Native full
compilation is checked separately, never inferred from these tests.
