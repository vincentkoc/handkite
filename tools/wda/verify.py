#!/usr/bin/env python3
"""Read-only signature and profile admission for one built iOS XCTest runner."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
from pathlib import Path
import plistlib
import re
import subprocess
import tempfile


TEAM = re.compile(r"[A-Z0-9]{10}\Z")
UDID = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{8}-[0-9a-fA-F]{16})\Z")
SHA1 = re.compile(r"[0-9a-f]{40}\Z")
RUNNER_ID = re.compile(r"[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+\.xctrunner(?:\.[A-Z0-9]{10})?\Z")
MAX_PROFILE_BYTES = 1024 * 1024
MAX_TOOL_BYTES = 1024 * 1024
DEFAULT_MIN_VALIDITY_HOURS = 72
NOT_AFTER = re.compile(r"notAfter=([A-Z][a-z]{2}) +(\d{1,2}) (\d{2}:\d{2}:\d{2}) (\d{4}) GMT\n?")


def tool(command: list[str], *, cwd: Path | None = None) -> bytes:
    result = subprocess.run(command, cwd=cwd, capture_output=True, check=False)
    if result.returncode or len(result.stdout) > MAX_TOOL_BYTES or len(result.stderr) > MAX_TOOL_BYTES:
        raise ValueError("verification_tool_failed")
    return result.stdout


def no_symlinks(path: Path, root: Path) -> bool:
    if not path.is_relative_to(root):
        return False
    current = root
    if current.is_symlink():
        return False
    for part in path.relative_to(root).parts:
        current = current / part
        if current.is_symlink():
            return False
    return True


def products(output: Path) -> tuple[Path, Path, Path]:
    root = output / "DerivedData/Build/Products"
    if not root.is_dir() or root.is_symlink():
        raise ValueError("products_unavailable")
    apps = [
        path for path in root.rglob("*.app")
        if path.is_dir() and (path / "PlugIns/HandkiteRunner.xctest").is_dir()
    ]
    runs = [path for path in root.rglob("*.xctestrun") if path.is_file()]
    if len(apps) != 1 or len(runs) != 1:
        raise ValueError("products_ambiguous")
    app, run = apps[0], runs[0]
    test = app / "PlugIns/HandkiteRunner.xctest"
    for path in (app, test, run, app / "Info.plist", app / "embedded.mobileprovision"):
        if not no_symlinks(path, root):
            raise ValueError("products_unsafe")
    if not app.is_relative_to(root / "Debug-iphoneos"):
        raise ValueError("products_wrong_platform")
    return app, test, run


def signed_items(app: Path) -> list[Path]:
    items = [app]
    for path in app.rglob("*"):
        if path.is_symlink():
            raise ValueError("products_unsafe")
        if (path.is_dir() and path.suffix in {".app", ".appex", ".xctest", ".framework"}) or (
            path.is_file() and path.suffix == ".dylib"
        ):
            items.append(path)
    return sorted(set(items), key=lambda path: (-len(path.parts), str(path)))


def signature(path: Path, team: str) -> str:
    tool(["/usr/bin/codesign", "--verify", "--strict=all", "--all-architectures", str(path)])
    result = subprocess.run(
        ["/usr/bin/codesign", "-d", "--verbose=4", str(path)],
        capture_output=True,
        check=False,
    )
    if result.returncode or len(result.stderr) > MAX_TOOL_BYTES:
        raise ValueError("signature_unavailable")
    text = result.stderr.decode("utf-8", errors="replace")
    values = dict(
        match.groups()
        for line in text.splitlines()
        if (match := re.fullmatch(r"(TeamIdentifier|Identifier)=(.+)", line))
    )
    if values.get("TeamIdentifier") != team or not values.get("Identifier"):
        raise ValueError("signature_identity_mismatch")
    return values["Identifier"]


def require_validity(expires: datetime, now: datetime, min_hours: float, kind: str) -> float:
    """Refuse an expired or soon-expiring profile or certificate; return hours left."""
    left = (expires - now).total_seconds() / 3600
    if left <= 0:
        raise ValueError(f"{kind}_expired")
    if left < min_hours:
        raise ValueError(f"{kind}_expires_soon")
    return left


def certificate_not_after(leaf: Path) -> datetime:
    text = tool(["/usr/bin/openssl", "x509", "-inform", "DER", "-noout", "-enddate", "-in", str(leaf)])
    match = NOT_AFTER.fullmatch(text.decode("ascii", errors="replace"))
    if not match:
        raise ValueError("signing_certificate_unavailable")
    try:
        parsed = datetime.strptime(" ".join(match.groups()), "%b %d %H:%M:%S %Y")
    except ValueError as error:
        raise ValueError("signing_certificate_unavailable") from error
    return parsed.replace(tzinfo=timezone.utc)


def leaf_certificate(app: Path) -> tuple[str, datetime]:
    """Return the leaf signing certificate SHA-1 and its notAfter."""
    with tempfile.TemporaryDirectory(prefix="handkite-wda-cert-") as directory:
        root = Path(directory)
        tool(["/usr/bin/codesign", "-d", "--extract-certificates", str(app)], cwd=root)
        leaf = root / "codesign0"
        if not leaf.is_file() or leaf.is_symlink() or leaf.stat().st_size > MAX_PROFILE_BYTES:
            raise ValueError("signing_certificate_unavailable")
        return hashlib.sha1(leaf.read_bytes()).hexdigest(), certificate_not_after(leaf)


def allowed_claim(claim: object, allowance: object) -> bool:
    if isinstance(allowance, str):
        if not isinstance(claim, str) or "*" in claim:
            return False
        if "*" not in allowance:
            return claim == allowance
        if allowance.count("*") != 1 or not allowance.endswith(".*"):
            raise ValueError("entitlement_allowance_unsupported")
        prefix = allowance[:-1]
        return claim.startswith(prefix) and len(claim) > len(prefix)
    if isinstance(allowance, bool):
        return type(claim) is bool and claim == allowance
    if isinstance(allowance, int):
        return type(claim) is int and claim == allowance
    if isinstance(allowance, list):
        if not all(isinstance(item, str) for item in allowance) or not isinstance(claim, list):
            raise ValueError("entitlement_allowance_unsupported")
        return all(isinstance(item, str) and any(allowed_claim(item, option) for option in allowance) for item in claim)
    raise ValueError("entitlement_allowance_unsupported")


def profile(
    app: Path,
    team: str,
    udid: str,
    runner_id: str,
    cert_sha1: str,
    min_hours: float = DEFAULT_MIN_VALIDITY_HOURS,
    now: datetime | None = None,
) -> tuple[float, float]:
    """Admit the embedded profile; return (hours left, lifetime in days)."""
    embedded = app / "embedded.mobileprovision"
    if embedded.stat().st_size > MAX_PROFILE_BYTES:
        raise ValueError("profile_invalid")
    decoded = tool(["/usr/bin/security", "cms", "-D", "-i", str(embedded)])
    try:
        fields = plistlib.loads(decoded)
        expires = fields["ExpirationDate"]
        created = fields["CreationDate"]
        entitlements = fields["Entitlements"]
        certs = fields["DeveloperCertificates"]
    except (KeyError, TypeError, ValueError, plistlib.InvalidFileException) as error:
        raise ValueError("profile_invalid") from error
    if (
        not isinstance(fields, dict)
        or not isinstance(expires, datetime)
        or not isinstance(created, datetime)
        or not isinstance(entitlements, dict)
    ):
        raise ValueError("profile_invalid")
    expires = expires.replace(tzinfo=timezone.utc) if expires.tzinfo is None else expires
    created = created.replace(tzinfo=timezone.utc) if created.tzinfo is None else created
    left = require_validity(expires, now or datetime.now(timezone.utc), min_hours, "profile")
    if created >= expires:
        raise ValueError("profile_invalid")
    lifetime_days = (expires - created).total_seconds() / 86400
    if (
        fields.get("TeamIdentifier") != [team]
        or udid not in fields.get("ProvisionedDevices", [])
        or not allowed_claim(f"{team}.{runner_id}", entitlements.get("application-identifier"))
        or entitlements.get("com.apple.developer.team-identifier") != team
        or entitlements.get("get-task-allow") is not True
        or not isinstance(certs, list)
        or cert_sha1 not in {hashlib.sha1(cert).hexdigest() for cert in certs if isinstance(cert, bytes)}
    ):
        raise ValueError("profile_identity_mismatch")
    signed = tool(["/usr/bin/codesign", "-d", "--entitlements", ":-", str(app)])
    try:
        actual = plistlib.loads(signed)
    except (ValueError, plistlib.InvalidFileException) as error:
        raise ValueError("entitlements_invalid") from error
    if not isinstance(actual, dict) or (
        actual.get("application-identifier") != f"{team}.{runner_id}"
        or actual.get("com.apple.developer.team-identifier") != team
        or actual.get("get-task-allow") is not True
    ):
        raise ValueError("signed_entitlement_identity_mismatch")
    if any(key not in entitlements or not allowed_claim(value, entitlements[key]) for key, value in actual.items()):
        raise ValueError("entitlements_mismatch")
    return left, lifetime_days


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def hash_tree(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError("products_unsafe")
        if path.is_file():
            name = path.relative_to(root).as_posix().encode("utf-8")
            digest.update(len(name).to_bytes(4, "big"))
            digest.update(name)
            digest.update(bytes.fromhex(hash_file(path)))
    return digest.hexdigest()


def verify(
    output: Path,
    team: str,
    udid: str,
    runner_id: str,
    cert_sha1: str,
    min_hours: float = DEFAULT_MIN_VALIDITY_HOURS,
) -> dict[str, str]:
    if (
        not min_hours >= 0
        or
        not output.is_absolute()
        or not TEAM.fullmatch(team)
        or not UDID.fullmatch(udid)
        or not RUNNER_ID.fullmatch(runner_id)
        or not SHA1.fullmatch(cert_sha1)
    ):
        raise ValueError("pins_invalid")
    app, test, run = products(output)
    with (app / "Info.plist").open("rb") as stream:
        actual_id = plistlib.load(stream).get("CFBundleIdentifier")
    if actual_id != runner_id:
        raise ValueError("runner_identity_mismatch")
    identifiers = {item: signature(item, team) for item in signed_items(app)}
    if identifiers.get(app) != runner_id or test not in identifiers:
        raise ValueError("runner_signature_mismatch")
    leaf_digest, cert_expires = leaf_certificate(app)
    if leaf_digest != cert_sha1:
        raise ValueError("signing_certificate_mismatch")
    now = datetime.now(timezone.utc)
    cert_left = require_validity(cert_expires, now, min_hours, "certificate")
    profile_left, lifetime_days = profile(app, team, udid, runner_id, cert_sha1, min_hours, now)
    # Hours and lifetime only, never identifiers: a ~7-day lifetime marks a free-account profile.
    return {
        "runner_tree_sha256": hash_tree(app),
        "xctestrun_sha256": hash_file(run),
        "profile_expires_in_hours": str(int(profile_left)),
        "certificate_expires_in_hours": str(int(cert_left)),
        "profile_lifetime_days": f"{lifetime_days:.1f}",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--team", required=True)
    parser.add_argument("--udid", required=True)
    parser.add_argument("--runner-id", required=True)
    parser.add_argument("--certificate-sha1", required=True)
    parser.add_argument(
        "--min-validity-hours",
        type=float,
        default=DEFAULT_MIN_VALIDITY_HOURS,
        help="refuse a profile or certificate with less time left (default: %(default)s)",
    )
    args = parser.parse_args()
    try:
        result = verify(
            args.output,
            args.team,
            args.udid,
            args.runner_id,
            args.certificate_sha1,
            args.min_validity_hours,
        )
    except (OSError, ValueError) as error:
        print(f"wda_verify={error.args[0] if isinstance(error, ValueError) else 'io_failed'}")
        return 1
    print("wda_verify=pass " + " ".join(f"{key}={value}" for key, value in result.items()))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
