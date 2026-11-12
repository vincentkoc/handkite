#!/usr/bin/env python3
"""Build a pinned iOS XCTest runner without installing or launching it."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import time


SOURCE_COMMIT = "02cb92f0e93c8b021443bcbb8eeb80932c113220"
PROJECT_SHA256 = "9a9cbda0e2ac34dac53ddb8253ce7b08ef58b25db44720f3281a47fc760aecff"
PROJECT = Path("WebDriverAgent.xcodeproj/project.pbxproj")
IOS_CONFIGS = ("EEF988321C486604005CA669", "EEF988331C486604005CA669")
ORIGINAL_BUNDLE = "PRODUCT_BUNDLE_IDENTIFIER = com.facebook.WebDriverAgentRunner;"
TEAM = re.compile(r"[A-Z0-9]{10}\Z")
UDID = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{8}-[0-9a-fA-F]{16})\Z")
BUNDLE = re.compile(r"[A-Za-z0-9]+(?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9]+(?:[A-Za-z0-9-]*[A-Za-z0-9])?)+\Z")


LOCK_HELPERS = Path("WebDriverAgentLib/Categories/XCUIDevice+FBHelpers.m")
LOCK_HEADER = Path("WebDriverAgentLib/Categories/XCUIDevice+FBHelpers.h")
LOCK_COMMANDS = Path("WebDriverAgentLib/Commands/FBCustomCommands.m")
LOCK_SOURCE_HASHES = {
    LOCK_HELPERS: "ecd4ffce2bee99210c3e2370f1462a42645e8440d6f86c059c39c96d03d8119d",
    LOCK_HEADER: "54e499e74a2ac9dc93a0a5021ac279a307d1f67111e1ef04a8912223a12afe41",
    LOCK_COMMANDS: "d42b495cbe6ae86577a97bb57d0c38604145f52cdf7073895b95ad963988c72a",
}
LOCK_REGISTER = '''+ (void)fb_registerAppforDetectLockState
{
  int notify_token;
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wstrict-prototypes"
  notify_register_dispatch("com.apple.springboard.lockstate", &notify_token, dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0), ^(int token) {
    uint64_t state = UINT64_MAX;
    notify_get_state(token, &state);
    fb_isLocked = state != 0;
  });
#pragma clang diagnostic pop
}'''
LOCK_REGISTER_FIXED = '''+ (void)fb_registerAppforDetectLockState
{
  int notify_token;
  uint32_t status = notify_register_dispatch("com.apple.springboard.lockstate", &notify_token, dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0), ^(int token) {
    fb_refreshLockState(token);
  });
  if (status == NOTIFY_STATUS_OK) {
    fb_refreshLockState(notify_token);
  } else {
    fb_invalidateLockState();
  }
}'''
LOCK_GETTER = '''- (BOOL)fb_isScreenLocked
{
  return fb_isLocked;
}'''
LOCK_GETTER_FIXED = '''- (BOOL)fb_isScreenLocked
{
  bool locked = false;
  return fb_copyLockState(&locked) && locked;
}
- (nullable NSNumber *)fb_observedScreenLockState
{
  bool locked = false;
  return fb_copyLockState(&locked) ? @(locked) : nil;
}'''
LOCK_ENDPOINT = '''+ (id<FBResponsePayload>)handleIsLocked:(FBRouteRequest *)request
{
  BOOL isLocked = [XCUIDevice sharedDevice].fb_isScreenLocked;
  return FBResponseWithObject(isLocked ? @YES : @NO);
}'''
LOCK_ENDPOINT_FIXED = '''+ (id<FBResponsePayload>)handleIsLocked:(FBRouteRequest *)request
{
  NSNumber *isLocked = [[XCUIDevice sharedDevice] fb_observedScreenLockState];
  if (nil == isLocked) {
    return FBResponseWithStatus([FBCommandStatus unknownErrorWithMessage:@"lock_state_unavailable"
                                                             traceback:nil]);
  }
  return FBResponseWithObject(isLocked);
}'''


def replace_once(text: str, before: str, after: str) -> str:
    if text.count(before) != 1:
        raise ValueError("lock_source_shape_mismatch")
    return text.replace(before, after)


def patch_lock_state(work: Path) -> dict:
    # Validate every preimage before changing the archived copy.
    original = {path: (work / path).read_bytes() for path in LOCK_SOURCE_HASHES}
    if any(hashlib.sha256(data).hexdigest() != LOCK_SOURCE_HASHES[path]
           for path, data in original.items()):
        raise ValueError("lock_source_mismatch")
    helpers = original[LOCK_HELPERS].decode("utf-8")
    helpers = replace_once(helpers, "#include <notify.h>",
                           '#include <notify.h>\n#include "FBHandkiteLockState.h"')
    helpers = replace_once(helpers, "static bool fb_isLocked;\n", "")
    helpers = replace_once(helpers, LOCK_REGISTER, LOCK_REGISTER_FIXED)
    helpers = replace_once(helpers, LOCK_GETTER, LOCK_GETTER_FIXED)
    if helpers.count("fb_isLocked") != 4:
        raise ValueError("lock_source_shape_mismatch")
    helpers = helpers.replace("fb_isLocked", "[self fb_isScreenLocked]")
    header = replace_once(original[LOCK_HEADER].decode("utf-8"),
                          "- (BOOL)fb_isScreenLocked;",
                          "- (BOOL)fb_isScreenLocked;\n- (nullable NSNumber *)fb_observedScreenLockState;")
    commands = replace_once(original[LOCK_COMMANDS].decode("utf-8"),
                            LOCK_ENDPOINT, LOCK_ENDPOINT_FIXED)
    output = {LOCK_HELPERS: helpers.encode(), LOCK_HEADER: header.encode(),
              LOCK_COMMANDS: commands.encode()}
    helper_path = LOCK_HELPERS.with_name("FBHandkiteLockState.h")
    if (work / helper_path).exists():
        raise ValueError("lock_helper_exists")
    output[helper_path] = Path(__file__).with_name("FBHandkiteLockState.h").read_bytes()
    with (work / helper_path).open("xb") as helper:
        helper.write(output[helper_path])
    for path, data in output.items():
        if path != helper_path:
            (work / path).write_bytes(data)
    return {
        "source_commit": SOURCE_COMMIT,
        "patch": "initial-lock-state-v1",
        "patch_tool_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "files": [{"path": str(path), "source_sha256": LOCK_SOURCE_HASHES.get(path),
                   "output_sha256": hashlib.sha256(data).hexdigest()}
                  for path, data in output.items()],
    }


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
        marker = f"\n\t\t{config} /* "
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


def xcode_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment.pop("CPATH", None)
    return environment


def resolved_settings(project: Path, team: str, udid: str,
                      generic_ios_destination: bool = False) -> dict[str, str]:
    destination = "generic/platform=iOS" if generic_ios_destination else f"platform=iOS,id={udid}"
    command = [
        "/usr/bin/xcodebuild", "-showBuildSettings",
        "-project", str(project), "-target", "WebDriverAgentRunner",
        "-configuration", "Debug", "-sdk", "iphoneos",
        "-destination", destination,
        f"DEVELOPMENT_TEAM={team}", "CODE_SIGN_STYLE=Automatic",
    ]
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
          allow_updates: bool, generic_ios_destination: bool = False) -> None:
    started = time.monotonic()
    progress("source_check", started)
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
    project_file = work / PROJECT
    project_file.write_bytes(patch_project(project_file.read_bytes(), bundle_id))
    patch_manifest = patch_lock_state(work)
    manifest_fd = os.open(output / "source-patches.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(manifest_fd, "w", encoding="utf-8") as manifest:
        json.dump(patch_manifest, manifest, sort_keys=True, indent=2)
        manifest.write("\n")
    progress("settings_check", started)
    settings = resolved_settings(work / "WebDriverAgent.xcodeproj", team, udid, generic_ios_destination)
    expected = {
        "PRODUCT_BUNDLE_IDENTIFIER": bundle_id,
        "DEVELOPMENT_TEAM": team,
        "CODE_SIGN_STYLE": "Automatic",
        "PLATFORM_NAME": "iphoneos",
    }
    if any(settings.get(key) != value for key, value in expected.items()):
        raise ValueError("build_settings_mismatch")
    destination = "generic/platform=iOS" if generic_ios_destination else f"platform=iOS,id={udid}"
    command = [
        "/usr/bin/xcodebuild", "clean", "build-for-testing",
        "-project", str(work / "WebDriverAgent.xcodeproj"),
        "-scheme", "WebDriverAgentRunner",
        "-configuration", "Debug",
        "-destination", destination,
        "-derivedDataPath", str(output / "DerivedData"),
        f"DEVELOPMENT_TEAM={team}", "CODE_SIGN_STYLE=Automatic",
    ]
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
    parser.add_argument("--generic-ios-destination", action="store_true",
                        help="build for generic iOS without requiring the pinned device to be connected")
    args = parser.parse_args()
    started = time.monotonic()
    try:
        build(args.source, args.output, args.team, args.udid, args.bundle_id,
              args.allow_provisioning_updates, args.generic_ios_destination)
    except (OSError, ValueError) as error:
        print(f"wda_build={error.args[0] if isinstance(error, ValueError) else 'io_failed'} "
              f"elapsed_seconds={time.monotonic() - started:.1f}", flush=True)
        return 1
    print(f"wda_build=completed_unverified elapsed_seconds={time.monotonic() - started:.1f}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
