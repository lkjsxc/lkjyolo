"""JSON-only command interface designed for Coder's short-lived bash calls."""

import argparse
import base64
import json
import os
from pathlib import Path
import subprocess
import sys

from . import __version__
from .core import Engine, Journal, YoloError, collect, default_state, run_worker, setup, submit


def bounded_int(low, high):
    def parse(value):
        number = int(value)
        if not low <= number <= high:
            raise argparse.ArgumentTypeError(f"must be between {low} and {high}")
        return number
    return parse


def decode_json(value):
    if value == "-":
        value = sys.stdin.read()
    elif value.startswith("@"):
        value = Path(value[1:]).read_text()
    try:
        result = json.loads(value)
    except ValueError as exc:
        raise YoloError(f"Invalid JSON arguments: {exc}") from exc
    if not isinstance(result, dict):
        raise YoloError("Arguments must be a JSON object")
    return result


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description="lkjyolo: persistent execution via authenticated Coder")
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument("--state", type=Path, default=default_state())
    commands = parser.add_subparsers(dest="action", required=True)
    config = commands.add_parser("setup", help="Configure pinned engine, never interrupt running work")
    config.add_argument("--engine", required=True)
    config.add_argument("--workspace", required=True)
    config.add_argument("--sha256")
    commands.add_parser("doctor", help="Check runtime, sudo and developer tools without changing them")
    commands.add_parser("tools", help="List the live engine tool schemas")

    def request(p):
        p.add_argument("--request-id", help="Reuse the same ID and arguments to recover a request without executing twice")
        p.add_argument("--client-wait", type=bounded_int(0, 45), default=45)
        p.add_argument("--max-output-tokens", type=bounded_int(1, 262144), default=4000)

    run = commands.add_parser("exec", help="Start a shell command; return a session if still running")
    request(run)
    run.add_argument("--cwd")
    run.add_argument("--tty", action="store_true")
    run.add_argument("--yield-ms", type=bounded_int(10, 40000), default=1000)
    run.add_argument("command")
    start = commands.add_parser("start", help="Start a persistent interactive shell or REPL")
    request(start)
    start.add_argument("--cwd")
    start.add_argument("--pipe", action="store_true")
    start.add_argument("command", nargs="?")
    for name in ("input", "poll", "wait", "interrupt"):
        p = commands.add_parser(name)
        request(p)
        p.add_argument("session_id")
        if name == "input":
            p.add_argument("chars", help="Literal input; JSON escapes can be supplied using call instead")
        if name == "wait":
            p.add_argument("--seconds", type=bounded_int(15, 40), default=35)
    call = commands.add_parser("call", help="Invoke an execution tool with exact JSON arguments")
    call.add_argument("tool")
    call.add_argument("--json", required=True, help="JSON object, @file, or - for stdin")
    call.add_argument("--request-id")
    call.add_argument("--client-wait", type=bounded_int(0, 45), default=45)
    result = commands.add_parser("result", help="Read a durable receipt; does not re-execute the request")
    result.add_argument("request_id")
    result.add_argument("--wait", type=bounded_int(0, 45), default=0)
    recent = commands.add_parser("requests", help="Recent receipts (commands and output omitted)")
    recent.add_argument("--limit", type=bounded_int(1, 1000), default=30)
    commands.add_parser("sessions", help="Known sessions with conservative lifecycle status")
    log = commands.add_parser("log", help="Read a bounded byte range from an engine output_ref")
    log.add_argument("path")
    log.add_argument("--offset", type=bounded_int(0, 2**63 - 1), default=0)
    log.add_argument("--limit", type=bounded_int(1, 1048576), default=16384)
    worker = commands.add_parser("_worker", help=argparse.SUPPRESS)
    worker.add_argument("request_id")
    return parser.parse_args(argv)


def result_body(result):
    if isinstance(result.get("structuredContent"), dict):
        return result["structuredContent"]
    for part in result.get("content", []):
        if part.get("type") == "text":
            try:
                value = json.loads(part["text"])
                if isinstance(value, dict):
                    return value
            except (ValueError, KeyError):
                pass
    return {}


def session_summary(state):
    engine = Engine(state)
    identity = engine.identity()
    generation = identity["generation"] if identity else None
    sessions = {}
    journal = Journal(state)
    # All receipts, because a busy long-running session may predate recent calls.
    with journal.db() as db:
        for row in db.execute("SELECT * FROM requests ORDER BY created"):
            row = dict(row)
            args = json.loads(row["arguments"])
            result = json.loads(row["result"]) if row["result"] else {}
            body = result_body(result)
            sid = body.get("session_id") or args.get("session_id")
            if not sid:
                continue
            # IDs can be reused after a restart; generation is part of identity.
            key = (row["generation"], sid)
            item = sessions.setdefault(key, {"session_id": sid, "generation": row["generation"]})
            item["last_request_id"] = row["id"]
            item["last_seen_at"] = row["updated"]
            if "exit_code" in body:
                item.update(status="exited", exit_code=body["exit_code"])
            elif row["generation"] != generation:
                item["status"] = "engine_restarted_or_stopped"
            elif result.get("isError") or row["status"] == "unknown":
                item["status"] = "unknown"
            elif body.get("session_id"):
                item["status"] = "running_at_last_observation"
    return {"sessions": list(sessions.values()), "note": "Observations, not a process liveness guarantee; poll or wait a current-generation session to refresh."}


def read_log(state, path, offset, limit):
    config = json.loads((Path(state) / "engine.json").read_text())
    root = Path(config["output_store_dir"]).resolve()
    path = Path(path).resolve(strict=True)
    if root not in path.parents:
        raise YoloError("log only accepts output_ref files in the engine output store; use Coder file tools for other files")
    with path.open("rb") as f:
        f.seek(offset)
        data = f.read(limit)
        size = os.fstat(f.fileno()).st_size
    value = {"path": str(path), "offset": offset, "next_offset": offset + len(data),
             "size_bytes": size, "eof": offset + len(data) >= size,
             "text": data.decode("utf-8", errors="replace")}
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        value["base64"] = base64.b64encode(data).decode()
        value["encoding_loss"] = True
    return value


def doctor(state):
    import shutil
    engine = Engine(state)
    settings = engine.settings()
    ready = engine.ready()
    sudo = shutil.which("sudo")
    sudo_ok = False
    if sudo:
        sudo_ok = subprocess.run([sudo, "-n", "true"], stdout=subprocess.DEVNULL,
                                 stderr=subprocess.DEVNULL, timeout=5).returncode == 0
    return {"version": __version__, "state": str(state), "workspace": settings["workspace"],
            "engine_ready": ready, "daemon": engine.identity(), "sudo_without_password": sudo_ok,
            "tools": {name: shutil.which(name, path=settings["environment"]["PATH"])
                      for name in ("git", "gh", "python3", "node", "npm", "cargo", "docker", "tmux")},
            "transport": "owner-only Unix socket; authenticated Coder MCP is the remote entrypoint",
            "permissions": "No command allowlist, path sandbox, approval prompt or execution deadline added by lkjyolo",
            "persistence": "Requests/logs survive reconnection; processes survive client disconnects, not engine/workspace restart"}


def dispatch(args):
    state = args.state.expanduser().absolute()
    if args.action == "setup":
        return setup(state, args.engine, args.workspace, args.sha256)
    if args.action == "_worker":
        run_worker(state, args.request_id)
        return None
    if args.action == "doctor":
        return doctor(state)
    if args.action == "tools":
        engine = Engine(state)
        engine.ensure()
        return engine.rpc("tools/list")
    if args.action == "result":
        return collect(state, args.request_id, args.wait)
    if args.action == "requests":
        journal = Journal(state)
        return {"requests": [{k: v for k, v in journal.receipt(journal.reconcile(r)).items() if k != "result"}
                             for r in journal.rows(args.limit)]}
    if args.action == "sessions":
        return session_summary(state)
    if args.action == "log":
        return read_log(state, args.path, args.offset, args.limit)
    if args.action == "call":
        return submit(state, args.tool, decode_json(args.json), args.request_id, args.client_wait)
    values = {"max_output_tokens": args.max_output_tokens}
    if args.action in ("exec", "start"):
        if args.command is not None:
            values["cmd"] = args.command
        if args.cwd:
            values["workdir"] = args.cwd
        if args.action == "exec":
            values.update(tty=args.tty, yield_time_ms=args.yield_ms)
            tool = "exec_command"
        else:
            values["tty"] = not args.pipe
            tool = "start_session"
    else:
        values["session_id"] = args.session_id
        tool = "write_stdin"
        if args.action == "wait":
            values["wait_seconds"] = args.seconds
            tool = "wait_for_exit"
        else:
            values["chars"] = args.chars if args.action == "input" else "\x03" if args.action == "interrupt" else ""
    return submit(state, tool, values, args.request_id, args.client_wait)


def main(argv=None):
    os.umask(0o077)
    try:
        value = dispatch(parse_args(argv))
        if value is not None:
            print(json.dumps(value, ensure_ascii=False, separators=(",", ":")))
            if value.get("isError") or value.get("status") in ("error", "unknown"):
                raise SystemExit(1)
    except (YoloError, OSError, ValueError, subprocess.SubprocessError) as exc:
        print(json.dumps({"isError": True, "error": str(exc)}, ensure_ascii=False))
        raise SystemExit(1)


if __name__ == "__main__":
    main()
