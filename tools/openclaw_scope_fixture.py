#!/usr/bin/env python3
"""No-device MCP fixture for testing OpenClaw agent tool isolation."""

import argparse
import json
import os
from pathlib import Path
import secrets
import stat
import sys


TOOLS = (
    "handkite_status",
    "handkite_observe",
)


def record_start(directory: Path) -> None:
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
        os.close(fd)
    finally:
        os.close(parent)


def response(request: dict):
    identifier = request.get("id")
    if identifier is None:
        return None
    method = request.get("method")
    if method == "initialize":
        result = {
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "handkite-scope-fixture", "version": "1"},
        }
    elif method == "ping":
        result = {}
    elif method == "tools/list":
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
        result = {"content": [{"type": "text", "text": "scope_fixture=connected"}]}
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
        record_start(args.start_dir)
    except (OSError, ValueError):
        print("scope_fixture=marker_unavailable", file=sys.stderr)
        return 2
    while line := sys.stdin.buffer.readline(64 * 1024 + 1):
        if len(line) > 64 * 1024 or not line.endswith(b"\n"):
            return 2
        try:
            request = json.loads(line)
            if not isinstance(request, dict):
                continue
            reply = response(request)
        except (TypeError, ValueError):
            continue
        if reply is not None:
            sys.stdout.write(json.dumps(reply, separators=(",", ":")) + "\n")
            sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
