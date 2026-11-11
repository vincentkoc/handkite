#!/usr/bin/env python3
"""Explicit asset check of a pinned amd64 image with container networking disabled."""

import argparse
import json
import os
import platform
import re
import stat
import subprocess
import sys
import uuid
from urllib.parse import urlsplit


INDEX = "sha256:2daafc9a0251416f25452ec561b3c2c1e9f2c9060e3e05b386fd6fe1cc1fd0ad"
CHILD = "sha256:c140655ebea9b0721bfe8da99a1239d762c258d14aa4fd8bef2ba63165105afa"
IMAGE = "ghcr.io/fdkevin0/anisette-server-rs@" + INDEX
PLATFORM = "linux/amd64"
COMMAND = ["--backend", "macos", "--check"]
EXPECTED_STDOUT = b"macos backend binaries verified\n"
LABEL = "com.handkite.anisette-asset-check"
MEMORY = 256 * 1024 * 1024
CLI_TIMEOUT = 15
CHECK_TIMEOUT = 30
OUTPUT_LIMIT = 1024 * 1024
CONTAINER_ID = re.compile(r"[0-9a-f]{64}\Z")


class Refused(Exception):
    """A fixed stage name replaces potentially sensitive tool diagnostics."""


def require(condition, stage):
    if not condition:
        raise Refused(stage)


def empty(value):
    # Docker omits some empty fields and serializes others as null.
    return value is None or value == [] or value == {}


class Docker:
    def __init__(self, context):
        self.context = context
        self.host = None
        self.socket_path = None
        self.socket_identity = None
        self.daemon_id = None

    def call(self, args, stage, timeout=CLI_TIMEOUT):
        route = ["--host", self.host] if self.host is not None else ["--context", self.context]
        try:
            result = subprocess.run(
                ["docker", *route, *args],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, timeout=timeout, check=False,
            )
        except (OSError, subprocess.SubprocessError):
            raise Refused(stage) from None
        require(len(result.stdout) <= OUTPUT_LIMIT and
                len(result.stderr) <= OUTPUT_LIMIT, stage)
        require(result.returncode == 0 and not result.stderr, stage)
        return result.stdout

    def object(self, args, stage, array=False):
        try:
            value = json.loads(self.call(args, stage))
        except (ValueError, UnicodeError):
            raise Refused(stage) from None
        if array:
            require(isinstance(value, list) and len(value) == 1, stage)
            value = value[0]
        require(isinstance(value, dict), stage)
        return value


def socket_identity(path, stage):
    try:
        value = os.stat(path)
    except OSError:
        raise Refused(stage) from None
    require(stat.S_ISSOCK(value.st_mode), stage)
    return value.st_dev, value.st_ino


def verify_route(docker):
    require(socket_identity(docker.socket_path, "endpoint") == docker.socket_identity, "endpoint")
    info = docker.object(["info", "--format", "{{json .}}"], "daemon")
    require(info.get("ID") == docker.daemon_id, "daemon")
    require(info.get("OSType") == "linux" and
            info.get("Architecture") in ("x86_64", "amd64"), "native-daemon")
    require(["driver-type", "io.containerd.snapshotter.v1"] in
            (info.get("DriverStatus") or []), "image-store")
    require(socket_identity(docker.socket_path, "endpoint") == docker.socket_identity, "endpoint")


def admit(docker, daemon_id):
    require(platform.system() == "Linux" and
            platform.machine() in ("x86_64", "amd64"), "native-host")
    require(not any(os.environ.get(key) for key in (
        "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH",
        "DOCKER_API_VERSION",
    )), "context")
    # Resolve the mutable context once; subsequent commands use only this endpoint.
    context = docker.object(["context", "inspect", docker.context], "context", True)
    require(context.get("Name") == docker.context, "context")
    endpoint = context.get("Endpoints", {}).get("docker", {})
    require(isinstance(endpoint, dict), "context")
    host = endpoint.get("Host")
    require(isinstance(host, str), "context")
    try:
        address = urlsplit(host)
        require(address.scheme == "unix" and not address.netloc and
                address.path.startswith("/") and not address.query and
                not address.fragment and "%" not in address.path, "context")
        identity = socket_identity(address.path, "context")
    except (OSError, ValueError):
        raise Refused("context") from None
    docker.host = host
    docker.socket_path = address.path
    docker.socket_identity = identity
    docker.daemon_id = daemon_id
    verify_route(docker)


def inspect_image(docker):
    value = docker.object(
        ["image", "inspect", "--platform", PLATFORM, IMAGE], "image", True,
    )
    # Docker 29 returns the platform's manifest digest here, not its config digest.
    require(value.get("Id") == CHILD and value.get("Os") == "linux" and
            value.get("Architecture") == "amd64", "image")
    require(IMAGE in (value.get("RepoDigests") or []), "image")
    config = value.get("Config")
    require(isinstance(config, dict), "image")
    require(config.get("User") == "10001:10001" and
            config.get("Entrypoint") == ["anisette-server"], "image")
    require(empty(config.get("Volumes")) and empty(config.get("Healthcheck")), "image")
    return config


def owned(value, name, token, expected_id=None):
    identity = value.get("Id")
    require(isinstance(identity, str) and CONTAINER_ID.fullmatch(identity), "ownership")
    require(expected_id is None or identity == expected_id, "ownership")
    require(value.get("Name") == "/" + name, "ownership")
    config = value.get("Config")
    require(isinstance(config, dict), "ownership")
    labels = config.get("Labels")
    require(isinstance(labels, dict) and labels.get(LABEL) == token, "ownership")
    return identity


def inspect_container(docker, target, stage):
    return docker.object(["container", "inspect", target], stage, True)


def contained(value, image_config, stage):
    config, host = value.get("Config"), value.get("HostConfig")
    require(isinstance(config, dict) and isinstance(host, dict), stage)
    descriptor = value.get("ImageManifestDescriptor")
    require(isinstance(descriptor, dict) and descriptor.get("digest") == CHILD, stage)
    # Container Image can be the config ID; the descriptor pins the child manifest.
    require(isinstance(value.get("Image"), str) and
            re.fullmatch(r"sha256:[0-9a-f]{64}", value["Image"]) and
            config.get("Image") == IMAGE, stage)
    require(value.get("Path") == "anisette-server" and value.get("Args") == COMMAND, stage)
    require(config.get("Entrypoint") == ["anisette-server"] and
            config.get("Cmd") == COMMAND and config.get("User") == "10001:10001", stage)
    require((config.get("Env") or []) == (image_config.get("Env") or []), stage)
    require(empty(config.get("Volumes")) and
            config.get("Healthcheck") == {"Test": ["NONE"]}, stage)
    for key in ("Tty", "OpenStdin", "StdinOnce", "AttachStdin"):
        require(config.get(key, False) is False, stage)
    require(config.get("AttachStdout") is True and config.get("AttachStderr") is True, stage)
    required = {
        "NetworkMode": "none", "ReadonlyRootfs": True, "Privileged": False,
        "PublishAllPorts": False, "AutoRemove": False, "CapDrop": ["ALL"],
        "SecurityOpt": ["no-new-privileges:true"], "NanoCpus": 1_000_000_000,
        "Memory": MEMORY, "MemorySwap": MEMORY, "PidsLimit": 32, "IpcMode": "none",
    }
    for key, expected in required.items():
        require(type(host.get(key)) is type(expected) and host.get(key) == expected, stage)
    for key in ("Binds", "Mounts", "Tmpfs", "VolumesFrom", "PortBindings", "CapAdd",
                "Devices", "DeviceRequests", "DeviceCgroupRules", "Links"):
        require(empty(host.get(key)), stage)
    for key in ("PidMode", "UTSMode", "UsernsMode"):
        require(host.get(key, "") == "", stage)
    restart, logging = host.get("RestartPolicy"), host.get("LogConfig")
    require(isinstance(restart, dict) and restart.get("Name") == "no" and
            restart.get("MaximumRetryCount", 0) == 0, stage)
    require(isinstance(logging, dict) and logging.get("Type") == "none" and
            empty(logging.get("Config")), stage)
    require(empty(value.get("Mounts")) and value.get("RestartCount") == 0, stage)
    network = value.get("NetworkSettings")
    require(isinstance(network, dict) and empty(network.get("Ports")), stage)
    networks = network.get("Networks")
    require(empty(networks) or isinstance(networks, dict) and set(networks) == {"none"}, stage)


def check_state(value, stage, status):
    state = value.get("State")
    require(isinstance(state, dict) and state.get("Status") == status, stage)
    for key in ("Running", "Paused", "Restarting", "Dead", "OOMKilled"):
        require(state.get(key) is False, stage)
    require(state.get("Pid") == 0 and state.get("Error", "") == "", stage)
    require(type(state.get("ExitCode")) is int and state.get("ExitCode") == 0, stage)


def create_args(name, token):
    return [
        "container", "create", "--name", name, "--label", LABEL + "=" + token,
        "--platform", PLATFORM, "--pull", "never", "--restart", "no",
        "--network", "none", "--ipc", "none", "--read-only",
        "--security-opt", "no-new-privileges:true", "--cap-drop", "ALL",
        "--user", "10001:10001", "--cpus", "1", "--memory", str(MEMORY),
        "--memory-swap", str(MEMORY), "--pids-limit", "32", "--log-driver", "none",
        "--no-healthcheck", "--attach", "stdout", "--attach", "stderr",
        "--entrypoint", "anisette-server", IMAGE, *COMMAND,
    ]


def listed(docker, filter_value):
    output = docker.call(
        ["container", "ls", "--all", "--no-trunc", "--filter", filter_value,
         "--format", "{{.ID}}"], "cleanup",
    )
    ids = output.splitlines()
    require(all(CONTAINER_ID.fullmatch(item.decode("ascii", errors="replace"))
                for item in ids), "cleanup")
    return [item.decode("ascii") for item in ids]


def cleanup(docker, name, token, identity):
    verify_route(docker)
    # A lost create receipt can be recovered only by this invocation's random name.
    matches = listed(docker, "name=^/" + name + "$")
    # An absent name cannot settle a timed-out create that has no ID receipt yet.
    require(matches or identity is not None, "cleanup")
    if matches:
        require(len(matches) == 1, "cleanup")
        value = inspect_container(docker, matches[0], "cleanup")
        identity = owned(value, name, token, identity)
        docker.call(["container", "rm", "--force", identity], "cleanup")
    if identity is not None:
        require(not listed(docker, "id=" + identity), "cleanup")
    require(not listed(docker, "name=^/" + name + "$"), "cleanup")
    verify_route(docker)
    return identity


def run_check(context, daemon_id):
    token = uuid.uuid4().hex
    name = "handkite-asset-check-" + token
    docker = Docker(context)
    receipt = {
        "asset_check": "failed", "stage": "arguments", "platform": PLATFORM,
        "index_digest": INDEX, "child_digest": CHILD, "execution": "not-started",
        "exit_code": None, "oom_killed": None, "stdout": "unverified",
        "cleanup": "not-needed", "container_name": None, "container_id": None,
    }
    created = False
    identity = None
    try:
        require(isinstance(context, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", context),
                "arguments")
        require(isinstance(daemon_id, str) and 1 <= len(daemon_id) <= 128 and
                re.fullmatch(r"[A-Za-z0-9:-]+", daemon_id), "arguments")
        admit(docker, daemon_id)
        image_config = inspect_image(docker)
        require(not listed(docker, "name=^/" + name + "$"), "ownership")
        verify_route(docker)
        created = True
        receipt["container_name"] = name
        output = docker.call(create_args(name, token), "create")
        require(re.fullmatch(rb"[0-9a-f]{64}\n", output), "create")
        identity = output[:-1].decode("ascii")
        value = inspect_container(docker, identity, "created-inspect")
        owned(value, name, token, identity)
        receipt["container_id"] = identity
        contained(value, image_config, "created-inspect")
        check_state(value, "created-inspect", "created")
        verify_route(docker)
        receipt["execution"] = "attempted"
        try:
            output = docker.call(["container", "start", "--attach", identity],
                                 "start", CHECK_TIMEOUT)
        finally:
            value = inspect_container(docker, identity, "exit-inspect")
            owned(value, name, token, identity)
            state = value.get("State", {})
            if state.get("Status") == "exited":
                code, oom = state.get("ExitCode"), state.get("OOMKilled")
                if type(code) is int and 0 <= code <= 255:
                    receipt["exit_code"] = code
                if type(oom) is bool:
                    receipt["oom_killed"] = oom
        contained(value, image_config, "exit-inspect")
        check_state(value, "exit-inspect", "exited")
        require(output == EXPECTED_STDOUT, "stdout")
        receipt.update(asset_check="passed", stage="complete", stdout="verified")
    except Refused as error:
        receipt["stage"] = str(error)
    except KeyboardInterrupt:
        receipt["stage"] = "interrupted"
    except (TypeError, AttributeError, KeyError):
        receipt["stage"] = "invalid-inspection"
    finally:
        if created:
            try:
                identity = cleanup(docker, name, token, identity)
                receipt["container_id"] = identity
                receipt["cleanup"] = "verified"
            except (Refused, KeyboardInterrupt, TypeError, AttributeError, KeyError):
                if receipt["asset_check"] == "passed":
                    receipt["stage"] = "cleanup"
                receipt["asset_check"] = "failed"
                receipt["cleanup"] = "unknown"
    return receipt


class Parser(argparse.ArgumentParser):
    def error(self, _message):
        self.exit(2, "asset_check=failed stage=arguments\n")


def main(argv=None):
    parser = Parser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--context", required=True, help="explicit local Unix Docker context")
    parser.add_argument("--expected-daemon-id", required=True, help="independently admitted daemon ID")
    args = parser.parse_args(argv)
    receipt = run_check(args.context, args.expected_daemon_id)
    print(json.dumps(receipt, sort_keys=True, separators=(",", ":")))
    return 0 if receipt["asset_check"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
