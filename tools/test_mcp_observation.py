"""Fixtures from <commit> mcp.rs::one_observation_yields_metadata_tree_and_image.

The producer's AQID fixture tests base64 transport, not an image codec.
"""
from copy import deepcopy
import json
import unittest
from unittest.mock import patch

from mcp_observation import decode_observation


def metadata():
    return {"device": "phone", "revision": 3,
            "window": {"started_unix_ms": 1, "finished_unix_ms": 2},
            "state": {"lock": "locked", "wda_ready": True, "foreground_app": None}}


def fixture(method="handkite_observe", request_id=2):
    payload = metadata()
    if method == "handkite_observe":
        payload = {"metadata": payload, "accessibility_tree": "<screen/>"}
    content = [{"type": "text", "text": json.dumps(payload)}]
    if method == "handkite_observe":
        content.append({"type": "image", "data": "AQID", "mimeType": "image/png"})
    return {"jsonrpc": "2.0", "id": request_id, "result": {"content": content}}


def set_payload(response, payload):
    response["result"]["content"][0]["text"] = json.dumps(payload)


def compact_fixture():
    return {"metadata": metadata(),
            "accessibility_tree": "<AppiumAUT> <Button type='Button' label='Café &amp; 🦊'/> </AppiumAUT>",
            "tree_format": "compact", "tree_format_version": 1,
            "tree_format_reason": "requested"}


class ObservationTests(unittest.TestCase):
    def decode(self, response, method="handkite_observe", request_id=2):
        return decode_observation(response, request_id, method, "phone")

    def test_frozen_status_and_observe_shapes(self):
        self.assertEqual(self.decode(fixture("handkite_status"), "handkite_status"),
                         {"wda_ready": True, "lock": "locked", "tree_chars": 0,
                          "image_bytes": 0})
        self.assertEqual(self.decode(fixture()),
                         {"wda_ready": True, "lock": "locked", "tree_chars": 9,
                          "image_bytes": 3})

    def test_lock_variants_and_jpeg(self):
        for lock in ("locked", "unlocked", "uncertain"):
            response = fixture()
            payload = json.loads(response["result"]["content"][0]["text"])
            payload["metadata"]["state"]["lock"] = lock
            set_payload(response, payload)
            response["result"]["content"][1]["mimeType"] = "image/jpeg"
            self.assertEqual(self.decode(response)["lock"], lock)

    def test_unknown_fields_are_preserved_and_not_exposed(self):
        response = fixture()
        payload = json.loads(response["result"]["content"][0]["text"])
        for obj in (response, response["result"], *response["result"]["content"],
                    payload, payload["metadata"], payload["metadata"]["state"]):
            obj["extension"] = "private-extension"
        set_payload(response, payload)
        before = deepcopy(response)
        summary = self.decode(response)
        self.assertEqual(response, before)
        self.assertEqual(set(summary), {"wda_ready", "lock", "tree_chars", "image_bytes"})
        for private in ("phone", "screen", "AQID", "private-extension", "foreground"):
            self.assertNotIn(private, repr(summary))

    def test_no_current_time_or_revision_comparison(self):
        response = fixture("handkite_status")
        payload = metadata()
        payload["revision"] = 0
        payload["window"] = {"started_unix_ms": 0, "finished_unix_ms": 0}
        set_payload(response, payload)
        self.assertTrue(self.decode(response, "handkite_status")["wda_ready"])

    def test_ids_are_exact_and_typed(self):
        for request_id in (1, True, 2.0, "2", None, {}, 2 ** 64):
            with self.subTest(request_id=request_id), self.assertRaises(ValueError):
                self.decode(fixture(request_id=request_id))
        for request_id in ("request", -1, 0, 2 ** 64 - 1):
            self.assertTrue(self.decode(fixture(request_id=request_id),
                                        request_id=request_id)["wda_ready"])
        for request_id in (True, None, 2.0, {}, -(2 ** 63) - 1, 2 ** 64):
            with self.subTest(expected=request_id), self.assertRaises(ValueError):
                self.decode(fixture(request_id=request_id), request_id=request_id)

    def test_rpc_errors_and_bad_envelopes(self):
        for change in ({"error": None}, {"error": {"message": "private-error"}},
                       {"jsonrpc": "1.0"}, {"result": None}, {"result": []}):
            response = fixture()
            response.update(change)
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.decode(response)
        for value in (True, 1, "false", None):
            response = fixture()
            response["result"]["isError"] = value
            with self.subTest(isError=value), self.assertRaises(ValueError):
                self.decode(response)
        response["result"]["isError"] = False
        self.assertTrue(self.decode(response)["wda_ready"])

    def test_wrong_wrappers_do_not_fall_back(self):
        for method, payloads in (
                ("handkite_status", ({"metadata": metadata()},
                                     {"observation": metadata()}, {"result": metadata()})),
                ("handkite_observe", (metadata(), {"observation": metadata()},
                                      {"metadata": {"observation": metadata()},
                                       "accessibility_tree": "<screen/>"}))):
            for payload in payloads:
                response = fixture(method)
                set_payload(response, payload)
                with self.subTest(method=method, payload=payload), self.assertRaises(ValueError):
                    self.decode(response, method)

    def test_device_and_state_admission(self):
        for change in ({"device": "other"}, {"device": None}, {"state": None},
                       {"state": {"lock": "unknown", "wda_ready": True}},
                       {"state": {"lock": "locked", "wda_ready": 1}},
                       {"state": {"lock": "locked", "wda_ready": False}},
                       {"state": {"lock": [], "wda_ready": True}}):
            response = fixture("handkite_status")
            payload = metadata()
            payload.update(change)
            set_payload(response, payload)
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.decode(response, "handkite_status")

    def test_malformed_or_ambiguous_text_refuses_without_disclosure(self):
        for text in (None, 1, "", "private-invalid-json", "[]", "null",
                     '{"device":"phone","device":"other"}',
                     '{"extension":NaN}', '{"extension":Infinity}', "\ud800"):
            response = fixture()
            response["result"]["content"][0]["text"] = text
            with self.subTest(text=text), self.assertRaises(ValueError) as error:
                self.decode(response)
            self.assertRegex(str(error.exception), r"^mcp_observation_[a-z]+$")
            self.assertIsNone(error.exception.__cause__)

    def test_exact_content_shape(self):
        for content in (None, {}, [], [None], [{"type": "image", "text": "{}"}],
                        fixture()["result"]["content"] * 2):
            response = fixture()
            response["result"]["content"] = content
            with self.subTest(content=content), self.assertRaises(ValueError):
                self.decode(response)
        with self.assertRaises(ValueError):
            self.decode(fixture(), "handkite_status")
        with self.assertRaises(ValueError):
            self.decode(fixture("handkite_status"))

    def test_empty_tree_and_invalid_image_refuse(self):
        for tree in (None, 0, "", " \n\t"):
            response = fixture()
            set_payload(response, {"metadata": metadata(), "accessibility_tree": tree})
            with self.subTest(tree=tree), self.assertRaises(ValueError):
                self.decode(response)
        for change in ({"type": "text"}, {"mimeType": "image/gif"},
                       {"mimeType": None}, {"data": ""}, {"data": None},
                       {"data": " "}, {"data": "AQID\n"}, {"data": "?"},
                       {"data": "AQI"}, {"data": "AQID===="}, {"data": "\u00e9"}):
            response = fixture()
            response["result"]["content"][1].update(change)
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.decode(response)

    def test_arguments_and_summary_counts_are_bounded(self):
        for method, device in (("observe", "phone"), (None, "phone"),
                               ("handkite_observe", ""), ("handkite_observe", "a" * 65),
                               ("handkite_observe", "phone/other")):
            with self.subTest(method=method, device=device), self.assertRaises(ValueError):
                decode_observation(fixture(), 2, method, device)
        with patch("mcp_observation.MAX_RESPONSE", 5), self.assertRaises(ValueError):
            self.decode(fixture())

    def test_compact_and_full_fallback_are_content_free_and_unchanged(self):
        for format_, reason, content in (("compact", "requested", compact_fixture()["accessibility_tree"]),
                                         ("full", "unsupported_xml", "<unknown/>"),
                                         ("full", "malformed_xml", "<broken>"),
                                         ("full", "limit_exceeded", "<large/>")):
            response = fixture()
            set_payload(response, {"metadata": metadata(), "accessibility_tree": content,
                                   "tree_format": format_, "tree_format_version": 1,
                                   "tree_format_reason": reason})
            before = deepcopy(response)
            summary = self.decode(response)
            self.assertEqual(summary, {"wda_ready": True, "lock": "locked",
                                       "tree_chars": len(content), "image_bytes": 3})
            self.assertEqual(response, before)

    def test_invalid_compact_or_format_metadata_refuses(self):
        for change in ({"tree_format": "other"}, {"tree_format": None},
                       {"tree_format_version": True}, {"tree_format_version": 2},
                       {"tree_format_reason": "unsupported_xml"},
                       {"tree_format": "full"},
                       {"accessibility_tree": {"v": 1}},
                       {"accessibility_tree": ""}):
            response = fixture()
            payload = compact_fixture()
            payload.update(change)
            set_payload(response, payload)
            with self.subTest(change=change), self.assertRaisesRegex(
                    ValueError, "^mcp_observation_tree$"):
                self.decode(response)

    def test_compact_limit_and_legacy_object_refusal(self):
        response = fixture()
        set_payload(response, compact_fixture())
        with patch("mcp_observation.MAX_COMPACT", 5), self.assertRaises(ValueError):
            self.decode(response)
        set_payload(response, {"metadata": metadata(), "accessibility_tree": {"root": []}})
        with self.assertRaises(ValueError):
            self.decode(response)


if __name__ == "__main__":
    unittest.main()
