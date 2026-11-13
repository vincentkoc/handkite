"""Offline filesystem and fake-transport tests; no personal state or device IO."""
import concurrent.futures
from contextlib import contextmanager
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import _supervised_files as files
import supervised_device as broker


def encoded(value):
    return json.dumps(value, separators=(",", ":")).encode() + b"\n"


def identity(meta):
    return dict(zip(("device", "inode", "uid", "gid", "mode"),
                    (meta.st_dev, meta.st_ino, meta.st_uid, meta.st_gid, meta.st_mode)))


class Fixture:
    def __init__(self, holds=0, mode="pin-once"):
        # An account-owned ancestor chain is part of the production contract.
        self.temp = tempfile.TemporaryDirectory(dir=Path.cwd())
        self.home = Path(self.temp.name)
        self.root = self.home / "Library/Application Support/Handkite"
        self.state = self.root / "state/iphone"
        self.state.mkdir(parents=True)
        for directory, subdirs, _ in os.walk(self.home):
            os.chmod(directory, 0o700)
        self.active = self.state / "UNLOCK-DISABLED"
        self.write(self.active, b"uncertain\n", 0o600)
        for i in range(holds):
            self.write(self.state / ("UNLOCK-DISABLED.fixture-%d.hold" % i),
                       b"old uncertainty %d\n" % i, 0o600)
        self.write(self.state / "unrelated", b"preserved", 0o600)
        self.receipt = self.snapshot()
        self.target = hashlib.sha256(b"{}\n").hexdigest()
        selected = self.root / "releases" / self.target
        selected.mkdir(parents=True)
        os.chmod(selected.parent, 0o700)
        os.chmod(selected, 0o700)
        (selected.parent / "current").symlink_to(self.target)
        values = {}
        for role, permission in broker.ROLES.items():
            fixed = {"config": self.root / "config/host.json",
                     "runtime_lock": self.root / "runtime/handkite.lock",
                     "manifest": selected.parent / (self.target + ".manifest.json"),
                     "staged_descriptor": self.root / "launchd/com.handkite.host.plist",
                     "registered_descriptor": self.home / "Library/LaunchAgents/com.handkite.host.plist"}
            fixed.update({name: selected / executable for name, executable in
                          (("selected_daemon", "handkited"), ("selected_cli", "handkitectl"),
                           ("selected_mcp", "handkite-mcp"))})
            path = fixed.get(role, self.home / "inputs" / role)
            data = b"fixture executable\n"
            if role == "config":
                data = encoded({"device_key": "phone", "usb_id": "fixture-device",
                                "wda_runner_bundle_id": "com.example.Runner.xctrunner"})
            if role == "binding":
                data = encoded({"schema_version": 1, "device_key": "phone", "usb_id": "fixture-device"})
            if role == "receipt":
                data = encoded(self.receipt)
            if role == "runtime_lock":
                data = b""
            if role == "manifest":
                data = b"{}\n"
            self.write(path, data, permission)
            values[role] = {"path": str(path), "sha256": hashlib.sha256(data).hexdigest(),
                            "fingerprint": list(files.fp(path.stat()))}
        self.value = {"version": 1, "mode": mode, "service_state": "stopped",
                      "selection": {"target": self.target, "fingerprint":
                                    list(files.fp((selected.parent / "current").lstat()))},
                      "files": values}
        self.profile_path = self.home / "profile.json"
        self.write(self.profile_path, encoded(self.value), 0o600)

    def write(self, path, data, mode):
        path.parent.mkdir(parents=True, exist_ok=True)
        # All fixture parents under this account are private.
        for parent in [path.parent, *path.parent.parents]:
            if parent == self.home.parent:
                break
            os.chmod(parent, 0o700)
        path.write_bytes(data)
        path.chmod(mode)

    def snapshot(self):
        markers = []
        for path in sorted(self.state.glob("UNLOCK-DISABLED*")):
            meta = path.stat()
            markers.append({"name": path.name, "identity": identity(meta), "size": meta.st_size,
                            "modified": [meta.st_mtime_ns // 10**9, meta.st_mtime_ns % 10**9],
                            "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
        return {"version": 1, "directory": identity(self.state.stat()), "markers": markers}

    def open(self):
        with self.profile_path.open("rb") as stream:
            p = broker.Profile(stream.fileno(), hashlib.sha256(stream.read()).hexdigest(),
                               self.value["mode"], str(self.home))
        try:
            return p, files.StopSet(str(self.home), self.receipt)
        except BaseException:
            p.close()
            raise

    def close(self):
        self.temp.cleanup()


class Crash(BaseException):
    pass


class Fake:
    def __init__(self, fixture, output=None):
        self.fixture, self.output = fixture, output
        self.calls, self.native_calls, self.cleanups = 0, 0, 0
        self.initial = "Unlocked" if fixture.value["mode"] == "clock-once" else "Locked"
        self.result = (0, b"unlock_idle=code_submitted\n", b"", False)
        self.seam = None

    def jobs(self):
        pass

    def controllers(self):
        pass

    def endpoint(self):
        return 3

    def native(self):
        self.native_calls += 1
        if self.native_calls == 1:
            if self.seam == "contenders":
                self.barrier.wait(timeout=5)
            return self.initial, True
        if self.seam == "before_restore":
            raise Crash()
        return ("Locked" if self.fixture.value["mode"] == "lock-once" else "Unlocked"), True

    def pin(self, stdin):
        self.calls += 1
        if self.seam == "before_spawn":
            raise Crash()
        if self.seam in ("operator_active", "crash_after_uncertain"):
            self.fixture.write(self.fixture.active, b"uncertain\n", 0o600)
            self.fixture.operator_active_pin = files.fp(self.fixture.active.stat())
            if self.seam == "crash_after_uncertain":
                raise Crash()
        os.read(stdin, 64)
        if self.seam == "after_input":
            raise Crash()
        if self.seam in ("attempt_cycle", "attempt_cycle_and_drift"):
            self.fixture.write(self.fixture.active, b"uncertain\n", 0o600)
            self.fixture.active.unlink()
            if self.seam == "attempt_cycle_and_drift":
                (self.fixture.state / "unrelated").write_bytes(b"tampered")
        if self.seam == "lost_output":
            self.output.failed = True
        if self.seam == "contenders":
            self.waiter.wait(timeout=5)
        if self.seam == "operator_active":
            return 1, b"", b"unclassified synthetic refusal\n", False
        return self.result

    def clock(self):
        self.calls += 1
        self.guard()
        if self.seam == "action_failure":
            raise files.Refused("synthetic failure")

    def lock(self):
        self.clock()

    def cleanup(self):
        self.cleanups += 1
        if self.seam == "cleanup_failure":
            raise files.Refused("synthetic failure")

    def receipt(self):
        files.require(self.fixture.receipt == self.fixture.snapshot(), "review_required")


@unittest.skipUnless(sys.platform == "darwin", "native filesystem policy uses macOS ACLs")
class BrokerTests(unittest.TestCase):
    def fixture(self, *args, **kwargs):
        f = Fixture(*args, **kwargs)
        self.addCleanup(f.close)
        return f

    def attempt(self, f, seam=None, result=None, output=None):
        p, stops = f.open()
        self.addCleanup(p.close)
        self.addCleanup(stops.close)
        output = output or broker.Output(io.StringIO())
        fake = Fake(f, output)
        fake.seam = seam
        if result is not None:
            fake.result = result
        read, write = os.pipe()
        os.write(write, b"246810\n")  # Synthetic fixture; never a personal PIN.
        os.close(write)
        try:
            code = broker.supervise(p, fake, stops, output, read)
        finally:
            os.close(read)
        return code, fake, output

    def test_failed_stop_construction_closes_profile(self):
        f = self.fixture(6)
        profiles = []
        original = broker.Profile

        def capture(*args):
            profile = original(*args)
            profiles.append(profile)
            return profile

        with patch.object(broker, "Profile", side_effect=capture), \
                patch.object(files, "StopSet", side_effect=Crash()):
            with self.assertRaises(Crash):
                f.open()
        self.assertEqual(len(profiles), 1)
        self.assertTrue(all(held.fd is None for held in profiles[0].files.values()))
        self.assertEqual(profiles[0].selection.fds, [])
        with self.assertRaises(OSError):
            os.fstat(profiles[0].fd)

    def test_exact_role_aliases_share_reads_but_recheck_fresh_contents(self):
        f = self.fixture()
        f.value["files"]["unlock_check"] = dict(f.value["files"]["operator"])
        f.value["files"]["daemon"] = dict(f.value["files"]["selected_daemon"])
        f.write(f.profile_path, encoded(f.value), 0o600)
        p, stops = f.open()
        self.addCleanup(p.close)
        self.addCleanup(stops.close)
        self.assertIs(p.files["unlock_check"], p.files["operator"])
        self.assertIs(p.files["daemon"], p.files["selected_daemon"])
        # Equal contents at different paths still retain distinct custody.
        self.assertIsNot(p.files["operator"], p.files["probe"])
        with patch.object(files, "read_bytes", wraps=files.read_bytes) as reads:
            p.recheck()
            self.assertEqual(reads.call_count, len(broker.ROLES) - 2)
        operator = Path(p.path("operator"))
        operator.chmod(0o700)
        operator.write_bytes(b"changed executable\n")
        operator.chmod(0o500)
        with self.assertRaises(files.Refused):
            p.recheck()

    def test_alias_with_different_expected_identity_or_digest_refuses(self):
        for changed in ("sha256", "fingerprint"):
            with self.subTest(changed=changed):
                f = self.fixture()
                alias = dict(f.value["files"]["operator"])
                if changed == "sha256":
                    alias[changed] = "0" * 64
                else:
                    alias[changed] = list(alias[changed])
                    alias[changed][1] += 1
                f.value["files"]["unlock_check"] = alias
                f.write(f.profile_path, encoded(f.value), 0o600)
                with self.assertRaises(files.Refused):
                    f.open()

    def test_failed_rename_validation_retains_descriptors_for_cleanup(self):
        for restoring in (False, True):
            for failure in ("stat", "metadata"):
                with self.subTest(restoring=restoring, failure=failure):
                    f = self.fixture(6)
                    profile, stops = f.open()
                    self.addCleanup(profile.close)
                    self.addCleanup(stops.close)
                    if restoring:
                        stops.rotate()
                    source = stops.hold if restoring else stops.active
                    held = stops.held[source]
                    descriptors = [held.fd, *held.anchor.fds]
                    original = os.fstat

                    def fail_after_rename(fd):
                        meta = original(fd)
                        if fd == held.fd and not (f.state / source).exists():
                            if failure == "stat":
                                raise OSError("synthetic stat failure")
                            fields = {name: getattr(meta, name) for name in (
                                "st_dev", "st_ino", "st_mode", "st_uid", "st_gid",
                                "st_nlink", "st_size", "st_mtime_ns", "st_ctime_ns")}
                            fields["st_size"] += 1
                            return SimpleNamespace(**fields)
                        return meta

                    with patch.object(files.os, "fstat", side_effect=fail_after_rename):
                        with self.assertRaises((OSError, files.Refused)):
                            (stops.restore if restoring else stops.rotate)()
                    self.assertIn(held, stops.held.values())
                    stops.block()
                    self.assertTrue(f.active.exists())
                    self.assertEqual(len(list(f.state.glob("*.hold"))), 6 if restoring else 7)
                    stops.close()
                    for fd in descriptors:
                        with self.assertRaises(OSError):
                            os.fstat(fd)

    def test_dynamic_zero_one_six_and_many_holds_restore_and_reuse_same_profile(self):
        for count in (0, 1, 6, 40):
            with self.subTest(count=count):
                f = self.fixture(count)
                original = f.profile_path.read_bytes(), f.profile_path.stat().st_ino
                for _ in range(2):
                    code, fake, output = self.attempt(f, "attempt_cycle")
                    self.assertEqual((code, fake.calls, fake.cleanups), (0, 1, 1))
                    self.assertEqual(f.snapshot(), f.receipt)
                    self.assertNotIn("246810", output.stream.getvalue())
                self.assertEqual(original, (f.profile_path.read_bytes(), f.profile_path.stat().st_ino))

    def test_uncertain_malformed_or_lost_output_preserves_hold_and_blocks_old_receipt(self):
        for result, seam in [((1, b"", b"unexpected 246810\n", False), None),
                             ((0, b"unlock_idle=code_submitted\n", b"", True), None),
                             (None, "lost_output")]:
            f = self.fixture(6)
            code, fake, output = self.attempt(f, seam, result)
            self.assertEqual((code, fake.calls), (3, 1))
            self.assertNotEqual(f.receipt, f.snapshot())
            self.assertTrue(f.active.exists())
            self.assertEqual(len(list(f.state.glob("*.hold"))), 7)
            self.assertNotIn("246810", output.stream.getvalue())
            with self.assertRaises(files.Refused):
                f.open()

    def test_crash_seams_leave_receipt_invalid_without_replay(self):
        for seam in ("before_spawn", "after_input", "before_restore"):
            f = self.fixture()
            with self.assertRaises(Crash):
                self.attempt(f, seam)
            self.assertFalse(f.active.exists())
            self.assertEqual(len(list(f.state.glob("*.hold"))), 1)
            self.assertNotEqual(f.receipt, f.snapshot())

    def test_operator_created_active_survives_uncertainty_and_crash_without_replay(self):
        for seam in ("operator_active", "crash_after_uncertain"):
            f = self.fixture(6)
            old_inode = f.active.stat().st_ino
            if seam == "crash_after_uncertain":
                with self.assertRaises(Crash):
                    self.attempt(f, seam)
            else:
                code, fake, _ = self.attempt(f, seam)
                self.assertEqual((code, fake.calls), (3, 1))
            self.assertEqual(files.fp(f.active.stat()), f.operator_active_pin)
            self.assertEqual(f.active.read_bytes(), b"uncertain\n")
            holds = list(f.state.glob("*.hold"))
            self.assertEqual(len(holds), 7)
            self.assertIn(old_inode, [path.stat().st_ino for path in holds])
            with patch.object(Fake, "pin", side_effect=AssertionError("replayed")) as dispatch:
                with self.assertRaises(files.Refused):
                    self.attempt(f)
                dispatch.assert_not_called()

    def test_native_cleanup_refuses_each_unproven_boundary(self):
        for case in ("job", "lock", "endpoint", "timeout", "late_refusal", "controller", "custody"):
            with self.subTest(case=case):
                f = self.fixture()
                p, stops = f.open()
                self.addCleanup(p.close)
                self.addCleanup(stops.close)
                native = broker.Native(p)
                native.jobs = unittest.mock.Mock()
                native.endpoint = unittest.mock.Mock(return_value=3)
                native.controllers = unittest.mock.Mock()
                lock_fd = None
                if case == "job":
                    native.jobs.side_effect = files.Refused("job_present")
                elif case == "lock":
                    import fcntl
                    lock_fd = os.open(p.path("runtime_lock"), os.O_RDWR)
                    fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                elif case in ("endpoint", "timeout"):
                    native.endpoint.return_value = 5 if case == "endpoint" else 0
                elif case == "controller":
                    native.controllers.side_effect = files.Refused("controller_present")
                elif case == "custody":
                    p.recheck = unittest.mock.Mock(side_effect=files.Refused("custody_changed"))
                try:
                    with patch.object(broker.time, "monotonic", return_value=0,
                            side_effect=[0, 0, 181] if case in ("timeout", "late_refusal") else None):
                        with self.assertRaises((files.Refused, BlockingIOError)):
                            native.cleanup()
                finally:
                    if lock_fd is not None:
                        os.close(lock_fd)
                if case in ("job", "lock"):
                    native.endpoint.assert_not_called()
                if case in ("endpoint", "timeout", "late_refusal"):
                    native.controllers.assert_not_called()

    def test_native_cleanup_success_retains_exact_order_and_waits_for_refusal(self):
        f = self.fixture()
        p, stops = f.open()
        self.addCleanup(p.close)
        self.addCleanup(stops.close)
        native, events = broker.Native(p), []
        native.jobs = lambda: events.append("jobs")
        native.controllers = lambda: events.append("controllers")
        results = iter((0, 3))
        native.endpoint = lambda deadline: (events.append("endpoint"), next(results))[1]
        p.recheck = lambda: events.append("custody")
        @contextmanager
        def lock(held):
            self.assertIs(held, p.files["runtime_lock"])
            events.append("lock")
            yield
            events.append("unlock")
        with patch.object(broker, "runtime_lock", side_effect=lock), \
             patch.object(broker.time, "monotonic", return_value=0), \
             patch.object(broker.time, "sleep", side_effect=lambda _: events.append("wait")):
            native.cleanup()
        self.assertEqual(events, ["jobs", "lock", "unlock", "endpoint", "wait",
                                  "endpoint", "controllers", "custody"])

    def test_proven_zero_can_restore_but_cleanup_failure_prevents_success(self):
        f = self.fixture()
        code, fake, _ = self.attempt(f, result=(0, b"unlock_idle=already_unlocked\n", b"", False))
        self.assertEqual((code, fake.calls), (3, 1))
        self.assertEqual(f.receipt, f.snapshot())
        f = self.fixture()
        code, fake, _ = self.attempt(f, "cleanup_failure")
        self.assertEqual((code, fake.calls, fake.cleanups), (3, 1, 1))
        self.assertNotEqual(f.receipt, f.snapshot())
        f = self.fixture()
        code, fake, _ = self.attempt(f, "cleanup_failure",
                                   result=(0, b"unlock_idle=already_unlocked\n", b"", False))
        self.assertEqual((code, fake.calls, fake.cleanups), (3, 1, 1))
        self.assertNotEqual(f.receipt, f.snapshot())
        self.assertTrue(f.active.exists())
        self.assertEqual(len(list(f.state.glob("*.hold"))), 1)

    def test_real_operator_marker_cycle_allows_only_exact_inventory_restoration(self):
        for seam in ("attempt_cycle", "attempt_cycle_and_drift"):
            f = self.fixture(6)
            code, fake, _ = self.attempt(f, seam)
            self.assertEqual(fake.calls, 1)
            if seam == "attempt_cycle":
                self.assertEqual(code, 0)
                self.assertEqual(f.receipt, f.snapshot())
            else:
                self.assertEqual(code, 3)
                self.assertNotEqual(f.receipt, f.snapshot())
                self.assertEqual(len(list(f.state.glob("*.hold"))), 7)

    def test_clock_and_lock_never_move_markers_and_cleanup_on_failure(self):
        for mode in ("clock-once", "lock-once"):
            for seam in (None, "action_failure", "cleanup_failure"):
                f = self.fixture(6, mode)
                p, stops = f.open()
                self.addCleanup(p.close)
                self.addCleanup(stops.close)
                fake = Fake(f)
                fake.initial = "Unlocked"
                fake.seam = seam
                output = broker.Output(io.StringIO())
                with patch.object(stops, "rotate", side_effect=AssertionError("marker mutation")):
                    code = broker.supervise(p, fake, stops, output, -1)
                self.assertEqual(code, 0 if seam is None else 3)
                self.assertEqual((fake.calls, fake.cleanups), (1, 1))
                self.assertEqual(f.receipt, f.snapshot())

    def test_contenders_cannot_both_dispatch_across_preflight_lock_and_rename(self):
        f = self.fixture()
        barrier, waiter = threading.Barrier(2), threading.Event()
        preflight, local = threading.Lock(), threading.local()
        original_lock = broker.runtime_lock
        @contextmanager
        def lock_at_seam(held):
            local.count = getattr(local, "count", 0) + 1
            if local.count == 1:
                # Both contenders finish the advisory preflight before racing
                # on the real nonblocking lock immediately before rename.
                with preflight, original_lock(held):
                    yield
            else:
                with original_lock(held):
                    yield
        def contender():
            p, stops = f.open()
            fake = Fake(f)
            fake.seam, fake.barrier, fake.waiter = "contenders", barrier, waiter
            read, write = os.pipe()
            os.write(write, b"246810\n")
            os.close(write)
            try:
                code = broker.supervise(p, fake, stops, broker.Output(io.StringIO()), read)
                if code != 0:
                    waiter.set()
                return code, fake.calls
            finally:
                os.close(read)
                stops.close()
                p.close()
        with patch.object(broker, "runtime_lock", side_effect=lock_at_seam):
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                results = list(pool.map(lambda _: contender(), range(2)))
        self.assertEqual(sorted(results), [(0, 1), (3, 0)])
        self.assertEqual(f.receipt, f.snapshot())

    def test_marker_missing_added_replaced_content_and_unrelated_inventory_drift(self):
        for case in range(6):
            f = self.fixture(1)
            p, stops = f.open()
            self.addCleanup(p.close)
            self.addCleanup(stops.close)
            marker = next(f.state.glob("*.hold"))
            if case == 0:
                marker.unlink()
            elif case == 1:
                f.write(f.state / "UNLOCK-DISABLED.extra.hold", b"extra", 0o600)
            elif case == 2:
                data = marker.read_bytes()
                marker.unlink()
                f.write(marker, data, 0o600)
            elif case == 3:
                marker.write_bytes(b"changed")
            elif case == 4:
                (f.state / "unrelated").unlink()
            else:
                f.write(f.state / "unexpected", b"new", 0o600)
            with self.assertRaises(files.Refused):
                stops.check()

    def test_unsafe_acl_links_ancestors_and_post_read_path_race(self):
        for case in range(5):
            f = self.fixture()
            p, stops = f.open()
            self.addCleanup(p.close)
            self.addCleanup(stops.close)
            if case == 0:
                os.link(f.active, f.state / "linked")
            elif case == 1:
                subprocess.run(["/bin/chmod", "+a", "everyone allow read", str(f.active)], check=True)
            elif case == 2:
                f.state.chmod(0o777)
            elif case == 3:
                held = f.state.with_name("held")
                f.state.rename(held)
                f.state.symlink_to(held)
            else:
                old = files.read_bytes
                def race(fd, cap):
                    result = old(fd, cap)
                    if fd == stops.held[stops.active].fd:
                        f.active.unlink()
                        f.write(f.active, result, 0o600)
                    return result
                with patch.object(files, "read_bytes", side_effect=race):
                    with self.assertRaises(files.Refused):
                        stops.held[stops.active].check()
                continue
            with self.assertRaises((files.Refused, OSError)):
                stops.check()

    def test_profile_mutation_wrong_digest_unsafe_mode_and_marker_bound_refuse(self):
        for case in range(4):
            f = self.fixture()
            p, stops = f.open()
            self.addCleanup(p.close)
            self.addCleanup(stops.close)
            if case == 0:
                f.profile_path.write_bytes(f.profile_path.read_bytes() + b" ")
            elif case == 1:
                p.sha = "0" * 64
            elif case == 2:
                f.profile_path.chmod(0o644)
            else:
                p.original = tuple([False] + list(p.original[1:]))
            with self.assertRaises(files.Refused):
                p.recheck()
        f = self.fixture(256)
        with self.assertRaises(files.Refused):
            f.open()

    def test_broken_output_admission_never_dispatches(self):
        class Broken(io.StringIO):
            def write(self, value):
                raise BrokenPipeError()
        f = self.fixture()
        code, fake, _ = self.attempt(f, output=broker.Output(Broken()))
        self.assertEqual((code, fake.calls), (3, 0))
        self.assertEqual(f.receipt, f.snapshot())

    def test_lost_final_response_does_not_replay_even_when_receipt_matches(self):
        class FinalFailure(io.StringIO):
            calls = 0
            def write(self, value):
                self.calls += 1
                if self.calls == 3:
                    raise BrokenPipeError()
                return super().write(value)
        f = self.fixture()
        code, fake, _ = self.attempt(f, output=broker.Output(FinalFailure()))
        self.assertEqual((code, fake.calls), (3, 1))
        self.assertEqual(f.receipt, f.snapshot())

    def test_pin_admission_refuses_named_fifo_regular_file_and_socket(self):
        import socket
        f = self.fixture()
        fifo = f.home / "named-fifo"
        os.mkfifo(fifo, 0o600)
        fd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
        try:
            with self.assertRaises(files.Refused):
                broker.private_pin_pipe(fd)
        finally:
            os.close(fd)
        with f.profile_path.open("rb") as stream:
            with self.assertRaises(files.Refused):
                broker.private_pin_pipe(stream.fileno())
        left, right = socket.socketpair()
        try:
            with self.assertRaises(files.Refused):
                broker.private_pin_pipe(left.fileno())
        finally:
            left.close()
            right.close()


class PureTests(unittest.TestCase):
    def test_endpoint_fragmentation_uses_one_deadline_per_query(self):
        native = broker.Native.__new__(broker.Native)
        native.p = SimpleNamespace(config={"usb_id": "fixture-device"})
        replies = [{"DeviceList": [{"DeviceID": 1, "Properties": {
            "SerialNumber": "fixture-device", "ConnectionType": "USB"}}]},
            {"MessageType": "Result", "Number": 3}]
        for step, completes in ((0.01, True), (1, False)):
            clock, channels = [0], []
            for reply in replies:
                payload = broker.plistlib.dumps(reply)
                wire = bytearray(broker.struct.pack("<IIII", len(payload)+16, 1, 8, 1) + payload)
                channel = unittest.mock.MagicMock()
                channel.__enter__.return_value = channel
                def receive(count, wire=wire):
                    clock[0] += step
                    count = min(count, 8, len(wire))
                    result = bytes(wire[:count])
                    del wire[:count]
                    return result
                channel.recv.side_effect = receive
                channels.append(channel)
            with patch.object(broker.socket, "socket", side_effect=channels), \
                 patch.object(broker.time, "monotonic", side_effect=lambda: clock[0]):
                if completes:
                    self.assertEqual(native.endpoint(), 3)
                else:
                    with self.assertRaisesRegex(files.Refused, "endpoint_timeout"):
                        native.endpoint()
                    channels[1].connect.assert_not_called()
            timeouts = [call.args[0] for call in channels[0].settimeout.call_args_list]
            self.assertTrue(all(0 < timeout <= 5 for timeout in timeouts))
            self.assertLess(timeouts[-1], timeouts[0])

    def test_slow_lease_reply_expires_before_any_lock_action(self):
        native = broker.Native.__new__(broker.Native)
        native.p = SimpleNamespace(path=lambda role: "/fixture/" + role,
            root="/fixture", recheck=lambda: None,
            files={"daemon": SimpleNamespace(check=lambda: None)})
        native.env = {}
        child = unittest.mock.Mock()
        child.data, child.fault = {"stderr": b"handkited ready\n"}, False
        child.process.pid, child.process.poll.return_value = 123, None
        child.wait.return_value = (0, b"", b"handkited ready\n", False)
        clock, wire = [0], bytearray(encoded({"type": "acquired", "lease": "a"*32}))
        channel = unittest.mock.MagicMock()
        channel.__enter__.return_value = channel
        def receive(count):
            clock[0] += 2
            result = bytes(wire[:1])
            del wire[:1]
            return result
        channel.recv.side_effect = receive
        meta = SimpleNamespace(st_mode=stat.S_IFSOCK | 0o600,
            st_uid=os.geteuid(), st_nlink=1)
        with patch.object(broker, "Child", return_value=child), \
             patch.object(broker.os, "lstat", return_value=meta), \
             patch.object(broker, "fp", return_value=()), \
             patch.object(broker.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout=b"123\n")), \
             patch.object(broker.socket, "socket", return_value=channel) as sockets, \
             patch.object(broker.time, "monotonic", side_effect=lambda: clock[0]):
            with self.assertRaises(files.Refused):
                native.lock()
        self.assertEqual(sockets.call_count, 1)
        self.assertEqual(channel.sendall.call_count, 1)
        self.assertEqual(json.loads(channel.sendall.call_args.args[0])["method"], "acquire")
        self.assertEqual(clock[0], 60)
        child.process.send_signal.assert_called_once_with(broker.signal.SIGTERM)
        child.process.kill.assert_not_called()

    def test_native_jobs_requires_both_fixed_account_domains_absent(self):
        native = broker.Native.__new__(broker.Native)
        native.env = {"PATH": "/usr/bin:/bin"}
        for codes in ((113, 113), (0, 113), (113, 0), (113, 5)):
            with patch.object(broker.subprocess, "run", side_effect=[
                    SimpleNamespace(returncode=code) for code in codes]) as run:
                if codes == (113, 113):
                    native.jobs()
                else:
                    with self.assertRaises(files.Refused):
                        native.jobs()
                for index, call in enumerate(run.call_args_list):
                    self.assertEqual(call.args[0], ["/bin/launchctl", "print",
                        "%s/%d/com.handkite.host" % (("user", "gui")[index], os.geteuid())])

    def test_lock_confirmation_requires_explicit_true_even_with_native_confirmation(self):
        for accepted in (None, False, 0, 1, "true", True):
            reply = {"type": "acted", "receipt": {"action_kind": "lock", "accepted": accepted},
                     "native_lock": "locked", "effect": "lock_confirmed"}
            self.assertEqual(broker.lock_confirmed(reply), accepted is True)

    def test_noncanonical_numbers_duplicates_and_argument_output_are_bounded(self):
        for value in (b'{"a":1,"a":2}', b'{"a":NaN}'):
            with self.assertRaises(files.Refused):
                files.exact_json(value)
        for value in ([False] * 9, [2**64] * 9, [1] * 8):
            with self.assertRaises(files.Refused):
                files.pin(value)
        output = io.StringIO()
        with patch("sys.stdout", output):
            self.assertEqual(broker.main(["pin-once", "--secret", "synthetic-private"]), 2)
        self.assertNotIn("synthetic-private", output.getvalue())

    def test_private_child_overflow_and_unknown_pin_output_never_promote(self):
        with patch.object(broker.Child, "identity", return_value=None):
            child = broker.Child([sys.executable, "-c", "import sys;sys.stdout.write('x'*70000)"],
                {}, subprocess.DEVNULL, os.getcwd(), lambda: None)
            code, out, err, fault = child.wait()
        self.assertEqual(code, 0)
        self.assertTrue(fault)
        self.assertLessEqual(len(out), 65536)
        self.assertEqual(broker.pin_result((0, b"246810\n", b"", False)), (False, False))
        self.assertEqual(broker.pin_result((0, b"unlock_idle=code_submitted\n", b"", True)),
                         (False, False))

    def test_child_wait_rechecks_identity_after_timeout_without_interrupting(self):
        child = broker.Child.__new__(broker.Child)
        child.process = unittest.mock.Mock()
        child.process.poll.side_effect = [None, None, None, None, 0]
        child.process.wait.side_effect = [subprocess.TimeoutExpired("fixture", 0.25), None, None]
        child.process.returncode = 0
        child.process.stdout, child.process.stderr = io.BytesIO(), io.BytesIO()
        child.identity = unittest.mock.Mock(side_effect=[None, files.Refused("identity"), None])
        child.threads = []
        child.data = {"stdout": bytearray(b"fixture"), "stderr": bytearray()}
        child.fault = False
        self.assertEqual(child.wait(), (0, b"fixture", b"", True))
        self.assertEqual(child.identity.call_count, 3)
        self.assertEqual(child.process.wait.call_args_list,
                         [unittest.mock.call(timeout=0.25)] * 3)
        child.process.terminate.assert_not_called()
        child.process.kill.assert_not_called()


if __name__ == "__main__":
    unittest.main()
