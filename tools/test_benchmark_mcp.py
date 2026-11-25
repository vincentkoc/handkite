import base64
import copy
import contextlib
import io
import json
import hashlib
import os
from pathlib import Path
import tempfile
import sys
import time
import unittest
from unittest.mock import patch
from types import SimpleNamespace

from benchmark_mcp import (BenchError, Campaign, CASES, Peer, compare, elements,
                           operation_stages, parse_screen, quantiles, summarize, merge_reports, reference_report, extract_report,
                           trace_rows, correlate_prefix, attach_stages, validate_plan, validate_measured_stages,
                           validate_client_stages, CAPTURE_NAMES, matches)


def screen_result(tree="<XCUIElementTypeApplication/>"):
    payload = {"metadata": {"device": "fixture", "revision": 1,
                           "window": {"started_unix_ms": 1, "finished_unix_ms": 2},
                           "state": {"wda_ready": True, "lock": "unlocked",
                                     "foreground_app": "example.notes"}},
               "accessibility_tree": tree}
    return {"content": [{"type": "text", "text": json.dumps(payload)},
                        {"type": "image", "mimeType": "image/png",
                         "data": "iVBORw0KGgo="}]}


def plan():
    tools = ("handkite_tap", "handkite_text", "handkite_swipe", "handkite_launch_app",
             "handkite_observe", "handkite_observe")
    cases = [{"name": name, "measure": {"tool": tool, "arguments": {}}}
             for name, tool in zip(CASES, tools)]
    cases[4]["measure"]["arguments"] = {"tree_format": "omit"}
    return {"version": 1, "cases": cases}


def image_result(action=False):
    result = screen_result()
    payload = json.loads(result["content"][0]["text"])
    del payload["metadata"]["state"], payload["metadata"]["revision"], payload["accessibility_tree"]
    payload.update(capture_kind="image_only", tree={"included":False,"reason":"not_captured"})
    result["content"][0]["text"] = json.dumps(payload)
    if action:
        result["content"].insert(0, {"type":"text", "text":json.dumps({
            "receipt":{"accepted":True},"effect":"changed","after_image":payload["metadata"]})})
        result["content"][-1]["data"] = base64.b64encode(b"\x89PNG\r\n\x1a\nchanged").decode()
    return result


class BenchmarkTests(unittest.TestCase):
    def test_image_only_fixture_failure_names_predicate_without_private_state(self):
        for predicate, value in (("app", "private.observed.app"), ("lock", "locked")):
            with self.subTest(predicate=predicate):
                class FakePeer:
                    healthy = True

                    def __init__(self):
                        self.calls = []
                        self.launched = False

                    def initialize(self):
                        pass

                    def tool(self, name, arguments):
                        call = {"id": len(self.calls) + 1, "operation": len(self.calls) + 1,
                                "tool": name, "elapsed_ms": 1, "response_bytes": 100}
                        self.calls.append(call)
                        if name == "handkite_launch_app":
                            self.launched = True
                            return image_result(action=True), call
                        result = screen_result()
                        if self.launched and name == "handkite_observe":
                            payload = json.loads(result["content"][0]["text"])
                            key = "foreground_app" if predicate == "app" else "lock"
                            payload["metadata"]["state"][key] = value
                            result["content"][0]["text"] = json.dumps(payload)
                        return result, call

                scenario = plan()
                scenario["cases"][3]["measure"]["expect"] = {
                    "app": "example.notes", "lock": "unlocked"}
                peer = FakePeer()
                campaign = Campaign(peer, scenario, 20, 0,
                                    selected_cases=["launch"], capture_mode="image_only")
                campaign.run()
                self.assertEqual(campaign.failure, "effect_mismatch")
                self.assertEqual(campaign.failure_context["phase"], "fixture_verification")
                self.assertEqual(campaign.failure_context["call"]["failed_expectations"], [predicate])
                self.assertEqual(campaign.failure_context["call"]["failed_state"], {
                    "lock": "locked" if predicate == "lock" else "unlocked",
                    "same_app_as_previous": predicate == "lock"})
                self.assertEqual(campaign.failure_kind, "fixture_failure")
                self.assertNotIn("private.observed.app", json.dumps(campaign.failure_context))
                self.assertEqual(sum(call["tool"] == "handkite_launch_app" for call in peer.calls), 1)
                summary = summarize(campaign.samples)["launch"]
                self.assertEqual((summary["completed"], summary["failures"], summary["input_failures"]),
                                 (0, 1, 0))

    def test_focused_text_trace_requires_narrow_proof_and_one_real_capture(self):
        names = ("service_request", "service_serialization", "service_write", "controller_dispatch",
                 "wire_image_encode", "wda_read_request", "usb_discovery", "usb_connect",
                 "wda_send", "wda_response", "owned_session_checks", "wda_write_request",
                 "native_lock", "action_session_checks", "text_prepare", "text_context_transaction",
                 "text_context_query", "text_final_focus", "text_readback")
        rows = [{"stage": name, "started_us": 0, "elapsed_us": 1} for name in names]
        rows += [{"stage": "text_prepared", "ok": True}, {"stage": "text_verified", "ok": True}]
        capture = {"stages": [{"stage": name, "elapsed_us": 1, "ok": True}
                              for name in sorted(CAPTURE_NAMES - {"source_request", "source_json"})]}
        rows.append({"capture": capture})
        # Give the real input its own interval outside the settings/query bracket.
        next(row for row in rows if row.get("stage") == "wda_write_request")["started_us"] = 10
        validate_measured_stages("type", rows, "image_only")
        for case, mode in [("tap", "image_only"), ("type", "paired")]:
            with self.assertRaises(BenchError):
                validate_measured_stages(case, rows, mode)
        for stage in ("text_verified", "text_final_focus", "text_context_query"):
            with self.assertRaises(BenchError):
                validate_measured_stages("type", [row for row in rows if row.get("stage") != stage], "image_only")
        with self.assertRaises(BenchError):
            validate_measured_stages("type", rows + [{"capture": capture}], "image_only")
        full = {"stages": [{"stage": name, "elapsed_us": 1, "ok": True} for name in sorted(CAPTURE_NAMES)]}
        refreshed = [{"capture":full}, {"stage":"text_prepare", "started_us":0, "elapsed_us":1}] + rows
        validate_measured_stages("type", refreshed, "image_only")

    def test_response_settings_and_query_posts_are_not_counted_as_input(self):
        rows = [{"stage":"text_context_transaction", "started_us":0, "elapsed_us":100},
                {"stage":"text_context_query", "started_us":20, "elapsed_us":30},
                {"stage":"wda_write_request", "started_us":1, "elapsed_us":9},
                {"stage":"wda_write_request", "started_us":20, "elapsed_us":30},
                {"stage":"wda_write_request", "started_us":80, "elapsed_us":10},
                {"stage":"wda_write_request", "started_us":110, "elapsed_us":40}]
        self.assertEqual(operation_stages(rows)["input_request"], .04)

    def test_outline_profile_records_optional_enrichment_without_hiding_missing_stages(self):
        names = ("service_request", "service_serialization", "service_write", "controller_dispatch",
                 "wire_image_encode", "wda_read_request", "usb_discovery", "usb_connect",
                 "wda_send", "wda_response", "owned_session_checks")
        rows = [{"stage": name, "started_us": 0, "elapsed_us": 1} for name in names]
        for enriched in (False, True):
            stages = CAPTURE_NAMES | {"outline_source_request", "outline_cost"}
            if not enriched:
                stages -= {"source_request"}
            capture = {"outline_nodes": 54 if enriched else 1881,
                       "stages": [{"stage": stage, "elapsed_us": 1, "ok": True}
                                  for stage in sorted(stages)]}
            trace = rows + [{"capture": capture}]
            validate_measured_stages("observe_tree", trace, "paired")
            self.assertIn("capture_0.outline_source_request", operation_stages(trace))
            for missing in ("outline_cost", "source_json", "ownership_after"):
                invalid = copy.deepcopy(trace)
                invalid[-1]["capture"]["stages"] = [sample for sample in capture["stages"]
                                                     if sample["stage"] != missing]
                with self.assertRaises(BenchError):
                    validate_measured_stages("observe_tree", invalid, "paired")

    def test_extract_requires_whole_completed_cases_and_keeps_parent_failure(self):
        rows = [{"case": case, "ordinal": n, "warmup": False, "outcome": "pass",
                 "elapsed_ms": 10, "cleanup_ms": 1, "fixture_verification_ms": 2,
                 "trace_valid": True, "capture_kind": "image_only",
                 "stages_ms": {"span.service_request.work": 9}}
                for case in ("tap", "type") for n in range(1, 21)]
        parent = {"version": 1, "calibration": False, "trace_valid": True, "trace_error": None,
                  "failure": "effect_mismatch", "failure_context": {"case": "swipe", "phase": "prepare"},
                  "selected_cases": list(CASES), "samples": rows, "summary": summarize(rows),
                  "capture_mode": "image_only", "service_pid": 123,
                  "plan_sha256": "a" * 64, "service_build": "b" * 40, "client_build": "b" * 40}
        extracted = extract_report(parent, "d" * 64, ["tap", "type"])
        self.assertEqual(extracted["failure"], None)
        self.assertEqual(extracted["extraction"], {"source_sha256": "d" * 64,
                         "parent_failure": "effect_mismatch", "parent_phase": "prepare", "parent_case": "swipe"})
        self.assertEqual(parent["failure"], "effect_mismatch")
        self.assertEqual(extracted["summary"]["swipe"]["attempts"], 0)
        for key, value in (("trace_valid", False), ("trace_error", "service_correlation_mismatch")):
            with self.assertRaises(BenchError):
                extract_report(dict(parent, **{key: value}), "d" * 64, ["tap"])
        invalid = copy.deepcopy(parent)
        invalid["failure_context"]["case"] = "tap"
        with self.assertRaises(BenchError):
            extract_report(invalid, "d" * 64, ["tap"])
        for key, value in (("outcome", "action_unconfirmed"), ("trace_valid", False),
                           ("ordinal", 2), ("cleanup_ms", None), ("fixture_verification_ms", None),
                           ("capture_kind", "paired"), ("failure_kind", "infrastructure_stop")):
            invalid = copy.deepcopy(parent)
            invalid["samples"][0][key] = value
            with self.assertRaises(BenchError):
                extract_report(invalid, "d" * 64, ["tap"])
        with self.assertRaises(BenchError):
            extract_report(parent, "d" * 64, ["swipe"])
        for warmup in (True, False):
            invalid = copy.deepcopy(parent)
            invalid["samples"].append(dict(rows[0], ordinal=21, warmup=warmup, outcome="tool_error"))
            with self.assertRaises(BenchError):
                extract_report(invalid, "d" * 64, ["tap"])
        remaining = copy.deepcopy(extracted)
        remaining.pop("extraction")
        remaining["selected_cases"] = [case for case in CASES if case not in ("tap", "type")]
        remaining["samples"] = [dict(rows[0], case=case, ordinal=n)
                                for case in remaining["selected_cases"] for n in range(1, 21)]
        remaining["summary"] = summarize(remaining["samples"])
        merged = merge_reports([(extracted, "e" * 64), (remaining, "f" * 64)])
        self.assertEqual(merged["cohorts"][0]["extraction"], extracted["extraction"])
        self.assertEqual(compare(merged, merged), [])

    def test_terminal_disconnect_keeps_completed_sample_stages_without_passing_campaign(self):
        names = ("service_request", "service_serialization", "service_write", "controller_dispatch",
                 "wire_image_encode", "wda_read_request", "usb_discovery", "usb_connect",
                 "wda_send", "wda_response", "owned_session_checks", "wda_write_request",
                 "native_lock", "action_session_checks")
        service = [{"stage": name, "started_us": 0, "elapsed_us": 1} for name in names]
        service += [{"capture": {"stages": [{"stage": name, "elapsed_us": 1, "ok": True}
                                               for name in sorted(CAPTURE_NAMES)]}} for _ in range(2)]
        client = [{"operation": 1, "stage": name, "started_us": 0, "elapsed_us": 1}
                  for name in ("mcp_request", "mcp_serialization", "mcp_write", "mcp_service_decode",
                               "mcp_tree_projection", "mcp_service_exchange")]
        client += [{"operation": 2, "stage": "mcp_service_exchange", "started_us": 2, "elapsed_us": 1}]
        for sequence, row in enumerate(service):
            row.update(handkite_timing=1, pid=9, operation=10, sequence=sequence)
        for sequence, row in enumerate(client):
            row.update(handkite_timing=1, pid=8, sequence=sequence)
        sample = {"operation": 1, "outcome": "pass", "case": "tap", "capture_kind": "paired",
                  "cleanup_ms": 1}
        campaign = SimpleNamespace(peer=SimpleNamespace(trace_error=None, trace=client,
                                   calls=[{"operation": 1}, {"operation": 2}]), samples=[sample])
        with self.assertRaisesRegex(BenchError, "service_correlation_mismatch"):
            attach_stages(campaign, service, 9)
        self.assertTrue(sample["trace_valid"])
        self.assertIn("input_request", sample["stages_ms"])
        sample.pop("cleanup_ms")
        with self.assertRaises(BenchError):
            attach_stages(campaign, service, 9)
        self.assertFalse(sample["trace_valid"])

    def test_correlation_preserves_only_a_proven_terminal_prefix(self):
        def client(decoded):
            return [{"stage": "mcp_service_exchange"}] + ([{"stage": "mcp_service_decode"}] if decoded else [])
        clients = {1: client(True), 2: client(True), 3: client(False)}
        service = {10: [{"stage": "service_request"}], 11: [{"stage": "service_request"}]}
        self.assertEqual(correlate_prefix(clients, service), ({1: 10, 2: 11}, False))
        self.assertEqual(correlate_prefix({1: client(True), 2: client(True)}, service),
                         ({1: 10, 2: 11}, True))
        with self.assertRaises(BenchError):
            correlate_prefix({1: client(True), 2: client(False), 3: client(True)}, service)
        with self.assertRaises(BenchError):
            correlate_prefix(clients, {10: [{"stage": "service_request"}],
                                       11: [{"stage": "wda_response"}],
                                       12: [{"stage": "service_request"}]})

    def test_reference_keeps_metrics_without_runtime_or_call_details(self):
        samples = [{"case":case,"outcome":"pass","elapsed_ms":10,"cleanup_ms":1,"trace_valid":True}
                   for case in CASES for _ in range(20)]
        report = {"version":1,"calibration":False,"trace_valid":True,"failure":None,
                  "plan_sha256":"a" * 64,"service_build":"b" * 40,"client_build":"c" * 40,
                  "service_pid":123,"calls":[{"private":"context"}],"samples":samples,
                  "summary":summarize(samples)}
        result = reference_report(report, "d" * 64)
        self.assertEqual(compare(result, report), [])
        self.assertEqual(result["reference_source_sha256"], "d" * 64)
        self.assertTrue({"calls","samples","service_pid"}.isdisjoint(result))
        for key, value in (("calibration", True), ("failure", "view_unsettled"), ("trace_valid", False)):
            invalid = dict(report, **{key:value})
            with self.assertRaises(BenchError):
                reference_report(invalid, "d" * 64)
        invalid = copy.deepcopy(report)
        invalid["summary"]["private_context"] = "private"
        with self.assertRaisesRegex(BenchError, "inconsistent_summary"):
            reference_report(invalid, "d" * 64)
        for key, value in (("passed", 0), ("completed", 19), ("infrastructure_stops", 1),
                           ("input_failures", 1), ("client_ms", {"n":0,"p50":10,"p95":10})):
            invalid = copy.deepcopy(report)
            invalid["summary"]["tap"][key] = value
            with self.assertRaises(BenchError):
                reference_report(invalid, "d" * 64)
        report["summary"]["tap"]["attempts"] = 19
        with self.assertRaises(BenchError):
            reference_report(report, "d" * 64)

    def test_image_measurement_keeps_fixture_verification_separate(self):
        class FakePeer:
            healthy = True
            def __init__(self):
                self.calls = []
            def initialize(self):
                pass
            def tool(self, tool, arguments):
                call = dict(id=len(self.calls), operation=len(self.calls), elapsed_ms=1, response_bytes=1, tool=tool)
                self.calls.append(dict(call, arguments=arguments))
                if tool == "handkite_tap":
                    self_outer.assertEqual(arguments["response"], {"capture":"image_only","tree_format":"omit"})
                    return image_result(action=True), call
                return screen_result(), call
        self_outer = self
        peer = FakePeer()
        campaign = Campaign(peer, plan(), 1, 0, selected_cases=["tap"], capture_mode="image_only")
        with contextlib.redirect_stdout(io.StringIO()):
            campaign.run()
        self.assertIsNone(campaign.failure)
        sample = campaign.samples[0]
        self.assertEqual(sample["capture_kind"], "image_only")
        self.assertEqual(sample["elapsed_ms"], 1)
        self.assertIn("fixture_verification_ms", sample)
        self.assertEqual(sum(call["tool"] == "handkite_tap" for call in peer.calls), 1)
        self.assertEqual(sum(call["tool"] == "handkite_observe" for call in peer.calls), 2)
        self.assertEqual(sample["operation"], next(call["operation"] for call in peer.calls if call["tool"] == "handkite_tap"))

    def test_image_capture_rejects_fabricated_full_metadata(self):
        result = image_result()
        self.assertEqual(parse_screen(result)["capture_kind"], "image_only")
        for key, value in (("revision", 1), ("state", {"lock":"unlocked"})):
            invalid = copy.deepcopy(result)
            payload = json.loads(invalid["content"][0]["text"])
            payload["metadata"][key] = value
            invalid["content"][0]["text"] = json.dumps(payload)
            with self.assertRaisesRegex(BenchError, "fabricated_image_metadata"):
                parse_screen(invalid)

    def test_settle_reads_are_bounded_and_require_matching_tree_image_and_state(self):
        class FakePeer:
            def __init__(self, dimension, changes):
                self.dimension, self.changes, self.calls = dimension, changes, []
            def tool(self, name, arguments):
                self.calls.append(name)
                self.assert_read(name)
                result = screen_result()
                payload = json.loads(result["content"][0]["text"])
                value = self.changes[len(self.calls) - 1]
                if self.dimension == "tree":
                    payload["accessibility_tree"] = f"<App label='{value}'/>"
                elif self.dimension == "state":
                    payload["metadata"]["state"]["foreground_app"] = str(value)
                else:
                    result["content"][1]["data"] = base64.b64encode(b"\x89PNG\r\n\x1a\n" + bytes([value])).decode()
                result["content"][0]["text"] = json.dumps(payload)
                call = {"tool":name}
                self.last_call = call
                return result, call
            @staticmethod
            def assert_read(name):
                if name != "handkite_observe":
                    raise AssertionError("settling must never send input")
        for dimension in ("tree", "image", "state"):
            for changes in ([0, 1, 1], [0, 1, 2]):
                peer = FakePeer(dimension, changes)
                campaign = Campaign(peer, plan(), 20, 0)
                campaign.lease_started = time.monotonic()
                if changes[-1] == changes[-2]:
                    campaign.step({"tool":"handkite_observe", "settle":3})
                else:
                    with self.assertRaisesRegex(BenchError, "view_unsettled"):
                        campaign.step({"tool":"handkite_observe", "settle":3})
                self.assertEqual(peer.calls, ["handkite_observe"] * 3)
                self.assertEqual(peer.last_call["settle_changed"], {
                    key: key == dimension and changes[-1] != changes[-2]
                    for key in ("device", "state", "tree", "image")})

    def test_settle_cannot_hide_input_or_expand_a_measured_request(self):
        for tool, count in (("handkite_swipe", 3), ("handkite_observe", 6), ("handkite_observe", True)):
            fixture = plan()
            fixture["cases"][0]["cleanup"] = [{"tool":tool, "settle":count}]
            with self.assertRaisesRegex(BenchError, "invalid_settle"):
                validate_plan(fixture)
        fixture = plan()
        fixture["cases"][-1]["measure"]["settle"] = 3
        with self.assertRaisesRegex(BenchError, "invalid_settle"):
            validate_plan(fixture)

    def test_merge_requires_complete_disjoint_cohorts_on_one_generation(self):
        def cohort(cases):
            rows = [{"case": case, "outcome": "pass", "elapsed_ms": 10, "cleanup_ms": 1,
                     "trace_valid": True}
                    for case in cases for _ in range(20)]
            return {"version": 1, "calibration": False, "trace_valid": True, "failure": None,
                    "trace_error": None, "plan_sha256": "same", "service_build": "same",
                    "client_build": "same", "service_pid": 1, "selected_cases": cases,
                    "samples": rows, "summary": summarize(rows)}
        first, second = cohort(["swipe"]), cohort([case for case in CASES if case != "swipe"])
        merged = merge_reports([(first, "first"), (second, "second")])
        self.assertEqual(len(merged["samples"]), 120)
        self.assertEqual(compare(merged, merged), [])
        self.assertEqual(merged["cohorts"][0]["sha256"], "first")
        for key, value in (("failure", "ViewChanged"), ("trace_valid", False),
                           ("service_build", "different"), ("service_pid", 2), ("calibration", True)):
            invalid = copy.deepcopy(second)
            invalid[key] = value
            with self.assertRaises(BenchError):
                merge_reports([(first, "first"), (invalid, "invalid")])
        for changes in ({"trace_valid": False}, {"cleanup_ms": None},
                        {"outcome": "AdapterNativeBefore", "failure_kind": "infrastructure_stop"}):
            invalid = copy.deepcopy(second)
            invalid["samples"][0].update(changes)
            invalid["summary"] = summarize(invalid["samples"])
            with self.assertRaises(BenchError):
                merge_reports([(first, "first"), (invalid, "invalid")])
        with self.assertRaises(BenchError):
            merge_reports([(first, "first"), (first, "duplicate")])
        with self.assertRaises(BenchError):
            merge_reports([(first, "first"), (cohort(["tap"]), "incomplete")])

    def test_partial_campaign_preserves_plan_and_case_lifecycle(self):
        class FakePeer:
            healthy = True
            def __init__(self):
                self.calls = []
            def initialize(self):
                pass
            def tool(self, tool, arguments):
                call = dict(id=len(self.calls), operation=len(self.calls), elapsed_ms=1,
                            response_bytes=1, tool=tool)
                self.calls.append(call)
                return screen_result(), call
        fixture = plan()
        fixture["cases"][-1].update(prepare=[{"tool": "handkite_observe"}],
                                    cleanup=[{"tool": "handkite_observe"}])
        original = copy.deepcopy(fixture)
        peer = FakePeer()
        campaign = Campaign(peer, fixture, 1, 1, selected_cases=["observe_tree"])
        with contextlib.redirect_stdout(io.StringIO()):
            campaign.run()
        self.assertIsNone(campaign.failure)
        self.assertEqual(fixture, original)
        self.assertEqual([s["case"] for s in campaign.samples], ["observe_tree"] * 2)
        self.assertEqual([s["warmup"] for s in campaign.samples], [True, False])
        self.assertEqual(sum(c["tool"] == "handkite_observe" for c in peer.calls), 7)
        for selected in ([], ["tap", "tap"], ["unknown"]):
            with self.assertRaisesRegex(BenchError, "invalid_selected_cases"):
                Campaign(peer, fixture, 1, 0, selected_cases=selected)

    def test_progress_pipe_failure_stops_without_repeating_completed_input(self):
        class FakePeer:
            healthy = True

            def __init__(self):
                self.calls = []

            def initialize(self):
                pass

            def tool(self, tool, arguments):
                call = dict(id=len(self.calls), operation=len(self.calls), elapsed_ms=1,
                            response_bytes=1, tool=tool)
                self.calls.append(call)
                return (image_result(action=True) if tool == "handkite_tap"
                        else screen_result()), call

        class BrokenProgress(io.StringIO):
            def __init__(self, fail_at, during_flush):
                super().__init__()
                self.rows = 0
                self.fail_at = fail_at
                self.during_flush = during_flush

            def write(self, value):
                if value.startswith("{"):
                    self.rows += 1
                if self.rows == self.fail_at and not self.during_flush:
                    raise BrokenPipeError("private transport details")
                return super().write(value)

            def flush(self):
                if self.rows == self.fail_at and self.during_flush:
                    raise BrokenPipeError("private transport details")
                return super().flush()

        for fail_at in (1, 20):
            for during_flush in (False, True):
                with self.subTest(fail_at=fail_at, during_flush=during_flush):
                    peer = FakePeer()
                    fixture = plan()
                    fixture["cases"][0]["cleanup"] = [{"tool": "handkite_observe"}]
                    campaign = Campaign(peer, fixture, 20, 0, selected_cases=["tap"],
                                        capture_mode="image_only")
                    with contextlib.redirect_stdout(BrokenProgress(fail_at, during_flush)):
                        campaign.run()
                    self.assertEqual(campaign.failure, "progress_output_closed")
                    self.assertEqual(campaign.failure_kind, "reporting_failure")
                    self.assertEqual(campaign.failure_context["phase"], "progress")
                    self.assertEqual(len(campaign.samples), fail_at)
                    self.assertTrue(all(row["outcome"] == "pass" for row in campaign.samples))
                    self.assertTrue(all(row["measurement_passed"] and "cleanup_ms" in row
                                        for row in campaign.samples))
                    self.assertEqual(sum(c["tool"] == "handkite_tap" for c in peer.calls), fail_at)
                    self.assertEqual(peer.calls[-1]["tool"], "handkite_release_control")
                    summary = summarize(campaign.samples)["tap"]
                    self.assertEqual((summary["attempts"], summary["input_failures"]), (fail_at, 0))
                    self.assertEqual(summary["completed"], 0)  # Trace validation is still required.
                    report = {"version": 1, "trace_valid": True, "failure": campaign.failure}
                    with self.assertRaisesRegex(BenchError, "incomplete_benchmark"):
                        compare(report, report, ["tap"])
                    self.assertNotIn("private transport details", json.dumps(campaign.samples))

    def test_cli_writes_failed_report_without_reusing_closed_progress_pipe(self):
        from benchmark_mcp import main

        campaign = SimpleNamespace(run=lambda: None, samples=[], selected_cases=["tap"],
                                   failure="progress_output_closed", failure_kind="reporting_failure",
                                   failure_context={"phase": "progress"}, _failed_scroll_images=None)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, trace, output = (root / name for name in ("plan.json", "trace.jsonl", "report.json"))
            source.write_text(json.dumps(plan()))
            trace.write_text("")
            argv = ["benchmark_mcp.py", "run", "--plan", str(source), "--output", str(output),
                    "--service-trace", str(trace), "--service-pid", "1", "--service-build", "a" * 40,
                    "--client-build", "b" * 40, "--", "unused-fixture-client"]
            with patch.object(sys, "argv", argv), patch("benchmark_mcp.Peer") as peer, \
                    patch("benchmark_mcp.Campaign", return_value=campaign), \
                    patch("benchmark_mcp.attach_stages"), patch("benchmark_mcp.time.sleep"), \
                    patch("builtins.print", side_effect=BrokenPipeError) as progress:
                peer.return_value.calls = []
                self.assertEqual(main(), 1)
                peer.return_value.close.assert_called_once()
                progress.assert_not_called()
            report = json.loads(output.read_text())
            self.assertEqual(report["failure"], "progress_output_closed")
            self.assertEqual(report["failure_kind"], "reporting_failure")
            self.assertFalse(report["qualified"])

    def test_nearest_rank_percentiles_and_invalid_samples(self):
        self.assertEqual(quantiles(list(range(1, 21))), {"n": 20, "p50": 10, "p95": 19})
        self.assertEqual(quantiles([9]), {"n": 1, "p50": 9, "p95": 9})
        for values in ([], [float("nan")], [float("inf")], [-1]):
            with self.assertRaises(BenchError):
                quantiles(values)

    def test_failures_are_counted_and_not_folded_into_success_quantiles(self):
        samples = [{"case": "tap", "outcome": "pass", "elapsed_ms": 10, "cleanup_ms": 1, "trace_valid": True},
                   {"case": "tap", "outcome": "action_unconfirmed", "elapsed_ms": 1},
                   {"case": "tap", "outcome": "pass", "elapsed_ms": 999, "warmup": True}]
        result = summarize(samples)["tap"]
        self.assertEqual((result["attempts"], result["passed"], result["failures"]), (2, 1, 1))
        self.assertEqual(result["client_ms"]["p50"], 10)

    def test_only_complete_trace_valid_trials_enter_statistics(self):
        complete = {"case": "type", "outcome": "pass", "elapsed_ms": 10,
                    "cleanup_ms": 1, "trace_valid": True}
        samples = [complete, dict(complete, cleanup_ms=None), dict(complete, trace_valid=False),
                   dict(complete, outcome="AdapterNativeBefore", failure_kind="infrastructure_stop",
                        elapsed_ms=999),
                   dict(complete, outcome="action_unconfirmed", failure_kind="input_or_unknown_failure")]
        result = summarize(samples)["type"]
        self.assertEqual((result["attempts"], result["completed"], result["passed"]), (5, 1, 1))
        self.assertEqual((result["failures"], result["infrastructure_stops"], result["input_failures"]), (3, 1, 1))
        self.assertEqual(result["client_ms"], {"n": 1, "p50": 10, "p95": 10})

    def test_cleanup_native_refusal_is_an_excluded_incomplete_trial_without_retry(self):
        class FakePeer:
            healthy = True
            def __init__(self):
                self.calls = []
            def initialize(self):
                pass
            def tool(self, name, arguments):
                call = {"id": len(self.calls), "operation": len(self.calls), "tool": name,
                        "elapsed_ms": 1, "response_bytes": 1}
                self.calls.append(call)
                if name == "handkite_key":
                    return {"isError": True, "structuredContent": {"code": "AdapterNativeBefore"}}, call
                result = screen_result()
                if name == "handkite_text":
                    result["content"].insert(0, {"type": "text", "text": json.dumps({
                        "receipt": {"accepted": True}, "effect": "changed", "after": {}})})
                return result, call

        fixture = plan()
        fixture["cases"][1]["cleanup"] = [{"tool": "handkite_key", "arguments": {"key": "enter"}}]
        peer = FakePeer()
        campaign = Campaign(peer, fixture, 20, 0, selected_cases=["type"])
        campaign.run()
        trial = campaign.samples[0]
        self.assertTrue(trial["measurement_passed"])
        self.assertEqual(trial["outcome"], "AdapterNativeBefore")
        self.assertEqual(trial["failed_phase"], "cleanup")
        self.assertEqual(trial["failure_kind"], "infrastructure_stop")
        self.assertNotIn("cleanup_ms", trial)
        self.assertEqual(campaign.failure_context["call"]["error_code"], "AdapterNativeBefore")
        summary = summarize(campaign.samples)["type"]
        self.assertEqual((summary["completed"], summary["infrastructure_stops"], summary["input_failures"]), (0, 1, 0))
        self.assertIsNone(summary["client_ms"])
        self.assertEqual([call["tool"] for call in peer.calls].count("handkite_text"), 1)
        self.assertEqual([call["tool"] for call in peer.calls].count("handkite_key"), 1)

    def test_qualification_rejects_stops_and_incomplete_traces_but_accepts_legacy_reference(self):
        rows = [{"case": "type", "outcome": "pass", "elapsed_ms": 10,
                 "cleanup_ms": 1, "trace_valid": True} for _ in range(20)]
        report = {"version": 1, "plan_sha256": "same", "trace_valid": True, "failure": None,
                  "samples": rows, "summary": summarize(rows)}
        self.assertEqual(compare(report, report, ["type"]), [])
        for changes in ({"trace_valid": False}, {"cleanup_ms": None},
                        {"outcome": "AdapterNativeBefore", "failure_kind": "infrastructure_stop"},
                        {"outcome": "action_unconfirmed", "failure_kind": "input_or_unknown_failure"}):
            invalid = copy.deepcopy(report)
            invalid["samples"].append(dict(rows[0], **changes))
            invalid["summary"] = summarize(invalid["samples"])
            self.assertEqual(compare(report, invalid, ["type"]), ["type:correctness_or_sample_count"])
        invalid = copy.deepcopy(report)
        invalid["samples"][0]["trace_valid"] = False
        self.assertEqual(compare(report, invalid, ["type"]), ["type:correctness_or_sample_count"])
        invalid = copy.deepcopy(report)
        invalid["samples"].append(dict(rows[0], warmup=True, outcome="AdapterNativeBefore",
                                       failure_kind="infrastructure_stop"))
        invalid["summary"] = summarize(invalid["samples"])
        self.assertEqual(compare(report, invalid, ["type"]), ["type:correctness_or_sample_count"])
        reference = {key: copy.deepcopy(value) for key, value in report.items() if key != "samples"}
        reference["reference_source_sha256"] = "d" * 64
        for summary in reference["summary"].values():
            for key in ("completed", "infrastructure_stops", "input_failures"):
                summary.pop(key)
        self.assertEqual(compare(reference, report, ["type"]), [])
        reference.pop("reference_source_sha256")
        self.assertEqual(compare(reference, report, ["type"]), ["type:correctness_or_sample_count"])

    def test_trace_gaps_or_mixed_processes_are_not_accepted(self):
        rows = [{"handkite_timing": 1, "pid": 7, "sequence": n, "operation": 1}
                for n in (8, 9)]
        self.assertEqual(trace_rows(rows), 7)
        self.assertEqual(trace_rows(list(reversed(rows))), 7)
        for key, value in (("sequence", 10), ("pid", 8)):
            invalid = copy.deepcopy(rows)
            invalid[1][key] = value
            with self.assertRaises(BenchError):
                trace_rows(invalid)

    def test_query_posts_are_not_reported_as_input(self):
        rows = [{"stage": "button_find", "started_us": 0, "elapsed_us": 100},
                {"stage": "wda_write_request", "started_us": 5, "elapsed_us": 80},
                {"stage": "wda_write_request", "started_us": 110, "elapsed_us": 50}]
        self.assertEqual(operation_stages(rows)["input_request"], .05)
        self.assertEqual(operation_stages(rows)["span.wda_write_request.work"], .13)

    def test_parallel_spans_have_distinct_covered_and_work_totals(self):
        rows = [{"stage": "wda_read_request", "started_us": start, "elapsed_us": 1000}
                for start in (0, 100, 200)]
        result = operation_stages(rows)
        self.assertEqual(result["span.wda_read_request.work"], 3)
        self.assertEqual(result["span.wda_read_request.covered"], 1.2)

    def test_wrapper_only_trace_is_not_a_valid_observation_measurement(self):
        rows = [{"stage": name, "started_us": 0, "elapsed_us": 1}
                for name in ("service_request", "service_serialization", "service_write")]
        with self.assertRaisesRegex(BenchError, "missing_service_stage"):
            validate_measured_stages("observe_tree", rows, "paired")

    def test_missing_capture_or_native_stage_rejects_otherwise_complete_action(self):
        names = ("service_request", "service_serialization", "service_write", "controller_dispatch",
                 "wire_image_encode", "wda_read_request", "usb_discovery", "usb_connect",
                 "wda_send", "wda_response", "owned_session_checks", "wda_write_request", "native_lock",
                 "action_session_checks")
        rows = [{"stage": name, "started_us": 0, "elapsed_us": 1} for name in names]
        capture = {"capture": {"stages": [{"stage": name, "elapsed_us": 1, "ok": True}
                                           for name in sorted(CAPTURE_NAMES)]}}
        rows.extend([copy.deepcopy(capture), copy.deepcopy(capture)])
        validate_measured_stages("tap", rows, "paired")
        for name in ("native_lock", "wda_write_request", "action_session_checks"):
            with self.assertRaises(BenchError):
                validate_measured_stages("tap", [row for row in rows if row.get("stage") != name], "paired")
        incomplete = copy.deepcopy(rows)
        incomplete[-1]["capture"]["stages"] = [sample for sample in incomplete[-1]["capture"]["stages"]
                                               if sample["stage"] != "source_request"]
        with self.assertRaises(BenchError):
            validate_measured_stages("tap", incomplete, "paired")

    def test_capture_stages_remain_separate_and_private_stage_names_are_rejected(self):
        rows = [{"capture": {"stages": [{"stage": "capture_total", "elapsed_us": n, "ok": True}]}}
                for n in (1000, 2000)]
        self.assertEqual(operation_stages(rows), {"capture_0.capture_total": 1,
                                                  "capture_1.capture_total": 2})
        with self.assertRaises(BenchError):
            operation_stages([{"stage": "private_content", "elapsed_us": 1}])

    def test_reused_pre_tree_requires_explicit_probe_evidence_and_full_post_tree(self):
        names = ("service_request", "service_serialization", "service_write", "controller_dispatch",
                 "wire_image_encode", "wda_read_request", "usb_discovery", "usb_connect",
                 "wda_send", "wda_response", "owned_session_checks", "wda_write_request", "native_lock",
                 "action_session_checks", "tree_cache_probe")
        rows = [{"stage": name, "started_us": 0, "elapsed_us": 1} for name in names]
        rows.append({"stage": "tree_cache_hit", "ok": True})
        stages = lambda names: [{"stage": name, "elapsed_us": 1, "ok": True} for name in sorted(names)]
        rows.extend([{"capture": {"reused_tree": True,
                                   "stages": stages(CAPTURE_NAMES - {"source_request", "source_json"})}},
                     {"capture": {"stages": stages(CAPTURE_NAMES)}}])
        validate_measured_stages("tap", rows, "paired")
        for invalid in [rows[:-2] + rows[-1:], [row for row in rows if row.get("stage") != "tree_cache_hit"]]:
            with self.assertRaises(BenchError):
                validate_measured_stages("tap", invalid, "paired")
        rows[-2]["capture"].pop("reused_tree")
        with self.assertRaises(BenchError):
            validate_measured_stages("tap", rows, "paired")

    def test_each_client_stage_is_required(self):
        rows = [{"stage": name} for name in ("mcp_request", "mcp_serialization", "mcp_write",
                "mcp_service_decode", "mcp_tree_projection", "mcp_service_exchange")]
        validate_client_stages(rows)
        for missing in rows:
            with self.assertRaisesRegex(BenchError, "incomplete_client_trace"):
                validate_client_stages([row for row in rows if row is not missing])

    def test_tree_stability_expectation_accepts_only_matching_fresh_trees(self):
        before = parse_screen(screen_result())
        after = parse_screen(screen_result("<Different/>"))
        self.assertTrue(matches(before, {"tree_changed": False}, before))
        self.assertFalse(matches(after, {"tree_changed": False}, before))
        self.assertTrue(matches(after, {"tree_changed": True}, before))
        self.assertFalse(matches(before, {"tree_changed": True}, before))

    def test_refused_action_has_no_tree_projection_stage(self):
        rows = [{"stage": name} for name in ("mcp_request", "mcp_serialization", "mcp_write",
                "mcp_service_decode", "mcp_service_exchange")]
        validate_client_stages(rows, observed=False)
        with self.assertRaisesRegex(BenchError, "incomplete_client_trace"):
            validate_client_stages(rows)
        with self.assertRaisesRegex(BenchError, "incomplete_client_trace"):
            validate_client_stages(rows[:-1], observed=False)

    def test_full_plan_never_accepts_pin_or_lifecycle_tools(self):
        self.assertEqual(validate_plan(plan()), plan())
        for tool in ("handkite_unlock", "handkite_home", "exec"):
            invalid = plan()
            invalid["cases"][0]["prepare"] = [{"tool": tool}]
            with self.assertRaises(BenchError):
                validate_plan(invalid)

    def test_image_only_is_explicit_and_cannot_masquerade_as_full_tree(self):
        result = screen_result()
        payload = json.loads(result["content"][0]["text"])
        del payload["accessibility_tree"]
        result["content"][0]["text"] = json.dumps(payload)
        with self.assertRaises(BenchError):
            parse_screen(result)
        payload.update(capture_kind="paired", tree={"included": False, "reason": "requested"})
        result["content"][0]["text"] = json.dumps(payload)
        self.assertIsNone(parse_screen(result)["tree"])
        payload["tree"]["included"] = 0
        result["content"][0]["text"] = json.dumps(payload)
        with self.assertRaises(BenchError):
            parse_screen(result)

    def test_real_pipe_preserves_response_and_content_free_trace(self):
        script = """import sys,json
for line in sys.stdin:
 r=json.loads(line)
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':{'ok':True}}),flush=True)
 print(json.dumps({'handkite_timing':1,'pid':7,'sequence':0,'operation':1,
                   'stage':'mcp_request','started_us':0,'elapsed_us':1}),file=sys.stderr,flush=True)
"""
        peer = Peer([sys.executable, "-u", "-c", script], timeout=2)
        try:
            result, call = peer.request("ping")
            self.assertEqual(result, {"ok": True})
            self.assertEqual((call["id"], call["operation"]), (1, 1))
        finally:
            peer.close()
        self.assertEqual(trace_rows(peer.trace), 7)

    def test_selector_ignores_invisible_nodes_and_refuses_missing_full_tree(self):
        result = screen_result("<AppiumAUT><XCUIElementTypeButton label='Done' visible='false'/>"
                               "<XCUIElementTypeButton label='Done' visible='true'/></AppiumAUT>")
        frame = parse_screen(result)
        self.assertEqual(len(elements(frame, {"type": "Button", "label": "Done"})), 1)
        frame["tree"] = None
        with self.assertRaises(BenchError):
            elements(frame, {"type": "Button"})

    def test_real_pipe_transport_drains_stderr_and_rejects_wrong_request_id(self):
        script = """import sys,json
for line in sys.stdin:
 r=json.loads(line)
 sys.stderr.write('ignored private native line\\n');sys.stderr.flush()
 print(json.dumps({'jsonrpc':'2.0','id':r['id']+1,'result':{}}),flush=True)
"""
        peer = Peer([sys.executable, "-u", "-c", script], timeout=2)
        try:
            with self.assertRaisesRegex(BenchError, "response_identity"):
                peer.request("ping")
            self.assertFalse(peer.healthy)
            self.assertEqual(peer.trace, [])
        finally:
            peer.close()

    def test_unknown_input_is_not_retried_and_skips_mutating_cleanup(self):
        class FakePeer:
            def __init__(self):
                self.calls = []
                self.healthy = True

            def initialize(self):
                pass

            def tool(self, name, arguments):
                call = {"id": len(self.calls) + 1, "operation": len(self.calls) + 1,
                        "tool": name, "elapsed_ms": 1, "response_bytes": 100}
                self.calls.append(call)
                if name == "handkite_tap":
                    return {"isError": True, "content": [{"type": "text", "text": json.dumps({
                        "receipt": {"accepted": True}, "effect": "unconfirmed", "after": None,
                        "private_error": "must not be retained"})}]}, call
                return screen_result(), call

        peer = FakePeer()
        scenario = plan()
        scenario["cases"][0]["cleanup"] = [{"tool": "handkite_key", "arguments": {"key": "enter"}}]
        campaign = Campaign(peer, scenario, count=20, warmups=0)
        campaign.run()
        names = [call["tool"] for call in peer.calls]
        self.assertEqual(names.count("handkite_tap"), 1)
        self.assertNotIn("handkite_key", names)
        self.assertEqual(names.count("handkite_observe"), 2)
        self.assertEqual(campaign.samples[0]["outcome"], "action_unconfirmed")
        self.assertEqual(campaign.failure_context["phase"], "measure")
        self.assertEqual(campaign.failure_context["call"]["tool"], "handkite_tap")
        self.assertEqual(campaign.failure_context["call"]["action_result"],
                         {"accepted": True, "effect": "unconfirmed", "observed": False})
        self.assertNotIn("must not be retained", json.dumps(campaign.failure_context))
        self.assertEqual(campaign.samples[0]["failure_kind"], "input_or_unknown_failure")
        summary = summarize(campaign.samples)["tap"]
        self.assertEqual((summary["input_failures"], summary["infrastructure_stops"]), (1, 0))

    def test_prepare_failure_keeps_its_step_and_call(self):
        class FakePeer:
            calls = []
            healthy = True

            def initialize(self):
                pass

            def tool(self, name, arguments):
                call = {"id": len(self.calls) + 1, "operation": len(self.calls) + 1,
                        "tool": name, "elapsed_ms": 1, "response_bytes": 100}
                self.calls.append(call)
                if name == "handkite_tap":
                    call["error_code"] = "ViewChanged"
                    return {"isError": True, "structuredContent": {"code": "ViewChanged"}}, call
                return screen_result(), call

        scenario = plan()
        scenario["cases"][0]["prepare"] = [{"tool": "handkite_tap", "arguments": {}}]
        campaign = Campaign(FakePeer(), scenario, count=20, warmups=0)
        campaign.run()
        self.assertEqual(len(campaign.samples), 1)
        self.assertEqual(campaign.samples[0]["outcome"], "ViewChanged")
        self.assertEqual(campaign.samples[0]["failed_phase"], "prepare")
        self.assertEqual(campaign.failure_context["phase"], "prepare")
        self.assertEqual(campaign.failure_context["step_index"], 0)
        self.assertEqual(campaign.failure_context["call"]["error_code"], "ViewChanged")

    def test_bootstrap_allows_guarded_home_preparation_but_no_locked_input(self):
        class FakePeer:
            healthy = True

            def __init__(self, lock):
                self.calls = []
                self.lock = lock

            def initialize(self):
                pass

            def tool(self, name, arguments):
                call = {"tool": name}
                self.calls.append(call)
                result = screen_result()
                payload = json.loads(result["content"][0]["text"])
                payload["metadata"]["state"]["lock"] = self.lock
                payload["metadata"]["state"]["foreground_app"] = "com.apple.springboard"
                result["content"][0]["text"] = json.dumps(payload)
                return result, call

        for lock in ("unlocked", "uncertain", "locked"):
            with self.subTest(lock=lock):
                reached_preflight = []

                def stop_before_preparation():
                    reached_preflight.append(True)
                    raise BenchError("fixture_stop")

                peer = FakePeer(lock)
                campaign = Campaign(peer, plan(), 20, 0, stop_before_preparation)
                campaign.run()
                self.assertEqual(bool(reached_preflight), lock != "locked")
                self.assertEqual(campaign.failure,
                                 "device_locked" if lock == "locked" else "fixture_stop")
                self.assertFalse(any(call["tool"] in
                                     {"handkite_tap", "handkite_launch_app", "handkite_text"}
                                     for call in peer.calls))

    def test_expectation_failure_reports_predicate_names_without_values(self):
        class FakePeer:
            calls = []
            healthy = True

            def tool(self, name, arguments):
                call = {"tool": name}
                self.calls.append(call)
                return screen_result(), call

        campaign = Campaign(FakePeer(), plan(), count=20, warmups=0)
        with self.assertRaisesRegex(BenchError, "effect_mismatch"):
            campaign.step({"tool": "handkite_observe", "expect": {
                "lock": "unlocked", "app": "different.fixture.app"}})
        self.assertEqual(campaign.peer.calls[-1]["failed_expectations"], ["app"])

    def test_gate_requires_valid_trace_matching_plan_and_twenty_successes(self):
        samples = [{"case": case, "outcome": "pass", "elapsed_ms": 100, "cleanup_ms": 1, "trace_valid": True}
                   for case in CASES for _ in range(20)]
        before = {"version": 1, "plan_sha256": "same", "trace_valid": True,
                  "failure": None, "summary": summarize(samples)}
        after = copy.deepcopy(before)
        self.assertEqual(compare(before, after), [])
        after["summary"]["tap"]["client_ms"]["p95"] = 130
        self.assertEqual(compare(before, after), ["tap:p95_regression"])
        after["trace_valid"] = False
        with self.assertRaises(BenchError):
            compare(before, after)

    def test_selected_comparison_keeps_full_gate_and_sample_requirements(self):
        samples = [{"case": case, "outcome": "pass", "elapsed_ms": 100, "cleanup_ms": 1, "trace_valid": True}
                   for case in ("type", "swipe") for _ in range(20)]
        before = {"version": 1, "plan_sha256": "same", "trace_valid": True,
                  "failure": None, "summary": summarize(samples)}
        after = copy.deepcopy(before)
        self.assertEqual(compare(before, after, ["type", "swipe"]), [])
        self.assertEqual(len(compare(before, after)), 4)
        after["summary"]["type"]["client_ms"]["p95"] = 130
        self.assertEqual(compare(before, after, ["type", "swipe"]), ["type:p95_regression"])
        after["summary"]["type"]["attempts"] = 19
        self.assertEqual(compare(before, after, ["type"]), ["type:correctness_or_sample_count"])
        for selected in ([], ["type", "type"], ["unknown"]):
            with self.assertRaises(BenchError):
                compare(before, after, selected)
        after["trace_valid"] = False
        with self.assertRaises(BenchError):
            compare(before, after, ["swipe"])


class SettingsFixtureTests(unittest.TestCase):
    def test_fresh_launch_is_only_available_for_settings_preparation(self):
        valid = {"tool": "handkite_launch_app",
                 "arguments": {"bundle_id": "com.apple.Preferences"}, "fresh_launch": True}
        for changes in ({"fresh_launch": False}, {"fresh_launch": 1},
                        {"arguments": {"bundle_id": "example.other"}},
                        {"tool": "handkite_text"}, {"when": {}}, {"selector": {}},
                        {"settle": 3}):
            with self.subTest(changes=changes), self.assertRaises(BenchError):
                invalid = plan()
                invalid["cases"][2]["prepare"] = [dict(valid, **changes)]
                validate_plan(invalid)
        for phase in ("measure", "cleanup"):
            with self.subTest(phase=phase), self.assertRaises(BenchError):
                invalid = plan()
                invalid["cases"][3][phase] = dict(valid) if phase == "measure" else [dict(valid)]
                validate_plan(invalid)

    def test_missing_settings_hook_refuses_before_any_call(self):
        peer = SimpleNamespace(calls=[])
        campaign = Campaign(peer, plan(), 1, 0)
        campaign.phase = "prepare"
        with self.assertRaisesRegex(BenchError, "settings_reset_unavailable"):
            campaign.step({"tool": "handkite_launch_app",
                           "arguments": {"bundle_id": "com.apple.Preferences"},
                           "fresh_launch": True})
        self.assertEqual(peer.calls, [])

    def run_fixture(self, root_after_reset=True, confirmed=True):
        events = []

        class FakePeer:
            healthy = True

            def __init__(self):
                self.calls = []
                self.terminated = False
                self.app = "com.apple.Preferences"

            def initialize(self):
                pass

            def tool(self, tool, arguments):
                events.append(tool)
                call = dict(id=len(self.calls), operation=len(self.calls),
                            elapsed_ms=1, response_bytes=1, tool=tool)
                self.calls.append(call)
                if tool == "handkite_launch_app":
                    self_outer.assertEqual(arguments["expected_revision"], len(self.calls) - 1)
                    self.app = "com.apple.Preferences"
                root = self.terminated and root_after_reset
                tree = ('<Application><StaticText label="Airplane Mode"/></Application>'
                        if root else '<Application><StaticText label="Keyboards"/></Application>')
                result = screen_result(tree)
                payload = json.loads(result["content"][0]["text"])
                payload["metadata"]["state"]["foreground_app"] = self.app
                payload["metadata"]["revision"] = len(self.calls)
                result["content"][0]["text"] = json.dumps(payload)
                if tool in ("handkite_launch_app", "handkite_swipe"):
                    result["content"].insert(0, {"type": "text", "text": json.dumps({
                        "receipt": {"accepted": True}, "effect": "changed",
                        "after": payload["metadata"]})})
                return result, call

        self_outer = self
        peer = FakePeer()

        def reset():
            events.append("terminate_settings")
            self.assertIsNone(campaign.screen)
            self.assertIsNotNone(campaign.lease_started)
            if isinstance(confirmed, Exception):
                raise confirmed
            peer.terminated = True
            peer.app = "com.apple.springboard"
            return confirmed

        scenario = plan()
        scenario["cases"][2]["prepare"] = [
            {"tool": "handkite_launch_app", "arguments": {"bundle_id": "com.apple.Preferences"},
             "fresh_launch": True},
            {"tool": "handkite_observe", "expect": {
                "app": "com.apple.Preferences", "lock": "unlocked",
                "text": {"type": "StaticText", "label": "Airplane Mode"}}},
        ]
        campaign = Campaign(peer, validate_plan(scenario), 1, 0,
                            selected_cases=["swipe"], settings_reset=reset)
        with contextlib.redirect_stdout(io.StringIO()):
            campaign.run()
        return campaign, events

    def test_stale_settings_subpage_is_reset_before_launch(self):
        campaign, events = self.run_fixture()
        self.assertIsNone(campaign.failure)
        start = events.index("terminate_settings")
        self.assertEqual(events[start:start + 3],
                         ["terminate_settings", "handkite_observe", "handkite_launch_app"])
        self.assertEqual(events.count("terminate_settings"), 1)
        self.assertEqual(events.count("handkite_swipe"), 1)

    def test_settings_subpage_after_reset_still_stops_before_swipe(self):
        campaign, events = self.run_fixture(root_after_reset=False)
        self.assertEqual(campaign.failure, "effect_mismatch")
        self.assertEqual(campaign.failure_context["phase"], "prepare")
        self.assertNotIn("handkite_swipe", events)
        self.assertEqual(events.count("terminate_settings"), 1)

    def test_unconfirmed_settings_reset_stops_before_launch(self):
        campaign, events = self.run_fixture(confirmed=False)
        self.assertEqual(campaign.failure, "settings_reset_unconfirmed")
        self.assertNotIn("handkite_launch_app", events)
        self.assertNotIn("handkite_swipe", events)

    def test_settings_reset_exception_stops_without_exposing_context(self):
        campaign, events = self.run_fixture(confirmed=RuntimeError("private context"))
        self.assertEqual(campaign.failure, "settings_reset_failed")
        self.assertNotIn("handkite_launch_app", events)
        self.assertNotIn("handkite_swipe", events)


class FocusedPlanTests(unittest.TestCase):
    def test_ordered_subset_and_full_plan_keep_measured_tool_mapping(self):
        focused = {"version": 1, "cases": [plan()["cases"][1]]}
        self.assertEqual(validate_plan(focused), focused)
        self.assertEqual(validate_plan(plan()), plan())
        focused["cases"][0]["measure"]["tool"] = "handkite_tap"
        with self.assertRaisesRegex(BenchError, "invalid_measured_tool"):
            validate_plan(focused)

    def test_empty_unknown_duplicate_and_unordered_cases_are_rejected(self):
        for names in ([], ["unknown"], ["type", "type"], ["type", "tap"], [None]):
            with self.subTest(names=names), self.assertRaisesRegex(BenchError, "invalid_case_matrix"):
                validate_plan({"version": 1, "cases": [{"name": name} for name in names]})

    def test_campaign_defaults_to_available_cases_and_rejects_missing_selection(self):
        focused = validate_plan({"version": 1, "cases": [plan()["cases"][1]]})
        self.assertEqual(Campaign(None, focused, 20, 0).selected_cases, ["type"])
        self.assertEqual(Campaign(None, plan(), 20, 0).selected_cases, list(CASES))
        for selected in ([], ["tap"], ["type", "tap"], ["type", "type"], ["unknown"]):
            with self.subTest(selected=selected), self.assertRaisesRegex(BenchError, "invalid_selected_cases"):
                Campaign(None, focused, 20, 0, selected_cases=selected)

    def test_cli_rejects_missing_requested_case_before_opening_trace_or_client(self):
        from benchmark_mcp import main

        raw = json.dumps({"version": 1, "cases": [plan()["cases"][1]]}).encode()
        argv = ["benchmark_mcp.py", "run", "--plan", "fixture.json", "--output", "result.json",
                "--service-trace", "trace.jsonl", "--service-pid", "1",
                "--service-build", "0" * 40, "--client-build", "0" * 40,
                "--cases", "tap", "--", "never-start"]
        with patch.object(sys, "argv", argv), patch.object(Path, "read_bytes", return_value=raw), \
                patch.object(Path, "open") as trace, patch("benchmark_mcp.Peer") as peer:
            with self.assertRaisesRegex(BenchError, "invalid_selected_cases"):
                main()
            trace.assert_not_called()
            peer.assert_not_called()

    def test_typing_fixture_requires_exact_append_and_guarded_single_backspace(self):
        from benchmark_mcp import BUDGETS_MS
        from xml.sax.saxutils import quoteattr

        source = Path(__file__).parent / "benchmarks" / "notes-typing.json"
        fixture = validate_plan(json.loads(source.read_text()))
        self.assertEqual([case["name"] for case in fixture["cases"]], ["type"])
        case = fixture["cases"][0]
        body = "Alpha " + "".join(chr(0x4e00 + n) for n in range(480)) + "Z"
        self.assertEqual(len(body.encode("utf-16-le")) // 2, 487)
        self.assertEqual(BUDGETS_MS["type"], 2000)
        steps = case["prepare"] + [case["measure"]] + case["cleanup"]
        self.assertEqual([step["tool"] for step in steps],
                         ["handkite_observe", "handkite_text", "handkite_observe",
                          "handkite_key", "handkite_observe"])
        self.assertEqual(case["measure"]["arguments"], {"value": "X", "selection": "current"})
        self.assertEqual(case["cleanup"][1]["arguments"], {"key": "backspace", "selection": "current"})
        for step, value in zip(steps, (body, body + "X", body + "X", body, body)):
            self.assertNotIn("when", step)
            self.assertEqual(step["expect"], {"app": "com.apple.mobilenotes", "lock": "unlocked",
                                             "keyboard": True, "text": {"type": "TextView", "value": value}})
        for value, accepted in ((body + "X", True), (body[:25] + "X" + body[25:], False)):
            result = screen_result("<XCUIElementTypeApplication><XCUIElementTypeKeyboard/>"
                                   "<XCUIElementTypeTextView value=" + quoteattr(value) + "/></XCUIElementTypeApplication>")
            screen = parse_screen(result)
            screen["metadata"]["state"]["foreground_app"] = "com.apple.mobilenotes"
            self.assertEqual(matches(screen, case["cleanup"][0]["expect"]), accepted)


class NotesScrollPlanTests(unittest.TestCase):
    def test_plan_keeps_exact_bounded_text_and_avoids_settings(self):
        from pathlib import Path

        source = Path(__file__).parent / "benchmarks" / "notes-scroll.json"
        fixture = validate_plan(json.loads(source.read_text()))
        self.assertNotIn("com.apple.Preferences", source.read_text())
        self.assertNotIn("fresh_launch", source.read_text())
        body = "Alpha\n" + "".join(chr(0x4e00 + n) for n in range(480)) + "\n"
        self.assertLess(len(body.encode("utf-16-le")) // 2, 512)
        self.assertEqual(len(body.splitlines()), 2)
        self.assertEqual(len(set(body[6:-1])), 480)
        cases = {case["name"]: case for case in fixture["cases"]}
        self.assertEqual(cases["swipe"]["measure"]["expect"], {
            "app": "com.apple.mobilenotes", "lock": "unlocked", "keyboard": True,
            "text": {"type": "TextView", "value": body},
            "scroll": {"viewport": [393, 852], "roi": [15, 115, 360, 325], "direction": "up"},
        })
        self.assertEqual(cases["type"]["measure"]["expect"]["text"]["value"],
                         "AlphaX\n" + body[6:])
        self.assertEqual(cases["launch"]["prepare"][1]["arguments"],
                         {"bundle_id": "com.apple.mobiletimer"})
        gesture = cases["swipe"]["measure"]["arguments"]
        self.assertEqual((gesture["from"]["y"], gesture["to"]["y"]), (420, 300))
        for case in fixture["cases"]:
            for step in case.get("cleanup", []):
                text = step.get("expect", {}).get("text")
                if text and "value" in text:
                    self.assertIn(text["value"], (body, "AlphaX\n" + body[6:]))

    def test_scroll_proof_is_opt_in_and_rejects_missing_or_stationary_images(self):
        from scroll_image import MotionResult

        result = screen_result()
        self.assertNotIn("image_bytes", parse_screen(result))
        before = parse_screen(result, retain_image=True)
        after = parse_screen(result, retain_image=True)
        expected = {"scroll": {"viewport": [393, 852], "roi": [15, 115, 360, 325],
                               "direction": "up"}}
        calls = []

        def verify(left, right, **kwargs):
            calls.append((left, right, kwargs))
            return MotionResult(False, "insufficient_change")

        with patch.dict(sys.modules, {"scroll_image": SimpleNamespace(verify_vertical_motion=verify)}):
            self.assertFalse(matches(after, expected, before))
        self.assertEqual(after["scroll_proof"]["reason"], "insufficient_change")
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][2], {"viewport": (393, 852), "roi": (15, 115, 360, 325),
                                     "direction": "up"})
        with self.assertRaisesRegex(BenchError, "scroll_images_unavailable"):
            matches(after, expected, parse_screen(result))

    def test_invalid_scroll_region_is_rejected_by_plan_before_execution(self):
        for region in ([0, 0, 500, 400], [0, 0, True, 40], [0, 0, 1, 40],
                       [0, 0, 63, 90], [0, 0, 64, 89]):
            fixture = plan()
            fixture["cases"][2]["measure"]["expect"] = {"scroll": {
                "viewport": [393, 852], "roi": region, "direction": "up"}}
            with self.assertRaisesRegex(BenchError, "invalid_scroll_expectation"):
                validate_plan(fixture)
        fixture["cases"][2]["measure"]["expect"]["scroll"]["roi"] = [0, 0, 64, 90]
        self.assertIs(validate_plan(fixture), fixture)


class CleanupScrollTests(unittest.TestCase):
    @staticmethod
    def result(image, *, action=False, body="body", refused=False):
        result = screen_result(f'<TextView type="TextView" value="{body}"/>')
        result["content"][-1]["data"] = base64.b64encode(b"\x89PNG\r\n\x1a\n" + image).decode()
        if action:
            result["content"].insert(0, {"type": "text", "text": json.dumps({
                "receipt": {"accepted": not refused}, "effect": "changed",
                "after": {}})})
        return result

    @staticmethod
    def step():
        return {"tool": "handkite_swipe", "arguments": {"duration_ms": 500}, "expect": {
            "app": "example.notes", "lock": "unlocked", "keyboard": False,
            "text": {"type": "TextView", "value": "body"},
            "scroll": {"viewport": [393, 852], "roi": [15, 115, 360, 325], "direction": "down"}}}

    class Peer:
        healthy = True
        def __init__(self, results):
            self.results = iter(results)
            self.calls = []
        def initialize(self):
            pass
        def tool(self, name, arguments):
            call = {"id": len(self.calls), "operation": len(self.calls), "tool": name,
                    "elapsed_ms": 10, "response_bytes": 1}
            self.calls.append(call)
            result = {} if name in ("handkite_acquire_control", "handkite_release_control") else next(self.results)
            return result, call

    def campaign(self, results, phase="cleanup"):
        fixture = plan()
        fixture["cases"][2]["cleanup"] = [self.step()]
        peer = self.Peer(results)
        campaign = Campaign(peer, fixture, 1, 0)
        campaign.phase = phase
        campaign.lease_started = time.monotonic()
        campaign.screen = parse_screen(self.result(b"before"), retain_image=True)
        return campaign, peer

    def test_transition_then_later_valid_uses_original_baseline_once(self):
        from scroll_image import MotionResult
        campaign, peer = self.campaign([self.result(b"transition", action=True), self.result(b"later")])
        with patch("scroll_image.verify_vertical_motion", return_value=MotionResult(True, "verified", 40)) as verify:
            call = campaign.step(self.step())
        self.assertEqual([c["tool"] for c in peer.calls], ["handkite_swipe", "handkite_observe"])
        self.assertEqual(verify.call_count, 1)
        self.assertEqual(verify.call_args.args, (b"\x89PNG\r\n\x1a\nbefore", b"\x89PNG\r\n\x1a\nlater"))
        self.assertEqual(verify.call_args.kwargs, {"viewport": (393, 852), "roi": (15, 115, 360, 325), "direction": "down"})
        self.assertIs(call, peer.calls[0])
        self.assertEqual(call["elapsed_ms"], 10)
        self.assertEqual(call["scroll_proof"]["shift_y"], 40)
        self.assertIsNone(campaign._failed_scroll_images)

    def test_later_noop_wrong_direction_and_body_drift_refuse_without_extra_reads(self):
        from scroll_image import MotionResult
        for reason, body in (("insufficient_change", "body"), ("no_coherent_translation", "body"),
                             ("verified", "changed-body")):
            with self.subTest(reason=reason, body=body):
                campaign, peer = self.campaign([self.result(b"transition", action=True), self.result(b"later", body=body)])
                with patch("scroll_image.verify_vertical_motion", return_value=MotionResult(reason == "verified", reason)):
                    with self.assertRaisesRegex(BenchError, "effect_mismatch"):
                        campaign.step(self.step())
                self.assertEqual([c["tool"] for c in peer.calls], ["handkite_swipe", "handkite_observe"])
                self.assertTrue(campaign._suppress_failure_observe)
                if reason != "verified":
                    self.assertEqual(campaign._failed_scroll_images,
                                     (b"\x89PNG\r\n\x1a\nbefore", b"\x89PNG\r\n\x1a\nlater"))

    def test_immediate_non_scroll_refusal_does_not_observe(self):
        campaign, peer = self.campaign([self.result(b"transition", action=True, body="wrong")])
        with patch("scroll_image.verify_vertical_motion") as verify:
            with self.assertRaisesRegex(BenchError, "effect_mismatch"):
                campaign.step(self.step())
        verify.assert_not_called()
        self.assertEqual([c["tool"] for c in peer.calls], ["handkite_swipe"])

    def test_campaign_failure_does_not_add_recovery_observe_and_still_releases(self):
        from scroll_image import MotionResult
        for refused in (False, True):
            with self.subTest(refused=refused):
                fixture = plan()
                fixture["cases"][5]["cleanup"] = [self.step()]
                results = [self.result(b"initial"), self.result(b"before"),
                           self.result(b"transition", action=True, refused=refused)]
                if not refused:
                    results.append(self.result(b"later"))
                peer = self.Peer(results)
                campaign = Campaign(peer, fixture, 1, 0, selected_cases=["observe_tree"])
                with patch("scroll_image.verify_vertical_motion", return_value=MotionResult(False, "insufficient_change")) as verify:
                    campaign.run()
                names = [c["tool"] for c in peer.calls]
                self.assertEqual(names.count("handkite_swipe"), 1)
                self.assertEqual(names.count("handkite_observe"), 2 if refused else 3)
                self.assertEqual(names[-1], "handkite_release_control")
                self.assertEqual(campaign.failure, "action_unconfirmed" if refused else "effect_mismatch")
                self.assertEqual(verify.call_count, 0 if refused else 1)

    def test_measured_scroll_remains_immediate_without_extra_observation(self):
        from scroll_image import MotionResult
        campaign, peer = self.campaign([self.result(b"immediate", action=True)], phase="measure")
        with patch("scroll_image.verify_vertical_motion", return_value=MotionResult(True, "verified")) as verify:
            campaign.step(self.step())
        self.assertEqual([c["tool"] for c in peer.calls], ["handkite_swipe"])
        self.assertEqual(verify.call_args.args[-1], b"\x89PNG\r\n\x1a\nimmediate")

    def test_failed_pair_writer_is_explicit_private_exclusive_and_hash_only(self):
        from scroll_image import MotionResult
        campaign, _ = self.campaign([self.result(b"transition", action=True), self.result(b"later")])
        with patch("scroll_image.verify_vertical_motion", return_value=MotionResult(False, "no_coherent_translation")):
            with self.assertRaises(BenchError):
                campaign.step(self.step())
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory) / "proof"
            self.assertEqual(list(Path(directory).iterdir()), [])
            result = campaign.write_failed_scroll_images(prefix)
            files = [Path(str(prefix) + suffix) for suffix in (".before.image", ".after.image")]
            self.assertEqual(len(list(Path(directory).iterdir())), 2)
            for path, data in zip(files, campaign._failed_scroll_images):
                self.assertEqual(path.read_bytes(), data)
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(result, dict(zip(("before_sha256", "after_sha256"),
                (hashlib.sha256(data).hexdigest() for data in campaign._failed_scroll_images))))
            self.assertNotIn(directory, json.dumps(result))
            with self.assertRaisesRegex(BenchError, "failure_image_write_failed"):
                campaign.write_failed_scroll_images(prefix)
            self.assertEqual([path.read_bytes() for path in files], list(campaign._failed_scroll_images))
            # Existing second output must survive and a newly-created first is rolled back.
            files[0].unlink()
            with self.assertRaisesRegex(BenchError, "failure_image_write_failed"):
                campaign.write_failed_scroll_images(prefix)
            self.assertFalse(files[0].exists())
            self.assertTrue(files[1].exists())
        with tempfile.TemporaryDirectory() as directory:
            os.chmod(directory, 0o755)
            with self.assertRaisesRegex(BenchError, "failure_image_write_failed"):
                campaign.write_failed_scroll_images(Path(directory) / "proof")
            self.assertEqual(list(Path(directory).iterdir()), [])


class RegionSettleTests(unittest.TestCase):
    REGION = {"viewport": [393, 852], "roi": [15, 115, 360, 325]}

    def step(self):
        return {"tool": "handkite_observe", "settle": 3,
                "settle_region": copy.deepcopy(self.REGION),
                "expect": {"app": "example.notes", "lock": "unlocked",
                           "text": {"type": "TextView", "value": "body"}}}

    def result(self, image, revision=1, body="body", app="example.notes"):
        result = CleanupScrollTests.result(image, body=body)
        payload = json.loads(result["content"][0]["text"])
        payload["metadata"]["revision"] = revision
        payload["metadata"]["state"]["foreground_app"] = app
        result["content"][0]["text"] = json.dumps(payload)
        return result

    def campaign(self, results):
        fixture = plan()
        fixture["cases"][2]["cleanup"] = [self.step()]
        peer = CleanupScrollTests.Peer(results)
        campaign = Campaign(peer, validate_plan(fixture), 1, 0)
        campaign.phase = "cleanup"
        campaign.lease_started = time.monotonic()
        return campaign, peer

    def test_outside_region_drift_passes_and_keeps_latest_real_revision(self):
        campaign, peer = self.campaign([self.result(b"first", 10), self.result(b"second", 11)])
        with patch("scroll_image.exact_rgb_region_digest", return_value="same-exact-region") as digest:
            call = campaign.step(self.step())
        self.assertEqual([c["tool"] for c in peer.calls], ["handkite_observe"] * 2)
        self.assertEqual(digest.call_count, 2)
        self.assertEqual(digest.call_args.kwargs, {"viewport": (393, 852), "roi": (15, 115, 360, 325)})
        self.assertEqual(campaign.screen["metadata"]["revision"], 11)
        self.assertEqual(campaign.screen["image_bytes"], b"\x89PNG\r\n\x1a\nsecond")
        self.assertEqual(call["settle_changed"], {"device": False, "state": False, "tree": False, "region": False})

    def test_inside_region_drift_and_full_tree_or_state_drift_refuse(self):
        for kind in ("region", "tree", "state"):
            with self.subTest(kind=kind):
                results = [self.result(str(i).encode(), body=str(i) if kind == "tree" else "body",
                                       app=str(i) if kind == "state" else "example.notes") for i in range(3)]
                campaign, peer = self.campaign(results)
                with patch("scroll_image.exact_rgb_region_digest",
                           side_effect=["a", "b", "c"] if kind == "region" else ["same"] * 3):
                    with self.assertRaisesRegex(BenchError, "view_unsettled"):
                        campaign.step(self.step())
                self.assertEqual(len(peer.calls), 3)
                self.assertTrue(peer.calls[-1]["settle_changed"][kind])

    def test_exact_region_never_skips_body_expectation_or_decode_failure(self):
        campaign, _ = self.campaign([self.result(b"a", body="wrong"), self.result(b"b", body="wrong")])
        with patch("scroll_image.exact_rgb_region_digest", return_value="same"):
            with self.assertRaisesRegex(BenchError, "effect_mismatch"):
                campaign.step(self.step())
        campaign, peer = self.campaign([self.result(b"a")])
        with patch("scroll_image.exact_rgb_region_digest", side_effect=ValueError("private detail")):
            with self.assertRaisesRegex(BenchError, "^settle_region_unavailable$"):
                campaign.step(self.step())
        self.assertEqual(len(peer.calls), 1)

    def test_default_still_requires_whole_image_equality(self):
        campaign, peer = self.campaign([self.result(str(i).encode()) for i in range(3)])
        step = self.step()
        del step["settle_region"]
        with patch("scroll_image.exact_rgb_region_digest") as digest:
            with self.assertRaisesRegex(BenchError, "view_unsettled"):
                campaign.step(step)
        digest.assert_not_called()
        self.assertEqual(len(peer.calls), 3)
        self.assertTrue(peer.calls[-1]["settle_changed"]["image"])

    def test_region_only_allowed_on_non_measured_swipe_cleanup_observation(self):
        for location in ("measure", "prepare", "input", "missing_settle"):
            with self.subTest(location=location):
                fixture = plan()
                step = self.step()
                if location == "measure":
                    fixture["cases"][5]["measure"] = step
                elif location == "prepare":
                    fixture["cases"][2]["prepare"] = [step]
                else:
                    fixture["cases"][2]["cleanup"] = [step]
                    if location == "input": step["tool"] = "handkite_swipe"
                    else: del step["settle"]
                with self.assertRaises(BenchError): validate_plan(fixture)
        for region in ({"viewport": [393, 852], "roi": [0, 0, 500, 100]},
                       dict(self.REGION, tolerance=1)):
            fixture = plan()
            step = self.step(); step["settle_region"] = region
            fixture["cases"][2]["cleanup"] = [step]
            with self.assertRaises(BenchError): validate_plan(fixture)

    def test_notes_plan_has_only_two_opt_in_regions(self):
        source = Path(__file__).parent / "benchmarks" / "notes-scroll.json"
        fixture = validate_plan(json.loads(source.read_text()))
        regions = [(case["name"], phase, step) for case in fixture["cases"]
                   for phase in ("prepare", "cleanup") for step in case.get(phase, []) if "settle_region" in step]
        self.assertEqual(len(regions), 2)
        expected_regions = [self.REGION, {"viewport": [393, 852], "roi": [15, 180, 360, 260]}]
        for (case, phase, step), region in zip(regions, expected_regions):
            self.assertEqual((case, phase, step["tool"], step["settle_region"]),
                             ("swipe", "cleanup", "handkite_observe", region))
        swipe = fixture["cases"][2]
        for step in [swipe["measure"], *swipe["cleanup"]]:
            if "scroll" in step.get("expect", {}):
                self.assertEqual(step["expect"]["scroll"]["roi"], self.REGION["roi"])


if __name__ == "__main__":
    unittest.main()
