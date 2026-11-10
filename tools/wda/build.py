#!/usr/bin/env python3
"""Build a pinned iOS XCTest runner without installing or launching it."""

import argparse
import hashlib
import io
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile


SOURCE_COMMIT = "02cb92f0e93c8b021443bcbb8eeb80932c113220"
PROJECT_SHA256 = "9a9cbda0e2ac34dac53ddb8253ce7b08ef58b25db44720f3281a47fc760aecff"
PROJECT = Path("WebDriverAgent.xcodeproj/project.pbxproj")
IOS_CONFIGS = ("EEF988321C486604005CA669", "EEF988331C486604005CA669")
ORIGINAL_BUNDLE = "PRODUCT_BUNDLE_IDENTIFIER = com.facebook.WebDriverAgentRunner;"
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


def patch_project(data: bytes, bundle_id: str) -> bytes:
    if hashlib.sha256(data).hexdigest() != PROJECT_SHA256:
        raise ValueError("project_source_mismatch")
    if not BUNDLE.fullmatch(bundle_id) or len(bundle_id) > 235:
        raise ValueError("bundle_id_invalid")
    text = data.decode("utf-8")
    replacement = f"PRODUCT_BUNDLE_IDENTIFIER = {bundle_id};"
    for config in IOS_CONFIGS:
        marker = f"\t\t{config} /* "
        start = text.find(marker)
        if start < 0 or text.find(marker, start + 1) >= 0:
            raise ValueError("runner_configuration_missing")
        end = text.find("\n\t\t};", start)
        if end < 0:
            raise ValueError("runner_configuration_missing")
        block = text[start:end]
        if (
            block.count(ORIGINAL_BUNDLE) != 1
            or "baseConfigurationReference = EEE5CABF1C80361500CBBDD9 /* IOSSettings.xcconfig */;" not in block
            or "INFOPLIST_FILE = WebDriverAgentRunner/Info.plist;" not in block
            or "USES_XCTRUNNER = YES;" not in block
        ):
            raise ValueError("runner_configuration_mismatch")
        text = text[:start] + block.replace(ORIGINAL_BUNDLE, replacement) + text[end:]
    return text.encode("utf-8")


def archive_source(source: Path, destination: Path) -> None:
    process = subprocess.Popen(
        ["git", "-C", str(source), "archive", "--format=tar", SOURCE_COMMIT],
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


def resolved_settings(project: Path, team: str, udid: str) -> dict[str, str]:
    command = [
        "/usr/bin/xcodebuild", "-showBuildSettings",
        "-project", str(project), "-target", "WebDriverAgentRunner",
        "-configuration", "Debug", "-sdk", "iphoneos",
        "-destination", f"platform=iOS,id={udid}",
        f"DEVELOPMENT_TEAM={team}", "CODE_SIGN_STYLE=Automatic",
    ]
    result = subprocess.run(command, capture_output=True, check=False)
    if result.returncode:
        raise ValueError("build_settings_unavailable")
    settings = {}
    for line in result.stdout.decode("utf-8", errors="replace").splitlines():
        if match := re.fullmatch(r"\s*([A-Z_]+) = (.*)", line):
            settings[match.group(1)] = match.group(2)
    return settings


def build(source: Path, output: Path, team: str, udid: str, bundle_id: str, allow_updates: bool) -> None:
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
    output.mkdir(mode=0o700)
    work = output / "source"
    work.mkdir(mode=0o700)
    archive_source(source, work)
    project_file = work / PROJECT
    project_file.write_bytes(patch_project(project_file.read_bytes(), bundle_id))
    settings = resolved_settings(work / "WebDriverAgent.xcodeproj", team, udid)
    expected = {
        "PRODUCT_BUNDLE_IDENTIFIER": bundle_id,
        "DEVELOPMENT_TEAM": team,
        "CODE_SIGN_STYLE": "Automatic",
        "PLATFORM_NAME": "iphoneos",
    }
    if any(settings.get(key) != value for key, value in expected.items()):
        raise ValueError("build_settings_mismatch")
    command = [
        "/usr/bin/xcodebuild", "clean", "build-for-testing",
        "-project", str(work / "WebDriverAgent.xcodeproj"),
        "-scheme", "WebDriverAgentRunner",
        "-configuration", "Debug",
        "-destination", f"platform=iOS,id={udid}",
        "-derivedDataPath", str(output / "DerivedData"),
        f"DEVELOPMENT_TEAM={team}", "CODE_SIGN_STYLE=Automatic",
    ]
    if allow_updates:
        command.append("-allowProvisioningUpdates")
    log = output / "build.log"
    fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        result = subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT, check=False)
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
    args = parser.parse_args()
    try:
        build(args.source, args.output, args.team, args.udid, args.bundle_id, args.allow_provisioning_updates)
    except (OSError, ValueError) as error:
        print(f"wda_build={error.args[0] if isinstance(error, ValueError) else 'io_failed'}")
        return 1
    print("wda_build=completed_unverified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
