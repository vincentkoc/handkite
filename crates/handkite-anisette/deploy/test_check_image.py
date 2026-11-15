#!/usr/bin/env python3
"""Offline fake-Docker tests; every subprocess call is intercepted."""

import copy
import io
import json
import os
import stat
import subprocess
import unittest
from contextlib import redirect_stderr, redirect_stdout
from types import SimpleNamespace
from unittest.mock import patch

import check_image as check


IDENTITY = "a" * 64
OTHER_ID = "b" * 64
TOKEN = "1" * 32
NAME = "handkite-asset-check-" + TOKEN
DAEMON = "synthetic-daemon"
ENDPOINT = "unix:///fixture/docker.sock"
PRIVATE = "synthetic-private-diagnostic"


class FakeDocker:
    def __init__(self):
        self.calls = []
        self.requests = []
        self.context = {"Name": "fixture", "Endpoints": {
            "docker": {"Host": ENDPOINT},
        }}
        self.info = {
            "ID": DAEMON, "OSType": "linux", "Architecture": "x86_64",
            "DriverStatus": [["driver-type", "io.containerd.snapshotter.v1"]],
        }
        self.socket = SimpleNamespace(st_mode=stat.S_IFSOCK, st_dev=1, st_ino=2)
        self.after_call = lambda args: None
        self.image = {
            "Id": check.CHILD, "Os": "linux", "Architecture": "amd64",
            "RepoDigests": [check.IMAGE], "Config": {
                "User": "10001:10001", "Entrypoint": ["anisette-server"],
                "Env": ["PATH=/usr/bin:/bin"], "Volumes": None,
            },
        }
        self.container = None
        self.mutate = lambda value: None
        self.create_mode = "normal"
        self.start_mode = "normal"
        self.start_output = check.EXPECTED_STDOUT
        self.remove_mode = "normal"
        self.list_mode = "normal"
        self.existing = False
        self.inspect_mode = "normal"

    def new_container(self):
        return {
            "Id": IDENTITY, "Name": "/" + NAME,
            "Image": "sha256:0595cc72ff099cf6f96fb41b965bf009a9039f69b053185a578c31401c66ff1e",
            "ImageManifestDescriptor": {"digest": check.CHILD},
            "Path": "anisette-server", "Args": ["--backend", "macos", "--check"],
            "RestartCount": 0, "Mounts": [], "NetworkSettings": {
                "Ports": {}, "Networks": {"none": {}},
            },
            "Config": {
                "Image": check.IMAGE, "User": "10001:10001",
                "Labels": {check.LABEL: TOKEN}, "Entrypoint": ["anisette-server"],
                "Cmd": ["--backend", "macos", "--check"],
                "Env": ["PATH=/usr/bin:/bin"], "Healthcheck": {"Test": ["NONE"]},
                "AttachStdout": True, "AttachStderr": True,
            },
            "HostConfig": {
                "NetworkMode": "none", "ReadonlyRootfs": True, "Privileged": False,
                "PublishAllPorts": False, "AutoRemove": False, "CapDrop": ["ALL"],
                "SecurityOpt": ["no-new-privileges:true"], "NanoCpus": 1_000_000_000,
                "Memory": 268435456, "MemorySwap": 268435456, "PidsLimit": 32,
                "IpcMode": "none", "RestartPolicy": {"Name": "no"},
                "LogConfig": {"Type": "none"},
            },
            "State": {
                "Status": "created", "Running": False, "Paused": False,
                "Restarting": False, "Dead": False, "OOMKilled": False,
                "Pid": 0, "ExitCode": 0, "Error": "",
            },
        }

    def __call__(self, argv, **kwargs):
        args = argv[3:]
        route = ["--context", "fixture"] if args == ["context", "inspect", "fixture"] else [
            "--host", ENDPOINT,
        ]
        if argv[:3] != ["docker", *route]:
            raise AssertionError("unexpected process or unpinned endpoint")
        if kwargs != dict(stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, timeout=(check.CHECK_TIMEOUT
                          if argv[3:5] == ["container", "start"] else check.CLI_TIMEOUT),
                          check=False):
            raise AssertionError("unbounded or interactive subprocess")
        self.calls.append(args)
        self.requests.append(argv)
        output, error, code = b"", b"", 0
        if args == ["context", "inspect", "fixture"]:
            output = json.dumps([self.context]).encode()
        elif args == ["info", "--format", "{{json .}}"]:
            output = json.dumps(self.info).encode()
        elif args == ["image", "inspect", "--platform", "linux/amd64", check.IMAGE]:
            output = json.dumps([self.image]).encode()
        elif args[:2] == ["container", "create"]:
            if self.create_mode != "timeout-absent":
                self.container = self.new_container()
                self.mutate(self.container)
            if self.create_mode.startswith("timeout"):
                raise subprocess.TimeoutExpired(argv, check.CLI_TIMEOUT, output=PRIVATE)
            output = (IDENTITY + "\n").encode()
            if self.create_mode == "bad-receipt":
                output = PRIVATE.encode()
        elif args[:2] == ["container", "inspect"]:
            if args[2] != IDENTITY or self.container is None:
                raise AssertionError("inspection escaped owned identity")
            if self.inspect_mode == "malformed":
                output = b"{broken"
            else:
                output = json.dumps([self.container]).encode()
        elif args == ["container", "start", "--attach", IDENTITY]:
            self.container["State"].update(Status="exited", ExitCode=0)
            output = self.start_output
            if self.start_mode == "timeout":
                self.container["State"].update(Status="running", Running=True, Pid=123)
                raise subprocess.TimeoutExpired(argv, check.CHECK_TIMEOUT, output=PRIVATE)
            if self.start_mode == "interrupt":
                raise KeyboardInterrupt()
            if self.start_mode in ("failed", "oom"):
                code, error = 137, PRIVATE.encode()
                self.container["State"].update(ExitCode=137, OOMKilled=self.start_mode == "oom")
            if self.start_mode == "stderr":
                error = PRIVATE.encode()
        elif args == ["container", "rm", "--force", IDENTITY]:
            if self.remove_mode == "failed":
                code, error = 1, PRIVATE.encode()
            elif self.remove_mode == "normal":
                self.container = None
            output = (IDENTITY + "\n").encode()
        elif args[:2] == ["container", "ls"]:
            if args != ["container", "ls", "--all", "--no-trunc", "--filter",
                        args[5], "--format", "{{.ID}}"]:
                raise AssertionError("unfiltered container listing")
            if args[5] not in ("name=^/" + NAME + "$", "id=" + IDENTITY):
                raise AssertionError("listing escaped task identity")
            if self.list_mode == "failed":
                code, error = 1, PRIVATE.encode()
            elif self.container is not None or self.existing:
                output = (IDENTITY + "\n").encode()
        else:
            raise AssertionError("unexpected Docker operation: " + repr(args))
        self.after_call(args)
        return subprocess.CompletedProcess(argv, code, output, error)


class CheckImageTests(unittest.TestCase):
    def setUp(self):
        self.fake = FakeDocker()
        for target, values in (
            ("check_image.subprocess.run", {"side_effect": self.fake}),
            ("check_image.platform.system", {"return_value": "Linux"}),
            ("check_image.platform.machine", {"return_value": "x86_64"}),
            ("check_image.os.stat", {"side_effect": lambda _path: self.fake.socket}),
            ("check_image.uuid.uuid4", {"return_value": SimpleNamespace(hex=TOKEN)}),
        ):
            mock = patch(target, **values)
            mock.start()
            self.addCleanup(mock.stop)
        environment = patch.dict(os.environ, {}, clear=True)
        environment.start()
        self.addCleanup(environment.stop)

    def run_check(self):
        receipt = check.run_check("fixture", DAEMON)
        self.assertNotIn(PRIVATE, json.dumps(receipt))
        self.assertLessEqual(len(self.actions("create")), 1)
        self.assertLessEqual(len(self.actions("start")), 1)
        self.assertLessEqual(len(self.actions("rm")), 1)
        return receipt

    def actions(self, verb):
        return [args for args in self.fake.calls if args[:2] == ["container", verb]]

    def assert_before_create(self, stage):
        receipt = self.run_check()
        self.assertEqual(receipt["stage"], stage)
        self.assertEqual(receipt["asset_check"], "failed")
        self.assertEqual(receipt["cleanup"], "not-needed")
        self.assertFalse(self.actions("create"))

    def test_success_and_containment_command(self):
        receipt = self.run_check()
        self.assertEqual(receipt["asset_check"], "passed")
        self.assertEqual(receipt["cleanup"], "verified")
        self.assertEqual(receipt["exit_code"], 0)
        self.assertIs(receipt["oom_killed"], False)
        self.assertEqual(receipt["stdout"], "verified")
        args = self.actions("create")[0]
        for flag, value in (
            ("--name", NAME), ("--label", check.LABEL + "=" + TOKEN),
            ("--platform", "linux/amd64"), ("--pull", "never"), ("--restart", "no"),
            ("--network", "none"), ("--ipc", "none"), ("--user", "10001:10001"),
            ("--security-opt", "no-new-privileges:true"), ("--cap-drop", "ALL"),
            ("--cpus", "1"), ("--memory", "268435456"), ("--memory-swap", "268435456"),
            ("--pids-limit", "32"), ("--log-driver", "none"),
        ):
            self.assertEqual(args[args.index(flag) + 1], value)
        self.assertIn("--read-only", args)
        self.assertIn("--no-healthcheck", args)
        self.assertEqual(args[-6:], ["--entrypoint", "anisette-server", check.IMAGE,
                                    "--backend", "macos", "--check"])
        for forbidden in ("--mount", "--volume", "-v", "--publish", "-p", "--env", "--privileged"):
            self.assertNotIn(forbidden, args)
        commands = [args[:2] for args in self.fake.calls]
        self.assertLess(commands.index(["container", "inspect"]),
                        commands.index(["container", "start"]))
        self.assertEqual(self.actions("rm"), [["container", "rm", "--force", IDENTITY]])
        self.assertEqual(self.actions("ls")[-2][5], "id=" + IDENTITY)
        self.assertEqual(self.actions("ls")[-1][5], "name=^/" + NAME + "$")

    def test_non_native_client_rejects_without_docker(self):
        for system, machine in (("Darwin", "x86_64"), ("Linux", "aarch64")):
            with self.subTest(system=system, machine=machine):
                with patch("check_image.platform.system", return_value=system), \
                        patch("check_image.platform.machine", return_value=machine):
                    self.assert_before_create("native-host")
                self.assertFalse(self.fake.calls)

    def test_transport_environment_rejects(self):
        for key in ("DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_TLS_VERIFY",
                    "DOCKER_CERT_PATH", "DOCKER_API_VERSION"):
            with self.subTest(key=key), patch.dict(os.environ, {key: PRIVATE}):
                self.assert_before_create("context")
        self.assertFalse(self.fake.calls)

    def test_context_must_be_local_unix_socket(self):
        for host in ("ssh://fixture", "tcp://localhost:2375", "unix://remote/socket",
                     "unix:///socket?query", "unix:///socket#fragment", "unix:///%2Fsocket"):
            with self.subTest(host=host):
                self.fake.context["Endpoints"]["docker"]["Host"] = host
                self.assert_before_create("context")
        self.fake.context["Endpoints"]["docker"]["Host"] = "unix:///fixture/docker.sock"
        with patch("check_image.os.stat", return_value=SimpleNamespace(st_mode=stat.S_IFREG)):
            self.assert_before_create("context")

    def test_daemon_identity_and_architecture(self):
        for field, wrong, stage in (("ID", "other", "daemon"),
                                    ("OSType", "windows", "native-daemon"),
                                    ("Architecture", "aarch64", "native-daemon")):
            with self.subTest(field=field):
                original = self.fake.info[field]
                self.fake.info[field] = wrong
                self.assert_before_create(stage)
                self.fake.info[field] = original

    def test_containerd_image_store_is_required(self):
        for status in (None, [], [["Backing Filesystem", "extfs"]],
                       [["driver-type", "other"]]):
            with self.subTest(status=status):
                self.fake.info["DriverStatus"] = status
                self.assert_before_create("image-store")

    def test_mutable_context_cannot_redirect_execution_or_cleanup(self):
        changes = []
        def after_call(args):
            if args[:2] == ["image", "inspect"] and not changes:
                changes.append("after-admission")
                self.fake.context["Endpoints"]["docker"]["Host"] = "unix:///other/docker.sock"
            elif args[:2] == ["container", "start"]:
                changes.append("before-cleanup")
                self.fake.context["Endpoints"]["docker"]["Host"] = "tcp://other:2375"
        self.fake.after_call = after_call
        receipt = self.run_check()
        self.assertEqual(receipt["asset_check"], "passed")
        self.assertEqual(changes, ["after-admission", "before-cleanup"])
        self.assertEqual(sum(args[0] == "context" for args in self.fake.calls), 1)
        self.assertTrue(all(argv[:3] == ["docker", "--host", ENDPOINT]
                            for argv in self.fake.requests[1:]))
        self.assertEqual(len(self.actions("rm")), 1)

    def test_endpoint_or_daemon_drift_before_create_start_and_cleanup(self):
        for boundary in ("create", "start", "cleanup"):
            for drift in ("inode", "device", "socket-type", "daemon"):
                with self.subTest(boundary=boundary, drift=drift):
                    self.fake = FakeDocker()
                    def after_call(args):
                        trigger = (
                            boundary == "create" and args[:2] == ["container", "ls"] or
                            boundary == "start" and args[:2] == ["container", "inspect"] or
                            boundary == "cleanup" and args[:2] == ["container", "start"]
                        )
                        if trigger:
                            if drift == "daemon":
                                self.fake.info["ID"] = "replacement-daemon"
                            elif drift == "inode":
                                self.fake.socket.st_ino = 3
                            elif drift == "device":
                                self.fake.socket.st_dev = 3
                            else:
                                self.fake.socket.st_mode = stat.S_IFREG
                    self.fake.after_call = after_call
                    with patch("check_image.subprocess.run", side_effect=self.fake):
                        receipt = self.run_check()
                    self.assertEqual(receipt["asset_check"], "failed")
                    self.assertFalse(self.actions("rm"))
                    if boundary == "create":
                        self.assertFalse(self.actions("create"))
                        self.assertEqual(receipt["cleanup"], "not-needed")
                    else:
                        self.assertEqual(receipt["cleanup"], "unknown")
                    if boundary != "cleanup":
                        self.assertFalse(self.actions("start"))

    def test_drift_during_absence_checks_cannot_prove_cleanup(self):
        def after_call(args):
            if args[:2] == ["container", "ls"] and args[5] == "id=" + IDENTITY:
                self.fake.info["ID"] = "replacement-daemon"
        self.fake.after_call = after_call
        receipt = self.run_check()
        self.assertEqual(receipt["asset_check"], "failed")
        self.assertEqual(receipt["cleanup"], "unknown")
        self.assertEqual(len(self.actions("rm")), 1)

    def test_config_digest_is_not_platform_image_identity(self):
        self.fake.image["Id"] = "sha256:0595cc72ff099cf6f96fb41b965bf009a9039f69b053185a578c31401c66ff1e"
        self.assert_before_create("image")

    def test_image_contract(self):
        baseline = copy.deepcopy(self.fake.image)
        cases = (("Id", check.INDEX), ("Os", "other"), ("Architecture", "arm64"),
                 ("RepoDigests", []), ("User", "0"), ("Entrypoint", ["sh"]),
                 ("Volumes", {"/data": {}}), ("Healthcheck", {"Test": ["CMD", "sh"]}))
        for key, value in cases:
            with self.subTest(key=key):
                self.fake.image = copy.deepcopy(baseline)
                target = self.fake.image if key in baseline else self.fake.image["Config"]
                target[key] = value
                self.assert_before_create("image")

    def test_omitted_null_and_empty_collections(self):
        for empty_value in (None, [], {}):
            with self.subTest(empty=empty_value):
                def mutate(value):
                    value["Mounts"] = empty_value
                    for key in ("Binds", "Mounts", "Tmpfs", "PortBindings", "CapAdd", "Devices"):
                        value["HostConfig"][key] = empty_value
                    value["Config"]["Volumes"] = empty_value
                    value["NetworkSettings"]["Ports"] = empty_value
                self.fake = FakeDocker()
                self.fake.mutate = mutate
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    self.assertEqual(self.run_check()["asset_check"], "passed")

    def test_created_container_policy_failure_never_starts(self):
        cases = (
            ("HostConfig", "ReadonlyRootfs", False), ("HostConfig", "NetworkMode", "bridge"),
            ("HostConfig", "Privileged", True), ("HostConfig", "PublishAllPorts", True),
            ("HostConfig", "AutoRemove", True), ("HostConfig", "CapDrop", []),
            ("HostConfig", "CapAdd", ["SYS_ADMIN"]), ("HostConfig", "SecurityOpt", []),
            ("HostConfig", "Binds", ["/fixture:/data"]),
            ("HostConfig", "Mounts", [{"Target": "/data"}]),
            ("HostConfig", "Tmpfs", {"/data": "rw"}),
            ("HostConfig", "PortBindings", {"6969/tcp": [{}]}),
            ("HostConfig", "Devices", [{"PathOnHost": "/dev/fixture"}]),
            ("HostConfig", "DeviceRequests", [{"Count": -1}]),
            ("HostConfig", "Memory", 0), ("HostConfig", "MemorySwap", -1),
            ("HostConfig", "PidsLimit", -1), ("HostConfig", "NanoCpus", 0),
            ("HostConfig", "PidMode", "host"), ("HostConfig", "IpcMode", "host"),
            ("HostConfig", "RestartPolicy", {"Name": "always"}),
            ("Config", "User", "0"), ("Config", "Cmd", ["--backend", "macos"]),
            ("Config", "Entrypoint", ["sh"]), ("Config", "Env", ["PRIVATE=value"]),
            ("Config", "Healthcheck", {"Test": ["CMD", "sh"]}),
            ("Config", "Tty", True), ("State", "Status", "running"),
            ("NetworkSettings", "Networks", {"bridge": {}}),
        )
        for section, key, value in cases:
            with self.subTest(section=section, key=key):
                self.fake = FakeDocker()
                self.fake.mutate = lambda obj: obj[section].__setitem__(key, value)
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    receipt = self.run_check()
                self.assertEqual(receipt["stage"], "created-inspect")
                self.assertFalse(self.actions("start"))
                self.assertEqual(receipt["cleanup"], "verified")

    def test_preexisting_name_is_never_touched(self):
        self.fake.existing = True
        self.assert_before_create("ownership")
        self.assertFalse(self.actions("inspect"))
        self.assertFalse(self.actions("rm"))

    def test_container_child_manifest_is_required_independently_of_config_id(self):
        for descriptor in (None, {}, {"digest": check.INDEX}, {"digest": "sha256:" + "0" * 64}):
            with self.subTest(descriptor=descriptor):
                self.fake = FakeDocker()
                self.fake.mutate = lambda obj: obj.__setitem__("ImageManifestDescriptor", descriptor)
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    receipt = self.run_check()
                self.assertEqual(receipt["stage"], "created-inspect")
                self.assertFalse(self.actions("start"))
                self.assertEqual(receipt["cleanup"], "verified")

    def test_lost_or_malformed_create_receipt_cleans_owned_container_without_start(self):
        for mode in ("timeout-present", "bad-receipt"):
            with self.subTest(mode=mode):
                self.fake = FakeDocker()
                self.fake.create_mode = mode
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    receipt = self.run_check()
                self.assertEqual(receipt["stage"], "create")
                self.assertEqual(receipt["cleanup"], "verified")
                self.assertEqual(receipt["container_id"], IDENTITY)
                self.assertFalse(self.actions("start"))

    def test_lost_create_without_visible_container_remains_unknown(self):
        self.fake.create_mode = "timeout-absent"
        receipt = self.run_check()
        self.assertEqual(receipt["cleanup"], "unknown")
        self.assertEqual(receipt["container_name"], NAME)
        self.assertFalse(self.actions("start"))
        self.assertFalse(self.actions("rm"))

    def test_identity_mismatch_prevents_start_and_removal(self):
        for key in ("Id", "Name", "Labels"):
            with self.subTest(key=key):
                self.fake = FakeDocker()
                def mutate(value):
                    if key == "Labels":
                        value["Config"][key] = {check.LABEL: "other"}
                    else:
                        value[key] = OTHER_ID if key == "Id" else "/other"
                self.fake.mutate = mutate
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    receipt = self.run_check()
                self.assertEqual(receipt["cleanup"], "unknown")
                self.assertIsNone(receipt["container_id"])
                self.assertFalse(self.actions("start"))
                self.assertFalse(self.actions("rm"))

    def test_start_failure_timeout_and_interrupt_never_replay(self):
        for mode in ("failed", "oom", "timeout", "interrupt", "stderr"):
            with self.subTest(mode=mode):
                self.fake = FakeDocker()
                self.fake.start_mode = mode
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    receipt = self.run_check()
                self.assertEqual(receipt["asset_check"], "failed")
                self.assertEqual(receipt["cleanup"], "verified")
                self.assertEqual(len(self.actions("start")), 1)
                if mode in ("failed", "oom"):
                    self.assertEqual(receipt["exit_code"], 137)
                    self.assertEqual(receipt["oom_killed"], mode == "oom")
                if mode == "timeout":
                    self.assertIsNone(receipt["exit_code"])

    def test_stdout_must_match_exactly(self):
        for output in (b"", check.EXPECTED_STDOUT.rstrip(), b"prefix\n" + check.EXPECTED_STDOUT,
                       check.EXPECTED_STDOUT + b"extra\n", PRIVATE.encode()):
            with self.subTest(output=output):
                self.fake = FakeDocker()
                self.fake.start_output = output
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    receipt = self.run_check()
                self.assertEqual(receipt["stage"], "stdout")
                self.assertEqual(receipt["cleanup"], "verified")

    def test_failed_removal_or_surviving_container_is_not_success(self):
        for mode in ("failed", "still-present"):
            with self.subTest(mode=mode):
                self.fake = FakeDocker()
                self.fake.remove_mode = mode
                with patch("check_image.subprocess.run", side_effect=self.fake):
                    receipt = self.run_check()
                self.assertEqual(receipt["asset_check"], "failed")
                self.assertEqual(receipt["cleanup"], "unknown")

    def test_invalid_inspection_is_sanitized_and_never_started(self):
        self.fake.inspect_mode = "malformed"
        receipt = self.run_check()
        self.assertEqual(receipt["stage"], "created-inspect")
        self.assertEqual(receipt["cleanup"], "unknown")
        self.assertFalse(self.actions("start"))

    def test_cli_receipt_and_exit_status(self):
        output = io.StringIO()
        with redirect_stdout(output):
            status = check.main(["--context", "fixture", "--expected-daemon-id", DAEMON])
        self.assertEqual(status, 0)
        receipt = json.loads(output.getvalue())
        self.assertEqual(receipt["asset_check"], "passed")
        self.assertNotIn(DAEMON, output.getvalue())
        self.assertNotIn("docker.sock", output.getvalue())

    def test_bad_arguments_never_echo_input_or_run_docker(self):
        for args in ([], ["--unknown", PRIVATE],
                     ["--context", "fixture", "--expected-daemon-id"]):
            error = io.StringIO()
            with redirect_stderr(error), self.assertRaises(SystemExit) as raised:
                check.main(args)
            self.assertEqual(raised.exception.code, 2)
            self.assertEqual(error.getvalue(), "asset_check=failed stage=arguments\n")
        self.assertFalse(self.fake.calls)


if __name__ == "__main__":
    unittest.main()
