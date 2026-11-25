import json
import sys
import time
import unittest

from handkitectl import DriverError, Session


def result(payload, error=False):
    return {"content": [{"type": "text", "text": json.dumps(payload)}], "isError": error}


class Fake(Session):
    def __init__(self, replies):
        super().__init__()
        self.replies = iter(replies)
        self.calls = []

    def _rpc(self, method, params):
        self.calls.append((method, params))
        return next(self.replies)


class DriverTests(unittest.TestCase):
    def test_one_session_reuses_observed_revision_and_action_delta(self):
        session = Fake([
            result({"control": "acquired"}),
            {"structuredContent": {"metadata": {"revision": 8}}},
            {"structuredContent": {"observation": {"metadata": {"revision": 10}}}},
            result({"control": "released"}),
        ])
        session.call("handkite_acquire_control")
        session.call("handkite_observe")
        session.call("handkite_tap", {"at": {"x": 1, "y": 2}})
        self.assertEqual(session.revision, 10)
        self.assertEqual(session.calls[1][1]["arguments"], {"tree_format": "elements", "image": "omit"})
        self.assertEqual(session.calls[2][1]["arguments"]["expected_revision"], 8)
        self.assertEqual(session.calls[2][1]["arguments"]["response"]["since_revision"], 8)
        session.call("handkite_release_control")
        self.assertFalse(session.control)

    def test_status_revision_and_navigation_result_need_no_tree(self):
        session = Fake([result({"control": "acquired"}),
                        result({"revision": 8, "revision_scope": "navigation"}),
                        result({"receipt": {"accepted": True}, "observation": {
                            "metadata": {"revision": 10, "revision_scope": "navigation"}}})])
        session.call("handkite_acquire_control")
        session.call("handkite_status")
        session.call("handkite_home")
        self.assertEqual(session.calls[2][1]["arguments"]["expected_revision"], 8)
        self.assertEqual(session.revision, 10)
        self.assertEqual(len(session.calls), 3)

    def test_non_object_metadata_does_not_crash_navigation_scope_check(self):
        for metadata in [[], None, False, "invalid"]:
            session = Fake([result({"metadata": metadata})])
            session.call("handkite_observe")
            self.assertIsNone(session.revision)
            self.assertIsNone(session.target_revision)
            self.assertEqual(len(session.calls), 1)

    def test_native_targets_are_explicit_and_never_gain_coordinate_response_fields(self):
        session = Fake([result({"revision": 8, "target_only": True, "targets": []}),
                        result({"revision": 9, "target_only": True, "targets": []})])
        session.control = True
        session.observe_targets()
        self.assertEqual(session.revision, 8)
        self.assertTrue(session.calls[0][1]["arguments"]["targets"])
        session.call("handkite_tap_element", {"target_id": "t_" + "a" * 32})
        self.assertEqual(session.calls[1][1]["arguments"], {"target_id": "t_" + "a" * 32, "expected_revision": 8})
        self.assertEqual(session.revision, 9)

    def test_navigation_observation_clears_old_target_handles(self):
        for tool in ["handkite_observe", "handkite_status"]:
            target = {"target_id": "t_" + "a" * 32, "label": "old"}
            session = Fake([
                result({"revision": 8, "target_only": True, "targets": [target]}),
                result({"revision_scope": "navigation", "target_actionable": False,
                        "metadata": {"revision": 9, "revision_scope": "navigation"}}),
            ])
            session.observe_targets()
            session.call(tool)
            self.assertEqual(session.revision, 9)
            self.assertIsNone(session.target_revision)
            self.assertEqual(session.targets, [])
            self.assertEqual(len(session.calls), 2)

    def test_presentation_tree_does_not_retain_target_authority(self):
        session = Fake([
            result({"revision": 8, "target_only": True, "targets": [
                {"target_id": "t_" + "a" * 32, "label": "old"}]}),
            result({"revision": 9, "revision_scope": "navigation",
                    "tree_scope": "presentation", "tree": {"included": True},
                    "accessibility_tree": "Text label=example",
                    "metadata": {"revision": 9, "revision_scope": "navigation"}}),
        ])
        session.observe_targets()
        session.call("handkite_observe", {"tree_format": "outline", "targets": False})
        self.assertEqual(session.revision, 9)
        self.assertIsNone(session.target_revision)
        self.assertEqual(session.targets, [])

    def test_pending_tap_preserves_only_delta_baseline_until_observed(self):
        a={"target_id":"t_"+"a"*32,"label":"first"}
        b={"target_id":"t_"+"b"*32,"label":"second"}
        session=Fake([result({"target_only":True,"revision":8,"targets":[a]}),
                      result({"target_only":True,"revision":None,"effect":"pending","receipt":{"accepted":True}}),
                      result({"target_only":True,"revision":9,"targets_delta":{"base_revision":8,"removed":[a["target_id"]],"added":[b],"changed":[]}})])
        session.control=True
        session.observe_targets()
        session.call("handkite_tap_element",{"target_id":a["target_id"]})
        self.assertIsNone(session.revision)
        self.assertEqual(session.target_revision,8)
        session.observe_targets()
        self.assertEqual(session.calls[2][1]["arguments"]["since_revision"],8)
        self.assertEqual(session.targets,[b])
        self.assertEqual(session.revision,9)

    def test_mismatched_native_delta_refuses_without_replay(self):
        session=Fake([result({"target_only":True,"revision":9,"targets_delta":{"base_revision":8,"removed":[],"added":[],"changed":[]}})])
        with self.assertRaisesRegex(DriverError,"target_delta_baseline_lost"):
            session.observe_targets()
        self.assertIsNone(session.revision)
        self.assertIsNone(session.target_revision)
        self.assertEqual(len(session.calls),1)

    def test_invalid_native_lease_clears_advisory_without_automatic_reacquisition(self):
        session = Fake([{"isError": True, "structuredContent": {"code": "InvalidLease"}},
                        result({"control": "acquired"})])
        session.control, session.revision = True, 8
        session.call("handkite_tap_element", {"target_id": "t_" + "a" * 32})
        self.assertFalse(session.control)
        self.assertIsNone(session.revision)
        self.assertEqual(len(session.calls), 1)
        session.call("handkite_acquire_control")
        self.assertTrue(session.control)
        self.assertEqual(len(session.calls), 2)

    def test_no_implicit_acquisition_or_observation(self):
        session = Fake([result({"control": "acquired"})])
        with self.assertRaisesRegex(DriverError, "explicit_acquire_required"):
            session.call("handkite_text", {"value": "fixture"})
        session.call("handkite_acquire_control")
        with self.assertRaisesRegex(DriverError, "observe_required"):
            session.call("handkite_text", {"value": "fixture"})
        self.assertEqual(len(session.calls), 1)

    def test_refusal_invalidates_revision_without_retry(self):
        session = Fake([result({"code": "ViewChanged"}, True)])
        session.control, session.revision = True, 8
        response = session.call("handkite_tap", {"at": {"x": 1, "y": 2}})
        self.assertTrue(response["result"]["isError"])
        self.assertIsNone(session.revision)
        self.assertEqual(len(session.calls), 1)

    def test_image_only_action_cannot_reuse_its_old_revision(self):
        session = Fake([result({"after_image": {"device": "fixture"}, "after": None})])
        session.control, session.revision = True, 8
        session.call("handkite_text", {"value": "fixture", "response": {
            "capture": "image_only", "tree_format": "omit", "image": "always"}})
        self.assertIsNone(session.revision)

    def test_transport_failure_poisoned_no_replay(self):
        session = Session()
        calls = []
        session._write = lambda frame, deadline: calls.append(frame)
        session._read = lambda deadline: {"jsonrpc": "2.0", "id": 99, "result": {}}
        with self.assertRaisesRegex(DriverError, "uncertain_no_replay"):
            session._rpc("tools/call", {})
        with self.assertRaisesRegex(DriverError, "session_uncertain"):
            session._rpc("tools/call", {})
        self.assertEqual(len(calls), 1)

    def test_invalid_arguments_are_not_sent_and_never_retried(self):
        session = Session()
        calls = []
        session.control, session.revision = True, 8
        session._write = lambda frame, deadline: calls.append(frame)
        session._read = lambda deadline: {"jsonrpc": "2.0", "id": 1,
                                          "error": {"code": -32602, "message": "private input"}}
        reply = session.call("handkite_text_element", {"target_id": "t_" + "a" * 32,
                                                        "value": "\n"})
        self.assertEqual(reply["result"]["structuredContent"],
                         {"code": "InvalidArguments", "delivery": "not_sent"})
        self.assertNotIn("private input", json.dumps(reply))
        self.assertIsNone(session.revision)
        self.assertFalse(session.poisoned)
        self.assertEqual(len(calls), 1)

    def test_other_rpc_errors_remain_uncertain(self):
        for error in [{"code": -32603}, {"code": "-32602"},
                      {"code": -32602.0, "message": "invalid"},
                      {"code": -32602}, {"code": -32602, "message": None}]:
            session = Session()
            session._write = lambda frame, deadline: None
            session._read = lambda deadline: {"jsonrpc": "2.0", "id": 1, "error": error}
            with self.assertRaisesRegex(DriverError, "uncertain_no_replay"):
                session._rpc("tools/call", {})
            self.assertTrue(session.poisoned)

    def test_ambiguous_or_mismatched_error_remains_uncertain(self):
        for extra in [{"id": 2}, {"result": {}}]:
            session = Session()
            session._write = lambda frame, deadline: None
            message = {"jsonrpc": "2.0", "id": 1,
                       "error": {"code": -32602, "message": "invalid"}, **extra}
            session._read = lambda deadline: message
            with self.assertRaisesRegex(DriverError, "uncertain_no_replay"):
                session._rpc("tools/call", {})
            self.assertTrue(session.poisoned)

    def test_explicit_response_and_revision_are_not_overridden(self):
        session = Fake([result({"after": {"revision": 9}})])
        session.control, session.revision = True, 8
        session.call("handkite_tap", {"expected_revision": 7, "at": {"x": 1, "y": 2},
                                      "response": {"tree_format": "full", "image": "always"}})
        args = session.calls[0][1]["arguments"]
        self.assertEqual(args["expected_revision"], 7)
        self.assertEqual(args["response"], {"tree_format": "full", "image": "always"})
        self.assertEqual(session.revision, 9)

    def test_malformed_success_clears_revision(self):
        session = Fake([{"content": []}])
        session.control, session.revision = True, 8
        session.call("handkite_tap", {"at": {"x": 1, "y": 2}})
        self.assertIsNone(session.revision)

    def test_silent_child_times_out_and_is_reaped(self):
        session = Session([sys.executable, "-c", "import time; time.sleep(30)"], timeout=0.05)
        started = time.monotonic()
        with self.assertRaisesRegex(DriverError, "uncertain_no_replay"):
            with session:
                self.fail("silent child cannot initialize")
        self.assertLess(time.monotonic() - started, 4)
        self.assertIsNone(session.process)


if __name__ == "__main__":
    unittest.main()
