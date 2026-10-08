# Session quota recovery — 2026-10-07

The pinned 0.2.1 execution engine retains a continuation until its final response
is collected. Completed commands with unread final responses therefore count
toward the 128-session limit. The incident was restored by collecting 124 completed
sessions with empty-input polling; the engine was not restarted.

## Runtime 0.3.1

The workspace CLI now handles the engine's exact quota rejection by collecting
known continuations and retrying the rejected command once, only if collection
freed capacity. The rejection occurs before process creation. Transport errors,
ambiguous outcomes, and other tool errors never trigger command retries.

New commands and collection share an admission lock. Input and collection also
use per-session locks held through the SQLite receipt commit. Known live process
IDs and sessions with an active interaction are skipped. Process metadata is only
a hint: the engine response decides completion. An empty-input poll which returns
an active session is buffered as well, without sending input or an interrupt.
All-live quota exhaustion remains an error, without stopping work.

Collected responses are saved in `managed_sessions` and in `auto-collect-...`
request receipts. Late empty-input polls consume saved responses in order, with
the same request-id replay guarantees as normal execution. Smaller output budgets
split buffered text at UTF-8 boundaries. Truncated raw logs retain their existing
`output_ref` and retention policy. Input is refused while collected output is
pending; the caller is instructed to poll first. Uncertain collection is recorded
and excluded from subsequent automatic collection.

New session handles use `lkj-...` with 128 UUID bits encoded as lowercase letters.
The syntax passes the existing gateway's validation. A durable mapping separates
public handles from reusable engine IDs, so an archived handle cannot address a
later process. Open pre-upgrade sessions are imported and continue under their
existing handles. No engine settings or quota limits were changed.

## Validation

The standard suite passes 21 tests; the opt-in Linux engine suite passes 22 tests.
Seven new engine regressions cover forgotten final responses, retained live
shells, complete truncated logs, all-live quota exhaustion, legacy migration,
concurrent admission and reused IDs, uncertain liveness with buffered output,
and smaller UTF-8 output budgets. Formatting and Clippy with warnings denied pass.
The release binary builds with the locked dependency graph.

The public MCP connection has verified continuation creation, completion, and
receipt recovery using a new `lkj-...` handle. A shell created before upgrading
also accepted input and exited normally after upgrading.

## Deployment and recovery

The CLI is installed at `/home/coder/.local/share/lkjyolo/releases/0.3.1/lkjyolo`,
through the existing `/home/coder/.local/bin/lkjyolo` symlink. The previous 0.3.0
binary is preserved. The installed binary's SHA-256 is
`0f3656e814f8521f5c8bad0615d31c9bdf238f157c6482f3ba91b3f1f6c98665`.
The public gateway and execution engine do not require a
restart for this change. The engine remains PID `1116066`, generation
`4c36fa6cd2d446d78d6755b8e2c8f659`.

The isolated build source is `/home/coder/workspace/lkjyolo-quota-20261007`.
It includes the existing gateway transport repair from the preceding deployment;
that source change was already present before this work. lkjmc and its production
deployment are outside this change.

When reverting the CLI, first finish or recover sessions using 0.3.1: the older
CLI does not understand new handles or buffered responses. Preserve the SQLite
journal, output store, and the 0.3.1 binary for recovery. The journal schema is
additive; request receipts and the old executable are not deleted.

## Follow-up: continuation capacity, runtime 0.3.2

At the owner's request, `max_exec_continuations` was increased from 128 to 512
in the workspace's `engine.json` and in the setup defaults. The separate explicit
session limit remains 64. Runtime 0.3.2 is installed with SHA-256
`e4318bd80875debb0608c8f2116d450a47aec225c807de0cf25ef96fa8cdeafd`.
Fresh setup was checked to produce 512, and the existing setup-preserves-live-engine
integration test, formatting, and release build passed.

The running 0.2.1 engine does not reload configuration. Its PID and generation
still match those above, so its effective continuation limit remains 128 until
restart. Three active engine-owned sessions were observed when staging the change;
restart timing was presented to the owner because restarting would terminate them.
No restart has been performed for this capacity change.
