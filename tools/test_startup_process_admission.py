"""Offline startup evidence only; no process, filesystem, or device queries."""
from dataclasses import FrozenInstanceError
import unittest

from startup_process_admission import StartupAdmission, XPCPROXY


EXPECTED = "/fixture/releases/selected/handkited"
BIRTH = "Fri Nov 13 12:00:00 2026"


class StartupAdmissionTests(unittest.TestCase):
    def begin(self):
        return StartupAdmission.begin(EXPECTED, 123, BIRTH, 10.0)

    def test_proxy_to_selected_same_identity(self):
        state = self.begin().observe(123, BIRTH, XPCPROXY, 10.0)
        self.assertEqual(state.status, "pending")
        state = state.observe(123, BIRTH, EXPECTED, 14.999)
        self.assertEqual(state.status, "selected")
        self.assertEqual(state.observe(123, BIRTH, EXPECTED, 100).status, "selected")

    def test_direct_selected_admission(self):
        self.assertEqual(self.begin().observe(123, BIRTH, EXPECTED, 10).status,
                         "selected")

    def test_original_deadline_never_resets(self):
        state = self.begin()
        for now in (11, 12, 13, 14, 14.999):
            state = state.observe(123, BIRTH, XPCPROXY, now)
            self.assertEqual(state.status, "pending")
            self.assertEqual(state.started_at, 10)
        for path in (XPCPROXY, EXPECTED):
            with self.subTest(path=path):
                refused = state.observe(123, BIRTH, path, 15)
                self.assertEqual(refused.status, "refused")
                self.assertEqual(refused.observe(123, BIRTH, EXPECTED, 15.1).status,
                                 "refused")

    def test_late_first_observation_refuses(self):
        for path in (XPCPROXY, EXPECTED):
            self.assertEqual(self.begin().observe(123, BIRTH, path, 16).status,
                             "refused")

    def test_selected_never_regresses_to_proxy(self):
        selected = self.begin().observe(123, BIRTH, EXPECTED, 11)
        refused = selected.observe(123, BIRTH, XPCPROXY, 11)
        self.assertEqual(refused.status, "refused")
        self.assertEqual(refused.observe(123, BIRTH, EXPECTED, 12).status, "refused")

    def test_pid_replacement_and_same_pid_birth_race_refuse(self):
        for initial_path in (XPCPROXY, EXPECTED):
            for pid, birth in ((124, BIRTH), (123, BIRTH + " changed")):
                for path in (XPCPROXY, EXPECTED):
                    with self.subTest(initial=initial_path, pid=pid, path=path):
                        state = self.begin().observe(123, BIRTH, initial_path, 11)
                        refused = state.observe(pid, birth, path, 11.001)
                        self.assertEqual(refused.status, "refused")
                        self.assertEqual(refused.observe(123, BIRTH, EXPECTED, 12).status,
                                         "refused")

    def test_unknown_or_noncanonical_paths_refuse(self):
        for path in (None, "", "/fixture/other", "/usr/libexec/xpcproxy/",
                     "/usr/libexec/../libexec/xpcproxy", "//usr/libexec/xpcproxy",
                     EXPECTED + "\x00", EXPECTED + "\n"):
            with self.subTest(path=path):
                self.assertEqual(self.begin().observe(123, BIRTH, path, 11).status,
                                 "refused")

    def test_malformed_identity_refuses_before_or_after_selection(self):
        for initial_path in (XPCPROXY, EXPECTED):
            for pid, birth in ((0, BIRTH), (-1, BIRTH), (True, BIRTH),
                               (123.0, BIRTH), (2 ** 31, BIRTH), (123, None),
                               (123, ""), (123, " "), (123, BIRTH + "\n"),
                               (123, "x" * 129), (123, (1, 2))):
                with self.subTest(initial=initial_path, pid=pid, birth=birth):
                    state = self.begin().observe(123, BIRTH, initial_path, 11)
                    self.assertEqual(state.observe(pid, birth, EXPECTED, 12).status,
                                     "refused")

    def test_clock_rollback_and_invalid_times_refuse(self):
        for initial_path in (XPCPROXY, EXPECTED):
            state = self.begin().observe(123, BIRTH, initial_path, 12)
            for now in (11.999, -1, None, True, "13", float("nan"),
                        float("inf"), -float("inf"), 10 ** 400):
                with self.subTest(initial=initial_path, now=now):
                    self.assertEqual(state.observe(123, BIRTH, EXPECTED, now).status,
                                     "refused")

    def test_initial_pins_are_validated_and_immutable(self):
        for path, pid, birth, now in ((XPCPROXY, 123, BIRTH, 10),
                                     ("relative", 123, BIRTH, 10),
                                     (EXPECTED, 0, BIRTH, 10),
                                     (EXPECTED, 123, "", 10),
                                     (EXPECTED, 123, BIRTH, float("nan")),
                                     (EXPECTED, 123, BIRTH, 10 ** 400),
                                     (EXPECTED, 123, BIRTH, 1e308)):
            with self.subTest(path=path, pid=pid, birth=birth, now=now):
                with self.assertRaises(ValueError):
                    StartupAdmission.begin(path, pid, birth, now)
        state = self.begin()
        with self.assertRaises(FrozenInstanceError):
            state.expected_path = "/fixture/other"
        advanced = state.observe(123, BIRTH, XPCPROXY, 11)
        self.assertEqual(state.last_at, 10)
        self.assertEqual(advanced.last_at, 11)


if __name__ == "__main__":
    unittest.main()
