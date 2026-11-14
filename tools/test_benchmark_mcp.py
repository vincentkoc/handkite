import base64
import copy
import contextlib
import io
import json
import sys
import time
import unittest

from benchmark_mcp import (BenchError, Campaign, CASES, Peer, compare, elements,
                           operation_stages, parse_screen, quantiles, summarize, merge_reports,
                           trace_rows, validate_plan, validate_measured_stages,
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
        for tool, count in (("handkite_swipe", 3), ("handkite_observe", 4), ("handkite_observe", True)):
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
            rows = [{"case": case, "outcome": "pass", "elapsed_ms": 10, "cleanup_ms": 1}
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

    def test_nearest_rank_percentiles_and_invalid_samples(self):
        self.assertEqual(quantiles(list(range(1, 21))), {"n": 20, "p50": 10, "p95": 19})
        self.assertEqual(quantiles([9]), {"n": 1, "p50": 9, "p95": 9})
        for values in ([], [float("nan")], [float("inf")], [-1]):
            with self.assertRaises(BenchError):
                quantiles(values)

    def test_failures_are_counted_and_not_folded_into_success_quantiles(self):
        samples = [{"case": "tap", "outcome": "pass", "elapsed_ms": 10},
                   {"case": "tap", "outcome": "action_unconfirmed", "elapsed_ms": 1},
                   {"case": "tap", "outcome": "pass", "elapsed_ms": 999, "warmup": True}]
        result = summarize(samples)["tap"]
        self.assertEqual((result["attempts"], result["passed"], result["failures"]), (2, 1, 1))
        self.assertEqual(result["client_ms"]["p50"], 10)

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

    def test_plan_requires_all_six_cases_and_never_accepts_pin_or_lifecycle_tools(self):
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
                    return {"isError": True, "content": []}, call
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
        self.assertEqual(campaign.samples, [])
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
        samples = [{"case": case, "outcome": "pass", "elapsed_ms": 100}
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


if __name__ == "__main__":
    unittest.main()
