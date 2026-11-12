import hashlib
import importlib.util
import os
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
    def test_xcode_environment_removes_only_host_include_path(self):
        with mock.patch.dict(os.environ, {"CPATH": "/usr/local/include", "TASK_MARKER": "kept"}):
            environment = build.xcode_environment()
            self.assertNotIn("CPATH", environment)
            self.assertEqual(environment["TASK_MARKER"], "kept")
            self.assertEqual(os.environ["CPATH"], "/usr/local/include")

    def test_settings_probe_uses_clean_xcode_environment(self):
        with (
            mock.patch.dict(os.environ, {"CPATH": "/usr/local/include"}),
            mock.patch.object(build.subprocess, "run", return_value=mock.Mock(
                returncode=0, stdout=b"    PLATFORM_NAME = iphoneos\n",
            )) as run,
        ):
            self.assertEqual(
                build.resolved_settings(Path("/project"), "ABCDE12345", MODERN_UDID),
                {"PLATFORM_NAME": "iphoneos"},
            )
        self.assertNotIn("CPATH", run.call_args.kwargs["env"])
        command = run.call_args.args[0]
        self.assertEqual(command[command.index("-destination") + 1], f"platform=iOS,id={MODERN_UDID}")

    def test_generic_settings_probe_uses_only_literal_generic_destination(self):
        with mock.patch.object(build.subprocess, "run", return_value=mock.Mock(
            returncode=0, stdout=b"    PLATFORM_NAME = iphoneos\n",
        )) as run:
            build.resolved_settings(Path("/project"), "ABCDE12345", MODERN_UDID, True)
        command = run.call_args.args[0]
        self.assertEqual(command[command.index("-destination") + 1], "generic/platform=iOS")
        self.assertNotIn("-allowProvisioningUpdates", command)
        self.assertNotIn("-allowProvisioningDeviceRegistration", command)

    def test_generic_destination_still_requires_valid_target_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "build"
            for team, udid in [("bad", MODERN_UDID), ("ABCDE12345", "bad")]:
                with self.assertRaisesRegex(ValueError, "identity_invalid"):
                    build.build(Path("/source"), output, team, udid, "com.example.Runner", False, True)
                self.assertFalse(output.exists())

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
        self.check_build_destination(False)
        self.check_build_destination(True)

    def test_failed_xcode_build_keeps_private_log_and_reports_failure(self):
        self.check_build_destination(False, returncode=65)

    def check_build_destination(self, generic, returncode=0):
        data = project_fixture()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "build"
            messages = [
                mock.call("wda_build_phase=source_check elapsed_seconds=1.0", flush=True),
                mock.call("wda_build_phase=source_prepare elapsed_seconds=2.0", flush=True),
                mock.call("wda_build_phase=settings_check elapsed_seconds=4.0", flush=True),
                mock.call("wda_build_phase=build_start elapsed_seconds=10.0", flush=True),
            ]

            def archive(_, destination):
                project = destination / build.PROJECT
                project.parent.mkdir(parents=True)
                project.write_bytes(data)

            def xcode(*args, **kwargs):
                self.assertEqual(report.call_args_list, messages)
                kwargs["stdout"].write(b"private Xcode output\n")
                return mock.Mock(returncode=returncode)

            with (
                mock.patch.object(build, "PROJECT_SHA256", hashlib.sha256(data).hexdigest()),
                mock.patch.object(build, "source_is_pinned", return_value=True),
                mock.patch.object(build, "archive_source", side_effect=archive),
                mock.patch.object(build, "patch_lock_state", return_value={"fixture": True}),
                mock.patch.object(build, "resolved_settings", return_value={
                    "PRODUCT_BUNDLE_IDENTIFIER": "com.example.Runner",
                    "DEVELOPMENT_TEAM": "ABCDE12345",
                    "CODE_SIGN_STYLE": "Automatic",
                    "PLATFORM_NAME": "iphoneos",
                }) as settings,
                mock.patch.object(build.subprocess, "run", side_effect=xcode) as run,
                mock.patch.object(build.time, "monotonic", side_effect=[10, 11, 12, 14, 20]),
                mock.patch("builtins.print") as report,
            ):
                if returncode:
                    with self.assertRaisesRegex(ValueError, "^xcode_build_failed$"):
                        build.build(Path("/source"), output, "ABCDE12345", MODERN_UDID,
                                    "com.example.Runner", False, generic)
                else:
                    build.build(Path("/source"), output, "ABCDE12345", MODERN_UDID,
                                "com.example.Runner", False, generic)
            self.assertEqual(report.call_args_list, messages)
            self.assertEqual(settings.call_args.args[-1], generic)
            command = run.call_args.args[0]
            self.assertEqual(command[:3], ["/usr/bin/xcodebuild", "clean", "build-for-testing"])
            self.assertEqual(command[command.index("-destination") + 1],
                             "generic/platform=iOS" if generic else f"platform=iOS,id={MODERN_UDID}")
            self.assertNotIn("-allowProvisioningUpdates", command)
            self.assertNotIn("-allowProvisioningDeviceRegistration", command)
            self.assertNotIn("PRODUCT_BUNDLE_IDENTIFIER=com.example.Runner", command)
            self.assertFalse(any(word in command for word in ("test-without-building", "install", "launch")))
            self.assertNotIn("CPATH", run.call_args.kwargs["env"])
            self.assertEqual((output / "build.log").stat().st_mode & 0o777, 0o600)
            self.assertEqual((output / "build.log").read_bytes(), b"private Xcode output\n")

    def test_cli_flushes_final_status_and_elapsed_time_without_error_details(self):
        arguments = [
            "build.py", "--source", "/source", "--output", "/output",
            "--team", "ABCDE12345", "--udid", MODERN_UDID,
            "--bundle-id", "com.example.Runner",
        ]
        for error, status, code in (
            (None, "completed_unverified", 0),
            (ValueError("xcode_build_failed"), "xcode_build_failed", 1),
            (ValueError("build_settings_unavailable"), "build_settings_unavailable", 1),
            (OSError("private path and account details"), "io_failed", 1),
        ):
            with (
                self.subTest(status=status),
                mock.patch("sys.argv", arguments),
                mock.patch.object(build, "build", side_effect=error) as run,
                mock.patch.object(build.time, "monotonic", side_effect=[10, 27.5]),
                mock.patch("builtins.print") as report,
            ):
                self.assertEqual(build.main(), code)
            run.assert_called_once_with(
                Path("/source"), Path("/output"), "ABCDE12345", MODERN_UDID,
                "com.example.Runner", False, False,
            )
            report.assert_called_once_with(
                f"wda_build={status} elapsed_seconds=17.5", flush=True,
            )

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
                mock.patch.object(build, "patch_lock_state", return_value={"fixture": True}),
                mock.patch.object(build, "resolved_settings", return_value={"PLATFORM_NAME": "iphonesimulator"}),
                mock.patch.object(build.subprocess, "run") as run,
            ):
                with self.assertRaisesRegex(ValueError, "build_settings_mismatch"):
                    build.build(Path("/source"), output, "ABCDE12345", "a" * 40, "com.example.Runner", True)
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
