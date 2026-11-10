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
    app = root / "WebDriverAgentRunner-Runner.app"
    test = app / "PlugIns/WebDriverAgentRunner.xctest"
    test.mkdir(parents=True)
    (app / "Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": RUNNER}))
    (app / "embedded.mobileprovision").write_bytes(b"fixture")
    (test / "test-binary").write_bytes(b"fixture")
    run = root / "WebDriverAgentRunner.xctestrun"
    run.write_bytes(b"fixture")
    return output, app, run


class VerifyTests(unittest.TestCase):
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
            self.assertEqual(verify.products(output), (app, app / "PlugIns/WebDriverAgentRunner.xctest", run))
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
                "application-identifier": f"{TEAM}.{RUNNER}",
                "com.apple.developer.team-identifier": TEAM,
                "get-task-allow": True,
            }
            fields = {
                "TeamIdentifier": [TEAM],
                "ProvisionedDevices": [MODERN_UDID],
                "ExpirationDate": datetime.now(timezone.utc) + timedelta(days=1),
                "DeveloperCertificates": [cert],
                "Entitlements": entitlements,
            }

            def command(arguments):
                return plistlib.dumps(fields if arguments[0] == "/usr/bin/security" else entitlements)

            with mock.patch.object(verify, "tool", side_effect=command):
                verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                fields["ProvisionedDevices"] = ["other"]
                with self.assertRaisesRegex(ValueError, "profile_identity_mismatch"):
                    verify.profile(app, TEAM, MODERN_UDID, RUNNER, digest)
                fields["ProvisionedDevices"] = [MODERN_UDID]
                fields["ExpirationDate"] = datetime.now(timezone.utc) - timedelta(days=1)
                with self.assertRaisesRegex(ValueError, "profile_expired"):
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
            test = app / "PlugIns/WebDriverAgentRunner.xctest"
            with (
                mock.patch.object(verify, "signature", side_effect=lambda path, _: RUNNER if path == app else "test.id") as signer,
                mock.patch.object(verify, "leaf_sha1", return_value=CERT),
                mock.patch.object(verify, "profile") as profile,
            ):
                runner_hash, run_hash = verify.verify(output, TEAM, MODERN_UDID, RUNNER, CERT)
            self.assertEqual(signer.call_count, 2)
            profile.assert_called_once_with(app, TEAM, MODERN_UDID, RUNNER, CERT)
            self.assertEqual(len(runner_hash), 64)
            self.assertEqual(run_hash, verify.hash_file(run))


if __name__ == "__main__":
    unittest.main()
