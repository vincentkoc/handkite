import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


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
            self.assertEqual([reply["id"] for reply in replies], [1, 2, 3, 4])
            self.assertEqual(
                [tool["name"] for tool in replies[1]["result"]["tools"]],
                ["handkite_status", "handkite_observe"],
            )
            self.assertEqual(
                replies[2]["result"]["content"][0]["text"],
                "scope_fixture=connected",
            )
            self.assertEqual(replies[3]["error"]["code"], -32601)
            markers = list(Path(directory).iterdir())
            self.assertEqual(len(markers), 1)
            self.assertEqual(markers[0].stat().st_size, 0)

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
