#!/usr/bin/env python3
"""Run the direct text injection matrix once per case through guarded MCP.

Each case sends one `handkite_text` call with final text and compares the
editor value read back from the action's own observation. Nothing is typed
through an input method and no failed or uncertain input is ever sent again.
The report holds case identifiers, outcomes, sizes and timings only: no typed
text, tree, screenshot or device identifier.
"""

import argparse
import json
import math
import os
import sys
import time
import unicodedata
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark_mcp  # noqa: E402
from benchmark_mcp import BenchError, Peer, elements, parse_screen, require  # noqa: E402

SPEC = Path(__file__).resolve().parent / "benchmarks" / "text-matrix.json"
NOTES = "com.apple.mobilenotes"
BUILDS = ("installed", "branch")
EXPECTATIONS = {"pass", "fail", "refused", "uncertain", "unknown"}
COMPARISONS = {"exact", "unchanged"}
MAX_INPUT_BYTES = 8_192
# The request never reached the device: safe to continue with the next case.
PRE_INPUT = {"Busy", "DeviceMismatch", "StaleObservation", "InvalidObservationWindow",
             "InvalidLease", "StaleRevision", "ViewChanged", "LockedAction",
             "AdapterIdentityCheck", "AdapterPreActionCapture", "AdapterOwnedBefore",
             "AdapterNativeBefore", "AdapterSessionStatus", "AdapterSessionCheck",
             "AdapterInputExpiredBeforeSend", "AdapterInputConnect"}
CASE_KEYS = {"id", "class", "input", "segments", "expected_readback", "comparison", "keyboards",
             "builds", "expect", "rerun_on_branch", "timing", "reason", "optional"}


def load_spec(path=SPEC):
    """Validate the whole spec before any device request is made."""
    try:
        raw = Path(path).read_bytes()
    except OSError:
        raise BenchError("spec_unreadable") from None
    require(len(raw) <= 1024 * 1024 and raw.isascii(), "spec_not_ascii")
    try:
        spec = json.loads(raw, object_pairs_hook=benchmark_mcp.unique_pairs)
    except ValueError:
        raise BenchError("spec_invalid_json") from None
    require(isinstance(spec, dict) and spec.get("version") == 1, "spec_version")
    keyboards = spec.get("keyboards")
    require(isinstance(keyboards, dict) and keyboards
            and all(type(k) is str and type(v) is str for k, v in keyboards.items()),
            "spec_keyboards")
    cases = spec.get("cases")
    require(isinstance(cases, list) and cases, "spec_cases")
    seen = set()
    for case in cases:
        require(isinstance(case, dict) and set(case) <= CASE_KEYS, "case_fields")
        name = case.get("id")
        require(type(name) is str and name.isascii() and name.replace("_", "").isalnum()
                and name not in seen, "case_id")
        seen.add(name)
        segments = case.get("segments")
        text = case.get("input")
        if segments is None:
            require(type(text) is str and text, "case_input")
            parts = [text]
        else:
            require(text is None and isinstance(segments, list) and len(segments) >= 2
                    and all(type(part) is str and part and "\n" not in part for part in segments),
                    "case_segments")
            parts = segments
        require(all(len(part.encode()) <= MAX_INPUT_BYTES and _scalars_valid(part)
                    for part in parts), "case_input")
        require(type(case.get("expected_readback")) is str, "case_expected")
        require(case.get("comparison") in COMPARISONS, "case_comparison")
        require(isinstance(case.get("keyboards"), list) and case["keyboards"]
                and set(case["keyboards"]) <= set(keyboards)
                and len(set(case["keyboards"])) == len(case["keyboards"]), "case_keyboards")
        require(isinstance(case.get("builds"), list) and case["builds"]
                and set(case["builds"]) <= set(BUILDS), "case_builds")
        expect = case.get("expect")
        require(isinstance(expect, dict) and "default" in expect
                and set(expect) <= set(keyboards) | {"default"}
                and set(expect.values()) <= EXPECTATIONS, "case_expect")
        refused = expect["default"] == "refused"
        require(refused == (case["comparison"] == "unchanged"), "case_refusal_rule")
        if refused:
            require(set(expect.values()) == {"refused"} and segments is None, "case_refusal_rule")
        elif segments is None:
            require(case["expected_readback"] == text, "case_expected")
        else:
            require(case["expected_readback"] == "\n".join(segments), "case_expected")
        require(type(case.get("rerun_on_branch")) is bool, "case_rerun")
        require(case.get("timing", False) in (True, False), "case_timing")
    return spec


def _scalars_valid(text):
    try:
        text.encode("utf-8")
    except UnicodeError:
        return False
    return True


def select_cases(spec, keyboard, build, only=None, include_optional=()):
    """Cases for one keyboard and build, in spec order; optional ones are opt-in."""
    require(keyboard in spec["keyboards"], "unknown_keyboard")
    require(build in BUILDS, "unknown_build")
    known = {case["id"] for case in spec["cases"]}
    require(only is None or (only and set(only) <= known and len(set(only)) == len(only)),
            "unknown_case")
    selected = []
    for case in spec["cases"]:
        if keyboard not in case["keyboards"] or build not in case["builds"]:
            require(only is None or case["id"] not in only, "case_not_runnable_here")
            continue
        if only is not None and case["id"] not in only:
            continue
        if case.get("optional") and case["optional"] not in include_optional:
            require(only is None or case["id"] not in only, "optional_case_needs_flag")
            continue
        selected.append(case)
    require(bool(selected), "no_cases_selected")
    return selected


def expectation(case, keyboard):
    return case["expect"].get(keyboard, case["expect"]["default"])


def compare(expected, typed):
    """`pass` only for scalar-for-scalar equality; equivalence is reported apart."""
    if typed == expected:
        return "pass"
    if unicodedata.normalize("NFC", typed) == unicodedata.normalize("NFC", expected):
        return "nfc_equal"
    return "mismatch"


def difference(expected, typed):
    """Sizes and the first differing scalar; the matrix text is synthetic."""
    index = next((i for i, (a, b) in enumerate(zip(expected, typed)) if a != b),
                 min(len(expected), len(typed)))
    def scalar(text):
        return f"U+{ord(text[index]):04X}" if index < len(text) else None
    return {"first_difference": index, "expected_scalar": scalar(expected),
            "actual_scalar": scalar(typed), "actual_scalars": len(typed)}


def sizes(text):
    return {"scalars": len(text), "utf16_units": len(text.encode("utf-16-le")) // 2,
            "utf8_bytes": len(text.encode())}


def editor_value(screen):
    """The single visible text view's value; a missing value is an empty editor."""
    state = screen["metadata"]["state"]
    require(state.get("lock") == "unlocked", "not_unlocked")
    require(state.get("foreground_app") == NOTES, "not_notes")
    found = elements(screen, {"type": "TextView"})
    require(len(found) == 1, "editor_not_unique")
    return found[0].get("value") or ""


def typed_suffix(before, after):
    return after[len(before):] if after.startswith(before) else None


class RpcRefused(Exception):
    """The server rejected the request arguments; nothing reached the device."""


class MatrixPeer(Peer):
    def tool(self, name, arguments):
        try:
            return super().tool(name, arguments)
        except BenchError as error:
            if (error.args != ("rpc_error",)
                    or self.calls[-1].get("rpc_error_code") != -32602):
                raise
            # A JSON-RPC argument rejection leaves the stream usable.
            self.healthy = True
            raise RpcRefused() from None


class Matrix:
    def __init__(self, peer, cases, keyboard, selection="current", clock=time.monotonic):
        require(selection in ("current", "strict"), "invalid_selection")
        self.peer = peer
        self.cases = cases
        self.keyboard = keyboard
        self.selection = selection
        self.clock = clock
        self.lease_started = None
        self.screen = None
        self.device = None
        self.rows = []

    def renew(self):
        if self.lease_started is not None:
            if self.clock() - self.lease_started < 45:
                return
            self.release()
        result, _ = self.peer.tool("handkite_acquire_control", {})
        require(result.get("isError") is not True, "acquire_failed")
        self.lease_started = self.clock()

    def release(self):
        if self.lease_started is not None:
            self.lease_started = None
            result, _ = self.peer.tool("handkite_release_control", {})
            require(result.get("isError") is not True, "release_failed")

    def observe(self):
        result, _ = self.peer.tool("handkite_observe", {})
        self.accept(parse_screen(result))
        return self.screen

    def accept(self, screen):
        require(screen["capture_kind"] == "paired" and screen["tree"] is not None, "full_tree_required")
        device = screen["metadata"]["device"]
        if self.device is None:
            self.device = device
        require(device == self.device, "device_changed")
        self.screen = screen

    def act(self, tool, arguments):
        """One dispatch. Returns (outcome, call); never sends the request twice."""
        self.renew()
        arguments = dict(arguments, expected_revision=self.screen["metadata"]["revision"])
        if self.selection == "current":
            arguments["selection"] = "current"
        try:
            result, call = self.peer.tool(tool, arguments)
        except RpcRefused:
            return "refused", self.peer.calls[-1]
        except BenchError:
            # The request may have reached the device. Diagnose once below,
            # then stop; a transport error is never an argument refusal.
            return "uncertain", self.peer.calls[-1]
        if result.get("isError") is True:
            code = call.get("error_code")
            return ("stop_pre_input" if code in PRE_INPUT else "uncertain"), call
        try:
            self.accept(parse_screen(result))
        except BenchError:
            return "uncertain", call
        return "sent", call

    def run_case(self, case):
        row = {"id": case["id"], "class": case["class"],
               "expected": expectation(case, self.keyboard)}
        parts = case.get("segments") or [case["input"]]
        row["sizes"] = sizes("\n".join(parts))
        before = editor_value(self.observe())
        require(bool(elements(self.screen, {"type": "Keyboard"})), "keyboard_not_visible")
        row["baseline_scalars"] = len(before)
        outcome = "sent"
        request_ms = []
        for index, part in enumerate(parts):
            if index:
                outcome, call = self.act("handkite_key", {"key": "enter"})
                if outcome != "sent":
                    break
            outcome, call = self.act("handkite_text", {"value": part})
            elapsed = call.get("elapsed_ms")
            request_ms.append(round(elapsed, 3) if isinstance(elapsed, float) else None)
            if "error_code" in call:
                row["error_code"] = call["error_code"]
            if outcome != "sent":
                break
        row["request_ms"] = request_ms
        if outcome == "sent":
            typed = typed_suffix(before, editor_value(self.screen))
            if typed is None:
                outcome = "mismatch"
                row["detail"] = "baseline_changed"
            else:
                outcome = compare(case["expected_readback"], typed)
                if outcome != "pass":
                    row["detail"] = difference(case["expected_readback"], typed)
        else:
            # One read-only observation to learn whether anything was typed.
            try:
                after = editor_value(self.observe())
                row["editor_changed"] = after != before
                if outcome in ("refused", "stop_pre_input") and after != before:
                    outcome = "uncertain"
            except BenchError as error:
                row["diagnosis"] = error.args[0]
                if outcome == "refused":
                    outcome = "uncertain"
        row["outcome"] = outcome
        row["as_expected"] = as_expected(row["expected"], outcome)
        return row

    def separate(self):
        """Start the next case on a new line instead of deleting anything."""
        if not editor_value(self.screen):
            return True
        outcome, _ = self.act("handkite_key", {"key": "enter"})
        return outcome == "sent"

    def run(self):
        self.peer.initialize()
        stopped = None
        try:
            require(editor_value(self.observe()) == "", "scratch_note_not_empty")
            require(bool(elements(self.screen, {"type": "Keyboard"})), "keyboard_not_visible")
            for case in self.cases:
                if stopped:
                    self.rows.append({"id": case["id"], "class": case["class"],
                                      "outcome": "not_run", "reason": stopped})
                    continue
                try:
                    row = self.run_case(case)
                except BenchError as error:
                    row = {"id": case["id"], "class": case["class"], "outcome": "uncertain",
                           "detail": error.args[0], "as_expected": False}
                self.rows.append(row)
                if row["outcome"] == "uncertain":
                    stopped = "after_uncertain_input"
                elif row["outcome"] in ("pass", "nfc_equal", "mismatch"):
                    try:
                        if not self.separate():
                            stopped = "separator_failed"
                    except BenchError:
                        stopped = "separator_failed"
        finally:
            try:
                self.release()
            except (BenchError, RpcRefused):
                pass
        return self.rows


def as_expected(expected, outcome):
    if expected == "unknown":
        return None
    if expected == "pass":
        return outcome == "pass"
    if expected == "fail":
        return outcome in ("mismatch", "nfc_equal")
    return outcome == expected


def typing_rate(rows):
    """Least-squares fit of request time against UTF-16 units over passing timing rows."""
    points = [(row["sizes"]["utf16_units"], row["request_ms"][0]) for row in rows
              if row.get("class") == "timing" and row.get("outcome") == "pass"
              and len(row.get("request_ms", [])) == 1
              and isinstance(row["request_ms"][0], float) and math.isfinite(row["request_ms"][0])]
    if len({units for units, _ in points}) < 2:
        return None
    n = len(points)
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    spread = sum((x - mean_x) ** 2 for x, _ in points)
    slope = sum((x - mean_x) * (y - mean_y) for x, y in points) / spread
    return {"samples": n, "ms_per_utf16_unit": round(slope, 3),
            "fixed_ms_per_action": round(mean_y - slope * mean_x, 3),
            "units_per_second": round(1000 / slope, 1) if slope > 0 else None}


def report(rows, keyboard, label, build, selection, frequency):
    counts = {}
    for row in rows:
        counts[row["outcome"]] = counts.get(row["outcome"], 0) + 1
    return {"version": 1, "keyboard": keyboard, "keyboard_label": label, "build": build,
            "selection": selection, "typing_frequency": frequency, "outcomes": counts,
            "unexpected": [row["id"] for row in rows if row.get("as_expected") is False],
            "typing_rate": typing_rate(rows), "cases": rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="mode", required=True)
    listing = commands.add_parser("list", help="print the cases for one keyboard and build")
    run = commands.add_parser("run", help="run the cases once against a live service")
    for sub in (listing, run):
        sub.add_argument("--spec", type=Path, default=SPEC)
        sub.add_argument("--keyboard", required=True)
        sub.add_argument("--build", choices=BUILDS, required=True)
        sub.add_argument("--cases", nargs="+")
        sub.add_argument("--include-deadline-case", action="store_true")
    run.add_argument("--keyboard-label", default="")
    run.add_argument("--selection", choices=("current", "strict"), default="current")
    run.add_argument("--typing-frequency", type=int,
                     help="record the service's HANDKITE_TYPING_FREQUENCY; does not set it")
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    try:
        spec = load_spec(args.spec)
        optional = ("--include-deadline-case",) if args.include_deadline_case else ()
        cases = select_cases(spec, args.keyboard, args.build, args.cases, optional)
        if args.mode == "list":
            for case in cases:
                print(f"{case['id']}\t{expectation(case, args.keyboard)}")
            return 0
        command = args.command[1:] if args.command[:1] == ["--"] else args.command
        require(len(args.keyboard_label) <= 64
                and (not args.keyboard_label or args.keyboard_label.isprintable()),
                "invalid_keyboard_label")
        require(not args.output.exists(), "output_exists")
        peer = MatrixPeer(command)
        try:
            rows = Matrix(peer, cases, args.keyboard, args.selection).run()
        finally:
            peer.close()
        result = report(rows, args.keyboard, args.keyboard_label, args.build, args.selection,
                        args.typing_frequency)
        fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w", encoding="ascii") as stream:
            json.dump(result, stream, indent=1, allow_nan=False)
            stream.write("\n")
        print(json.dumps({"outcomes": result["outcomes"], "unexpected": result["unexpected"],
                          "typing_rate": result["typing_rate"]}))
        return 0
    except BenchError as error:
        print(f"text_matrix={error.args[0]}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
