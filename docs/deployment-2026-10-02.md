# lkjyolo direct MCP deployment — 2026-10-02

## Scope and ownership

The owner requested a Rust development MCP with broad workspace permissions,
a smaller ChatGPT interface, and deployment through the existing home infrastructure.
The owner explicitly authorized using `lkjsxc/b` and `lkjsxc/gitops` for management.

Live observations superseded the older August inventory: the physical host is
`archserver`, reached through its existing NetBird SSH and authentik SSO. The
public TLS proxy is NGINX inside Incus project `foundation`, instance `forgejo`.
The Coder service is project `services`, instance `coder`, at `10.203.47.120:7080`.
No host management key or socket was copied into a workspace.

## Deployed configuration

- Public MCP: `https://coder.lkjsxc.com/lkjyolo/mcp`.
- OAuth metadata: `https://coder.lkjsxc.com/.well-known/oauth-protected-resource/lkjyolo/mcp`.
- OAuth resource/issuer: existing `https://coder.lkjsxc.com`.
- Gateway: `lkjyolo-gateway.service`, a DynamicUser service inside `foundation/forgejo`.
- Listener: `127.0.0.1:8787`; no new public or inter-container port was opened.
- Upstream TLS DNS override: `coder.lkjsxc.com -> 127.0.0.1`, retaining HTTPS,
  SNI and certificate verification. Public-address hairpin routing is unavailable
  from this container; the original Coder NGINX route remains the upstream.
- Source configuration: `deploy/coder-gateway.json`, `deploy/lkjyolo-gateway.service`,
  and `deploy/nginx-lkjyolo.conf` in this repository.
- Workspace: `lkjsxc/tomato-ocelot-73`; the gateway checks the configured owner ID
  on every authenticated request. Tokens stay in gateway memory and only go to
  the same Coder OAuth resource, never to workspace commands or receipts.
- Runtime version: `0.3.0`, built from commit
  `eb1a722266a7ca0b61f487241c11981427f6f1b1` on the target Linux environment.
- Deployed executable SHA-256:
  `7a5b2b91beaed1ad67d6a665c3df2a222bcae901ec8153504e49c9c6ac3a864e`.
- Plugin: private existing `home-coder` identity, display name `lkjyolo`, version
  `0.4.0`. Existing prompt values, ownership and visibility were preserved.

The previous Home Coder app binding was explicitly replaced with an empty app
binding file and the dedicated remote MCP configuration. The old connection was
not uninstalled. The published package's MCP endpoint, name and implicit skill
activation were read back after upload. Saving the package does not itself prove
that a host has refreshed its cached installation or completed the new connection.

## Backup and change

Before changing the public proxy, the operator captured its configuration,
NGINX configuration dump, Coder service state and instance configuration under
`/root/lkjyolo-change-20261002T141500Z` on the physical host (mode 0700).
These backups remain on the host and are not in Git.

The existing `public-apps.conf` was compared with the backup to detect drift.
A single include was inserted into the `coder.lkjsxc.com` server block and the
candidate was renamed into place. `nginx -t` passed before `systemctl reload nginx`.
Existing Host/SNI guards, TLS settings and Coder/OAuth routes were preserved.

An initial private listener inside `services/coder` was unreachable from the
proxy under the existing network policy. Its new service, binary and configuration
were removed after moving the gateway to the proxy container. No firewall or
Incus isolation rule was relaxed.

The workspace CLI was atomically upgraded from Rust 0.2.0. Its execution engine
remained PID `1116066`, generation `4c36fa6cd2d446d78d6755b8e2c8f659`.
The Coder service remained PID `147893`; Coder, PostgreSQL, NGINX and Forgejo were
still active at the post-deployment observation around `2026-10-02T14:26Z`.

## Validation

- Rust formatting, Clippy and release build passed.
- 20 unit/HTTP/bridge tests pass, including owner checks, token/command separation,
  transport validation, guarded edits, archive contents and bounded inputs.
- 15 opt-in real Linux engine tests pass, including direct MCP execution, file
  editing, result recovery, duplicate replay, PTY state, interruption and logs.
- GitHub Actions passed for the initial implementation branch.
- Gateway health reports `0.3.0`; it listens only on loopback.
- Public missing/invalid credentials return 401 with the OAuth metadata challenge.
- Public OAuth metadata returns the original Coder resource and issuer.
- Coder's existing health route returns 200 through the unchanged TLS route.

Authenticated end-to-end execution through the new ChatGPT connection and natural
tool selection remain unverified. After the Mac was unlocked, the installed plugin
page and local cache showed version 0.4.0. The custom MCP connection form was
prepared with the dedicated endpoint, OAuth DCR, and `coder:all`; completion of
the user's OAuth consent is still pending confirmation. The existing Coder
connector and workspace-side direct MCP operations were verified independently.

On 2026-10-03 (JST), plugin 0.4.1 was saved as
`pluginrel_6abfcd2a9af4819184fd875183a17a2d` and read back successfully. AI-facing
skill instructions and discovery descriptions are now English; Japanese interface
text and request examples are preserved. Skill validation and the existing
archive-preservation test passed. Runtime code, endpoints, permissions, and
invocation policy did not change; no service restart was needed.

## Rollback

On the physical host:

```sh
/root/lkjyolo-change-20261002T141500Z/rollback.sh </dev/null
```

This restores the previous NGINX application configuration, validates it, reloads
NGINX, and disables the added gateway service. It does not restart Coder, Forgejo,
PostgreSQL, or any workspace.

To revert the workspace CLI separately, atomically point
`/home/coder/.local/bin/lkjyolo` back to the preserved
`/home/coder/.local/share/lkjyolo/releases/0.2.0/lkjyolo`. Preserve the journal and
engine process. The former plugin release is `pluginrel_6abfb1fb654481919fd633d621f208d3`;
restoring its source requires publishing it as a new plugin version.
