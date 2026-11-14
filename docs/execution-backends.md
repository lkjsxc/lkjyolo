# Execution backends

The default `exec_command` uses `src/pipes.rs`. `tty=true` and the legacy CLI
`start` use the pinned external engine. The two paths share the request journal,
file tools, HTTP MCP transport and advisory warnings.

## Investigation

`src/package.rs` downloads the Linux x86_64 release asset from
[CeraCharlesCC/chatgpt-exec-mcp v0.2.1](https://github.com/CeraCharlesCC/chatgpt-exec-mcp/tree/v0.2.1).
`src/lib.rs` pins version 0.2.1 and SHA-256
`790fc21733c51373d9967931a834325b2e9f49581e3e9187cf7fb54cb62435f6`.
The source is not vendored in lkjyolo. The installer retains the upstream license,
NOTICE and third-party notices. CI independently downloads the same asset.
The tag archive and release asset were fetched for this investigation; the asset
matched the pinned digest. No installed engine or live service was used.

`src/engine.rs` verifies the executable digest, starts a detached engine with
`engine.json`, and sends JSON-RPC MCP HTTP calls over an owner-only Unix socket.
It verifies daemon PID/start-time/boot identity and assigns an engine generation.
It refuses to replace a live unresponsive process or change its configuration.
`src/runtime.rs` launches a detached worker per journaled operation, serializes
execution by request ID, and saves the first response. An interrupted transport
can recover it with `get_result`. The gateway calls the workspace CLI through
Coder, or the same runtime directly for its authenticated local backend.

Before this change both non-terminal execution and PTYs went through that
engine. Upstream's `src/session.rs::spawn_capture_writer` receives tagged stdout
and stderr chunks but calls the same `OutputStore::append` for both. It loses the
stream tag before the MCP result is constructed. Its vendored
`vendor/codex-utils-pty/src/process.rs::exit_code_from_status` folds signals into
128 + signal, making them indistinguishable from explicit exit codes. A gateway
JSON wrapper cannot recover either distinction.

lkjyolo uses these engine capabilities:

| Capability | Integration and retained behavior |
| --- | --- |
| `exec_command` / `start_session` | PTY/explicit sessions; default ordinary commands now use native pipes |
| `write_stdin` / `wait_for_exit` | Serialized terminal input, polling, waits and Ctrl-C; native sessions support polling/waits/cancel with closed stdin |
| PTY | Persistent shells, REPLs, terminal input, terminal-dependent tools |
| truncation / `output_ref` | Engine logs and native per-stream raw refs are read by lkjyolo file tools |
| session quotas | `src/sessions.rs` imports old handles, collects completed sessions on quota errors, archives unread output and prevents handle reuse |
| `list_sessions` | Reconstructed engine observations plus native supervisor observations, with the existing bounded/paged MCP response |
| `get_result` | lkjyolo's SQLite receipts, independent of engine output collection |
| `read_file` / `apply_patch` | Already native in `src/files.rs`; neither requires the engine |

`tests/engine.rs` is an opt-in ignored suite. It covers quota recovery, archived
output, concurrent retries/admission/polls, disconnect recovery, interactive shell
state, cancellation, log retrieval, generation loss and setup preserving an active
engine. CLI execs in that suite now explicitly select PTYs. The PTY log test
expects CRLF, which also demonstrates why pipes are preferable for raw logs.
Native backend and real HTTP tests run without ignores.

## Choice

A native pipe backend for non-interactive execution is the smallest independent
implementation that preserves stream identity and real process status. It uses
existing dependencies (`std`, `libc`, SQLite) and the existing durable worker,
without a second native daemon or another dependency. Keeping the engine for
terminal work retains tested terminal handling and quota recovery.

Replacing the entire engine would require reimplementing PTYs, interactive input,
terminal state and their lifecycle in this session. A thin fork would still need
to change capture storage, output projection and status conversion upstream, then
maintain separate releases, digests and packaging. Neither is needed for ordinary
command execution. Wrapping the old output string cannot undo its information loss.

Pipes preserve byte streams and separate diagnostics from normal output, and give
an LLM unambiguous exit/signal information. They do not preserve cross-stream
interleaving order. Programs can buffer differently when `isatty` is false. Native
stdin is `/dev/null`; use `tty=true` when input or terminal behavior is needed.
PTYs provide terminal input, line editing, REPLs and persistent shell state, but
stdout/stderr share one terminal and may contain echo, CRLF and ANSI sequences.

## Native lifecycle and response contract

The detached request worker starts `/bin/bash -c` in its own process group, with
the configured explicit environment and workspace, or an absolute `workdir`.
It drains both nonblocking pipes fairly in a `poll` loop, with bounded buffers.
SQLite stores supervisor identity, current capture counts and status. After the
yield deadline it saves the first response and continues supervising the process.
Output and state changes are saved immediately; idle duration snapshots are saved
at most once per second, independently of pipe polling. The first response always
uses a fresh snapshot. Concurrent duplicate workers exit without waiting for the
owning worker's request lock, including after that first response is saved.
Thus request status `done` is not command completion. `finished=true` indicates
that the shell exited and both output pipes reached EOF; during termination it
also waits for the command group to disappear or receive KILL. Descendants retaining a
pipe keep the job discoverable. Processes which deliberately detach from the
command group and close its pipes are outside this lifecycle.

Responses contain `stream_mode=pipes`, `stdout`, `stderr`, nullable `exit_code`
and `signal`, monotonic `duration_ms`, `finished`, `timed_out`, `cancelled`,
`truncated` and per-stream `*_truncated` / `*_capture_truncated` flags. A signal
termination has `exit_code=null` and the Linux signal number. Running jobs have
both null. Invalid UTF-8 uses replacement text plus exact `*_base64`; saved logs
always contain the original bytes. The text budget is four raw bytes per token,
split equally between streams. It is an approximation, not a tokenizer count.

Each `*_ref` contains `path`, `offset` immediately after displayed bytes,
`end_offset` for the retained snapshot, `total_bytes` observed so far and
`available`. Read omitted bytes with `read_file(path, offset, limit)` until
`end_offset`. Later polls return newly captured output; they do not replay
omitted bytes from earlier responses. Read from byte zero to recover a full
retained stream, including invalid UTF-8 or text cut mid-character.

Delivery cursors and the request receipt commit in one SQLite transaction while
holding a session lock. Concurrent polls cannot consume the same byte range.
The original execution result is never refreshed on replay: a stable request ID
returns its first saved response, even if it contains a now-finished session ID.
A new poll gets a new request ID. Warnings are saved in the execution receipt and
survive `get_result`. Native capacity is released on exit without a final poll.

Cancellation sets a durable flag; only the owning supervisor signals its own
command group (INT then KILL after 250 ms). Optional `timeout_ms` uses TERM then
KILL. There is no default execution deadline. Capture/bookkeeping errors after
spawn trigger group cleanup; a supervisor that disappears unexpectedly is
reported as unknown, without re-executing. A killed supervisor may leave child
processes alive, so inspect side effects before replacement work. Restarting the
external PTY engine does not affect native commands. Workspace/container restarts
still lose processes, while receipts and retained logs remain.

Native limits are currently constants: 512 active jobs, first 1 MiB per stream,
4 GiB retained logs, and seven-day retention. Capacity is reserved at admission
for both full stream caps. On admission only finished/dead jobs' logs may be
evicted, oldest first; active logs are preserved. Retention/eviction marks refs
unavailable. Output beyond the stream cap is drained and counted but discarded;
`*_capture_truncated` distinguishes this from response-only truncation. Failed
capture reports `capture_error`. Receipts and session metadata have no automatic
expiry, as they are also the idempotency record. Engine logs retain their own
separate limits from `engine.json`.

## Breaking changes and limits

Default results replace `output` / `output_ref` / `output_truncated` with separate
stream fields and refs. Native stdin is closed. Signals have no fabricated exit
code. CLI `tools` now reports the public six-tool interface without starting an
engine. Native `max_output_tokens` and yield validation match the public MCP
limits. `timeout_ms` is optional and rejected with `tty=true`.

The terminal engine keeps merged `output` / `output_ref` and its shell-style exit
code. Its responses are labeled `stream_mode=engine_merged`; `stdout`, `stderr`,
`signal` and total `duration_ms` are null because upstream cannot provide them.
Its existing `call_wall_time_seconds` describes only the tool call. No attempt is
made to infer whether code 143 meant explicit `exit 143` or SIGTERM.

The native backend is Linux-only, as is the managed runtime. Tests use fresh
ports, private throwaway state, test-local shell homes, and copies of the pinned
engine. Production Coder OAuth routing and workspace/container restart behavior
are not exercised by the local HTTP test.
