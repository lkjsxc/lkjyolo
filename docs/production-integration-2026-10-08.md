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

## Completed deployment evidence

PR [#3](https://github.com/lkjsxc/lkjyolo/pull/3) merged at
2026-10-08T04:08:35Z as `124a0cf7821f087ed759b72bc0b039d4a6d1a763`.
Both CI runs for the final PR head completed successfully, including all
39 standard and 23 Linux engine tests, formatting, Clippy and release packaging.

The workspace CLI is now 0.3.3, installed at
`/home/coder/.local/share/lkjyolo/releases/0.3.3/lkjyolo`, with SHA-256
`4e5cee173ee89c751ad68f7eb4facfb8cd88d1a4e31fadc12fc3edd934f5d21f`.
The helper is installed beside it and linked from
`/home/coder/.local/bin/lkjyolo-diagnostics`, with SHA-256
`f4d4647663e1618d7e13e3b644bac8a4b75c98ee3d961040a714a153a58e081d`.
The old 0.3.2 runtime remains available. Installation returned
`configuration.unchanged=true`, and before/after daemon identities matched
exactly: PID `1116066`, generation `4c36fa6cd2d446d78d6755b8e2c8f659`.

A shell created through the public MCP before installation accepted input
afterward, reported `lkjyolo 0.3.3`, and exited with code 0. A separate public
MCP command successfully ran the installed helper against that saved receipt;
it classified the exit correctly without returning stored command output.
Bounded aggregate inspection also succeeded. Unauthenticated MCP initialization
returned 401, OAuth resource metadata retained the Coder issuer/resource, and
Coder's public health endpoint returned `OK`.

The existing private USER-scoped plugin
`plugins_6ac613bccb448191ab031fa8c767d563` was updated to 0.5.1, release
`pluginrel_6ac7174ddcdc81919126f58b574ddbe0`. All seven source files were
read back and matched the candidate. Existing author metadata, full prompts,
app binding, MCP endpoint, scope and audience were preserved. Saving and
read-back do not establish whether every installed client refreshed its cache.

## Gateway rollout completed

NetBird device authentication was renewed by the owner. The existing policy
accepted an authenticated root SSH connection to `archserver`; no sudoers,
SSH policy, group membership or management credential changes were needed.
Host root UUID `ff870ada-399f-48af-a72f-eba83c50aea0` and Incus pool UUID
`68bc4d9d-fffc-471c-b276-f69f73eb84a9` were verified before deployment.

Under the existing host `private/operations.lock`, the prior gateway binary,
configuration and systemd unit were backed up to the root-only directory
`/root/lkjyolo-gateway-rollout-20261008T045600Z`. The candidate SHA-256 and
version were checked inside `foundation/forgejo` before an atomic rename.
Only `lkjyolo-gateway.service` was restarted. At
`2026-10-08T04:56:46.223555Z`, its new PID was `276316`, its version was
0.3.3, and `/lkjyolo/healthz` returned `status=ok, version=0.3.3`.

The deployed binary's SHA-256 matches the workspace CLI:
`4e5cee173ee89c751ad68f7eb4facfb8cd88d1a4e31fadc12fc3edd934f5d21f`.
The prior binary hash was
`4e211c95194a0c299a8d37d436fe7584012cda480c66d5f870c3efb14d51e9a4`.
Gateway configuration and service-file hashes matched before and after:
`664b6c05e647d805b0bf5fc75a97fc27344e55a39c63c761946e06d72e837890`
and `da483f79725cb7647fe5e7fe16d6d55c6938fddd7ccada08b5ab913df8a665dc`.
NGINX, Forgejo and Coder retained PIDs `2598`, `133722` and `147893`.
The gateway still listens only on `127.0.0.1:8787`.

After deployment, authenticated public MCP execution returned the 0.3.3
version and a continuation handle. Empty-input polling collected its remaining
output with exit code 0, and `get_result` recovered the same saved receipt.
Public unauthenticated MCP initialization returned 401; OAuth metadata retained
the existing Coder resource and issuer; Coder health remained `OK`.
The workspace engine retained PID `1116066` and generation
`4c36fa6cd2d446d78d6755b8e2c8f659`.

To restore the preserved gateway binary, run on the physical host as root:

```sh
/root/lkjyolo-gateway-rollout-20261008T045600Z/rollback.sh
```

The rollback acquires the host writer lock, atomically restores the old binary
and restarts only the gateway. It does not restore databases or workspace state.
Pre/post observations and the rollback script remain in the root-only directory.

## Deferred engine capacity activation

The separate 512-continuation engine setting remains staged until an engine
restart can be scheduled without terminating active work. Gateway and CLI
rollout did not restart that shared engine; its effective limit remains 128.
