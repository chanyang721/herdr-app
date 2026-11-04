# Local fork guarded close v1

Contract agreed on 2026-11-05. This adapter is source-only until integration and
review pass. No release or installed application/core update is part of this work.
The native base is app v0.1.13 (93b097f).

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
