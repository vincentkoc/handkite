#!/usr/bin/env python3
"""Repeatable, content-free MCP measurements against one existing controller.

The runner never starts a service, changes settings, unlocks, or retries input.
Its subprocess owns one MCP session; reports contain no arguments or UI content.
"""
import argparse
import base64
from dataclasses import asdict
import hashlib
import json
import math
import os
from pathlib import Path
import queue
import stat
import subprocess
import sys
import threading
import time
import xml.etree.ElementTree as ET


MAX_FRAME = 16 * 1024 * 1024
MAX_TRACE_ROWS = 100_000
CASES = ("tap", "type", "swipe", "launch", "observe_image", "observe_tree")
ACTIONS = {"handkite_tap", "handkite_text", "handkite_swipe", "handkite_launch_app",
           "handkite_click_button", "handkite_key"}
ALLOWED = ACTIONS | {"handkite_observe"}
BUDGETS_MS = {"tap": 2000, "type": 2000, "swipe": 2000, "launch": 3000,
              "observe_image": 500, "observe_tree": 1500}
ERROR_CODES = {"Busy", "DeviceMismatch", "StaleObservation", "InvalidObservationWindow",
               "InvalidLease", "StaleRevision", "ViewChanged", "LockedAction",
               "AdapterIdentityCheck", "AdapterPreActionCapture", "AdapterOwnedBefore",
               "AdapterNativeBefore", "AdapterButtonAdmission", "AdapterButtonResolve",
               "AdapterButtonClick", "AdapterSessionStatus", "AdapterSessionCheck",
               "AdapterInputExpiredBeforeSend", "AdapterInputConnect", "AdapterInputAfterSend",
               "AdapterInputTimedOutAfterSend", "AdapterInputResponseRejected",
               "AdapterOwnershipAfterAck"}
SPAN_NAMES = {"button_find", "button_attributes", "wire_image_encode", "controller_dispatch",
              "action_session_checks", "native_lock", "native_lock_spawn", "native_lock_process",
              "native_lock_read_decode", "owned_session_checks",
              "home_confirmation", "home_confirmation_attempt", "home_active_app_wait",
              "service_request", "service_serialization", "service_write", "wda_read_request",
              "wda_write_request", "usb_discovery", "usb_connect", "wda_send", "wda_response",
              "mcp_request", "mcp_write", "mcp_serialization", "mcp_service_exchange",
              "mcp_service_decode", "mcp_tree_projection", "tree_cache_probe",
              "text_prepare", "text_context_transaction", "text_context_query",
              "text_final_focus", "text_readback"}
CAPTURE_NAMES = {"ownership_before", "screenshot_request", "source_request", "app_request",
                 "lock_request", "screenshot_json", "source_json", "app_json", "lock_json",
                 "image_decode", "payload_validation", "ownership_after", "capture_total"}
COHERENCE_NAMES = {"screenshot_before_request", "screenshot_before_json", "capture_coherence",
                   "app_before_request", "app_before_json", "lock_before_request", "lock_before_json"}


class BenchError(Exception):
    """Fixed error category; never include a raw response or transport output."""


def require(condition, code):
    if not condition:
        raise BenchError(code)


def unique_pairs(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate_json_key")
        result[key] = value
    return result


def decode(raw):
    require(len(raw) <= MAX_FRAME, "frame_too_large")
    try:
        return json.loads(raw, object_pairs_hook=unique_pairs,
                          parse_constant=lambda _: require(False, "nonfinite_json"))
    except (ValueError, UnicodeError, RecursionError):
        raise BenchError("invalid_json") from None


def quantiles(values):
    require(bool(values) and all(math.isfinite(x) and x >= 0 for x in values),
            "invalid_samples")
    ordered = sorted(values)
    return {"n": len(ordered), "p50": ordered[math.ceil(len(ordered) * .50) - 1],
            "p95": ordered[math.ceil(len(ordered) * .95) - 1]}


def trace_rows(rows):
    """Validate one contiguous process stream, including non-measured calls."""
    require(bool(rows), "missing_trace")
    pid = rows[0].get("pid")
    require(type(pid) is int and all(type(row.get("sequence")) is int for row in rows),
            "invalid_trace")
    ordered = sorted(rows, key=lambda row: row["sequence"])
    sequence = ordered[0]["sequence"]
    for row in ordered:
        require(row.get("handkite_timing") == 1 and row.get("pid") == pid
                and row.get("sequence") == sequence, "trace_gap")
        require(type(row.get("operation")) is int, "invalid_trace")
        sequence += 1
    return pid


def covered_ms(intervals):
    total = 0
    end = None
    for lo, hi in sorted(intervals):
        if end is None or lo > end:
            total += hi - lo
        elif hi > end:
            total += hi - end
        end = hi if end is None else max(end, hi)
    return total / 1000


def operation_stages(rows):
    """Separate time covered by a span class from its overlapping work sum."""
    result = {}
    intervals = {}
    captures = 0
    writes = []
    finds = []
    for row in rows:
        stage = row.get("stage")
        if "elapsed_us" in row:
            require(stage in SPAN_NAMES, "unknown_trace_stage")
            value = row["elapsed_us"]
            require(type(value) in (int, float) and math.isfinite(value) and value >= 0,
                    "invalid_trace_duration")
            start = row.get("started_us")
            require(type(start) in (int, float) and math.isfinite(start) and start >= 0,
                    "invalid_trace_start")
            interval = (start, start + value)
            intervals.setdefault(stage, []).append(interval)
            name = f"span.{stage}.work"
            result[name] = result.get(name, 0) + value / 1000
            if stage in ("button_find", "text_context_transaction"):
                finds.append(interval)
            elif stage == "wda_write_request":
                writes.append((interval, value / 1000))
        if "capture" in row:
            for sample in row["capture"]["stages"]:
                require(sample.get("stage") in CAPTURE_NAMES | COHERENCE_NAMES, "unknown_capture_stage")
                require(sample.get("ok") is True, "failed_capture_trace")
                require(type(sample.get("elapsed_us")) is int and sample["elapsed_us"] >= 0,
                        "invalid_trace_duration")
                name = f"capture_{captures}.{sample['stage']}"
                result[name] = sample["elapsed_us"] / 1000
            captures += 1
    for stage, spans in intervals.items():
        result[f"span.{stage}.covered"] = covered_ms(spans)
    # Element searches use POST too; do not count them as device input.
    dispatch = [(start, end) for (start, end), _ in writes
                if not any(lo <= start and end <= hi for lo, hi in finds)]
    if dispatch:
        result["input_request"] = covered_ms(dispatch)
    return result


def validate_measured_stages(case, rows, capture_kind):
    names = {row.get("stage") for row in rows if "elapsed_us" in row}
    required = {"service_request", "service_serialization", "service_write", "controller_dispatch",
                "wire_image_encode", "wda_read_request", "usb_discovery", "usb_connect",
                "wda_send", "wda_response", "owned_session_checks"}
    if case in CASES[:4]:
        required.update({"wda_write_request", "action_session_checks"})
    if case in ("tap", "type", "launch"):
        required.add("native_lock")
    require(required <= names, "missing_service_stage")
    captures = [row["capture"] for row in rows if "capture" in row]
    narrow = "text_prepare" in names and any(row.get("stage") == "text_prepared" and row.get("ok") is True for row in rows)
    if narrow:
        require(case == "type" and capture_kind == "image_only"
                and {"text_context_transaction", "text_context_query", "text_final_focus", "text_readback"} <= names
                and any(row.get("stage") == "text_verified" and row.get("ok") is True for row in rows),
                "invalid_text_trace")
    refreshed = narrow and len(captures) == 2
    if refreshed:
        require(sum(row.get("stage") == "text_prepare" and "elapsed_us" in row for row in rows) == 2,
                "invalid_text_refresh")
    require(len(captures) == (2 if refreshed else 1 if narrow else 2 if case in CASES[:4] else 1), "missing_capture")
    require(capture_kind in ("paired", "image_only"), "unknown_capture_kind")
    for index, capture in enumerate(captures):
        expected = CAPTURE_NAMES.copy()
        reused = capture.get("reused_tree", False)
        require(type(reused) is bool, "invalid_cache_trace")
        if reused:
            require(case in CASES[:4] and index == 0 and "tree_cache_probe" in names
                    and any(row.get("stage") == "tree_cache_hit" and row.get("ok") is True
                            for row in rows), "invalid_cache_trace")
            expected -= {"source_request", "source_json"}
        if capture_kind == "image_only" and index == len(captures) - 1:
            expected -= {"source_request", "source_json"}
        samples = capture["stages"]
        present = {sample.get("stage") for sample in samples}
        if present & COHERENCE_NAMES:
            require(not reused and "source_request" in expected, "invalid_coherence_trace")
            expected |= COHERENCE_NAMES
        require(expected == {sample.get("stage") for sample in samples}
                and len(expected) == len(samples), "missing_capture_stage")
    if case in CASES[:4]:
        require("input_request" in operation_stages(rows), "missing_input_stage")


class Peer:
    def __init__(self, command, timeout=70):
        require(command and all(type(x) is str and x for x in command), "invalid_command")
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, env=dict(os.environ, HANDKITE_TIMINGS="1"))
        self.timeout = timeout
        self.responses = queue.Queue(maxsize=4)
        self.trace = []
        self.trace_error = None
        self.trace_lock = threading.Lock()
        self.calls = []
        self.sent = 0
        self.identifier = 0
        self.healthy = True
        self.threads = [threading.Thread(target=self._stdout, daemon=True),
                        threading.Thread(target=self._stderr, daemon=True)]
        for thread in self.threads:
            thread.start()

    def _stdout(self):
        try:
            while True:
                line = self.process.stdout.readline(MAX_FRAME + 1)
                if not line:
                    self.responses.put(BenchError("transport_eof"))
                    return
                if len(line) > MAX_FRAME or not line.endswith(b"\n"):
                    self.responses.put(BenchError("invalid_frame"))
                    return
                self.responses.put(line)
        except OSError:
            self.responses.put(BenchError("transport_read"))

    def _stderr(self):
        while True:
            line = self.process.stderr.readline(MAX_FRAME + 1)
            if not line:
                return
            # Unstructured stderr can contain private native diagnostics; discard it.
            if len(line) > 16_384:
                with self.trace_lock:
                    self.trace_error = "oversized_diagnostic"
                continue
            try:
                row = decode(line)
            except BenchError:
                continue
            if isinstance(row, dict) and row.get("handkite_timing") == 1:
                with self.trace_lock:
                    if len(self.trace) >= MAX_TRACE_ROWS:
                        self.trace_error = "trace_overflow"
                    else:
                        self.trace.append(row)

    def request(self, method, params=None):
        self.sent += 1
        self.identifier += 1
        request = {"jsonrpc": "2.0", "id": self.identifier, "method": method}
        if params is not None:
            request["params"] = params
        call = {"id": self.identifier, "operation": self.sent, "method": method}
        started = time.monotonic_ns()
        self.calls.append(call)
        try:
            self.process.stdin.write(json.dumps(request, allow_nan=False).encode() + b"\n")
            self.process.stdin.flush()
            raw = self.responses.get(timeout=self.timeout)
            if isinstance(raw, BenchError):
                raise raw
            response = decode(raw)
            require(isinstance(response, dict) and response.get("jsonrpc") == "2.0"
                    and type(response.get("id")) is int
                    and response["id"] == self.identifier, "response_identity")
            call["response_bytes"] = len(raw)
            error = response.get("error")
            if isinstance(error, dict) and type(error.get("code")) is int:
                call["rpc_error_code"] = error["code"]
            require("error" not in response and isinstance(response.get("result"), dict),
                    "rpc_error")
            return response["result"], call
        except queue.Empty:
            self.healthy = False
            raise BenchError("transport_timeout") from None
        except (OSError, ValueError):
            self.healthy = False
            raise BenchError("transport_write") from None
        except BenchError:
            self.healthy = False
            raise
        finally:
            call["elapsed_ms"] = (time.monotonic_ns() - started) / 1_000_000

    def initialize(self):
        self.request("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                    "clientInfo": {"name": "handkite-benchmark", "version": "1"}})
        self.sent += 1
        self.process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        self.process.stdin.flush()
        catalog, _ = self.request("tools/list", {})
        names = {tool.get("name") for tool in catalog.get("tools", [])}
        require(ALLOWED <= names, "missing_capability")

    def tool(self, name, arguments):
        try:
            result, call = self.request("tools/call", {"name": name, "arguments": arguments})
        finally:
            self.calls[-1]["tool"] = name
        call["tool"] = name
        detail = result.get("structuredContent", {})
        if isinstance(detail, dict):
            if detail.get("code") in ERROR_CODES:
                call["error_code"] = detail["code"]
            changed = detail.get("changed")
            if isinstance(changed, dict):
                call["changed"] = {key: changed[key] for key in
                                   ("state", "accessibility_tree", "screenshot")
                                   if type(changed.get(key)) is bool}
        return result, call

    def close(self):
        until = time.monotonic() + 1
        while self.healthy and time.monotonic() < until:
            with self.trace_lock:
                if any(row.get("operation") == self.sent and row.get("stage") == "mcp_request"
                       for row in self.trace):
                    break
            time.sleep(.01)
        self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        for thread in self.threads:
            thread.join(timeout=1)
        self.process.stdout.close()
        self.process.stderr.close()


def parse_screen(result, retain_image=False):
    require(result.get("isError") is not True, "tool_error")
    payloads = [decode(item["text"]) for item in result.get("content", [])
                if item.get("type") == "text" and isinstance(item.get("text"), str)]
    screen = next((value for value in payloads if isinstance(value, dict)
                   and "metadata" in value), None)
    require(screen is not None, "missing_observation")
    metadata = screen["metadata"]
    image_only = screen.get("capture_kind") == "image_only"
    require(isinstance(metadata, dict) and isinstance(metadata.get("device"), str), "invalid_observation")
    if image_only:
        require(set(metadata) == {"device", "window"}, "fabricated_image_metadata")
    else:
        require(type(metadata.get("revision")) is int and metadata["revision"] >= 0, "invalid_observation")
        state = metadata.get("state")
        require(isinstance(state, dict) and state.get("wda_ready") is True, "not_ready")
    window = metadata.get("window")
    require(isinstance(window, dict)
            and type(window.get("started_unix_ms")) is int
            and type(window.get("finished_unix_ms")) is int
            and 0 <= window["started_unix_ms"] <= window["finished_unix_ms"], "invalid_window")
    images = [item for item in result["content"] if item.get("type") == "image"]
    require(len(images) == 1 and images[0].get("mimeType") in ("image/png", "image/jpeg"),
            "missing_image")
    try:
        image = base64.b64decode(images[0]["data"], validate=True)
    except (ValueError, KeyError):
        raise BenchError("invalid_image") from None
    require(image.startswith(b"\x89PNG\r\n\x1a\n") or image.startswith(b"\xff\xd8"),
            "invalid_image")
    tree = screen.get("accessibility_tree")
    if tree is None:
        marker = screen.get("tree")
        require(isinstance(marker, dict) and marker.get("included") is False
                and marker.get("reason") == ("not_captured" if image_only else "requested")
                and not any(key in screen for key in ("accessibility_tree_delta", "tree_format"))
                and screen.get("capture_kind") in ("paired", "image_only"), "missing_tree")
    else:
        require(not image_only, "fabricated_image_tree")
        require(isinstance(tree, str) and 0 < len(tree) <= 2 * 1024 * 1024
                and "<!DOCTYPE" not in tree and "<!ENTITY" not in tree, "invalid_tree")
    parsed = {"metadata": metadata, "tree": tree, "capture_kind": screen.get("capture_kind", "paired"),
              "image_digest": hashlib.sha256(image).hexdigest()}
    if retain_image:
        parsed["image_bytes"] = image
    return parsed


def elements(screen, selector):
    require(screen is not None and screen.get("tree") is not None, "full_tree_required")
    require(isinstance(selector, dict) and set(selector) <= {"type", "label", "value", "name"}
            and bool(selector), "invalid_selector")
    try:
        root = ET.fromstring(screen["tree"])
    except ET.ParseError:
        raise BenchError("invalid_xml") from None
    matches = []
    for node in root.iter():
        attrs = dict(node.attrib, type=node.attrib.get("type", node.tag.removeprefix("XCUIElementType")))
        attrs["type"] = attrs["type"].removeprefix("XCUIElementType")
        if all(attrs.get(key) == value for key, value in selector.items()) and attrs.get("visible") != "false":
            matches.append(attrs)
    return matches


def completed_trial(row):
    cleanup = row.get("cleanup_ms")
    return (row.get("outcome") == "pass" and row.get("trace_valid") is True
            and row.get("failure_kind") is None
            and type(cleanup) in (int, float) and math.isfinite(cleanup) and cleanup >= 0
            and (row.get("capture_kind") != "image_only"
                 or type(row.get("fixture_verification_ms")) in (int, float)
                 and math.isfinite(row["fixture_verification_ms"])
                 and row["fixture_verification_ms"] >= 0))


def summarize(samples):
    result = {}
    for case in CASES:
        rows = [row for row in samples if row["case"] == case and not row.get("warmup")]
        passed = [row for row in rows if completed_trial(row)]
        infrastructure = sum(row.get("failure_kind") == "infrastructure_stop" for row in rows)
        stages = {}
        for row in passed:
            for stage, value in row.get("stages_ms", {}).items():
                stages.setdefault(stage, []).append(value)
        result[case] = {"attempts": len(rows), "passed": len(passed),
                        "completed": len(passed),
                        "infrastructure_stops": infrastructure,
                        "input_failures": sum(row.get("failure_kind") == "input_or_unknown_failure" for row in rows),
                        "failures": len(rows) - len(passed) - infrastructure,
                        "client_ms": quantiles([row["elapsed_ms"] for row in passed]) if passed else None,
                        "stages_ms": {name: quantiles(values) for name, values in stages.items()},
                        "pre_tree_reuses": sum(row.get("pre_tree_reused") is True for row in passed),
                        "budget_ms": BUDGETS_MS[case]}
        verification = [row["fixture_verification_ms"] for row in passed if "fixture_verification_ms" in row]
        if verification:
            result[case]["fixture_verification_ms"] = quantiles(verification)
    return result


def qualified_summary(row, legacy=False):
    completed = row.get("passed", 0) if legacy else row.get("completed", 0)
    return (completed >= 20 and row.get("passed") == completed
            and row.get("attempts", 0) >= completed and row.get("failures") == 0
            and row.get("infrastructure_stops", 0) == 0 and row.get("input_failures", 0) == 0
            and (row.get("client_ms") or {}).get("n") == completed)


def qualified_case(report, case):
    legacy = "samples" not in report and bool(report.get("reference_source_sha256"))
    if not qualified_summary(report["summary"][case], legacy=legacy):
        return False
    if "samples" not in report:
        return True
    rows = [row for row in report["samples"] if row.get("case") == case]
    return (all(completed_trial(row) for row in rows)
            and sum(not row.get("warmup") for row in rows) == report["summary"][case]["completed"])


def compare(before, after, selected=None):
    require(before.get("version") == after.get("version") == 1
            and before.get("trace_valid") is True and after.get("trace_valid") is True
            and before.get("failure") is None and after.get("failure") is None,
            "incomplete_benchmark")
    require(before.get("plan_sha256") == after.get("plan_sha256"), "different_plans")
    selected = CASES if selected is None else selected
    require(bool(selected) and len(selected) == len(set(selected))
            and set(selected) <= set(CASES), "invalid_selected_cases")
    failures = []
    for case in selected:
        old, new = before["summary"][case], after["summary"][case]
        if not all(qualified_case(report, case) for report in (before, after)):
            failures.append(f"{case}:correctness_or_sample_count")
            continue
        for percentile in ("p50", "p95"):
            limit = old["client_ms"][percentile] * 1.05 + 20
            if new["client_ms"][percentile] > limit:
                failures.append(f"{case}:{percentile}_regression")
    return failures


def merge_reports(parts):
    """Combine disjoint passing cohorts from one unchanged service generation."""
    require(len(parts) >= 2, "missing_cohorts")
    identity_keys = ("plan_sha256", "service_build", "client_build", "service_pid")
    identity = {key: parts[0][0][key] for key in identity_keys}
    capture_mode = parts[0][0].get("capture_mode", "paired")
    seen, samples, cohorts = set(), [], []
    for report, digest in parts:
        require(report.get("version") == 1 and report.get("trace_valid") is True
                and report.get("failure") is None and report.get("trace_error") is None
                and report.get("calibration") is False and not report.get("cohorts"),
                "incomplete_cohort")
        require(all(report.get(key) == value for key, value in identity.items()), "different_generation")
        require(report.get("capture_mode", "paired") == capture_mode, "different_capture_modes")
        selected = report.get("selected_cases", [])
        require(bool(selected) and len(selected) == len(set(selected))
                and set(selected) <= set(CASES) and not seen.intersection(selected), "overlapping_cohorts")
        rows = report["samples"]
        require(all(row["case"] in selected and completed_trial(row) for row in rows), "incomplete_cohort")
        summary = summarize(rows)
        require(summary == report["summary"], "inconsistent_summary")
        require(all(qualified_summary(summary[case])
                    for case in selected), "incomplete_cohort")
        seen.update(selected)
        samples.extend(dict(row, cohort=len(cohorts)) for row in rows)
        cohort = {"sha256": digest, "selected_cases": selected}
        if "extraction" in report:
            validate_extraction(report["extraction"])
            cohort["extraction"] = report["extraction"]
        cohorts.append(cohort)
    require(seen == set(CASES), "missing_cohorts")
    return dict(identity, version=1, calibration=False, selected_cases=list(CASES), capture_mode=capture_mode,
                trace_valid=True, trace_kind="validated_cohorts", trace_error=None,
                failure=None, failure_context=None, qualified=True, cohorts=cohorts,
                samples=samples, summary=summarize(samples))


def validate_extraction(value):
    failures = ERROR_CODES | {"effect_mismatch", "view_unsettled", "action_unconfirmed", "tool_error"}
    require(isinstance(value, dict)
            and set(value) == {"source_sha256", "parent_failure", "parent_phase", "parent_case"}
            and isinstance(value["source_sha256"], str) and len(value["source_sha256"]) == 64
            and all(c in "0123456789abcdef" for c in value["source_sha256"])
            and (value["parent_failure"] is None
                 or isinstance(value["parent_failure"], str) and value["parent_failure"] in failures)
            and value["parent_phase"] in (None, "prepare", "measure", "fixture_verification", "cleanup")
            and value["parent_case"] in (None, *CASES), "invalid_extraction_provenance")


def extract_report(report, digest, selected):
    """Retain whole completed cases, without accepting an incomplete trace."""
    require(report.get("version") == 1 and report.get("trace_valid") is True
            and report.get("trace_error") is None and report.get("calibration") is False
            and not report.get("cohorts") and not report.get("extraction"), "incomplete_cohort")
    require(bool(selected) and len(selected) == len(set(selected)) and set(selected) <= set(CASES)
            and set(selected) <= set(report.get("selected_cases", [])), "invalid_selected_cases")
    mode = report.get("capture_mode", "paired")
    require(mode in ("paired", "image_only") and type(report.get("service_pid")) is int
            and report["service_pid"] > 0, "invalid_cohort_identity")
    for key, lengths in (("plan_sha256", (64,)), ("service_build", (40, 64)), ("client_build", (40, 64))):
        value = report.get(key)
        require(isinstance(value, str) and len(value) in lengths
                and all(c in "0123456789abcdef" for c in value), "invalid_cohort_identity")
    context = report.get("failure_context") or {}
    require(isinstance(context, dict), "invalid_extraction_provenance")
    provenance = {"source_sha256": digest, "parent_failure": report.get("failure"),
                  "parent_phase": context.get("phase"), "parent_case": context.get("case")}
    validate_extraction(provenance)
    require(report.get("failure") is None or context.get("case") not in selected,
            "failed_selected_case")
    rows = [dict(row) for row in report["samples"] if row.get("case") in selected]
    for case in selected:
        case_rows = [row for row in rows if row["case"] == case]
        measured = [row for row in case_rows if not row.get("warmup")]
        ordinals = [row.get("ordinal") for row in measured]
        require(len(measured) >= 20 and all(type(n) is int for n in ordinals)
                and sorted(ordinals) == list(range(1, len(measured) + 1)), "incomplete_cohort")
        for row in case_rows:
            kind = "image_only" if mode == "image_only" and case != "observe_tree" else "paired"
            durations = [row.get("cleanup_ms")]
            if kind == "image_only":
                durations.append(row.get("fixture_verification_ms"))
            require(completed_trial(row)
                    and row.get("capture_kind") == kind
                    and bool(row.get("stages_ms"))
                    and all(type(n) in (int, float) and math.isfinite(n) and n >= 0 for n in durations),
                    "incomplete_cohort")
    identity = {key: report[key] for key in ("plan_sha256", "service_build", "client_build", "service_pid")}
    return dict(identity, version=1, calibration=False, selected_cases=[case for case in CASES if case in selected],
                capture_mode=mode, trace_valid=True,
                trace_kind="validated_completed_cohort", trace_error=None, failure=None,
                failure_context=None, qualified=True, extraction=provenance, samples=rows, summary=summarize(rows))


def reference_report(report, digest):
    require(report.get("calibration") is False and compare(report, report) == [], "incomplete_reference")
    require(isinstance(report.get("samples"), list), "missing_reference_samples")
    summary = summarize(report["samples"])
    require(summary == report["summary"] and set(report["summary"]) == set(CASES), "inconsistent_summary")
    require(report.get("capture_mode", "paired") in ("paired", "image_only"), "invalid_capture_mode")
    for pin in (report["plan_sha256"], report["service_build"], report["client_build"], digest):
        require(isinstance(pin, str) and len(pin) in (40, 64)
                and all(c in "0123456789abcdef" for c in pin), "invalid_build_pin")
    allowed_stages = {"input_request"} | {
        f"{prefix}span.{stage}.{kind}" for prefix in ("", "mcp.")
        for stage in SPAN_NAMES for kind in ("work", "covered")
    } | {f"{prefix}capture_{index}.{stage}" for prefix in ("", "mcp.")
         for index in (0, 1) for stage in CAPTURE_NAMES}
    for case in CASES:
        row = summary[case]
        require(qualified_summary(row)
                and type(row["attempts"]) is int and row["passed"] == row["attempts"] >= 20
                and row["failures"] == 0 and row["client_ms"]["n"] == row["passed"], "incomplete_reference")
        require(set(row) <= {"attempts", "passed", "completed", "infrastructure_stops", "input_failures",
                             "failures", "client_ms", "stages_ms",
                             "pre_tree_reuses", "budget_ms", "fixture_verification_ms"}
                and set(row["stages_ms"]) <= allowed_stages, "unknown_reference_metric")
        require(all(type(value) in (int, float) and math.isfinite(value) and value >= 0
                    for key, value in row.items()
                    if key not in {"client_ms", "stages_ms", "fixture_verification_ms"}), "invalid_reference_metric")
        for value in [row["client_ms"], *row["stages_ms"].values(),
                      *([row["fixture_verification_ms"]] if "fixture_verification_ms" in row else [])]:
            require(set(value) == {"n", "p50", "p95"}
                    and all(type(number) in (int, float) and math.isfinite(number) and number >= 0
                            for number in value.values()), "invalid_reference_metric")
    return {"version": 1, "trace_valid": True, "failure": None,
            "plan_sha256": report["plan_sha256"],
            "capture_mode": report.get("capture_mode", "paired"),
            "service_build": report["service_build"], "client_build": report["client_build"],
            "reference_source_sha256": digest, "summary": summary}


def write_report(path, report):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as output:
        json.dump(report, output, allow_nan=False, separators=(",", ":"))
        output.write("\n")


def validate_plan(plan):
    require(isinstance(plan, dict) and plan.get("version") == 1, "invalid_plan")
    cases = plan.get("cases")
    require(isinstance(cases, list) and bool(cases)
            and all(isinstance(case, dict) for case in cases), "invalid_case_matrix")
    names = [case.get("name") for case in cases]
    require(names == [name for name in CASES if name in names], "invalid_case_matrix")
    expected = dict(zip(CASES, ("handkite_tap", "handkite_text", "handkite_swipe",
                               "handkite_launch_app", "handkite_observe", "handkite_observe")))
    for case in cases:
        require(case.get("measure", {}).get("tool") == expected[case["name"]], "invalid_measured_tool")
        require(all(isinstance(case.get(key, []), list) and len(case.get(key, [])) <= 20
                    for key in ("prepare", "cleanup")), "invalid_step_count")
        for step in case.get("prepare", []) + [case["measure"]] + case.get("cleanup", []):
            require(isinstance(step, dict) and step.get("tool") in ALLOWED
                    and set(step) <= {"tool", "arguments", "selector", "when", "expect", "settle", "settle_region", "fresh_launch"},
                    "invalid_step")
            require(isinstance(step.get("arguments", {}), dict), "invalid_arguments")
            if "fresh_launch" in step:
                require(step["fresh_launch"] is True
                        and any(step is candidate for candidate in case.get("prepare", []))
                        and step.get("tool") == "handkite_launch_app"
                        and step.get("arguments") == {"bundle_id": "com.apple.Preferences"}
                        and "when" not in step and "selector" not in step,
                        "invalid_fresh_launch")
            if "settle" in step:
                require(type(step["settle"]) is int and 3 <= step["settle"] <= 5
                        and step["tool"] == "handkite_observe" and not step.get("arguments")
                        and "selector" not in step and step is not case["measure"], "invalid_settle")
            if "settle_region" in step:
                require("settle" in step and case["name"] == "swipe"
                        and any(step is candidate for candidate in case.get("cleanup", [])),
                        "invalid_settle_region")
                settle_region(step["settle_region"])
            for key in ("expect", "when"):
                expectations = step.get(key, {})
                require(isinstance(expectations, dict)
                        and set(expectations) <= {"app", "app_not", "lock", "keyboard",
                                                  "text", "row_moved", "tree_changed", "scroll"},
                        "invalid_expectation")
                if "scroll" in expectations:
                    scroll_expectation(expectations["scroll"])
        image_only = case["measure"].get("arguments", {}).get("tree_format") == "omit"
        require(image_only == (case["name"] == "observe_image"), "invalid_observation_matrix")
    return plan


def select_cases(plan, selected=None):
    available = [case["name"] for case in plan["cases"]]
    selected = available if selected is None else selected
    require(bool(selected) and len(set(selected)) == len(selected)
            and set(selected) <= set(CASES) and set(selected) <= set(available),
            "invalid_selected_cases")
    return selected


def scroll_expectation(value):
    require(isinstance(value, dict) and set(value) == {"viewport", "roi", "direction"},
            "invalid_scroll_expectation")
    viewport, roi = value["viewport"], value["roi"]
    require(isinstance(viewport, list) and len(viewport) == 2
            and all(type(n) is int and 1 <= n <= 4096 for n in viewport)
            and isinstance(roi, list) and len(roi) == 4
            and all(type(n) is int and n >= 0 for n in roi)
            and roi[2] >= 64 and roi[3] >= 90
            and roi[0] + roi[2] <= viewport[0] and roi[1] + roi[3] <= viewport[1]
            and value["direction"] in ("up", "down"), "invalid_scroll_expectation")
    return tuple(viewport), tuple(roi), value["direction"]


def settle_region(value):
    require(isinstance(value, dict) and set(value) == {"viewport", "roi"},
            "invalid_settle_region")
    viewport, roi, _ = scroll_expectation(dict(value, direction="up"))
    return viewport, roi


def matches(screen, expected, previous=None):
    if not expected:
        return True
    if screen is None:
        return False
    state = screen["metadata"]["state"]
    for key, value in expected.items():
        if key == "app" and state.get("foreground_app") != value:
            return False
        if key == "app_not" and state.get("foreground_app") == value:
            return False
        if key == "lock" and state.get("lock") != value:
            return False
        if key == "keyboard" and bool(elements(screen, {"type": "Keyboard"})) != value:
            return False
        if key == "text" and len(elements(screen, value)) != 1:
            return False
        if key == "row_moved":
            if previous is None:
                return False
            before = elements(previous, value["selector"])
            after = elements(screen, value["selector"])
            if len(before) != 1 or len(after) != 1:
                return False
            try:
                difference = abs(float(before[0]["y"]) - float(after[0]["y"]))
                minimum = float(value["minimum"])
            except (KeyError, ValueError, TypeError):
                raise BenchError("invalid_row_geometry") from None
            require(math.isfinite(difference) and math.isfinite(minimum) and minimum >= 0,
                    "invalid_row_geometry")
            if difference < minimum:
                return False
        if key == "tree_changed":
            require(type(value) is bool, "invalid_tree_change_expectation")
            if previous is None or (screen["tree"] != previous["tree"]) != value:
                return False
        if key == "scroll":
            viewport, roi, direction = scroll_expectation(value)
            require(previous is not None and isinstance(previous.get("image_bytes"), bytes)
                    and isinstance(screen.get("image_bytes"), bytes), "scroll_images_unavailable")
            try:
                from scroll_image import verify_vertical_motion
                motion = verify_vertical_motion(previous["image_bytes"], screen["image_bytes"],
                                                viewport=viewport, roi=roi, direction=direction)
            except (ImportError, ValueError):
                raise BenchError("scroll_verification_unavailable") from None
            screen["scroll_proof"] = asdict(motion)
            if not motion.verified:
                return False
        require(key in {"app", "app_not", "lock", "keyboard", "text", "row_moved", "tree_changed", "scroll"},
                "unknown_expectation")
    return True


class Campaign:
    def __init__(self, peer, plan, count, warmups, preflight=None, selected_cases=None, capture_mode="paired",
                 settings_reset=None):
        self.peer, self.plan, self.count, self.warmups = peer, plan, count, warmups
        self.screen = None
        self.device = None
        self.lease_started = None
        self.samples = []
        self.failure = None
        self._failed_scroll_images = None
        self._suppress_failure_observe = False
        self.failure_kind = None
        self.failure_context = None
        self.phase, self.case, self.step_index = "initialize", None, None
        self.preflight = preflight
        self.settings_reset = settings_reset
        require(capture_mode in ("paired", "image_only"), "invalid_capture_mode")
        self.capture_mode = capture_mode
        self.selected_cases = select_cases(plan, selected_cases)
        self.retain_images = any("settle_region" in step or "scroll" in step.get("expect", {})
                                 for case in plan.get("cases", [])
                                 for step in case.get("prepare", []) + [case["measure"]] + case.get("cleanup", []))

    def write_failed_scroll_images(self, output_prefix):
        """Explicit private evidence export; no image bytes or paths enter reports."""
        require(self._failed_scroll_images is not None, "no_failed_scroll_images")
        prefix = Path(output_prefix)
        require(prefix.name not in ("", ".", ".."), "invalid_failure_image_prefix")
        directory = None
        created = []
        try:
            directory = os.open(prefix.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            metadata = os.fstat(directory)
            require(stat.S_ISDIR(metadata.st_mode) and metadata.st_uid == os.getuid()
                    and metadata.st_mode & 0o077 == 0, "failure_image_directory_not_private")
            names = [prefix.name + suffix for suffix in (".before.image", ".after.image")]
            for name, data in zip(names, self._failed_scroll_images):
                fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                             0o600, dir_fd=directory)
                created.append(name)
                with os.fdopen(fd, "wb") as output:
                    output.write(data)
            return dict(zip(("before_sha256", "after_sha256"),
                            (hashlib.sha256(data).hexdigest() for data in self._failed_scroll_images)))
        except (OSError, BenchError):
            if directory is not None:
                for name in created:
                    try:
                        os.unlink(name, dir_fd=directory)
                    except OSError:
                        pass
            raise BenchError("failure_image_write_failed") from None
        finally:
            if directory is not None:
                os.close(directory)

    def _expect(self, expected, previous, call):
        failed = [key for key, value in expected.items()
                  if not matches(self.screen, {key: value}, previous)]
        if "scroll_proof" in self.screen:
            call["scroll_proof"] = self.screen["scroll_proof"]
        if "scroll" in failed and self._failed_scroll_images is None:
            images = (previous.get("image_bytes"), self.screen.get("image_bytes"))
            if all(isinstance(data, bytes) and 0 < len(data) <= MAX_FRAME
                   and data.startswith((b"\x89PNG\r\n\x1a\n", b"\xff\xd8")) for data in images):
                self._failed_scroll_images = images
        if failed:
            call["failed_expectations"] = failed
            state = self.screen["metadata"]["state"]
            lock = state.get("lock")
            app = state.get("foreground_app")
            previous_app = previous["metadata"]["state"].get("foreground_app") if previous else None
            call["failed_state"] = {
                "lock": lock if lock in ("locked", "unlocked", "uncertain") else "unknown",
                "same_app_as_previous": app == previous_app if app and previous_app else None,
            }
            raise BenchError("effect_mismatch")

    def _cleanup_scroll(self, step):
        # The later frame is unconditional after ACK, not a retry after failed
        # motion proof. Compare it to the original pre-action image.
        previous = self.screen
        immediate = dict(step, expect={key: value for key, value in step["expect"].items()
                                     if key != "scroll"})
        try:
            call = self.step(immediate)
            require(call is not None, "missing_cleanup_action")
            later = self.step({"tool": "handkite_observe"})
            self._expect(step["expect"], previous, later)
            call["scroll_proof"] = self.screen["scroll_proof"]
            return call
        except BenchError:
            # No second observation after failed proof, and no observation at
            # all after refused input. Normal release/close remain unchanged.
            self._suppress_failure_observe = True
            raise

    def renew(self):
        if self.lease_started is not None:
            if time.monotonic() - self.lease_started < 45:
                return
            result, _ = self.peer.tool("handkite_release_control", {})
            require(result.get("isError") is not True, "release_failed")
            self.lease_started = None
        result, _ = self.peer.tool("handkite_acquire_control", {})
        require(result.get("isError") is not True, "acquire_failed")
        self.lease_started = time.monotonic()

    def step(self, step, image_only=False):
        if not matches(self.screen, step.get("when")):
            return None
        if (self.phase == "cleanup" and step.get("tool") == "handkite_swipe"
                and "scroll" in step.get("expect", {})):
            return self._cleanup_scroll(step)
        if step.get("fresh_launch"):
            require(step["fresh_launch"] is True and self.phase == "prepare"
                    and step.get("tool") == "handkite_launch_app"
                    and step.get("arguments") == {"bundle_id": "com.apple.Preferences"}
                    and callable(self.settings_reset), "settings_reset_unavailable")
            self.renew()
            require(matches(self.screen, {"lock": "unlocked"}), "device_locked")
            self.screen = None
            try:
                confirmed = self.settings_reset()
            except BenchError:
                raise
            except Exception:
                raise BenchError("settings_reset_failed") from None
            require(confirmed is True, "settings_reset_unconfirmed")
            self.step({"tool": "handkite_observe"})
        if "settle" in step:
            region = settle_region(step["settle_region"]) if "settle_region" in step else None
            previous = None
            for _ in range(step["settle"]):
                call = self.step({"tool": "handkite_observe"})
                image_digest = self.screen["image_digest"]
                if region is not None:
                    try:
                        from scroll_image import exact_rgb_region_digest
                        image_digest = exact_rgb_region_digest(
                            self.screen.get("image_bytes"), viewport=region[0], roi=region[1])
                    except (ImportError, ValueError):
                        raise BenchError("settle_region_unavailable") from None
                current = (self.screen["metadata"]["device"], self.screen["metadata"]["state"],
                           self.screen["tree"], image_digest)
                if previous is not None:
                    call["settle_changed"] = {key: left != right for key, left, right in
                                              zip(("device", "state", "tree", "region" if region else "image"),
                                                  previous, current)}
                if current == previous:
                    require(matches(self.screen, step.get("expect")), "effect_mismatch")
                    return call
                previous = current
            raise BenchError("view_unsettled")
        self.renew()
        tool = step["tool"]
        arguments = dict(step.get("arguments", {}))
        if image_only:
            options = {"capture": "image_only", "tree_format": "omit"}
            if tool in ACTIONS:
                arguments["response"] = options
            else:
                require(tool == "handkite_observe", "invalid_image_tool")
                arguments.update(options)
        if tool in ACTIONS:
            require(self.screen is not None, "missing_action_baseline")
            arguments["expected_revision"] = self.screen["metadata"]["revision"]
        if "selector" in step:
            require(tool == "handkite_click_button", "invalid_selector_tool")
            found = elements(self.screen, step["selector"])
            require(len(found) == 1, "ambiguous_target")
            target = found[0]
            require(target.get("visible") == "true" and target.get("enabled") == "true",
                    "target_not_actionable")
            try:
                rect = {key: float(target[key]) for key in ("x", "y", "width", "height")}
            except (KeyError, ValueError):
                raise BenchError("invalid_target_rectangle") from None
            require(all(math.isfinite(value) and 0 <= value <= 10_000 for value in rect.values())
                    and rect["width"] > 0 and rect["height"] > 0, "invalid_target_rectangle")
            arguments["target"] = dict(rect, label=target.get("label", ""))
        previous = self.screen
        result, call = self.peer.tool(tool, arguments)
        if tool in ACTIONS:
            receipt = {}
            for item in result.get("content", []):
                if item.get("type") == "text":
                    try:
                        value = decode(item["text"])
                    except BenchError:
                        break
                    if isinstance(value, dict):
                        receipt = value
                    break
            transport = receipt.get("receipt")
            accepted = transport.get("accepted") if isinstance(transport, dict) else None
            effect = receipt.get("effect")
            # Keep fixed receipt facts even when a failed capture makes isError true.
            call["action_result"] = {
                "accepted": accepted if type(accepted) is bool else None,
                "effect": effect if effect in ("changed", "lock_confirmed", "unconfirmed") else None,
                "observed": receipt.get("after_image" if image_only else "after") is not None,
            }
        if result.get("isError") is True:
            code = result.get("structuredContent", {}).get("code")
            if code in ERROR_CODES:
                call["error_code"] = code
                raise BenchError(code)
            raise BenchError("action_unconfirmed" if tool in ACTIONS else "tool_error")
        if tool in ACTIONS:
            require(call["action_result"] == {"accepted": True, "effect": "changed", "observed": True},
                    "action_unconfirmed")
        self.screen = parse_screen(result, retain_image=self.retain_images)
        require((self.screen["capture_kind"] == "image_only") == image_only, "wrong_capture_mode")
        if image_only and tool in ACTIONS:
            require(previous is not None and self.screen["image_digest"] != previous["image_digest"],
                    "unchanged_action_image")
        if self.device is None:
            self.device = self.screen["metadata"]["device"]
        require(self.screen["metadata"]["device"] == self.device, "device_changed")
        self._expect({} if image_only else step.get("expect", {}), previous, call)
        return call

    def run(self):
        sample = None
        trial_call_start = 0
        fixture_observed = False
        try:
            self.peer.initialize()
            self.renew()
            self.step({"tool": "handkite_observe"})
            # Home has conservative lock metadata; scenario preparation may
            # launch its app through the controller's guarded native admission.
            require(self.screen["metadata"]["state"].get("lock") in
                    {"unlocked", "uncertain"}, "device_locked")
            if self.preflight is not None:
                self.preflight()
            # Keep scenario ordering fixed across baseline and candidate runs.
            for case in self.plan["cases"]:
                if case["name"] not in self.selected_cases:
                    continue
                self.case = case["name"]
                for ordinal in range(self.warmups + self.count):
                    fixture_observed = False
                    sample = {"case": case["name"], "ordinal": ordinal - self.warmups + 1,
                              "warmup": ordinal < self.warmups, "outcome": "not_started"}
                    self.samples.append(sample)
                    trial_call_start = len(self.peer.calls)
                    prepare_start = time.monotonic()
                    self.phase = "prepare"
                    for self.step_index, step in enumerate(case.get("prepare", [])):
                        self.step(step)
                    sample["prepare_ms"] = (time.monotonic() - prepare_start) * 1000
                    start = len(self.peer.calls)
                    try:
                        self.phase, self.step_index = "measure", None
                        previous = self.screen
                        image_only = self.capture_mode == "image_only" and case["name"] != "observe_tree"
                        call = self.step(case["measure"], image_only=image_only)
                        require(call is not None, "skipped_measurement")
                        sample.update({key: call[key] for key in
                                       ("id", "operation", "elapsed_ms", "response_bytes")})
                        sample["capture_kind"] = self.screen["capture_kind"]
                        if image_only:
                            # This read qualifies the fixture outside the image-only request.
                            self.phase = "fixture_verification"
                            started = time.monotonic()
                            verification_call = self.step({"tool": "handkite_observe"})
                            fixture_observed = verification_call is not None
                            try:
                                self._expect(case["measure"].get("expect") or {}, previous,
                                             verification_call)
                            finally:
                                if "scroll_proof" in self.screen:
                                    sample["scroll_proof"] = self.screen["scroll_proof"]
                            sample["fixture_verification_ms"] = (time.monotonic() - started) * 1000
                        sample["measurement_passed"] = True
                    except BenchError as error:
                        sample["outcome"] = str(error)
                        if "operation" not in sample and len(self.peer.calls) > start:
                            sample.update({key: self.peer.calls[-1][key] for key in
                                           ("id", "operation", "elapsed_ms", "response_bytes")
                                           if key in self.peer.calls[-1]})
                        raise
                    cleanup_start = time.monotonic()
                    self.phase = "cleanup"
                    for self.step_index, step in enumerate(case.get("cleanup", [])):
                        self.step(step)
                    sample["cleanup_ms"] = (time.monotonic() - cleanup_start) * 1000
                    sample["outcome"] = "pass"
                    self.phase, self.step_index = "progress", None
                    try:
                        print(json.dumps({key: sample[key] for key in
                                          ("case", "ordinal", "warmup", "outcome", "elapsed_ms")}), flush=True)
                    except BrokenPipeError:
                        raise BenchError("progress_output_closed") from None
        except BenchError as error:
            self.failure = str(error)
            call = self.peer.calls[-1] if len(self.peer.calls) > trial_call_start else None
            # Only this fixed adapter refusal guarantees that dispatch never began.
            infrastructure = (self.failure == "AdapterNativeBefore" and call is not None
                              and call.get("tool") in ACTIONS
                              and call.get("error_code") == self.failure)
            fixture = (self.phase == "fixture_verification" and self.failure == "effect_mismatch"
                       and fixture_observed)
            reporting = self.phase == "progress" and self.failure == "progress_output_closed"
            self.failure_kind = ("reporting_failure" if reporting else
                                 "infrastructure_stop" if infrastructure else
                                 "fixture_failure" if fixture else "input_or_unknown_failure")
            if sample is not None and not reporting:
                sample.update(outcome=self.failure, failure_kind=self.failure_kind, failed_phase=self.phase)
            self.failure_context = {"phase": self.phase, "case": self.case,
                                    "step_index": self.step_index,
                                    "call": dict(self.peer.calls[-1]) if self.peer.calls else None}
            # A fresh read can diagnose a failed action; never repeat its input.
            if (not self._suppress_failure_observe
                    and self.failure in {"action_unconfirmed", "effect_mismatch", "tool_error"}):
                try:
                    self.step({"tool": "handkite_observe"})
                except BenchError:
                    pass
        finally:
            if self.lease_started is not None and self.peer.healthy:
                try:
                    self.peer.tool("handkite_release_control", {})
                except BenchError:
                    self.failure = self.failure or "release_failed"
                    self.failure_kind = self.failure_kind or "input_or_unknown_failure"
                self.lease_started = None


def validate_client_stages(rows, observed=True):
    required = {"mcp_request", "mcp_serialization", "mcp_write", "mcp_service_decode",
                "mcp_service_exchange"}
    if observed:
        required.add("mcp_tree_projection")
    require(required <= {row.get("stage") for row in rows}, "incomplete_client_trace")


def correlate_prefix(client_by_op, service_by_op):
    client = [op for op, rows in sorted(client_by_op.items())
              if any(row.get("stage") == "mcp_service_exchange" for row in rows)]
    decoded = [op for op, rows in sorted(client_by_op.items())
               if any(row.get("stage") == "mcp_service_decode" for row in rows)]
    all_service = sorted(op for op in service_by_op if op > 0)
    service = [op for op, rows in sorted(service_by_op.items())
               if any(row.get("stage") == "service_request" for row in rows)]
    require(service == all_service[:len(service)]
            and decoded == client[:len(service)] and len(decoded) == len(service),
            "service_correlation_mismatch")
    return dict(zip(client, service)), len(client) == len(service) == len(all_service)


def attach_stages(campaign, service_rows, expected_pid):
    peer = campaign.peer
    require(peer.trace_error is None, "client_trace_invalid")
    trace_rows(peer.trace)
    require(trace_rows(service_rows) == expected_pid, "service_pid_changed")
    client_by_op = {}
    service_by_op = {}
    for row in sorted(peer.trace, key=lambda row: row["sequence"]):
        client_by_op.setdefault(row["operation"], []).append(row)
    for row in sorted(service_rows, key=lambda row: row["sequence"]):
        service_by_op.setdefault(row["operation"], []).append(row)
    correspondence, complete = correlate_prefix(client_by_op, service_by_op)
    for call in peer.calls:
        if call["operation"] in correspondence:
            call["service_operation"] = correspondence[call["operation"]]
    for sample in campaign.samples:
        if "operation" not in sample:
            continue
        op = sample["operation"]
        sample["trace_valid"] = False
        if op not in correspondence:
            continue
        validate_client_stages(client_by_op[op], observed=sample["outcome"] == "pass"
                               and sample.get("capture_kind") != "image_only")
        service = service_by_op[correspondence[op]]
        require({"service_request", "service_serialization", "service_write"}
                <= {row.get("stage") for row in service}, "incomplete_service_trace")
        if sample["outcome"] == "pass":
            validate_measured_stages(sample["case"], service, sample["capture_kind"])
        stages = {"mcp." + key: value for key, value in operation_stages(client_by_op[op]).items()}
        stages.update(operation_stages(service))
        sample["service_operation"] = correspondence[op]
        captures = [row["capture"] for row in service if "capture" in row]
        sample["pre_tree_reused"] = bool(captures and captures[0].get("reused_tree") is True)
        sample["stages_ms"] = stages
        sample["trace_valid"] = (sample["outcome"] == "pass" and "cleanup_ms" in sample
                                 and (sample.get("capture_kind") != "image_only"
                                      or "fixture_verification_ms" in sample))
    # A terminal transport failure must not erase already validated measurements.
    # The whole campaign still fails; only its proven prefix retains stage data.
    require(complete, "service_correlation_mismatch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="mode", required=True)
    run = commands.add_parser("run")
    run.add_argument("--plan", type=Path, required=True)
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--failure-images-prefix", type=Path,
                     help="export a failed scroll pair into an existing private directory")
    run.add_argument("--service-trace", type=Path, required=True)
    run.add_argument("--service-pid", type=int, required=True)
    run.add_argument("--service-build", required=True)
    run.add_argument("--client-build", required=True)
    run.add_argument("--count", type=int, default=20)
    run.add_argument("--warmups", type=int, default=1)
    run.add_argument("--calibrate", action="store_true", help="one trial per case; cannot qualify the gate")
    run.add_argument("--capture", choices=("paired", "image_only"), default="paired",
                     help="image_only measures opt-in image results, then separately times full fixture verification")
    run.add_argument("--cases", nargs="+", choices=CASES,
                     help="run selected plan cases; omitted means all available plan cases")
    run.add_argument("command", nargs=argparse.REMAINDER)
    diff = commands.add_parser("compare")
    diff.add_argument("before", type=Path)
    diff.add_argument("after", type=Path)
    diff.add_argument("--cases", nargs="+", choices=CASES,
                      help="compare only these cohorts; omitted means all six cases")
    merge = commands.add_parser("merge")
    merge.add_argument("--output", type=Path, required=True)
    merge.add_argument("reports", nargs="+", type=Path)
    reference = commands.add_parser("reference")
    reference.add_argument("report", type=Path)
    reference.add_argument("--output", type=Path, required=True)
    extract = commands.add_parser("extract")
    extract.add_argument("report", type=Path)
    extract.add_argument("--output", type=Path, required=True)
    extract.add_argument("--cases", nargs="+", choices=CASES, required=True)
    args = parser.parse_args()
    if args.mode == "compare":
        before, after = decode(args.before.read_bytes()), decode(args.after.read_bytes())
        failures = compare(before, after, args.cases)
        print(json.dumps({"passed": not failures, "failures": failures,
                          "selected_cases": list(CASES if args.cases is None else args.cases),
                          "capture_before": before.get("capture_mode", "paired"),
                          "capture_after": after.get("capture_mode", "paired")}))
        return int(bool(failures))
    if args.mode == "merge":
        parts = [path.read_bytes() for path in args.reports]
        report = merge_reports([(decode(part), hashlib.sha256(part).hexdigest()) for part in parts])
        write_report(args.output, report)
        print(json.dumps({"complete": True, "cohorts": len(parts), "cases": len(CASES)}))
        return 0
    if args.mode == "reference":
        raw = args.report.read_bytes()
        result = reference_report(decode(raw), hashlib.sha256(raw).hexdigest())
        write_report(args.output, result)
        print(json.dumps({"complete": True, "cases": len(result["summary"])}))
        return 0
    if args.mode == "extract":
        raw = args.report.read_bytes()
        result = extract_report(decode(raw), hashlib.sha256(raw).hexdigest(), args.cases)
        write_report(args.output, result)
        print(json.dumps({"complete": True, "selected_cases": result["selected_cases"]}))
        return 0
    require(20 <= args.count <= 100 and 0 <= args.warmups <= 5, "invalid_sample_count")
    if args.calibrate:
        args.count, args.warmups = 1, 0
    for pin in (args.service_build, args.client_build):
        require(len(pin) in (40, 64) and all(c in "0123456789abcdef" for c in pin), "invalid_build_pin")
    plan_bytes = args.plan.read_bytes()
    plan = validate_plan(decode(plan_bytes))
    selected = select_cases(plan, args.cases)
    require(not args.output.exists(), "output_already_exists")
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    with args.service_trace.open("rb") as trace:
        trace.seek(0, os.SEEK_END)
        start = trace.tell()
        peer = Peer(command)
        def preflight():
            until = time.monotonic() + 1
            while time.monotonic() < until:
                trace.seek(start)
                active = False
                for line in trace.read(MAX_FRAME + 1).splitlines():
                    try:
                        row = decode(line)
                        active |= isinstance(row, dict) and row.get("pid") == args.service_pid \
                            and row.get("stage") == "service_request"
                    except BenchError:
                        continue
                with peer.trace_lock:
                    client_active = any(row.get("stage") == "mcp_request" for row in peer.trace)
                if active and client_active:
                    return
                time.sleep(.025)
            raise BenchError("timing_not_enabled")
        campaign = Campaign(peer, plan, args.count, args.warmups, preflight, selected, args.capture)
        try:
            campaign.run()
        finally:
            peer.close()
        time.sleep(.2)
        trace.seek(start)
        rows = []
        data = trace.read(MAX_FRAME + 1)
        require(len(data) <= MAX_FRAME, "service_trace_too_large")
        for line in data.splitlines():
            try:
                row = decode(line)
                if isinstance(row, dict) and row.get("handkite_timing") == 1:
                    rows.append(row)
            except BenchError:
                continue
        trace_error = None
        try:
            attach_stages(campaign, rows, args.service_pid)
            trace_valid = True
        except BenchError as error:
            trace_valid = False
            trace_error = str(error)
            campaign.failure = campaign.failure or str(error)
        report = {"version": 1, "plan_sha256": hashlib.sha256(plan_bytes).hexdigest(),
                  "capture_mode": args.capture,
                  "calibration": args.calibrate,
                  "selected_cases": campaign.selected_cases,
                  "service_build": args.service_build, "client_build": args.client_build,
                  "service_pid": args.service_pid, "trace_valid": trace_valid,
                  "trace_error": trace_error,
                  "failure": campaign.failure, "samples": campaign.samples,
                  "failure_kind": campaign.failure_kind,
                  "failure_context": campaign.failure_context, "calls": peer.calls,
                  "summary": summarize(campaign.samples)}
        report["qualified"] = (not args.calibrate and campaign.failure is None and trace_valid
                               and all(qualified_case(report, case) for case in campaign.selected_cases))
        write_report(args.output, report)
        compact = {name: {key: value[key] for key in ("attempts", "passed", "completed",
                                                   "infrastructure_stops", "input_failures", "failures", "client_ms")}
                   for name, value in report["summary"].items()}
        result = {"complete": campaign.failure is None, "qualified": report["qualified"],
                  "summary": compact}
        if args.failure_images_prefix is not None and campaign._failed_scroll_images is not None:
            try:
                result["failure_image_hashes"] = campaign.write_failed_scroll_images(args.failure_images_prefix)
            except BenchError as error:
                result["failure_image_error"] = str(error)
        if campaign.failure != "progress_output_closed":
            print(json.dumps(result), flush=True)
        return int(campaign.failure is not None)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BenchError as error:
        print(json.dumps({"failure": str(error)}), file=sys.stderr)
        sys.exit(1)
