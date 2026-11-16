from datetime import datetime, timedelta, timezone
import importlib.util
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("wda_verify", Path(__file__).with_name("verify.py"))
assert SPEC and SPEC.loader
verify = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verify)

TEAM = "ABCDE12345"
UDID = "a" * 40
MODERN_UDID = "00008120-00123456789A1234"
RUNNER = "com.example.Runner.xctrunner"
CERT = "b" * 40


def fixture(directory: str) -> tuple[Path, Path, Path]:
    output = Path(directory) / "build"
    root = output / "DerivedData/Build/Products/Debug-iphoneos"
    app = root / "HandkiteRunner-Runner.app"
    test = app / "PlugIns/HandkiteRunner.xctest"
    test.mkdir(parents=True)
    (app / "Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": RUNNER}))
    (app / "embedded.mobileprovision").write_bytes(b"fixture")
    (test / "test-binary").write_bytes(b"fixture")
    run = root / "HandkiteRunner.xctestrun"
    run.write_bytes(b"fixture")
    return output, app, run


class VerifyTests(unittest.TestCase):
    def test_tool_postpones_annotations_and_preserves_subprocess_contract(self):
        self.assertEqual(verify.tool.__annotations__["cwd"], "Path | None")
        command = ["fixture-tool", "--read-only"]
        directory = Path("/fixture")
        with mock.patch.object(verify.subprocess, "run", return_value=mock.Mock(
            returncode=0, stdout=b"fixture", stderr=b"",
        )) as run:
            self.assertEqual(verify.tool(command, cwd=directory), b"fixture")
        run.assert_called_once_with(command, cwd=directory, capture_output=True, check=False)

    def test_entitlement_allowances_are_bounded(self):
        self.assertTrue(verify.allowed_claim("ABCDE12345.com.example.Runner", "ABCDE12345.*"))
        self.assertFalse(verify.allowed_claim("OTHER12345.com.example.Runner", "ABCDE12345.*"))
        self.assertFalse(verify.allowed_claim("ABCDE12345.*", "ABCDE12345.*"))
        self.assertTrue(verify.allowed_claim(["ABCDE12345.runner"], ["ABCDE12345.*"]))
        self.assertFalse(verify.allowed_claim(["ABCDE12345.runner", "OTHER12345.runner"], ["ABCDE12345.*"]))
        with self.assertRaisesRegex(ValueError, "entitlement_allowance_unsupported"):
            verify.allowed_claim("ABCDE12345.runner", "ABCDE12345.r*er")
        with self.assertRaisesRegex(ValueError, "entitlement_allowance_unsupported"):
            verify.allowed_claim({"nested": True}, {"nested": True})

    def test_udid_accepts_legacy_and_modern_forms_only(self):
        self.assertIsNotNone(verify.UDID.fullmatch(UDID))
        self.assertIsNotNone(verify.UDID.fullmatch(MODERN_UDID))
        for invalid in (
            "a" * 39,
            "00008120-00123456789A123",
            "00008120-00123456789A12345",
            "00008120_00123456789A1234",
        ):
            self.assertIsNone(verify.UDID.fullmatch(invalid))

    def test_exact_runner_and_xctestrun_are_required(self):
        with tempfile.TemporaryDirectory() as directory:
            output, app, run = fixture(directory)
            self.assertEqual(verify.products(output), (app, app / "PlugIns/HandkiteRunner.xctest", run))
            run.with_name("extra.xctestrun").write_bytes(b"other")
            with self.assertRaisesRegex(ValueError, "products_ambiguous"):
                verify.products(output)

    def test_symlinked_nested_code_is_rejected_before_signer(self):
        with tempfile.TemporaryDirectory() as directory:
            output, app, _ = fixture(directory)
            (app / "Frameworks").mkdir()
            (app / "Frameworks/unsafe.dylib").symlink_to("/etc/hosts")
            with self.assertRaisesRegex(ValueError, "products_unsafe"):
                verify.signed_items(app)
            with self.assertRaisesRegex(ValueError, "products_unsafe"):
                verify.hash_tree(app)

    def test_profile_requires_device_cert_and_signed_entitlements(self):
        with tempfile.TemporaryDirectory() as directory:
            _, app, _ = fixture(directory)
            import hashlib
            cert = b"synthetic-cert"
            digest = hashlib.sha1(cert).hexdigest()
            entitlements = {
                "application-identifier": f"{TEAM}.*",
                "com.apple.developer.team-identifier": TEAM,
                "get-task-allow": True,
                "keychain-access-groups": [f"{TEAM}.*"],
                "com.apple.developer.extra-profile-allowance": True,
            }
            signed = {
                "application-identifier": f"{TEAM}.{RUNNER}",
                "com.apple.developer.team-identifier": TEAM,
                "get-task-allow": True,
                "keychain-access-groups": [f"{TEAM}.{RUNNER}"],
            }
            fields = {
                "TeamIdentifier": [TEAM],
                "ProvisionedDevices": [MODERN_UDID],
                "CreationDate": datetime.now(timezone.utc) - timedelta(days=1),
                "ExpirationDate": datetime.now(timezone.utc) + timedelta(days=6),
                "DeveloperCertificates": [cert],
                "Entitlements": entitlements,
            }

            def command(arguments):
                return plistlib.dumps(fields if arguments[0] == "/usr/bin/security" else signed)

            with mock.patch.object(verify, "tool", side_effect=command):
                left, lifetime = verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                self.assertAlmostEqual(left, 6 * 24, delta=0.1)
                self.assertAlmostEqual(lifetime, 7.0, delta=0.01)
                fields["ProvisionedDevices"] = ["other"]
                with self.assertRaisesRegex(ValueError, "profile_identity_mismatch"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                fields["ProvisionedDevices"] = [MODERN_UDID]
                fields["ExpirationDate"] = datetime.now(timezone.utc) - timedelta(days=1)
                with self.assertRaisesRegex(ValueError, "profile_expired"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                fields["ExpirationDate"] = datetime.now(timezone.utc) + timedelta(days=1)
                with self.assertRaisesRegex(ValueError, "profile_expires_soon"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                left, _ = verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest, min_hours=12)
                self.assertAlmostEqual(left, 24, delta=0.1)
                created = fields.pop("CreationDate")
                with self.assertRaisesRegex(ValueError, "profile_invalid"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                fields["CreationDate"] = fields["ExpirationDate"]
                with self.assertRaisesRegex(ValueError, "profile_invalid"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest, min_hours=12)
                fields["CreationDate"] = created
                fields["ExpirationDate"] = datetime.now(timezone.utc) + timedelta(days=6)
                signed["keychain-access-groups"] = [f"{TEAM}.unrelated"]
                entitlements["keychain-access-groups"] = [f"{TEAM}.{RUNNER}"]
                with self.assertRaisesRegex(ValueError, "entitlements_mismatch"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                signed["keychain-access-groups"] = [f"{TEAM}.{RUNNER}"]
                signed["com.apple.developer.unlisted"] = True
                with self.assertRaisesRegex(ValueError, "entitlements_mismatch"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                del signed["com.apple.developer.unlisted"]
                signed["application-identifier"] = f"{TEAM}.wrong"
                with self.assertRaisesRegex(ValueError, "signed_entitlement_identity_mismatch"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)

    def test_runner_identity_rejects_before_any_signature_check(self):
        with tempfile.TemporaryDirectory() as directory:
            output, app, _ = fixture(directory)
            with (app / "Info.plist").open("wb") as stream:
                plistlib.dump({"CFBundleIdentifier": "com.example.other"}, stream)
            with mock.patch.object(verify, "signature") as signer:
                with self.assertRaisesRegex(ValueError, "runner_identity_mismatch"):
                    verify.verify(output, TEAM, UDID, RUNNER, CERT)
            signer.assert_not_called()

    def test_complete_fixture_checks_each_nested_signature_and_returns_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            output, app, run = fixture(directory)
            cert_expires = datetime.now(timezone.utc) + timedelta(days=300, minutes=1)
            with (
                mock.patch.object(verify, "signature", side_effect=lambda path, _: RUNNER if path == app else "test.id") as signer,
                mock.patch.object(verify, "leaf_certificate", return_value=(CERT, cert_expires)),
                mock.patch.object(verify, "profile", return_value=(150.5, 7.0)) as profile,
            ):
                result = verify.verify(output, TEAM, MODERN_UDID, RUNNER, CERT)
            self.assertEqual(signer.call_count, 2)
            profile.assert_called_once_with(app, TEAM, MODERN_UDID, RUNNER, CERT, 72, mock.ANY)
            self.assertEqual(len(result["runner_tree_sha256"]), 64)
            self.assertEqual(result["xctestrun_sha256"], verify.hash_file(run))
            self.assertEqual(result["profile_expires_in_hours"], "150")
            self.assertEqual(result["certificate_expires_in_hours"], str(300 * 24))
            self.assertEqual(result["profile_lifetime_days"], "7.0")

    def test_certificate_expiry_is_refused_before_profile_admission(self):
        with tempfile.TemporaryDirectory() as directory:
            output, app, _ = fixture(directory)
            for delta, outcome in (
                (timedelta(hours=-1), "certificate_expired"),
                (timedelta(hours=10), "certificate_expires_soon"),
            ):
                with (
                    mock.patch.object(verify, "signature", side_effect=lambda path, _: RUNNER if path == app else "test.id"),
                    mock.patch.object(
                        verify, "leaf_certificate", return_value=(CERT, datetime.now(timezone.utc) + delta)
                    ),
                    mock.patch.object(verify, "profile") as profile,
                ):
                    with self.assertRaisesRegex(ValueError, outcome):
                        verify.verify(output, TEAM, MODERN_UDID, RUNNER, CERT)
                profile.assert_not_called()

    def test_negative_or_nan_min_validity_is_refused(self):
        for hours in (-1, float("nan")):
            with self.assertRaisesRegex(ValueError, "pins_invalid"):
                verify.verify(Path("/fixture"), TEAM, MODERN_UDID, RUNNER, CERT, hours)

    def test_certificate_not_after_parses_openssl_enddate_only(self):
        leaf = Path("/fixture/codesign0")
        for output, expected in (
            (b"notAfter=Nov 16 12:00:00 2027 GMT\n", datetime(2027, 11, 16, 12, tzinfo=timezone.utc)),
            (b"notAfter=Jan  2 03:04:05 2027 GMT\n", datetime(2027, 1, 2, 3, 4, 5, tzinfo=timezone.utc)),
        ):
            with mock.patch.object(verify, "tool", return_value=output) as tool:
                self.assertEqual(verify.certificate_not_after(leaf), expected)
            self.assertEqual(tool.call_args.args[0][0], "/usr/bin/openssl")
        for output in (b"", b"notAfter=garbage\n", b"notAfter=Feb 30 00:00:00 2027 GMT\n"):
            with mock.patch.object(verify, "tool", return_value=output):
                with self.assertRaisesRegex(ValueError, "signing_certificate_unavailable"):
                    verify.certificate_not_after(leaf)

    def test_pass_line_reports_hours_and_lifetime_without_identifiers(self):
        result = {
            "runner_tree_sha256": "c" * 64,
            "xctestrun_sha256": "d" * 64,
            "profile_expires_in_hours": "150",
            "certificate_expires_in_hours": "7200",
            "profile_lifetime_days": "7.0",
        }
        argv = [
            "verify.py", "--output", "/fixture", "--team", TEAM, "--udid", MODERN_UDID,
            "--runner-id", RUNNER, "--certificate-sha1", CERT, "--min-validity-hours", "24",
        ]
        with (
            mock.patch("sys.argv", argv),
            mock.patch.object(verify, "verify", return_value=result) as checked,
            mock.patch("builtins.print") as printed,
        ):
            self.assertEqual(verify.main(), 0)
        self.assertEqual(checked.call_args.args[-1], 24)
        line = printed.call_args.args[0]
        self.assertIn("profile_expires_in_hours=150", line)
        self.assertIn("certificate_expires_in_hours=7200", line)
        self.assertIn("profile_lifetime_days=7.0", line)
        for secret in (TEAM, MODERN_UDID, CERT, RUNNER):
            self.assertNotIn(secret, line)


if __name__ == "__main__":
    unittest.main()
