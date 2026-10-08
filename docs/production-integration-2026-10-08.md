# Production integration — 2026-10-08

The owner requested integration into main and production deployment of the
unfinished lkjyolo work. Runtime 0.3.3 combines the already deployed 0.3.2
continuation recovery and Coder transport repair with current main's receipt
ordering fixes and the new read-only diagnostic helper. Plugin 0.5.1 adds
failure attribution and recovery guidance while preserving its identity,
existing prompts, private audience, app binding and OAuth endpoint.

The original dirty diagnostic, quota and gateway checkouts remain intact.
Source integration is performed in `codex/lkjyolo-production-20261008`.
The historical investigation notes describe the pre-integration state;
they are not a statement of the final release's deployment status.

## Verification

Formatting and Clippy with warnings denied passed. The standard test suite
passed 39 tests; the isolated Linux execution-engine suite passed 23 tests.
The engine binary's SHA-256 was verified against the pinned 0.2.1 digest.
Release builds include both `lkjyolo` and `lkjyolo-diagnostics`.

Diagnostic test fixtures explicitly use mode 0700, matching the journal's
requirements regardless of the caller's umask. The overlapping wait/poll
regression now observes the per-session lock and validates serialized delivery
of the same final exit. Existing unit regressions for out-of-order receipts,
malformed exit codes and preserving completed observations remain unchanged.

## Deployment plan

Install 0.3.3 at a new versioned path and atomically switch the CLI symlink.
Install the helper beside it with its own symlink. Preserve 0.3.2 and the
shared engine, journal, logs and active sessions. Do not restart the execution
engine to apply the previously staged 512-continuation setting: restarting
would terminate active processes. Its effective limit remains 128 until a
separately scheduled restart.

Publish the package to the existing private `lkjyolo` plugin, using the
observed release ID to guard against concurrent edits and reading back the
changed files afterward. Update the existing gateway only after authenticated
NetBird access is available, backing up its binary and checking health and
public authentication afterward. Deployment evidence is appended after each
completed step.
