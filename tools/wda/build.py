#!/usr/bin/env python3
"""Build a pinned iOS XCTest runner without installing or launching it."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import time


SOURCE_COMMIT = "1262085ed9a13ef4e0d0e222cf0a918ecfe4b565"
SOURCE_SUBDIRECTORY = "packages/webdriveragent"
PROJECT_SHA256 = "bbc76ffc83962e55e6a0f941e86e71ee978d501b0e29c388579788a48056c3f3"
SCHEME_SHA256 = "73d246bbc783189cc0400a3f93a6f4174a86396d522f92c7c525598292f9daba"
PROJECT = Path("WebDriverAgent.xcodeproj/project.pbxproj")
SCHEME = "HandkiteRunner"
SCHEME_FILE = Path("WebDriverAgent.xcodeproj/xcshareddata/xcschemes/HandkiteRunner.xcscheme")
TEAM = re.compile(r"[A-Z0-9]{10}\Z")
UDID = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{8}-[0-9a-fA-F]{16})\Z")
BUNDLE = re.compile(r"[A-Za-z0-9]+(?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9]+(?:[A-Za-z0-9-]*[A-Za-z0-9])?)+\Z")


def source_is_pinned(source: Path) -> bool:
    if not source.is_absolute() or not source.is_dir():
        return False
    commands = (
        (["git", "-C", str(source), "rev-parse", "--show-toplevel"], str(source.resolve())),
        (["git", "-C", str(source), "rev-parse", "HEAD"], SOURCE_COMMIT),
        (["git", "-C", str(source), "status", "--porcelain=v1", "--untracked-files=all"], ""),
    )
    for command, expected in commands:
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode or result.stdout.strip() != expected:
            return False
    return True


def source_manifest(work: Path) -> dict:
    for path, expected in ((PROJECT, PROJECT_SHA256), (SCHEME_FILE, SCHEME_SHA256)):
        if hashlib.sha256((work / path).read_bytes()).hexdigest() != expected:
            raise ValueError("project_source_mismatch")
    return {
        "repository": "vincentkoc/handkite-wda",
        "source_commit": SOURCE_COMMIT,
        "source_subdirectory": SOURCE_SUBDIRECTORY,
        "project_sha256": PROJECT_SHA256,
        "scheme_sha256": SCHEME_SHA256,
    }


def archive_source(source: Path, destination: Path) -> None:
    process = subprocess.Popen(
        ["git", "-C", str(source), "archive", "--format=tar", f"{SOURCE_COMMIT}:{SOURCE_SUBDIRECTORY}"],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    assert process.stdout is not None
    try:
        with tarfile.open(fileobj=process.stdout, mode="r|") as archive:
            for member in archive:
                name = PurePosixPath(member.name)
                if (
                    name.is_absolute()
                    or ".." in name.parts
                    or not name.parts
                    or not (member.isdir() or member.isfile())
                ):
                    raise ValueError("source_archive_unsafe")
                target = destination.joinpath(*name.parts)
                if member.isdir():
                    target.mkdir(mode=0o700, parents=True, exist_ok=True)
                else:
                    target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                    stream = archive.extractfile(member)
                    if stream is None:
                        raise ValueError("source_archive_unsafe")
                    with target.open("xb") as output:
                        while chunk := stream.read(1024 * 1024):
                            output.write(chunk)
                    target.chmod(member.mode & 0o777)
    finally:
        process.stdout.close()
        if process.wait() != 0:
            raise ValueError("source_archive_failed")


def xcode_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment.pop("CPATH", None)
    return environment


def resolved_settings(project: Path, team: str, udid: str, bundle_id: str,
                      generic_ios_destination: bool = False, unsigned: bool = False) -> dict[str, str]:
    destination = "generic/platform=iOS" if generic_ios_destination else f"platform=iOS,id={udid}"
    command = [
        "/usr/bin/xcodebuild", "-showBuildSettings",
        "-project", str(project), "-target", "WebDriverAgentRunner",
        "-configuration", "Debug", "-sdk", "iphoneos",
        "-destination", destination,
        f"DEVELOPMENT_TEAM={team}", "CODE_SIGN_STYLE=Automatic",
        f"HANDKITE_RUNNER_BUNDLE_ID={bundle_id}",
    ]
    if unsigned:
        command += ["CODE_SIGNING_ALLOWED=NO", "CODE_SIGNING_REQUIRED=NO"]
    result = subprocess.run(command, capture_output=True, check=False, env=xcode_environment())
    if result.returncode:
        raise ValueError("build_settings_unavailable")
    settings = {}
    for line in result.stdout.decode("utf-8", errors="replace").splitlines():
        if match := re.fullmatch(r"\s*([A-Z_]+) = (.*)", line):
            settings[match.group(1)] = match.group(2)
    return settings


def progress(phase: str, started: float) -> None:
    print(f"wda_build_phase={phase} elapsed_seconds={time.monotonic() - started:.1f}", flush=True)


def build(source: Path, output: Path, team: str, udid: str, bundle_id: str,
          allow_updates: bool, generic_ios_destination: bool = False, unsigned: bool = False) -> None:
    started = time.monotonic()
    progress("source_check", started)
    if unsigned and allow_updates:
        raise ValueError("unsigned_provisioning_conflict")
    if not TEAM.fullmatch(team) or not UDID.fullmatch(udid):
        raise ValueError("identity_invalid")
    if not BUNDLE.fullmatch(bundle_id) or len(bundle_id) > 235:
        raise ValueError("bundle_id_invalid")
    if not output.is_absolute() or not output.parent.is_dir() or output.exists():
        raise ValueError("output_unavailable")
    if output.resolve().is_relative_to(source.resolve()):
        raise ValueError("output_overlaps_source")
    if not source_is_pinned(source):
        raise ValueError("source_unqualified")
    progress("source_prepare", started)
    output.mkdir(mode=0o700)
    work = output / "source"
    work.mkdir(mode=0o700)
    archive_source(source, work)
    provenance = source_manifest(work)
    provenance["signing"] = "unsigned" if unsigned else "xcode"
    manifest_fd = os.open(output / "source-provenance.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(manifest_fd, "w", encoding="utf-8") as manifest:
        json.dump(provenance, manifest, sort_keys=True, indent=2)
        manifest.write("\n")
    progress("settings_check", started)
    settings = resolved_settings(work / "WebDriverAgent.xcodeproj", team, udid, bundle_id, generic_ios_destination, unsigned)
    expected = {
        "PRODUCT_BUNDLE_IDENTIFIER": bundle_id,
        "PRODUCT_NAME": SCHEME,
        "DEVELOPMENT_TEAM": team,
        "CODE_SIGN_STYLE": "Automatic",
        "PLATFORM_NAME": "iphoneos",
    }
    if unsigned:
        expected["CODE_SIGNING_ALLOWED"] = "NO"
        expected["CODE_SIGNING_REQUIRED"] = "NO"
    if any(settings.get(key) != value for key, value in expected.items()):
        raise ValueError("build_settings_mismatch")
    destination = "generic/platform=iOS" if generic_ios_destination else f"platform=iOS,id={udid}"
    command = [
        "/usr/bin/xcodebuild", "clean", "build-for-testing",
        "-project", str(work / "WebDriverAgent.xcodeproj"),
        "-scheme", SCHEME,
        "-configuration", "Debug",
        "-destination", destination,
        "-derivedDataPath", str(output / "DerivedData"),
        f"DEVELOPMENT_TEAM={team}", "CODE_SIGN_STYLE=Automatic",
        f"HANDKITE_RUNNER_BUNDLE_ID={bundle_id}",
    ]
    if unsigned:
        command += ["CODE_SIGNING_ALLOWED=NO", "CODE_SIGNING_REQUIRED=NO"]
    if allow_updates:
        command.append("-allowProvisioningUpdates")
    log = output / "build.log"
    fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        progress("build_start", started)
        result = subprocess.run(
            command, stdout=stream, stderr=subprocess.STDOUT,
            check=False, env=xcode_environment(),
        )
    if result.returncode:
        raise ValueError("xcode_build_failed")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--team", required=True)
    parser.add_argument("--udid", required=True)
    parser.add_argument("--bundle-id", required=True)
    parser.add_argument("--allow-provisioning-updates", action="store_true")
    parser.add_argument("--unsigned", action="store_true",
                        help="compile without signing; output still requires signed verification before installation")
    parser.add_argument("--generic-ios-destination", action="store_true",
                        help="build for generic iOS without requiring the pinned device to be connected")
    args = parser.parse_args()
    started = time.monotonic()
    try:
        build(args.source, args.output, args.team, args.udid, args.bundle_id,
              args.allow_provisioning_updates, args.generic_ios_destination, args.unsigned)
    except (OSError, ValueError) as error:
        print(f"wda_build={error.args[0] if isinstance(error, ValueError) else 'io_failed'} "
              f"elapsed_seconds={time.monotonic() - started:.1f}", flush=True)
        return 1
    print(f"wda_build=completed_unverified elapsed_seconds={time.monotonic() - started:.1f}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
