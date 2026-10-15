# Multi-user rollout — 2026-10-15

Runtime 0.4.0 adds opt-in routing to the authenticated user's own Coder workspace.
The legacy owner-only configuration remains supported and configurations cannot
implicitly widen access. Each tool call lists only `owner=me`, selects a stable
workspace ID, fetches the full workspace, and checks its owner ID before execution.
The existing owner's workspace has an explicit per-user override.

Optional first-use installation downloads a checksum-pinned runtime from the
same HTTPS origin. Existing CLI installations and execution engines are not
replaced or restarted. New installs use the existing pinned engine installer.

Plugin 0.6.0 has a portable build without personal App bindings, plus the
unchanged private-package build for updating the owner's installed plugin.
The public onboarding page uses the existing approval-based account request flow
and quota-controlled Coder template. No account is automatically approved by the
gateway, and no GitHub or external-service credential is shared.

## Verification before deployment

- Formatting and Clippy with warnings denied passed.
- 48 unit/HTTP/package/diagnostic tests passed.
- 23 isolated Linux engine tests passed.
- New integration coverage verifies distinct authenticated users with the same
  request ID, refuses a foreign workspace before execution, and keeps bearer
  credentials out of shell commands.
- Bootstrap tests reject tampered downloads and preserve an existing install.
- Both private and portable ZIPs build; portable manifests contain no App bindings.

Public directory submission is separate from this direct, approval-based rollout.

## Production acceptance

- Deployed gateway 0.4.0 from `5e31466`; NGINX directory-alias correction is
  `b860ff5`. HTTPS onboarding, download and OAuth discovery endpoints return 200.
  An unauthenticated MCP POST returns 401 with the protected-resource metadata URL.
- A synthetic ordinary user completed the existing authentik → Coder HTTPS SSO
  flow, created a tier-1 workspace named `lkjyolo`, and listed the five tools.
  Its own session credential, obtained by SSO, authenticated to the public MCP
  gateway. This did not require an administrator credential in a user workspace.
- The first command installed runtime 0.4.0 and the pinned execution engine in
  that fresh workspace. `exec_command`, `get_result`, `apply_patch`, and
  `read_file` passed live checks. Session input/polling is covered by the isolated
  real-engine suite; this short live command completed without a session.
- The owner and new user submitted the identical request ID and received their
  separate outputs. The owner's repository path was absent in the new workspace.
  The existing owner's CLI (0.3.3) and engine generation remained unchanged.
- The synthetic workspace was deleted, its Coder session revoked, its authentik
  account disabled and password made unusable, and its groups removed.
- Existing private plugin updated in place to 0.6.0 with the same App binding and
  private audience. The downloadable portable ZIP contains no personal App binding.
- GitHub CI passed both push and pull-request runs for `b860ff5`: formatting,
  Clippy, the normal tests, real-engine tests, release build and both ZIP packages.

Runtime SHA-256: `ad7f5d001ea5faead9ae8b94879c3560323e9be3b3d1f8f04db95061a3e48eeb`.
Portable ZIP SHA-256: `9ddfa5643cd8848fb9ac44bf1ecb5215a29c9a573a11407525fec6fe755e3d2c`.

On the management host, the private rollout directory contains the previous
binary/config/NGINX snippet and a guarded `rollback.py`. Its `--check` verifies
platform identity and both previous/current digests; `--apply` restores only this
rollout while holding the infrastructure writer lock. The rollback check passed;
the restore was not executed. NGINX was reloaded, only the gateway restarted,
and Forgejo's process remained running.

User entry point: https://coder.lkjsxc.com/lkjyolo/ . New accounts still require
administrator approval. Each user configures their own OAuth connection; account
or organization support for custom MCP is required. The live test exercised SSO
and the public MCP protocol, not a second person's ChatGPT user interface or a
public directory installation.
