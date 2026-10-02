import json
import os
from pathlib import Path
import tempfile
import unittest

from lkjyolo.cli import decode_json, parse_args, read_log, result_body
from lkjyolo.core import Journal, YoloError, private_dir, write_json


class UnitTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_duplicate_request_preserves_receipt_and_rejects_conflicting_payload(self):
        j = Journal(self.state)
        j.prepare("build-1", "exec_command", {"cmd": "true", "tty": False})
        j.update("build-1", status="done", result='{"structuredContent":{"exit_code":0}}')
        same = j.prepare("build-1", "exec_command", {"tty": False, "cmd": "true"})
        self.assertEqual(same["status"], "done")
        with self.assertRaisesRegex(YoloError, "different arguments"):
            j.prepare("build-1", "exec_command", {"cmd": "false"})
        self.assertEqual(len(j.rows()), 1)

    def test_invalid_request_does_not_create_row(self):
        j = Journal(self.state)
        for key in ("../outside", "", "x\ny", "x" * 121):
            with self.assertRaises(YoloError):
                j.prepare(key, "exec_command", {"cmd": "true"})
        self.assertEqual(j.rows(), [])

    def test_missing_worker_becomes_unknown_never_queued_again(self):
        j = Journal(self.state)
        j.prepare("uncertain", "exec_command", {"cmd": "dangerous mutation"})
        j.update("uncertain", status="running", worker_pid=999999999, worker_fingerprint="missing")
        row = j.reconcile(j.get("uncertain"))
        self.assertEqual(row["status"], "unknown")
        self.assertEqual(j.prepare("uncertain", "exec_command", {"cmd": "dangerous mutation"})["status"], "unknown")

    def test_reconcile_stale_read_does_not_overwrite_completed_receipt(self):
        j = Journal(self.state)
        j.prepare("race", "exec_command", {"cmd": "true"})
        j.update("race", status="running", worker_pid=999999999, worker_fingerprint="missing")
        stale = j.get("race")
        j.update("race", status="done", result='{"structuredContent":{"exit_code":0}}')
        self.assertEqual(j.reconcile(stale)["status"], "done")

    def test_state_rejects_shared_directory_and_symlink(self):
        shared = self.state / "shared"
        shared.mkdir(mode=0o755)
        shared.chmod(0o755)
        with self.assertRaises(YoloError):
            private_dir(shared)
        link = self.state / "link"
        link.symlink_to(self.state, target_is_directory=True)
        with self.assertRaises(YoloError):
            private_dir(link)

    def test_log_ranges_binary_recovery_and_path_escape(self):
        outputs = self.state / "outputs"
        outputs.mkdir()
        log = outputs / "capture"
        log.write_bytes(b"abc\xffdef")
        write_json(self.state / "engine.json", {"output_store_dir": str(outputs)})
        value = read_log(self.state, str(log), 2, 3)
        self.assertEqual(value["next_offset"], 5)
        self.assertEqual(value["base64"], "Y/9k")
        self.assertFalse(value["eof"])
        with self.assertRaises(YoloError):
            read_log(self.state, str(self.state / "engine.json"), 0, 10)
        (outputs / "escape").symlink_to(self.state / "engine.json")
        with self.assertRaises(YoloError):
            read_log(self.state, str(outputs / "escape"), 0, 10)

    def test_json_input_and_error_result_shape(self):
        file = self.state / "args.json"
        file.write_text('{"cmd":"printf \\\"$HOME\\\""}')
        self.assertEqual(decode_json("@" + str(file))["cmd"], 'printf "$HOME"')
        with self.assertRaises(YoloError):
            decode_json("[]")
        self.assertEqual(result_body({"content": [{"type": "text", "text": '{"exit_code":4}'}]}), {"exit_code": 4})


if __name__ == "__main__":
    unittest.main()
