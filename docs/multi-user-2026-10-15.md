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

Deployment and live acceptance evidence will be appended after each step.
Public directory submission is separate from this direct, approval-based rollout.
