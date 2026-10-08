# Failure attribution and non-disruptive diagnostics — 2026-10-08

> This document records the earlier investigation before integration.
> The subsequent release and deployment are tracked in
> [production integration](production-integration-2026-10-08.md).

## Scope and observed interruptions

The owner requested practical improvements to repeated interruptions described as
safety checks. The change separates evidence-based failure handling from host
policy decisions. It does not bypass a refused action, loosen authentication,
change app permissions, or claim to alter OpenAI's internal safety system.

The ChatGPT app-specific permission for lkjyolo MCP was independently inspected
and was already **Allow all actions**. The global default was **Allow low-risk
actions**, but lkjyolo had its own override. Ordinary authenticated workspace
execution succeeded at the beginning of this investigation.

Later in this investigation, the host explicitly refused two calls with this
exact message:

> リクエストの安全性を確認できなかったため、このツールの呼び出しは OpenAI によってブロックされました。

1. `write_stdin`, request ID
   `safety-diagnosis-linux-result-20261008-867bcf`: empty input, waiting for the
   output of the isolated Linux integration tests. No command input or interrupt
   was supplied. The full terminal result is not available to this investigation.
2. `exec_command`, request ID
   `safety-diagnosis-install-and-package-20261008-ad3385`: guarded installation
   of the separate diagnostic helper and packaging of the plugin update. The
   intended operation did not replace the live runtime, restart a service, change
   permissions or expose credentials. Installation/package completion was not
   confirmed. Do not treat a refusal or missing receipt as proof of postconditions.

These refusals are distinct from observed nonzero shell exits. Their internal
classification reason is not exposed. The refused operations were not retried
through another interface. The exact date is 2026-10-08, timezone Asia/Tokyo;
precise host event timestamps were not included in the tool responses.
A single refusal is not evidence that every tool or the whole workspace is
inaccessible. This document update is independent incident reporting, not a
retry of either refused operation.

A separate prior incident involved completed continuations retaining engine
capacity until final output collection. That incident does not explain an
explicit host refusal. Runtime 0.3.2 already includes local continuation recovery;
its uncommitted source must not be overwritten by the older runtime on main.

## Implemented, not deployed

- A separate native `lkjyolo-diagnostics` executable reads the existing journal
  using SQLite read-only/query-only mode and a bounded snapshot. No execution
  socket, worker, command replay, session collection, state reconciliation or
  schema initialization is involved. SQLite may use its normal WAL/shared-memory
  bookkeeping; the program does not modify request records.
- Default output aggregates the latest 100 receipts; the allowed sample is 1–1000.
  `--request-id` inspects one exact ID. Arguments are not selected. Saved result
  bodies up to 1 MiB are parsed internally but never returned. Larger, malformed
  or missing results remain explicitly unavailable rather than successful.
- Output distinguishes queued/running observations, execution uncertainty, tool
  errors, typed zero/nonzero command exits and continuation receipts. `done`
  means a saved response, not command success. Historical session records do not
  establish current process liveness.
- Missing journal entries leave execution **undetermined**. They do not prove
  non-execution, pre-dispatch rejection, or an OpenAI safety decision. Every
  report says `platform_safety_verdict=not_observable` and
  `automatic_replay=false`. Receipt counts are not a block rate or a unique
  command success rate; pre-journal failures and read-only calls are absent.
- Candidate plugin 0.5.1 documents the distinction between explicit host safety
  refusals, OAuth/permission errors, transport/upstream failures, tool failures
  and command exits. A single failed call must not be described as proof the
  whole workspace is inaccessible. Independent authorized work can continue.
  Previously supplied user decisions need not be requested again.
- For ordinary transport uncertainty, retain the original operation ID and inspect
  saved receipts and actual effects. For explicit host refusals, preserve a
  sanitized record of the exact message, timestamp, tool, ID, authorized target
  and intended effect. Do not disguise, rephrase or route around a refusal.

The helper is optional diagnostics, not a required preflight for routine work.
Only the two plugin version fields and the skill instructions change. Plugin
identity, scope, audience, app binding, OAuth endpoint and MCP tool annotations
are unchanged. The old home-coder plugin is outside this change. The plugin
candidate has not been uploaded or published; the inspected stored plugin is 0.5.0.

## Validation actually completed

```sh
cargo fmt --all
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release --bin lkjyolo-diagnostics
```

The standard run passed 38 tests, including 15 new diagnostics tests. Formatting,
Clippy with warnings denied, and the release build completed successfully.
The 16 Linux engine tests are opt-in rather than silently treated as executed by
that standard command. A separate opt-in run was launched; its initial response
showed two passing tests, but its terminal result collection was explicitly
blocked. Do not report all 16 as passed or assume the process is still running.

New tests cover typed exits, legacy receipt envelopes, uncertainty/error priority,
bounded sampling, missing IDs/state, malformed and oversized saved results,
no secret output, unknown field-value sanitization, no dead-worker reconciliation,
no state initialization and sanitized CLI failures.

Live smoke checks using the candidate executable correctly distinguished a known
nonzero shell exit from a saved response and left a never-submitted ID
undetermined. No conclusion about platform false positives can be drawn from
those checks. A latest-100 snapshot is not a safety-block-rate estimate.

## Preserved baseline and unfinished deployment

The candidate checkout is
`/home/coder/workspace/lkjyolo-diagnostics-20261008`, branch
`work/diagnostics-20261008`, based on independently verified main
`4030cef016dfb0f537fdd9646a8135a403d4aba4`. Changes are local and uncommitted;
no push or source integration was performed in this investigation.

The built helper is `target/release/lkjyolo-diagnostics`, SHA-256
`b8bd5442cbbbc001b16853fc2a1af7a7e410f268739287dce5cfdb8295025925`.
Its installation attempt was refused. Installation has not been verified and
must not be advertised as complete. Inspect actual state before any later
installation, since a missing response is not a general non-execution guarantee.

At the last successful baseline verification, the deployed CLI was still 0.3.2,
SHA-256 `e4318bd80875debb0608c8f2116d450a47aec225c807de0cf25ef96fa8cdeafd`.
The shared engine was ready, PID `1116066`, generation
`4c36fa6cd2d446d78d6755b8e2c8f659`. No engine restart, runtime downgrade,
authentication change or app permission change was performed as part of this work.

Do not install the checkout's older `lkjyolo` runtime over deployed 0.3.2.
Concurrent project work, sessions, private output and the journal must remain
intact. The diagnostic executable is separate from the live runtime.

The sanitized findings can accompany a support report, but cannot replace the
host's exact refusal messages. Keep credentials, raw journal files, full
environment dumps and unrelated private command output out of reports.
No report has been submitted to OpenAI Support by this investigation.
