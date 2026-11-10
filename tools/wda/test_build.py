import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("wda_build", Path(__file__).with_name("build.py"))
assert SPEC and SPEC.loader
build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build)
MODERN_UDID = "00008120-00123456789A1234"


def project_fixture():
    def block(identifier, settings):
        return (
            f"\t\t{identifier} /* Debug */ = {{\n"
            "\t\t\tisa = XCBuildConfiguration;\n"
            "\t\t\tbaseConfigurationReference = EEE5CABF1C80361500CBBDD9 /* IOSSettings.xcconfig */;\n"
            "\t\t\tbuildSettings = {\n"
            f"\t\t\t\t{settings}\n"
            "\t\t\t\tINFOPLIST_FILE = WebDriverAgentRunner/Info.plist;\n"
            "\t\t\t\tUSES_XCTRUNNER = YES;\n"
            "\t\t\t};\n"
            "\t\t};\n"
        )

    original = build.ORIGINAL_BUNDLE
    return (
        "\n" + block(build.IOS_CONFIGS[0], original)
        + block(build.IOS_CONFIGS[1], original)
        + "\t\tCONFIG_LIST = { buildConfigurations = (\n"
        + f"\t\t\t\t{build.IOS_CONFIGS[0]} /* Debug */,\n"
        + f"\t\t\t\t{build.IOS_CONFIGS[1]} /* Release */,\n"
        + "\t\t); };\n"
        + "\t\tOTHER /* tvOS */ = { PRODUCT_BUNDLE_IDENTIFIER = com.facebook.WebDriverAgentRunner; };\n"
    ).encode()


class BuildTests(unittest.TestCase):
    def test_udid_accepts_legacy_and_modern_forms_only(self):
        self.assertIsNotNone(build.UDID.fullmatch("a" * 40))
        self.assertIsNotNone(build.UDID.fullmatch(MODERN_UDID))
        for invalid in (
            "a" * 39,
            "00008120-00123456789A123",
            "00008120-00123456789A12345",
            "00008120_00123456789A1234",
        ):
            self.assertIsNone(build.UDID.fullmatch(invalid))

    def test_patch_changes_only_two_pinned_ios_configs(self):
        data = project_fixture()
        with mock.patch.object(build, "PROJECT_SHA256", hashlib.sha256(data).hexdigest()):
            result = build.patch_project(data, "com.example.Runner")
        self.assertEqual(result.count(b"PRODUCT_BUNDLE_IDENTIFIER = com.example.Runner;"), 2)
        self.assertEqual(result.count(build.ORIGINAL_BUNDLE.encode()), 1)
        self.assertIn(b"OTHER /* tvOS */", result)
        with self.assertRaisesRegex(ValueError, "project_source_mismatch"):
            build.patch_project(data, "com.example.Runner")

    def test_invalid_identity_never_creates_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "build"
            with self.assertRaisesRegex(ValueError, "identity_invalid"):
                build.build(Path("/source"), output, "bad", "a" * 40, "com.example.Runner", False)
            self.assertFalse(output.exists())

    def test_build_requires_checked_settings_and_never_installs_or_launches(self):
        data = project_fixture()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "build"

            def archive(_, destination):
                project = destination / build.PROJECT
                project.parent.mkdir(parents=True)
                project.write_bytes(data)

            with (
                mock.patch.object(build, "PROJECT_SHA256", hashlib.sha256(data).hexdigest()),
                mock.patch.object(build, "source_is_pinned", return_value=True),
                mock.patch.object(build, "archive_source", side_effect=archive),
                mock.patch.object(build, "resolved_settings", return_value={
                    "PRODUCT_BUNDLE_IDENTIFIER": "com.example.Runner",
                    "DEVELOPMENT_TEAM": "ABCDE12345",
                    "CODE_SIGN_STYLE": "Automatic",
                    "PLATFORM_NAME": "iphoneos",
                }),
                mock.patch.object(build.subprocess, "run", return_value=mock.Mock(returncode=0)) as run,
            ):
                build.build(Path("/source"), output, "ABCDE12345", MODERN_UDID, "com.example.Runner", False)
            command = run.call_args.args[0]
            self.assertEqual(command[:3], ["/usr/bin/xcodebuild", "clean", "build-for-testing"])
            self.assertIn(f"platform=iOS,id={MODERN_UDID}", command)
            self.assertNotIn("-allowProvisioningUpdates", command)
            self.assertNotIn("-allowProvisioningDeviceRegistration", command)
            self.assertNotIn("PRODUCT_BUNDLE_IDENTIFIER=com.example.Runner", command)
            self.assertFalse(any(word in command for word in ("test-without-building", "install", "launch")))
            self.assertEqual((output / "build.log").stat().st_mode & 0o777, 0o600)

    def test_mismatched_resolved_settings_prevent_build(self):
        data = project_fixture()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "build"

            def archive(_, destination):
                project = destination / build.PROJECT
                project.parent.mkdir(parents=True)
                project.write_bytes(data)

            with (
                mock.patch.object(build, "PROJECT_SHA256", hashlib.sha256(data).hexdigest()),
                mock.patch.object(build, "source_is_pinned", return_value=True),
                mock.patch.object(build, "archive_source", side_effect=archive),
                mock.patch.object(build, "resolved_settings", return_value={"PLATFORM_NAME": "iphonesimulator"}),
                mock.patch.object(build.subprocess, "run") as run,
            ):
                with self.assertRaisesRegex(ValueError, "build_settings_mismatch"):
                    build.build(Path("/source"), output, "ABCDE12345", "a" * 40, "com.example.Runner", True)
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
