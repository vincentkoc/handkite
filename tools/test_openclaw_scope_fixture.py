import base64
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zlib

from openclaw_scope_fixture import Fixture, response


FIXTURE = Path(__file__).with_name("openclaw_scope_fixture.py")


class ScopeFixtureTests(unittest.TestCase):
    def test_protocol_and_single_start_marker_without_device_access(self):
        with tempfile.TemporaryDirectory() as directory:
            requests = [
                {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
                {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
                {
                    "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "handkite_status", "arguments": {}},
                },
                {
                    "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": "handkite_home", "arguments": {}},
                },
                {
                    "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": {"name": "handkite_observe", "arguments": {}},
                },
                {
                    "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                    "params": {"name": "handkite_observe", "arguments": {"extra": True}},
                },
            ]
            payload = "".join(json.dumps(request) + "\n" for request in requests)
            result = subprocess.run(
                [sys.executable, "-B", str(FIXTURE), "--start-dir", directory],
                input=payload,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0)
            self.assertEqual(result.stderr, "")
            replies = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual([reply["id"] for reply in replies], [1, 2, 3, 4, 5, 6])
            self.assertEqual(
                [tool["name"] for tool in replies[1]["result"]["tools"]],
                ["handkite_status", "handkite_observe"],
            )
            status = json.loads(replies[2]["result"]["content"][0]["text"])
            self.assertEqual(status["device"], "synthetic-fixture")
            self.assertEqual(status["revision"], 1)
            self.assertEqual(replies[3]["error"]["code"], -32601)
            self.assertEqual(replies[5]["error"]["code"], -32601)
            markers = list(Path(directory).iterdir())
            self.assertEqual(len(markers), 1)
            self.assertEqual(markers[0].stat().st_mode & 0o777, 0o600)
            receipt = json.loads(markers[0].read_text())
            self.assertEqual({key: receipt[key] for key in (
                "initialize_calls", "list_calls", "status_calls", "observe_calls",
                "output_failed", "clean_eof")}, {
                "initialize_calls": 1, "list_calls": 1, "status_calls": 1,
                "observe_calls": 1, "output_failed": False, "clean_eof": True,
            })
            observation, image = replies[4]["result"]["content"]
            payload = json.loads(observation["text"])
            self.assertEqual(payload["metadata"], status)
            self.assertIn(receipt["tree_label"], payload["accessibility_tree"])
            self.assertNotIn(receipt["tree_label"], json.dumps(status))
            self.assertNotIn(receipt["image_square"], observation["text"])
            self.assertEqual(image["mimeType"], "image/png")
            png = base64.b64decode(image["data"], validate=True)
            self.assertEqual(png[:8], b"\x89PNG\r\n\x1a\n")
            size = int.from_bytes(png[33:37], "big")
            self.assertEqual(png[37:41], b"IDAT")
            pixels = zlib.decompress(png[41:41 + size])
            self.assertEqual(len(pixels), 64 * (128 * 3 + 1))
            for side, x in (("left", 32), ("right", 96)):
                index = 32 * (128 * 3 + 1) + 1 + x * 3
                self.assertEqual(pixels[index:index + 3],
                                 bytes((220, 30, 30) if side == receipt["image_square"]
                                       else (255, 255, 255)))

    def test_failed_output_is_recorded_without_transcript(self):
        class BrokenOutput(io.StringIO):
            def flush(self):
                raise BrokenPipeError()

        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            try:
                reply = response({"id": "not-recorded", "method": "tools/call",
                                  "params": {"name": "handkite_status"}}, fixture)
                self.assertFalse(fixture.emit(BrokenOutput(), reply))
                marker, = Path(directory).iterdir()
                receipt = json.loads(marker.read_text())
                self.assertEqual(receipt["status_calls"], 1)
                self.assertTrue(receipt["output_failed"])
                self.assertFalse(receipt["clean_eof"])
                self.assertNotIn("not-recorded", marker.read_text())
            finally:
                os.close(fixture.fd)

    def test_incomplete_request_does_not_claim_clean_eof(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                [sys.executable, "-B", str(FIXTURE), "--start-dir", directory],
                input='{"id":1,"method":"tools/list"}',
                text=True, capture_output=True, check=False,
            )
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, "")
            marker, = Path(directory).iterdir()
            self.assertFalse(json.loads(marker.read_text())["clean_eof"])

    def test_unprivate_marker_directory_refuses_before_protocol(self):
        with tempfile.TemporaryDirectory() as directory:
            os.chmod(directory, 0o755)
            result = subprocess.run(
                [sys.executable, "-B", str(FIXTURE), "--start-dir", directory],
                input='{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n',
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, "")
            self.assertEqual(result.stderr, "scope_fixture=marker_unavailable\n")
            self.assertEqual(list(Path(directory).iterdir()), [])


if __name__ == "__main__":
    unittest.main()
