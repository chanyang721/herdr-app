# Close guard installation order

This review procedure does not authorize installation or restart. The final
candidate must preserve the personal fork master features and installed core
baseline. The CI-only app base is not a final integrated release.

1. Review and build the integrated core/helper package first. Keep the installed
   core message_sent behavior (6809ff5) and fork-specific functionality. Confirm
   the helper supports original-operation recovery and prebound natural-exit audit.
2. Only after explicit deployment authorization, apply the reviewed core package
   first through its session-preserving update procedure. Configure close_guard
   argv to the reviewed helper. Do not use an unguarded fallback.
3. Before app replacement, read the actual target core hello / hc schema and
   server.status. Check its identity, exact reviewed binary/source receipt and
   these methods: terminal.kill_batch, terminal.close_reconcile, and
   terminal.close_snapshot. A version such as 0.1.18 alone is insufficient:
   released cores at that version may lack this contract. Verify helper machine
   identity matches that core and its frozen guard startup binding.
4. In isolated test homes/cores only, verify full receipts, denial, uncertain
   completion, no-signal recovery and prebound natural-exit audit. Never kill a
   real session as a compatibility probe. Require the final app native build,
   GPUI regression evidence and independent review before app replacement.
5. Apply the app last at the exact reviewed integrated fork commit. If any
   capability/configuration/proof check fails, retain the existing app and
   sessions; do not install this candidate or weaken its guard. An incompatible
   core encountered later causes close commands to refuse before recording a
   new operation, preserving the UI. App/window quit still preserves sessions.

The automatic idle closer leaves the last session of the last workspace for a
person to close manually. This protection is intentional. Signal-free post-exit
audit creates no new termination permission.

Old or unbound natural exits remain visible until a valid pre-exit binding and
actual Exit witness can be audited. Missing records alone never authorize
cleanup. The app snapshot check cannot substitute for helper report, review,
acceptance and learning verification.

## Damaged local evidence

The notice includes the full close-pending operation-file path. Preserve its
bytes and diagnostic message. Restore that same operation from a verified backup
or an independently verified original plan; do not guess roots/identities or
delete the file to bypass the guard. Run app.close_evidence_reload after repair.
The command validates all files and keeps any in-memory uncertain scope; it
does not close a session, release an unknown hold or restart the app. If the
original complete plan cannot be recovered, retain the pane and escalate the
evidence to the core/helper reviewer instead of fabricating cleanup permission.

Completed-operation files are retired after both current UI absence and a
confirmed core layout document prove the terminated identities are no longer
represented. A failed layout write retains the original recovery key.
