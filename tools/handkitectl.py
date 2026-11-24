#!/usr/bin/env python3
"""Persistent, serial MCP driver. Screen data stays in memory unless printed by the caller."""
import argparse
import json
import pathlib
import os
import select
import subprocess
import sys
import time


class DriverError(Exception):
    """A fixed diagnostic; never includes screen content or request arguments."""


class Session:
    """One MCP child and one owner. No request, including input, is retried."""

    def __init__(self, command=None, timeout=45):
        self.command = command or [str(pathlib.Path.home() / "Library/Application Support/Handkite/releases/current/handkite-mcp"),
                                   "--service", "--capabilities", "observation,navigation,interaction"]
        self.timeout = timeout
        self.process = None
        self.buffer = b""
        self.request_id = 0
        self.revision = None
        self.target_revision = None
        self.targets = []
        self.poisoned = False
        self.control = False

    def __enter__(self):
        self.process = subprocess.Popen(self.command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, bufsize=0)
        os.set_blocking(self.process.stdin.fileno(), False)
        os.set_blocking(self.process.stdout.fileno(), False)
        try:
            self._rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                     "clientInfo": {"name": "handkitectl", "version": "1"}})
            self._write({"jsonrpc": "2.0", "method": "notifications/initialized"}, time.monotonic() + self.timeout)
        except BaseException:
            self.close()
            raise
        return self

    def _write(self, frame, deadline):
        data = (json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
        if len(data) > 64 * 1024:
            raise DriverError("request_too_large")
        descriptor = self.process.stdin.fileno()
        while data:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([], [descriptor], [], remaining)[1]:
                raise DriverError("mcp_write_timeout")
            try:
                written = os.write(descriptor, data)
            except BlockingIOError:
                continue
            if written <= 0:
                raise DriverError("mcp_write_failed")
            data = data[written:]

    def _read(self, deadline):
        descriptor = self.process.stdout.fileno()
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise DriverError("mcp_read_timeout")
            if b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                return json.loads(line)
            if not select.select([descriptor], [], [], remaining)[0]:
                raise DriverError("mcp_read_timeout")
            try:
                data = os.read(descriptor, 65536)
            except BlockingIOError:
                continue
            if not data:
                raise DriverError("mcp_eof")
            self.buffer += data
            if len(self.buffer) > 16 * 1024 * 1024:
                raise DriverError("mcp_frame_limit")

    def _rpc(self, method, params):
        if self.poisoned:
            raise DriverError("session_uncertain_close_without_replay")
        self.request_id += 1
        request_id = self.request_id
        deadline = time.monotonic() + self.timeout
        try:
            self._write({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}, deadline)
            while True:
                message = self._read(deadline)
                if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
                    raise DriverError("mcp_protocol_failed")
                if "id" not in message and isinstance(message.get("method"), str):
                    continue
                if type(message.get("id")) is not int or message["id"] != request_id:
                    raise DriverError("mcp_reply_id_mismatch")
                error = message.get("error")
                if (method == "tools/call" and "result" not in message
                        and isinstance(error, dict) and type(error.get("code")) is int
                        and error["code"] == -32602 and isinstance(error.get("message"), str)):
                    # The local server rejects arguments before device dispatch.
                    # Preserve that proof; transport and other RPC errors remain uncertain.
                    payload = {"code": "InvalidArguments", "delivery": "not_sent"}
                    return {"isError": True, "structuredContent": payload,
                            "content": [{"type": "text", "text": json.dumps(payload)}]}
                if "error" in message or not isinstance(message.get("result"), dict):
                    raise DriverError("mcp_request_failed")
                return message["result"]
        except Exception:
            self.poisoned = True
            self.revision = None
            raise DriverError("mcp_exchange_uncertain_no_replay") from None

    def tools(self):
        return self._rpc("tools/list", {})

    def call(self, tool, arguments=None):
        if arguments is not None and not isinstance(arguments, dict):
            raise DriverError("invalid_arguments")
        args = dict(arguments or {})
        if not isinstance(tool, str) or not tool.startswith("handkite_"):
            raise DriverError("invalid_tool")
        target_action = tool in {"handkite_tap_element", "handkite_text_element"}
        action = tool not in {"handkite_status", "handkite_observe", "handkite_acquire_control", "handkite_release_control"}
        if action:
            if not self.control:
                raise DriverError("explicit_acquire_required")
            if "expected_revision" not in args:
                if self.revision is None:
                    raise DriverError("observe_required")
                args["expected_revision"] = self.revision
        if (action and not target_action) or tool == "handkite_observe":
            if not args or action and "response" not in args:
                options = {"tree_format": "elements", "image": "omit"}
                if self.revision is not None:
                    options["since_revision"] = self.revision
                if action:
                    args["response"] = options
                else:
                    args.update(options)
        if tool == "handkite_observe" and args.get("targets") and self.target_revision is not None:
            args.setdefault("since_revision", self.target_revision)
        started = time.monotonic()
        result = self._rpc("tools/call", {"name": tool, "arguments": args})
        elapsed_ms = round((time.monotonic() - started) * 1000)
        if action or tool == "handkite_observe":
            self.revision = None
        if result.get("isError"):
            # Refusals and uncertain effects need a new explicit observation/decision.
            self.revision = None
            self.target_revision, self.targets = None, []
            payload = result.get("structuredContent")
            if isinstance(payload, dict) and payload.get("code") == "InvalidLease":
                self.control = False
        else:
            payload = result.get("structuredContent")
            if payload is None:
                content = result.get("content", [])
                if content and content[0].get("type") == "text":
                    try:
                        payload = json.loads(content[0]["text"])
                    except (ValueError, TypeError):
                        payload = None
            if isinstance(payload, dict):
                if payload.get("control") == "acquired":
                    self.control = True
                elif payload.get("control") == "released":
                    self.control = False
                observation = payload.get("observation", payload)
                meta = observation.get("metadata", {}) if isinstance(observation, dict) else {}
                if action and not meta:
                    meta = payload.get("after") or {}
                revision = meta.get("revision", payload.get("revision")) if isinstance(meta, dict) else payload.get("revision")
                if type(revision) is int and revision >= 0:
                    self.revision = revision
                    if payload.get("target_only"):
                        self._target_update(payload, revision)
                elif action or tool == "handkite_observe":
                    self.revision = None
                if payload.get("revision_scope") == "navigation" or (isinstance(meta, dict) and meta.get("revision_scope") == "navigation"):
                    self.target_revision, self.targets = None, []
                if action and not target_action or tool in {"handkite_acquire_control", "handkite_release_control"}:
                    self.target_revision, self.targets = None, []
        return {"elapsed_ms": elapsed_ms, "result": result}

    def _target_update(self, payload, revision):
        rows = payload.get("targets")
        if rows is None:
            delta = payload.get("targets_delta")
            if not isinstance(delta, dict) or delta.get("base_revision") != self.target_revision:
                self.target_revision, self.targets, self.revision = None, [], None
                raise DriverError("target_delta_baseline_lost")
            inventory = {row["target_id"]: row for row in self.targets}
            try:
                for target in delta["removed"]:
                    del inventory[target]
                for row in delta["changed"]:
                    if row["target_id"] not in inventory:
                        raise KeyError("target")
                    inventory[row["target_id"]] = row
                for row in delta["added"]:
                    if row["target_id"] in inventory:
                        raise KeyError("target")
                    inventory[row["target_id"]] = row
                rows = list(inventory.values())
            except (KeyError, TypeError):
                self.target_revision, self.targets, self.revision = None, [], None
                raise DriverError("invalid_target_delta") from None
        if not isinstance(rows, list) or len(rows) > 2048:
            self.target_revision, self.targets, self.revision = None, [], None
            raise DriverError("invalid_target_inventory")
        self.targets, self.target_revision = rows, revision

    def observe_targets(self):
        """Opt in to native one-use handles, distinct from presentation eN IDs."""
        return self.call("handkite_observe", {"targets": True, "tree_format": "elements", "image": "omit"})

    def close(self):
        if self.process is None:
            return
        # Closing stdin lets the MCP server release its own lease, even after an
        # uncertain action. Never send another input or start another process.
        try:
            self.process.stdin.close()
        except (OSError, ValueError):
            pass
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                try:
                    self.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    raise DriverError("mcp_cleanup_unproven") from None
        self.process.stdout.close()
        self.process = None

    def __exit__(self, *_):
        self.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=float, default=45)
    parser.add_argument("command", nargs=argparse.REMAINDER,
                        help="optional -- handkite-mcp --service --capabilities observation,navigation,interaction")
    options = parser.parse_args()
    if not 0 < options.timeout <= 240:
        parser.error("timeout must be between 0 and 240 seconds")
    command = options.command[1:] if options.command[:1] == ["--"] else options.command
    try:
        with Session(command or None, options.timeout) as session:
            for line in sys.stdin:
                try:
                    request = json.loads(line)
                    if not isinstance(request, dict) or set(request) - {"tool", "arguments"}:
                        raise DriverError("invalid_driver_request")
                    reply = session.call(request["tool"], request.get("arguments"))
                except (ValueError, TypeError, KeyError):
                    raise DriverError("invalid_driver_request") from None
                print(json.dumps(reply, ensure_ascii=False), flush=True)
                if reply["result"].get("isError"):
                    # A streaming script must not blindly advance past a refusal.
                    return 2
        return 0
    except DriverError as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
