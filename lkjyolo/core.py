"""Owner-only local transport, daemon lifecycle and durable request receipts.

There is deliberately no command allowlist, path sandbox or approval prompt.
The authenticated Coder user and operating system define authority.
"""

import contextlib
import fcntl
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import stat
import subprocess
import sys
import time
import uuid

from . import __version__

PROTOCOL = "2026-07-28"
TOOLS = {"exec_command", "start_session", "write_stdin", "wait_for_exit"}
TERMINAL = {"done", "error", "unknown"}


class YoloError(Exception):
    pass


def default_state():
    return Path(os.environ.get("LKJYOLO_STATE", str(Path.home() / ".local/state/lkjyolo"))).expanduser().absolute()


def private_dir(path):
    path = Path(path)
    if path.is_symlink():
        raise YoloError(f"State directory must not be a symlink: {path}")
    path.mkdir(parents=True, mode=0o700, exist_ok=True)
    info = path.stat()
    if info.st_uid != os.getuid() or not stat.S_ISDIR(info.st_mode):
        raise YoloError(f"State directory is not owned by this user: {path}")
    if stat.S_IMODE(info.st_mode) & 0o077:
        raise YoloError(f"State directory must have mode 0700: {path}")
    return path


def write_json(path, value):
    path = Path(path)
    tmp = path.with_name(path.name + "." + uuid.uuid4().hex + ".tmp")
    with tmp.open("x", encoding="utf-8") as f:
        os.chmod(tmp, 0o600)
        json.dump(value, f, ensure_ascii=False, indent=2)
        f.write("\n")
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


@contextlib.contextmanager
def locked(path):
    with open(path, "a+b") as lock:
        os.chmod(path, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def fingerprint(pid):
    """Linux process start time plus boot ID: never trust a reused PID."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
        fields = raw[raw.rfind(")") + 2:].split()
        if fields[0] == "Z":
            return None
        boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
        return f"{boot}:{fields[19]}"
    except (OSError, IndexError):
        return None


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path, timeout=50):
        super().__init__("localhost", timeout=timeout)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class Engine:
    def __init__(self, state):
        self.state = private_dir(state)
        self.socket = self.state / "mcp.sock"
        self.config_path = self.state / "settings.json"

    def settings(self):
        try:
            return json.loads(self.config_path.read_text())
        except FileNotFoundError as exc:
            raise YoloError("Not configured. Run lkjyolo setup --engine /absolute/path/to/chatgpt-exec-mcp --workspace /absolute/path first.") from exc

    def ready(self):
        try:
            with contextlib.closing(UnixHTTP(self.socket, timeout=1)) as c:
                c.request("GET", "/readyz")
                r = c.getresponse()
                return r.status == 200 and r.read() == b"ready"
        except (OSError, http.client.HTTPException):
            return False

    def identity(self):
        try:
            value = json.loads((self.state / "daemon.json").read_text())
            if fingerprint(value["pid"]) == value["fingerprint"] and value["fingerprint"]:
                return value
        except (OSError, ValueError, KeyError):
            pass
        return None

    def ensure(self):
        with locked(self.state / "daemon.lock"):
            if self.ready():
                identity = self.identity()
                if identity is None:
                    raise YoloError("Socket is active but has no verified daemon identity; inspect it before recovery.")
                return identity
            if self.identity():
                raise YoloError("The managed engine is alive but unresponsive; inspect doctor before restarting.")
            settings = self.settings()
            executable = Path(settings["engine"])
            if not executable.is_file() or not os.access(executable, os.X_OK):
                raise YoloError(f"Engine is not executable: {executable}")
            actual = hashlib.sha256(executable.read_bytes()).hexdigest()
            if actual != settings["engine_sha256"]:
                raise YoloError("Engine digest changed. Review the binary and run setup with the intended digest.")
            env = dict(settings["environment"])
            with open(self.state / "engine.log", "ab") as log:
                os.chmod(self.state / "engine.log", 0o600)
                child = subprocess.Popen(
                    [str(executable), "--config", str(self.state / "engine.json"), "--listen-unix", str(self.socket)],
                    cwd=settings["workspace"], env=env, stdin=subprocess.DEVNULL,
                    stdout=log, stderr=log, start_new_session=True,
                )
            identity = {"pid": child.pid, "fingerprint": fingerprint(child.pid), "generation": uuid.uuid4().hex,
                        "started_at": time.time(), "engine_sha256": actual}
            write_json(self.state / "daemon.json", identity)
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                if self.ready():
                    return identity
                if child.poll() is not None:
                    raise YoloError(f"Engine exited ({child.returncode}); inspect {self.state / 'engine.log'}")
                time.sleep(.05)
            raise YoloError(f"Engine readiness timed out; inspect {self.state / 'engine.log'}")

    def rpc(self, method, params=None, timeout=50):
        params = dict(params or {})
        params["_meta"] = {
            "io.modelcontextprotocol/protocolVersion": PROTOCOL,
            "io.modelcontextprotocol/clientInfo": {"name": "lkjyolo", "version": __version__},
            "io.modelcontextprotocol/clientCapabilities": {},
        }
        request_id = uuid.uuid4().hex
        payload = json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}).encode()
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                   "MCP-Protocol-Version": PROTOCOL, "Mcp-Method": method}
        if method == "tools/call":
            headers["Mcp-Name"] = params["name"]
        with contextlib.closing(UnixHTTP(self.socket, timeout=timeout)) as c:
            c.request("POST", "/mcp", payload, headers)
            response = c.getresponse()
            raw = response.read(8 * 1024 * 1024 + 1)
            if len(raw) > 8 * 1024 * 1024:
                raise YoloError("Engine response exceeds 8 MiB; outcome unknown. Do not retry the command.")
            if response.status != 200:
                raise YoloError(f"Engine HTTP {response.status}: {raw[:2000].decode(errors='replace')}")
        envelope = json.loads(raw)
        if envelope.get("id") != request_id:
            raise YoloError("Engine returned a mismatched request ID")
        if "error" in envelope:
            return {"isError": True, "error": envelope["error"]}
        return envelope["result"]


class Journal:
    def __init__(self, state):
        self.state = private_dir(state)
        self.path = self.state / "requests.sqlite3"
        with self.db() as db:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS requests (
                    id TEXT PRIMARY KEY, digest TEXT NOT NULL, tool TEXT NOT NULL,
                    arguments TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                    updated REAL NOT NULL, worker_pid INTEGER, worker_fingerprint TEXT,
                    generation TEXT, result TEXT
                );
                CREATE INDEX IF NOT EXISTS requests_created ON requests(created);
            """)
        os.chmod(self.path, 0o600)

    @contextlib.contextmanager
    def db(self):
        db = sqlite3.connect(self.path, timeout=15)
        db.row_factory = sqlite3.Row
        try:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA synchronous=FULL")
            with db:
                yield db
        finally:
            db.close()

    def get(self, request_id):
        with self.db() as db:
            row = db.execute("SELECT * FROM requests WHERE id=?", (request_id,)).fetchone()
        if row is None:
            raise YoloError(f"Unknown request_id: {request_id}")
        return dict(row)

    def prepare(self, request_id, tool, arguments):
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,119}", request_id):
            raise YoloError("request_id must contain 1–120 letters, digits, dots, underscores or hyphens")
        if tool not in TOOLS:
            raise YoloError(f"Unsupported tool: {tool}")
        if not isinstance(arguments, dict):
            raise YoloError("Tool arguments must be a JSON object")
        canonical = json.dumps(arguments, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        digest = hashlib.sha256((tool + "\n" + canonical).encode()).hexdigest()
        with self.db() as db:
            db.execute("INSERT OR IGNORE INTO requests(id,digest,tool,arguments,status,created,updated) VALUES(?,?,?,?,?,?,?)",
                       (request_id, digest, tool, canonical, "queued", time.time(), time.time()))
        row = self.get(request_id)
        if row["digest"] != digest:
            raise YoloError("request_id already belongs to different arguments; use a new ID for a new action")
        return row

    def update(self, request_id, **fields):
        allowed = {"status", "worker_pid", "worker_fingerprint", "generation", "result"}
        if not fields or not set(fields) <= allowed:
            raise ValueError("Invalid journal update")
        fields["updated"] = time.time()
        with self.db() as db:
            db.execute("UPDATE requests SET " + ",".join(f"{key}=?" for key in fields) + " WHERE id=?",
                       (*fields.values(), request_id))

    def rows(self, limit=100):
        with self.db() as db:
            return [dict(r) for r in db.execute("SELECT * FROM requests ORDER BY created DESC LIMIT ?", (limit,))]

    def check_session_generation(self, arguments, generation):
        sid = arguments.get("session_id")
        if not sid:
            return
        # Refuse known stale handles before sending input to a replacement engine.
        with self.db() as db:
            rows = db.execute("SELECT result,generation FROM requests WHERE tool IN ('exec_command','start_session') AND status='done' ORDER BY created DESC")
            for row in rows:
                result = json.loads(row["result"])
                if result.get("structuredContent", {}).get("session_id") == sid:
                    if row["generation"] != generation:
                        raise YoloError("Session belongs to a stopped/restarted engine. Inspect its request receipts; start a new session explicitly.")
                    return
        raise YoloError("Session was not created through this lkjyolo journal. Use its originating client or start a new lkjyolo session.")

    def reconcile(self, row):
        if row["status"] == "running" and fingerprint(row["worker_pid"]) != row["worker_fingerprint"]:
            result = json.dumps({
                "error": "Worker disappeared after dispatch became possible. Inspect sessions, saved logs and command side effects; this request will never be automatically re-executed.",
                "isError": True,
            })
            with self.db() as db:
                db.execute("UPDATE requests SET status='unknown',result=?,updated=? WHERE id=? AND status='running' AND worker_pid=?",
                           (result, time.time(), row["id"], row["worker_pid"]))
            return self.get(row["id"])
        return row

    def receipt(self, row):
        value = {"request_id": row["id"], "status": row["status"], "tool": row["tool"],
                 "created_at": row["created"], "updated_at": row["updated"]}
        if row["generation"]:
            value["generation"] = row["generation"]
        if row["result"]:
            value["result"] = json.loads(row["result"])
        else:
            value["next"] = f"lkjyolo result {row['id']} --wait 45"
        return value


def launcher():
    # Installed zipapps are launched by path; source checkouts use -m.
    main = Path(sys.argv[0]).resolve()
    if main.suffix == ".pyz":
        return [sys.executable, str(main)]
    return [sys.executable, "-m", "lkjyolo"]


def launch_worker(state, request_id):
    with open(Path(state) / "workers.log", "ab") as log:
        os.chmod(Path(state) / "workers.log", 0o600)
        return subprocess.Popen(launcher() + ["--state", str(state), "_worker", request_id],
                                stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                start_new_session=True).pid


def submit(state, tool, arguments, request_id=None, wait=45):
    journal = Journal(state)
    request_id = request_id or uuid.uuid4().hex
    row = journal.prepare(request_id, tool, arguments)
    if row["status"] == "queued":
        launch_worker(state, request_id)
    return collect(state, request_id, wait)


def collect(state, request_id, wait=0):
    journal = Journal(state)
    deadline = time.monotonic() + wait
    while True:
        row = journal.reconcile(journal.get(request_id))
        if row["status"] in TERMINAL or time.monotonic() >= deadline:
            return journal.receipt(row)
        time.sleep(.05)


def run_worker(state, request_id):
    os.umask(0o077)
    journal = Journal(state)
    locks = private_dir(Path(state) / "request-locks")
    with locked(locks / (request_id + ".lock")):
        row = journal.reconcile(journal.get(request_id))
        if row["status"] != "queued":
            return
        journal.update(request_id, status="running", worker_pid=os.getpid(), worker_fingerprint=fingerprint(os.getpid()))
        dispatched = False
        try:
            engine = Engine(state)
            identity = engine.ensure()
            journal.update(request_id, generation=identity["generation"])
            arguments = json.loads(row["arguments"])
            journal.check_session_generation(arguments, identity["generation"])
            timeout = max(50, arguments.get("wait_seconds", 0) + 15, arguments.get("yield_time_ms", 0) / 1000 + 15)
            dispatched = True
            result = engine.rpc("tools/call", {"name": row["tool"], "arguments": arguments}, timeout=timeout)
            journal.update(request_id, status="error" if result.get("isError") else "done", result=json.dumps(result, ensure_ascii=False))
        except Exception as exc:
            journal.update(request_id, status="unknown" if dispatched else "error", result=json.dumps({
                "isError": True, "error": str(exc),
                "execution_may_have_started": dispatched,
                "retry": "Inspect this receipt and side effects; never repeat an uncertain mutation with a new ID." if dispatched else "Resolve the error, then submit a new request ID.",
            }))


def setup(state, executable, workspace, expected_sha=None):
    if sys.platform != "linux":
        raise YoloError("The managed runtime requires Linux (/proc process identity and Unix sockets).")
    engine = Engine(state)
    executable = Path(executable).expanduser().resolve(strict=True)
    workspace = Path(workspace).expanduser().resolve(strict=True)
    if not workspace.is_dir():
        raise YoloError("workspace must be a directory")
    digest = hashlib.sha256(executable.read_bytes()).hexdigest()
    if expected_sha and expected_sha != digest:
        raise YoloError("Engine checksum mismatch")
    version = subprocess.check_output([str(executable), "--version"], text=True).strip()
    if version != "chatgpt-exec-mcp 0.2.1":
        raise YoloError(f"Unsupported engine: {version}; expected 0.2.1")
    # Stable development paths, not the transient Coder SSH caller's environment.
    paths = [str(Path.home() / p) for p in (".local/bin", ".cargo/bin")]
    paths += ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"]
    environment = {"HOME": str(Path.home()), "USER": os.environ.get("USER", "coder"),
                   "PATH": ":".join(dict.fromkeys(paths + os.environ.get("PATH", "").split(":"))),
                   "LANG": os.environ.get("LANG") or "C.UTF-8", "TERM": "xterm-256color"}
    settings = {"version": 1, "engine": str(executable), "engine_sha256": digest,
                "workspace": str(workspace), "environment": environment}
    with locked(engine.state / "daemon.lock"):
        if engine.ready() or engine.identity():
            old = engine.settings()
            if old["engine_sha256"] == digest and old["workspace"] == str(workspace):
                return {"configured": True, "unchanged": True, "state": str(engine.state)}
            raise YoloError("Engine is running; setup will not replace its configuration or terminate active work.")
        write_json(engine.config_path, settings)
        write_json(engine.state / "engine.json", {
            "version": 1, "workspace": str(workspace), "shell": "/bin/bash",
            "output_store_dir": str(engine.state / "outputs"),
            "child_env": {"inherit": list(environment), "rules": []},
            "max_explicit_sessions": 64, "max_exec_continuations": 128,
            "explicit_session_idle_timeout": 604800, "exec_continuation_idle_timeout": 604800,
            "reaper_interval": 60, "output_cap_bytes": 1048576,
            "output_store_retention": 604800, "output_store_max_bytes": 4294967296,
        })
    return {"configured": True, "state": str(engine.state), "engine_version": version,
            "engine_sha256": digest, "workspace": str(workspace), "authority": "caller OS permissions; sudo and Docker remain available"}
