#!/usr/bin/env python3
"""Fixed, explicitly invoked PIN/Clock/lock supervision; profiles are custody only."""
from __future__ import annotations

import hashlib
import json
import os
import plistlib
import pwd
import re
import signal
import socket
import stat
import struct
import subprocess
import sys
import threading
import time

from _supervised_files import (HeldFile, PathAnchor, Refused, StopSet, digest,
    exact_json, fields, fp, no_acl, pin, proc, process_path, read_bytes, require,
    runtime_lock)

MODES = ("pin-once", "clock-once", "lock-once")
RUNNER_REASONS = frozenset("""InvalidBundleId InvalidTimeout LookupFailed LookupTimedOut
    EndpointBusy EndpointUnproven EndpointCleanupUnproven StartupTimedOut
    PlanStartUnproven AutomationAuthorizationRequired AutomationInitializationFailed
    AutomationWaitTimedOut InitializationFailed BootstrapFailed
    RunnerExited RunnerFailed TaskFailed OwnershipLost ProcessIdentityUnproven RunnerStopUnproven
    process_target_path process_usb_discovery process_usb_deadline process_output_create
    process_command_spawn process_command_deadline process_command_exit process_metadata_read
    process_metadata_limit process_metadata_decode process_metadata_contract process_inventory_schema
    process_inventory_pid process_inventory_path process_inventory_ambiguous process_identity_changed
    process_preexisting process_missing_after_start""".split())
ROLES = {"config": 0o600, "binding": 0o600, "receipt": 0o600,
         "unlock_check": 0o500, "operator": 0o500, "probe": 0o500,
         "daemon": 0o500, "selected_daemon": 0o500, "selected_cli": 0o500,
         "selected_mcp": 0o500, "manifest": 0o400,
         "staged_descriptor": 0o400, "registered_descriptor": 0o400,
         "runtime_lock": 0o600}


def socket_budget(channel, deadline, category):
    remaining = deadline - time.monotonic()
    require(remaining > 0, category)
    channel.settimeout(remaining)


class Profile:
    def __init__(self, fd, expected, mode, home):
        self.fd, self.home, self.mode, self.files = os.dup(fd), home, mode, {}
        self.original = fp(os.fstat(self.fd))
        self.sha = digest(expected)
        try:
            data = self.check()
            self.value = value = exact_json(data)
            fields(value, ("version", "mode", "service_state", "selection", "files"))
            require(type(value["version"]) is int and value["version"] == 1 and
                    value["mode"] == mode and mode in MODES and
                    value["service_state"] == "stopped", "profile_contract")
            fields(value["files"], ROLES)
            self.root = home + "/Library/Application Support/Handkite"
            selection = value["selection"]
            fields(selection, ("target", "fingerprint"))
            self.target, self.link_pin = digest(selection["target"]), pin(selection["fingerprint"])
            fixed = {"config": self.root + "/config/host.json",
                     "runtime_lock": self.root + "/runtime/handkite.lock",
                     "manifest": self.root + "/releases/" + self.target + ".manifest.json",
                     "staged_descriptor": self.root + "/launchd/com.handkite.host.plist",
                     "registered_descriptor": home + "/Library/LaunchAgents/com.handkite.host.plist"}
            fixed.update({role: self.root + "/releases/" + self.target + "/" + name
                          for role, name in (("selected_daemon", "handkited"),
                          ("selected_cli", "handkitectl"), ("selected_mcp", "handkite-mcp"))})
            retained = {}
            for role, permission in ROLES.items():
                entry = value["files"][role]
                fields(entry, ("path", "sha256", "fingerprint"))
                if role in fixed:
                    require(entry["path"] == fixed[role], "fixed_path")
                cap = {"config": 4096, "binding": 4096, "receipt": 131072,
                       "manifest": 16384, "staged_descriptor": 16384,
                       "registered_descriptor": 16384, "runtime_lock": 0}.get(role, 20*1024*1024)
                require(type(entry["path"]) is str, "account_path")
                sha, expected = digest(entry["sha256"]), pin(entry["fingerprint"])
                key = (entry["path"], permission, sha, expected, cap)
                if key not in retained:
                    retained[key] = HeldFile(entry["path"], home, permission, sha, expected, cap)
                self.files[role] = retained[key]
                require(role == "runtime_lock" or self.files[role].original[6] > 0,
                        "empty_input")
            require(self.files["manifest"].sha == self.target, "manifest_digest")
            require(self.files["staged_descriptor"].check() ==
                    self.files["registered_descriptor"].check(), "descriptor_mismatch")
            self.selection = PathAnchor(self.root + "/releases/current", home)
            config = exact_json(self.files["config"].check())
            binding = exact_json(self.files["binding"].check())
            fields(config, ("device_key", "usb_id", "wda_runner_bundle_id"))
            fields(binding, ("schema_version", "device_key", "usb_id"))
            require(type(binding["schema_version"]) is int and binding["schema_version"] == 1 and
                    all(type(binding[k]) is str and 0 < len(binding[k]) <= 256 and
                        binding[k] == config.get(k) for k in ("device_key", "usb_id")) and
                    type(config.get("wda_runner_bundle_id")) is str and
                    0 < len(config["wda_runner_bundle_id"]) <= 255,
                    "device_binding")
            self.config = config
            self.recheck()
        except BaseException:
            self.close()
            raise

    def check(self):
        meta = os.fstat(self.fd)
        require(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.geteuid() and
                meta.st_nlink == 1 and stat.S_IMODE(meta.st_mode) == 0o600 and
                fp(meta) == self.original, "profile_private")
        no_acl(self.fd)
        data = read_bytes(self.fd, 131072)
        require(hashlib.sha256(data).hexdigest() == self.sha and
                fp(os.fstat(self.fd)) == self.original, "profile_changed")
        return data

    def recheck(self):
        self.check()
        for held in dict.fromkeys(self.files.values()):
            held.check()
        self.selection.check()
        require(fp(os.stat("current", dir_fd=self.selection.parent, follow_symlinks=False)) ==
                self.link_pin and os.readlink("current", dir_fd=self.selection.parent) ==
                self.target, "selection_changed")

    def path(self, role):
        return self.value["files"][role]["path"]

    def close(self):
        for held in dict.fromkeys(self.files.values()):
            held.close()
        if hasattr(self, "selection"):
            self.selection.close()
        os.close(self.fd)


class Output:
    def __init__(self, stream):
        self.stream, self.failed = stream, False

    def emit(self, mode, phase, result, dispatches=0, diagnostic=None):
        try:
            value = {"mode": mode, "phase": phase, "result": result,
                     "dispatch_count": dispatches}
            if diagnostic is not None:
                value["diagnostic"] = diagnostic
            self.stream.write(json.dumps(value, separators=(",", ":")) + "\n")
            self.stream.flush()
        except (OSError, ValueError):
            self.failed = True


class Child:
    """Retain one exact child identity and bounded private output; never replay."""
    def __init__(self, args, env, stdin, cwd, check):
        self.args, self.check = args, check
        self.process = subprocess.Popen(args, stdin=stdin, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, cwd=cwd, env=env, close_fds=True, bufsize=0)
        self.birth = None
        self.fault = False
        self.data = {"stdout": bytearray(), "stderr": bytearray()}
        self.threads = []
        for name in self.data:
            thread = threading.Thread(target=self.drain,
                args=(getattr(self.process, name), name), daemon=True)
            self.threads.append(thread)
            thread.start()
        if self.process.poll() is None:
            try:
                self.birth = proc(self.process.pid)[2:]
                self.identity()
            except Exception:
                self.fault = True

    def drain(self, stream, name):
        try:
            while True:
                chunk = os.read(stream.fileno(), 4096)
                if not chunk:
                    break
                if len(self.data[name]) + len(chunk) <= 65536:
                    self.data[name].extend(chunk)
                else:
                    self.fault = True
        except Exception:
            self.fault = True

    def identity(self):
        require(self.process.poll() is None and self.birth is not None, "child_exited")
        observed = proc(self.process.pid)
        require(observed[:2] == (os.geteuid(), os.getpid()) and
                observed[2:] == self.birth and
                process_path(self.process.pid) == self.args[0], "child_identity")
        result = subprocess.run(["/bin/ps", "-p", str(self.process.pid), "-o", "command="],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        require(result.returncode == 0 and result.stdout.decode().strip() == " ".join(self.args),
                "child_argv")
        self.check()

    def wait(self):
        # Do not interrupt a native PIN/restoration/runner-join bracket on timeout.
        while self.process.poll() is None:
            try:
                self.identity()
            except Exception:
                if self.process.poll() is None:
                    self.fault = True
            try:
                self.process.wait(timeout=0.25)
            except subprocess.TimeoutExpired:
                pass
        for thread in self.threads:
            thread.join(5)
        self.fault |= any(thread.is_alive() for thread in self.threads)
        if not any(thread.is_alive() for thread in self.threads):
            self.process.stdout.close()
            self.process.stderr.close()
        return self.process.returncode, bytes(self.data["stdout"]), bytes(self.data["stderr"]), self.fault


class Native:
    def __init__(self, profile):
        self.p = profile
        self.env = {"HOME": profile.home, "PATH": "/usr/bin:/bin:/usr/sbin:/sbin"}
        self.child = None
        self.guard = profile.recheck

    def jobs(self):
        for domain in ("user", "gui"):
            result = subprocess.run(["/bin/launchctl", "print",
                "%s/%d/com.handkite.host" % (domain, os.geteuid())],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, env=self.env, timeout=5)
            require(result.returncode == 113, "job_not_absent")

    def controllers(self):
        result = subprocess.run(["/bin/ps", "-axo", "pid=,comm="],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        require(result.returncode == 0, "process_inventory")
        for line in result.stdout.decode().splitlines():
            parts = line.strip().split(None, 1)
            if len(parts) == 2:
                name = os.path.basename(parts[1]).lower()
                require(not any(term in name for term in ("handkited", "handkite-probe",
                    "handkite-runner", "handkite-mcp", "handkite-preparation",
                    "handkite-unlock-once", "webdriveragent", "xcodebuild", "xctest")),
                    "controller_present")

    def receipt(self):
        held = self.p.files["receipt"]
        held.check()
        os.lseek(held.fd, 0, os.SEEK_SET)
        child = Child([self.p.path("unlock_check"), "--check-stop-state"], self.env,
                      held.fd, self.p.root, self.p.files["unlock_check"].check)
        code, out, err, fault = child.wait()
        require(code == 0 and out == b"stop_snapshot=match\n" and not err and not fault,
                "review_required")
        held.check()

    def native(self):
        env = dict(self.env, HANDKITE_DEVICE_UDID=self.p.config["usb_id"])
        child = Child([self.p.path("probe"), "native-lock"], env, subprocess.DEVNULL,
                      self.p.root, self.p.files["probe"].check)
        code, out, err, fault = child.wait()
        match = re.fullmatch(rb"native_lock=(Locked|Unlocked|Uncertain) unlocked_since_boot=(Some\(true\)|Some\(false\)|None)\n", out)
        require(code == 0 and not err and not fault and match is not None, "native_unproven")
        return match[1].decode(), match[2] == b"Some(true)"

    def endpoint(self, deadline=None):
        def query(message):
            expires = time.monotonic() + 5
            if deadline is not None:
                expires = min(expires, deadline)
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                socket_budget(channel, expires, "endpoint_timeout")
                channel.connect("/var/run/usbmuxd")
                body = plistlib.dumps(dict(ClientVersionString="Handkite supervision",
                    ProgName="handkite", kLibUSBMuxVersion=3, **message))
                socket_budget(channel, expires, "endpoint_timeout")
                channel.sendall(struct.pack("<IIII", len(body)+16, 1, 8, 1) + body)
                def receive(count):
                    data = bytearray()
                    while len(data) < count:
                        socket_budget(channel, expires, "endpoint_timeout")
                        part = channel.recv(count-len(data))
                        require(bool(part), "mux_eof")
                        data.extend(part)
                    return data
                length, version, kind, tag = struct.unpack("<IIII", receive(16))
                require(16 <= length <= 1048576 and (version, kind, tag) == (1, 8, 1), "mux_frame")
                reply = plistlib.loads(receive(length-16))
                require(time.monotonic() < expires, "endpoint_timeout")
                return reply
        devices = query({"MessageType": "ListDevices"}).get("DeviceList", [])
        matches = [entry for entry in devices if entry.get("Properties", {}).get("SerialNumber") ==
                   self.p.config["usb_id"] and entry.get("Properties", {}).get("ConnectionType") == "USB"]
        require(len(matches) == 1, "usb_identity")
        reply = query({"MessageType": "Connect", "DeviceID": matches[0]["DeviceID"],
                       "PortNumber": socket.htons(8100)})
        require(reply.get("MessageType") == "Result" and type(reply.get("Number")) is int,
                "endpoint_unproven")
        return reply["Number"]

    def cleanup(self):
        self.jobs()
        with runtime_lock(self.p.files["runtime_lock"]):
            pass
        deadline = time.monotonic() + 180
        while True:
            require(time.monotonic() < deadline, "cleanup_timeout")
            result = self.endpoint(deadline)
            require(time.monotonic() < deadline, "cleanup_timeout")
            require(result in (0, 3), "endpoint_unproven")
            if result == 3:
                break
            time.sleep(min(0.25, max(0, deadline-time.monotonic())))
        self.controllers()
        self.p.recheck()

    def pin(self, stdin):
        self.child = Child([self.p.path("operator"), "--config", self.p.path("config"),
            "--owned-wda-session", "--temporary-idle-settings", "--supervised-visibility-fallback"],
            self.env, stdin, self.p.root, self.p.files["operator"].check)
        return self.child.wait()

    def clock(self):
        self.child = Child([self.p.path("probe"), "touch-trial-owned-auto-clock", "--config",
            self.p.path("config"), "--transport", "w3c"], self.env, subprocess.DEVNULL,
            self.p.root, self.p.files["probe"].check)
        code, out, err, fault = self.child.wait()
        allowed = all(re.fullmatch(rb"touch_trial_owned_step=[a-z_0-9]+ elapsed_us=(?:[0-9]+|none) outcome=[a-z_]+ reason=[a-z_]+|touch_trial_owned_auto_clock=[a-z_]+(?: reason=[a-z_]+)?", line)
                      for line in err.splitlines())
        require(code == 0 and out == b"touch_trial_owned_auto_clock=confirmed transport=w3c\n" and
                allowed and not fault, "clock_unproven")

    def lock(self):
        child = self.child = Child([self.p.path("daemon"), "--owned-wda"], self.env,
            subprocess.DEVNULL, self.p.root, self.p.files["daemon"].check)
        deadline, lease, success = time.monotonic() + 180, None, False
        def rpc(value):
            child.identity()
            require(not child.fault, "daemon_fault")
            path = self.p.root + "/runtime/handkite.sock"
            before = os.lstat(path)
            require(stat.S_ISSOCK(before.st_mode) and before.st_uid == os.geteuid() and
                    before.st_nlink == 1 and stat.S_IMODE(before.st_mode) == 0o600, "socket_policy")
            owners = subprocess.run(["/usr/sbin/lsof", "-t", "--", path],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
            require(owners.returncode == 0 and owners.stdout.split() ==
                    [str(child.process.pid).encode()], "socket_owner")
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                expires = min(deadline, time.monotonic() + 60)
                socket_budget(channel, expires, "action_deadline")
                channel.connect(path)
                require(fp(os.lstat(path)) == fp(before), "socket_changed")
                socket_budget(channel, expires, "action_deadline")
                channel.sendall(json.dumps(value, separators=(",", ":")).encode() + b"\n")
                channel.shutdown(socket.SHUT_WR)
                data = bytearray()
                while not data.endswith(b"\n"):
                    socket_budget(channel, expires, "action_deadline")
                    part = channel.recv(65536)
                    require(bool(part) and len(data)+len(part) <= 16*1024*1024, "rpc_frame")
                    data.extend(part)
                child.identity()
                require(not child.fault, "daemon_fault")
                reply = exact_json(data)
                require(type(reply) is dict, "rpc_object")
                require(time.monotonic() < expires, "action_deadline")
                return reply
        try:
            while b"handkited ready\n" not in child.data["stderr"]:
                child.identity()
                require(not child.fault and time.monotonic() < deadline, "ready_unproven")
                time.sleep(0.1)
            self.p.recheck()
            reply = rpc({"method": "acquire", "owner": "test", "ttl_ms": 120000})
            require(reply.get("type") == "acquired" and type(reply.get("lease")) is str and
                    re.fullmatch(r"[0-9a-f]{32}", reply["lease"]), "lease_unproven")
            lease = reply["lease"]
            self.guard()
            reply = rpc({"method": "act", "owner": "test", "lease": lease, "request": {
                "device": self.p.config["device_key"], "expected_revision": None,
                "action": {"type": "lock"}}})
            require(lock_confirmed(reply), "lock_unproven")
            success = True
        finally:
            if lease is not None and child.process.poll() is None:
                try:
                    require(rpc({"method": "release", "owner": "test", "lease": lease}).get("type") ==
                            "released", "release_unproven")
                except Exception:
                    success = False
            if child.process.poll() is None:
                try:
                    child.identity()
                    child.process.send_signal(signal.SIGTERM)
                except Exception:
                    child.fault = True
            code, out, err, fault = child.wait()
            require(success and code == 0 and not out and err == b"handkited ready\n" and
                    not fault, "lock_cleanup_unproven")


def lock_confirmed(reply):
    return (reply.get("type") == "acted" and type(reply.get("receipt")) is dict and
            reply["receipt"].get("action_kind") == "lock" and
            reply["receipt"].get("accepted") is True and reply.get("native_lock") == "locked" and
            reply.get("effect") == "lock_confirmed")


def pin_result(result):
    code, out, err, fault = result
    submitted = code == 0 and out == b"unlock_idle=code_submitted\n" and not err and not fault
    zero = code == 0 and out == b"unlock_idle=already_unlocked\n" and not err
    zero |= code == 1 and not out and bool(re.fullmatch(
        rb"handkite-unlock-once: unlock_idle=(?:disabled|refused(?: reason=(?:service_layout|protected_config_required|service_ownership|protected_config_unavailable|runner_unconfigured|gate_unavailable|runner_unavailable))?)\n", err))
    runner = re.fullmatch(
        rb"handkite-unlock-once: unlock_idle=refused reason=runner_unavailable runner=([A-Za-z_]+)\n", err)
    zero |= code == 1 and not out and bool(runner) and runner[1].decode("ascii") in RUNNER_REASONS
    return submitted, zero and not fault


def pin_diagnostic(result):
    code, out, err, fault = result
    summary = {"exit_code": code if type(code) is int and -255 <= code <= 255 else None,
               "child_integrity": "unproven" if fault else "checked",
               "reported_outcome": "unclassified"}
    if not err and out in (b"unlock_idle=code_submitted\n", b"unlock_idle=already_unlocked\n"):
        summary["reported_outcome"] = out.removeprefix(b"unlock_idle=").strip().decode("ascii")
        return summary
    if out or not err.startswith(b"handkite-unlock-once: unlock_idle="):
        return summary
    # Only native enum labels may cross this boundary; never forward child text.
    labels = set("""service_layout protected_config_required service_ownership
        protected_config_unavailable runner_unconfigured gate_unavailable runner_unavailable
        owned_cleanup_unproven clear_request_unproven gate_clear_failed restoration_unproven
        settings_not_run code_submitted already_unlocked backend_initialization gate_access
        worker_join disabled device_mismatch manual_required no_keypad unreadable_progress
        length_mismatch residual_input incomplete incomplete_index_unproven
        unlocked_before_last_digit wrong_or_uncertain backend_disconnected backend_unavailable
        progress_mismatch gate_unproven""".split())
    allowed = {"runner": RUNNER_REASONS, "reason": labels | {"session_admission", "session_preflight",
        "session_creation_unproven", "owned_session_unproven"}, "action": labels,
        "restoration": {"verified", "not_needed", "unproven"},
        "phase": set("""initial_snapshot owned_admission wake_dispatch wake_postcapture
            wake_admission normal_confirmation reveal_dispatch reveal_postcapture
            empty_confirmation digit_input final_confirmation""".split()),
        "step": set("""session_before source_read lock_read direct_field session_after
            input_admission session_match parse_source native_lock viewport_read freshness empty_admission
            reveal_settle_limit complete""".split()),
        "progress": {"unchanged", "unexpected"},
        "first_unconfirmed_index": set("123456")}
    timing_labels = set("lt_1ms lt_10ms lt_100ms lt_1s lt_5s lt_10s lt_30s lt_120s ge_120s".split())
    for stage in ("session_before", "source", "lock", "direct_field", "session_after", "parse", "native"):
        allowed[stage + "_time"] = timing_labels
    try:
        line = err.removeprefix(b"handkite-unlock-once: ").decode("ascii")
        if not line.endswith("\n") or "\n" in line[:-1]:
            return summary
        fields = [part.split("=") for part in line[:-1].split(" ")]
        if any(len(pair) != 2 for pair in fields):
            return summary
        values = dict(fields)
        if len(values) != len(fields) or values.pop("unlock_idle", None) not in {
                "disabled", "refused", "unknown"}:
            return summary
        if any(key not in allowed or value not in allowed[key] for key, value in values.items()):
            return summary
        summary["reported_outcome"] = fields[0][1]
        summary.update(values)
    except UnicodeDecodeError:
        pass
    return summary


def private_pin_pipe(fd):
    before = os.fstat(fd)
    # Darwin creates anonymous pipes as 0660 and rejects fchmod; no directory
    # entry exists through which another account can open this descriptor.
    require(stat.S_ISFIFO(before.st_mode) and before.st_nlink == 0 and
            before.st_uid == os.geteuid() and stat.S_IMODE(before.st_mode) in (0o600, 0o660),
            "private_pin_pipe")
    no_acl(fd)
    require(fp(os.fstat(fd)) == fp(before), "private_pin_pipe")


def supervise(profile, backend, stops, output, stdin):
    mode, dispatched, rotated = profile.mode, 0, False
    stage = "preflight"
    cleanup_started, post_attempted = False, False
    def cleanup_once():
        nonlocal cleanup_started, stage
        stage = "cleanup"
        require(not cleanup_started, "cleanup_already_attempted")
        cleanup_started = True
        backend.cleanup()
    def post_native():
        nonlocal post_attempted, stage
        stage = "native_after"
        require(not post_attempted, "native_after_already_attempted")
        post_attempted = True
        return backend.native()
    try:
        profile.recheck()
        stops.check()
        backend.guard = lambda: (profile.recheck(), stops.check())
        if mode == "pin-once":
            private_pin_pipe(stdin)
        output.emit(mode, "admission", "checked", dispatched)
        require(not output.failed, "output_failed")
        backend.jobs()
        backend.controllers()
        with runtime_lock(profile.files["runtime_lock"]):
            pass
        require(backend.endpoint() == 3, "endpoint_busy")
        stage = "native_before"
        state, boot = backend.native()
        require(state in ("Locked", "Unlocked") and boot, "native_precondition")
        if mode == "pin-once":
            require(state == "Locked", "native_not_locked")
            stage = "stop_rotation"
            with runtime_lock(profile.files["runtime_lock"]):
                profile.recheck()
                backend.jobs()
                stops.check()
                require(not output.failed, "output_failed")
                rotated = True
                stops.rotate()
            dispatched = 1
            stage = "operator"
            result = backend.pin(stdin)
            stage = "operator_result"
            output.emit(mode, "operator", "returned", dispatched, pin_diagnostic(result))
            submitted, zero = pin_result(result)
            submitted &= not output.failed
            zero &= not output.failed
            if not submitted:
                if not zero:
                    stops.block()
                cleanup_once()
                after, prior_unlock = post_native()
                if zero:
                    require(after in ("Locked", "Unlocked") and prior_unlock,
                            "native_zero_unproven")
                    stage = "stop_restoration"
                    with runtime_lock(profile.files["runtime_lock"]):
                        profile.recheck()
                        backend.jobs()
                        stops.accept_cleared_attempt()
                        stops.restore()
                stage = "operator_result"
                raise Refused("known_zero" if zero else "review_required")
            cleanup_once()
            require(post_native() == ("Unlocked", True), "native_unlock_unproven")
            stage = "stop_restoration"
            with runtime_lock(profile.files["runtime_lock"]):
                profile.recheck()
                backend.jobs()
                stops.accept_cleared_attempt()
                stops.restore()
            stage = "receipt_check"
            backend.receipt()
        elif mode == "clock-once":
            require(state == "Unlocked", "native_not_unlocked")
            profile.recheck()
            stops.check()
            require(not output.failed, "output_failed")
            dispatched = 1
            stage = "operator"
            backend.clock()
            cleanup_once()
            require(post_native() == ("Unlocked", True), "native_clock_unproven")
        else:
            if state != "Locked":
                profile.recheck()
                stops.check()
                require(not output.failed, "output_failed")
                dispatched = 1
                stage = "operator"
                backend.lock()
            cleanup_once()
            require(post_native() == ("Locked", True), "native_lock_unproven")
        stage = "final_custody"
        profile.recheck()
        stops.check()
        require(not output.failed, "output_failed")
        output.emit(mode, "complete", "confirmed", dispatched)
        return 0 if not output.failed else 3
    except Exception:
        failed_stage = stage
        if rotated:
            try:
                stops.block()
            except Exception:
                pass
        if dispatched and not cleanup_started:
            try:
                cleanup_once()
            except Exception:
                pass
        if dispatched and not post_attempted:
            try:
                post_native()
            except Exception:
                pass
        # No restart, replacement child, or inferred success after lost output.
        output.emit(mode, "stopped", "review_required", dispatched, {"stage": failed_stage})
        return 3


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    output, profile, stops = Output(sys.stdout), None, None
    if not (len(argv) == 5 and argv[0] in MODES and argv[1:4] ==
            ["--profile-fd", "3", "--profile-sha256"] and
            re.fullmatch(r"[0-9a-f]{64}", argv[4])):
        output.emit("unknown", "admission", "invalid")
        return 2
    mode = argv[0]
    try:
        require(sys.platform == "darwin", "platform")
        require(os.lseek(3, 0, os.SEEK_CUR) == 0, "profile_offset")
        home = pwd.getpwuid(os.geteuid()).pw_dir
        profile = Profile(3, argv[4], mode, home)
        backend = Native(profile)
        backend.receipt()
        stops = StopSet(home, exact_json(profile.files["receipt"].check()))
        return supervise(profile, backend, stops, output, sys.stdin.fileno())
    except Exception:
        output.emit(mode, "admission", "review_required", diagnostic={"stage": "profile_admission"})
        return 3
    finally:
        if stops:
            stops.close()
        if profile:
            profile.close()


if __name__ == "__main__":
    raise SystemExit(main())
