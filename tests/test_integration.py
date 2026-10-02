"""Real Linux engine tests. Set LKJYOLO_TEST_ENGINE to the pinned executable."""
import concurrent.futures
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import unittest

from lkjyolo.cli import result_body
from lkjyolo.core import Engine, Journal, collect, fingerprint, setup

ROOT = Path(__file__).resolve().parents[1]
ENGINE = os.environ.get("LKJYOLO_TEST_ENGINE")


@unittest.skipUnless(sys.platform == "linux" and ENGINE, "requires Linux and LKJYOLO_TEST_ENGINE")
class IntegrationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="lkjyolo-test-")
        self.state = Path(self.tmp.name)
        setup(self.state, ENGINE, self.state)

    def tearDown(self):
        engine = Engine(self.state)
        identity = engine.identity()
        if identity:
            os.kill(identity["pid"], signal.SIGTERM)
            for _ in range(100):
                if not fingerprint(identity["pid"]):
                    break
                time.sleep(.02)
        self.tmp.cleanup()

    def command(self, *args):
        return [sys.executable, "-m", "lkjyolo", "--state", str(self.state), *args]

    def cli(self, *args, ok=True):
        p = subprocess.run(self.command(*args), cwd=ROOT, capture_output=True, text=True, timeout=55)
        if ok:
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        return json.loads(p.stdout)

    def body(self, value):
        self.assertEqual(value["status"], "done", value)
        return result_body(value["result"])

    def test_shell_exit_environment_sudo_available_and_arbitrary_cwd(self):
        value = self.body(self.cli("exec", "--cwd", "/tmp", "--yield-ms", "1000",
                                   "printf '%s\\n' \"$HOME\"; pwd; command -v cargo; exit 7"))
        self.assertEqual(value["exit_code"], 7)
        self.assertIn("/tmp", value["output"])
        self.assertIn(str(Path.home()), value["output"])

    def test_client_disconnect_and_replay_does_not_duplicate(self):
        target = self.state / "counter"
        cmd = f"printf X >> {shlex.quote(str(target))}; sleep 1; printf COMPLETE"
        args = ("exec", "--request-id", "disconnected", "--yield-ms", "4000", cmd)
        client = subprocess.Popen(self.command(*args), cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 8
        while not target.exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertTrue(target.exists())
        client.kill()
        client.communicate(timeout=3)
        receipt = self.cli(*args)
        self.assertIn("COMPLETE", self.body(receipt)["output"])
        self.assertEqual(target.read_text(), "X")
        self.assertEqual(self.cli(*args), receipt)

    def test_concurrent_duplicate_dispatches_once(self):
        target = self.state / "counter"
        cmd = f"printf X >> {shlex.quote(str(target))}; sleep .3; printf READY"
        args = ("exec", "--request-id", "concurrent", cmd)
        with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
            results = list(pool.map(lambda _: self.cli(*args), range(6)))
        self.assertTrue(all(r == results[0] for r in results))
        self.assertEqual(target.read_text(), "X")
        bad = self.cli("exec", "--request-id", "concurrent", "false", ok=False)
        self.assertTrue(bad["isError"])

    def test_interactive_shell_keeps_state_and_accepts_stdin(self):
        sid = self.body(self.cli("start", "--request-id", "shell"))["session_id"]
        self.cli("input", sid, "export LKJYOLO_CHECK=retained; cd /tmp\n")
        output = self.body(self.cli("input", sid, "printf 'STATE=%s\\n' \"$LKJYOLO_CHECK\"; pwd\n"))
        self.assertIn("STATE=retained", output["output"])
        self.assertIn("/tmp", output["output"])
        ended = self.body(self.cli("input", sid, "exit\n"))
        self.assertEqual(ended["exit_code"], 0)

    def test_background_session_wait_and_journal(self):
        start = self.cli("exec", "--yield-ms", "10", "sleep .5; printf FINISHED")
        sid = self.body(start)["session_id"]
        finish = self.body(self.cli("wait", "--seconds", "15", sid))
        self.assertEqual(finish["exit_code"], 0)
        self.assertIn("FINISHED", finish["output"])
        sessions = self.cli("sessions")["sessions"]
        self.assertEqual(sessions[0]["status"], "exited")
        recent = self.cli("requests")
        self.assertEqual(len(recent["requests"]), 2)
        self.assertNotIn("cmd", json.dumps(recent))

    def test_ctrl_c_interrupts_process(self):
        sid = self.body(self.cli("exec", "--yield-ms", "10", "sleep 60"))["session_id"]
        result = self.body(self.cli("interrupt", sid))
        if "exit_code" not in result:
            result = self.body(self.cli("wait", "--seconds", "15", sid))
        self.assertNotEqual(result["exit_code"], 0)

    def test_truncated_output_is_recoverable_exactly(self):
        result = self.body(self.cli("exec", "--max-output-tokens", "40", "python3 -c 'print(\"0123456789\" * 10000)'"))
        self.assertTrue(result["output_truncated"])
        ref = result["output_ref"]
        saved = self.cli("log", ref["path"], "--limit", "200000")
        self.assertEqual(saved["text"], "0123456789" * 10000 + "\n")
        self.assertTrue(saved["eof"])

    def test_invalid_tool_arguments_fail_without_command_execution(self):
        result = self.cli("call", "exec_command", "--json", '{"cmd":"true","unknown_field":1}', ok=False)
        self.assertEqual(result["status"], "error")
        self.assertTrue(result["result"]["isError"])

    def test_runtime_configuration_does_not_restart_active_engine(self):
        self.cli("exec", "true")
        identity = Engine(self.state).identity()
        configured = setup(self.state, ENGINE, self.state)
        self.assertTrue(configured["unchanged"])
        self.assertEqual(Engine(self.state).identity(), identity)
        mode = (self.state / "mcp.sock").stat().st_mode & 0o777
        self.assertEqual(mode, 0o600)

    def test_engine_restart_marks_old_session_and_refuses_input(self):
        sid = self.body(self.cli("start"))["session_id"]
        identity = Engine(self.state).identity()
        os.kill(identity["pid"], signal.SIGTERM)
        for _ in range(100):
            if not fingerprint(identity["pid"]):
                break
            time.sleep(.02)
        result = self.cli("input", sid, "printf UNEXPECTED\n", ok=False)
        self.assertEqual(result["status"], "error")
        self.assertFalse(result["result"]["execution_may_have_started"])
        self.assertIn("stopped/restarted", result["result"]["error"])

    def test_unregistered_session_is_rejected_before_dispatch(self):
        result = self.cli("input", "amber-river", "printf UNEXPECTED\n", ok=False)
        self.assertEqual(result["status"], "error")
        self.assertFalse(result["result"]["execution_may_have_started"])

    def test_installed_zipapp_worker_launch_through_symlink(self):
        subprocess.run([sys.executable, str(ROOT / "scripts/build.py")], check=True, capture_output=True)
        link = self.state / "lkjyolo"
        link.symlink_to(ROOT / "dist/lkjyolo.pyz")
        process = subprocess.run([str(link), "--state", str(self.state), "exec", "--request-id", "zipapp", "printf ZIPAPP"],
                                 cwd="/tmp", capture_output=True, text=True, timeout=15)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(self.body(json.loads(process.stdout))["output"], "ZIPAPP")


if __name__ == "__main__":
    unittest.main()
