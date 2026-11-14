#!/usr/bin/env python3
"""No-device MCP fixture for testing OpenClaw agent tool isolation."""

import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import stat
import struct
import sys
import zlib


TOOLS = (
    "handkite_status",
    "handkite_observe",
)


def record_start(directory: Path) -> int:
    if not directory.is_absolute():
        raise ValueError("start directory must be absolute")
    parent = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        metadata = os.fstat(parent)
        if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid():
            raise ValueError("start directory owner is invalid")
        if metadata.st_mode & 0o077:
            raise ValueError("start directory is not private")
        marker = f"started-{os.getpid()}-{secrets.token_hex(8)}"
        fd = os.open(
            marker,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
            0o600,
            dir_fd=parent,
        )
        return fd
    finally:
        os.close(parent)


def fixture_image(side: str) -> bytes:
    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data)))

    left = 16 if side == "left" else 80
    rows = bytearray()
    for y in range(64):
        rows.append(0)
        for x in range(128):
            rows.extend((220, 30, 30) if left <= x < left + 32 and 16 <= y < 48
                        else (255, 255, 255))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 128, 64, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


class Fixture:
    def __init__(self, directory: Path):
        self.fd = record_start(directory)
        self.receipt = {
            "version": 1, "initialize_calls": 0, "list_calls": 0,
            "status_calls": 0, "observe_calls": 0, "output_failed": False,
            "clean_eof": False, "tree_label": "fixture-" + secrets.token_hex(4),
            "image_square": secrets.choice(("left", "right")),
        }
        try:
            self.image = base64.b64encode(fixture_image(self.receipt["image_square"])).decode("ascii")
            self.save()
        except BaseException:
            os.close(self.fd)
            raise

    def save(self):
        data = json.dumps(self.receipt, separators=(",", ":")).encode() + b"\n"
        os.lseek(self.fd, 0, os.SEEK_SET)
        if os.write(self.fd, data) != len(data):
            raise OSError("incomplete receipt")
        os.ftruncate(self.fd, len(data))
        os.fsync(self.fd)

    def record(self, counter):
        self.receipt[counter] += 1
        if self.receipt[counter] > 1_000_000:
            raise OSError("fixture call limit")
        self.save()

    def emit(self, stream, reply):
        try:
            stream.write(json.dumps(reply, separators=(",", ":")) + "\n")
            stream.flush()
            return True
        except (OSError, ValueError):
            self.receipt["output_failed"] = True
            self.save()
            return False

    def observation(self, tool):
        metadata = {
            "device": "synthetic-fixture", "revision": 1,
            "window": {"started_unix_ms": 1, "finished_unix_ms": 2},
            "state": {"lock": "unlocked", "wda_ready": True,
                      "foreground_app": "fixture.synthetic"},
        }
        if tool == "handkite_status":
            return {"content": [{"type": "text", "text": json.dumps(metadata)}]}
        tree = '<AppiumAUT><StaticText label="' + self.receipt["tree_label"] + '"/></AppiumAUT>'
        return {"content": [
            {"type": "text", "text": json.dumps({"metadata": metadata, "accessibility_tree": tree})},
            {"type": "image", "mimeType": "image/png", "data": self.image},
        ]}


def response(request: dict, fixture: Fixture):
    identifier = request.get("id")
    if identifier is None:
        return None
    method = request.get("method")
    if method == "initialize":
        fixture.record("initialize_calls")
        result = {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "handkite-scope-fixture", "version": "1"},
        }
    elif method == "ping":
        result = {}
    elif method == "tools/list":
        fixture.record("list_calls")
        result = {
            "tools": [
                {
                    "name": name,
                    "description": "Scope fixture only; no device connection.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "additionalProperties": False,
                    },
                }
                for name in TOOLS
            ]
        }
    elif (
        method == "tools/call"
        and isinstance(request.get("params"), dict)
        and request["params"].get("name") in TOOLS
        and request["params"].get("arguments", {}) == {}
    ):
        tool = request["params"]["name"]
        fixture.record("status_calls" if tool == "handkite_status" else "observe_calls")
        result = fixture.observation(tool)
    else:
        return {
            "jsonrpc": "2.0",
            "id": identifier,
            "error": {"code": -32601, "message": "method or tool unavailable"},
        }
    return {"jsonrpc": "2.0", "id": identifier, "result": result}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--start-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        fixture = Fixture(args.start_dir)
    except (OSError, ValueError):
        print("scope_fixture=marker_unavailable", file=sys.stderr)
        return 2
    try:
        while line := sys.stdin.buffer.readline(64 * 1024 + 1):
            if len(line) > 64 * 1024 or not line.endswith(b"\n"):
                return 2
            try:
                request = json.loads(line)
                if not isinstance(request, dict):
                    continue
                reply = response(request, fixture)
            except (TypeError, ValueError):
                continue
            if reply is not None and not fixture.emit(sys.stdout, reply):
                return 2
        fixture.receipt["clean_eof"] = True
        fixture.save()
        return 0
    except OSError:
        return 2
    finally:
        os.close(fixture.fd)


if __name__ == "__main__":
    raise SystemExit(main())
