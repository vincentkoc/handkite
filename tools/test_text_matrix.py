"""Checks for the text matrix spec, its loader and the single-dispatch runner."""

import base64
import json
import re
import tempfile
import unittest
from unittest.mock import patch
import unicodedata
from pathlib import Path
from xml.sax.saxutils import quoteattr

import text_matrix
from benchmark_mcp import BenchError

ROOT = Path(__file__).resolve().parent.parent
RUST_MATRIX = ROOT / "crates" / "handkite-host" / "src" / "text_matrix.rs"
IMAGE = base64.b64encode(b"\x89PNG\r\n\x1a\n" + b"fixture").decode()


def rust_matrix():
    """The Rust unit-test matrix, decoded from its ASCII escapes."""
    pairs = re.findall(r'\(\s*"([a-z0-9_]+)",\s*"((?:[^"\\]|\\.)*)",?\s*\)',
                       RUST_MATRIX.read_text(encoding="ascii"))
    def decode(text):
        text = re.sub(r"\\u\{([0-9a-f]+)\}", lambda match: chr(int(match.group(1), 16)), text)
        return text.replace('\\"', '"').replace("\\\\", "\\")
    return {name: decode(text) for name, text in pairs}


class FakeEditor:
    """An in-process MCP peer with one text view, optionally rewriting input."""

    def __init__(self, rewrite=lambda text: text, errors=None, app=text_matrix.NOTES,
                 lock="unlocked", initial="", keyboard=True, refuse=None, animate=True):
        self.animate = animate
        self.settling = False
        self.value = initial
        self.rewrite = rewrite
        self.errors = errors or {}
        self.refuse = refuse or text_matrix_refusal
        self.app = app
        self.lock = lock
        self.keyboard = keyboard
        self.revision = 0
        self.calls = []
        self.sent = []
        self.controlled = False

    def initialize(self):
        pass

    def screen(self):
        keyboard = "<XCUIElementTypeKeyboard/>" if self.keyboard else ""
        # The page source cuts every value at 512 UTF-16 units.
        shown = self.value.encode("utf-16-le")[:2 * text_matrix.TREE_VALUE_CAP].decode(
            "utf-16-le", errors="ignore")
        value = f" value={quoteattr(shown)}" if shown else ""
        value = value.replace("\n", "&#10;")
        tree = (f"<XCUIElementTypeApplication><XCUIElementTypeTextView{value}/>"
                f"{keyboard}</XCUIElementTypeApplication>")
        metadata = {"device": "fixture", "revision": self.revision,
                    "state": {"wda_ready": True, "lock": self.lock, "foreground_app": self.app},
                    "window": {"started_unix_ms": 1, "finished_unix_ms": 2}}
        body = {"metadata": metadata, "accessibility_tree": tree}
        return {"content": [{"type": "text", "text": json.dumps(body)},
                            {"type": "image", "mimeType": "image/png", "data": IMAGE}]}

    def tool(self, name, arguments):
        call = {"tool": name, "elapsed_ms": 10.0}
        self.calls.append(call)
        if name == "handkite_acquire_control":
            self.controlled = True
            return {}, call
        if name == "handkite_release_control":
            self.controlled = False
            return {}, call
        if name == "handkite_observe":
            if self.settling:
                # Keyboard animation finished after the action's own capture.
                self.settling = False
                self.revision += 1
            return self.screen(), call
        assert self.controlled
        image_only = arguments.pop("response", None) == text_matrix.IMAGE_ONLY
        if self.settling or arguments["expected_revision"] != self.revision:
            call["error_code"] = "ViewChanged"
            return {"isError": True, "structuredContent": {"code": "ViewChanged"}}, call
        if name == "handkite_key":
            assert arguments == {"key": "enter", "expected_revision": self.revision,
                                 "selection": "current"}
            self.sent.append("\n")
            self.value += "\n"
        else:
            text = arguments["value"]
            if self.refuse(text):
                raise text_matrix.RpcRefused()
            self.sent.append(text)
            code = self.errors.get(text)
            if code:
                call["error_code"] = code
                if code == "AdapterInputAfterSend":
                    self.value += text[:1]
                return {"isError": True, "structuredContent": {"code": code}}, call
            call["elapsed_ms"] = 100.0 + 5.0 * len(text.encode("utf-16-le")) / 2
            self.value += self.rewrite(text)
        self.revision += 1
        self.settling = self.animate
        result = self.screen()
        if image_only:
            call["image_only"] = True
            window = {"started_unix_ms": 1, "finished_unix_ms": 2}
            result["content"][0]["text"] = json.dumps({
                "metadata": {"device": "fixture", "window": window},
                "capture_kind": "image_only",
                "tree": {"included": False, "reason": "not_captured"}})
        result["content"].insert(0, {"type": "text", "text": json.dumps({
            "receipt": {"accepted": True}, "effect": "changed"})})
        return result, call


def text_matrix_refusal(text):
    return (len(text.encode()) > 4096 or any(unicodedata.category(c) == "Cc" for c in text)
            or any(c in "\u2028\u2029" or "\uf700" <= c <= "\uf8ff" for c in text))


def run(peer, keyboard="english", build="branch", only=None, optional=()):
    spec = text_matrix.load_spec()
    cases = text_matrix.select_cases(spec, keyboard, build, only, optional)
    rows = []
    for number, note in enumerate(text_matrix.batches(cases)):
        if number:
            # Each later batch is one operator run in a new empty note.
            peer.value = ""
            peer.settling = False
        rows += text_matrix.Matrix(peer, note, keyboard).run()
    return {row["id"]: row for row in rows}


class SpecTests(unittest.TestCase):
    def test_spec_is_ascii_valid_and_matches_the_rust_matrix(self):
        spec = text_matrix.load_spec()
        by_id = {case["id"]: case for case in spec["cases"]}
        matrix = rust_matrix()
        self.assertEqual(len(matrix), 28)
        for name, text in matrix.items():
            self.assertEqual(by_id[name]["input"], text, name)
            self.assertEqual(by_id[name]["expected_readback"], text, name)
        for required in ("simplified_chinese", "traditional_chinese", "korean_syllables",
                         "arabic", "hebrew", "thai", "emoji_zwj_family", "emoji_flags",
                         "multiline_enter_key", "time_ascii_20", "time_ascii_500",
                         "time_mixed_units_500"):
            self.assertIn(required, by_id)
        self.assertEqual(len(by_id["time_ascii_500"]["input"]), 500)
        mixed = text_matrix.units(by_id["time_mixed_units_500"]["input"])
        self.assertTrue(480 <= mixed < text_matrix.NOTE_BUDGET, mixed)

    def test_key_like_refusals_never_run_on_the_installed_build(self):
        spec = text_matrix.load_spec()
        installed = {case["id"] for case in
                     text_matrix.select_cases(spec, "english", "installed")}
        branch = {case["id"] for case in text_matrix.select_cases(spec, "english", "branch")}
        self.assertEqual(branch - installed, {"refuse_line_separator", "refuse_function_key"})
        self.assertLessEqual({"refuse_newline", "refuse_tab", "refuse_over_limit"}, installed)
        for build in text_matrix.BUILDS:
            self.assertNotIn("deadline_ascii_2000", {case["id"] for case in
                             text_matrix.select_cases(spec, "english", build)})
        with self.assertRaises(BenchError):
            text_matrix.select_cases(spec, "english", "installed", ["refuse_function_key"])
        with self.assertRaises(BenchError):
            text_matrix.select_cases(spec, "english", "branch", ["deadline_ascii_2000"])
        with self.assertRaises(BenchError):
            text_matrix.select_cases(spec, "pinyin", "branch", ["time_ascii_20"])
        with self.assertRaises(BenchError):
            text_matrix.select_cases(spec, "klingon", "branch")

    def test_long_cases_can_run_under_pinyin_and_kana_in_separate_notes(self):
        spec = text_matrix.load_spec()
        ids = ["time_ascii_500", "time_mixed_units_500"]
        for keyboard in ("pinyin", "japanese_kana"):
            cases = text_matrix.select_cases(spec, keyboard, "branch", ids)
            self.assertEqual([case["id"] for case in cases], ids)
            self.assertEqual(len(text_matrix.batches(cases)), 2)

    def test_loader_rejects_malformed_specs(self):
        good = json.loads(text_matrix.SPEC.read_text(encoding="ascii"))
        def broken(change):
            spec = json.loads(json.dumps(good))
            change(spec)
            return json.dumps(spec)
        def set_case(key, value):
            return lambda spec: spec["cases"][0].__setitem__(key, value)
        documents = [
            "{", "[]", '{"version":1,"version":1}', json.dumps(good, ensure_ascii=False),
            broken(lambda spec: spec.__setitem__("version", 2)),
            broken(lambda spec: spec.__setitem__("cases", [])),
            broken(lambda spec: spec["cases"].append(spec["cases"][0])),
            broken(set_case("id", "../x")),
            broken(set_case("input", "")),
            broken(set_case("expected_readback", "different")),
            broken(set_case("comparison", "nfc")),
            broken(set_case("keyboards", ["klingon"])),
            broken(set_case("builds", ["other"])),
            broken(set_case("expect", {"default": "maybe"})),
            broken(set_case("expect", {"english": "pass"})),
            broken(set_case("rerun_on_branch", "yes")),
            broken(set_case("device", "x")),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "spec.json"
            for document in documents:
                path.write_text(document, encoding="utf-8")
                with self.assertRaises(BenchError, msg=document[:60]):
                    text_matrix.load_spec(path)
            with self.assertRaises(BenchError):
                text_matrix.load_spec(Path(directory) / "missing.json")


class CompareTests(unittest.TestCase):
    def test_only_scalar_equality_passes(self):
        self.assertEqual(text_matrix.compare("\u304c", "\u304c"), "pass")
        self.assertEqual(text_matrix.compare("\u304b\u3099", "\u304c"), "nfc_equal")
        self.assertEqual(text_matrix.compare('"a"', "\u201ca\u201d"), "mismatch")
        self.assertEqual(text_matrix.typed_suffix("ab\n", "ab\ncd"), "cd")
        self.assertIsNone(text_matrix.typed_suffix("ab", "xbcd"))
        self.assertEqual(text_matrix.sizes("a\U0001f44b"),
                         {"scalars": 2, "utf16_units": 3, "utf8_bytes": 5})
        detail = text_matrix.difference("ab", "aX")
        self.assertEqual((detail["first_difference"], detail["expected_scalar"],
                          detail["actual_scalar"]), (1, "U+0062", "U+0058"))


class RunnerTests(unittest.TestCase):
    def test_unknown_receipt_stops_even_with_a_readable_observation(self):
        class UnknownReceipt(FakeEditor):
            def tool(self, name, arguments):
                result, call = super().tool(name, arguments)
                if name == "handkite_text":
                    result["content"][0]["text"] = json.dumps({
                        "receipt": {"accepted": None}, "effect": "unconfirmed"})
                return result, call
        peer = UnknownReceipt()
        rows = run(peer, only=["ascii_sentence", "simplified_chinese"])
        self.assertEqual(rows["ascii_sentence"]["outcome"], "uncertain")
        self.assertIsNone(rows["ascii_sentence"]["accepted"])
        self.assertEqual(rows["simplified_chinese"]["outcome"], "not_run")
        self.assertNotIn("\n", peer.sent)

    def test_only_invalid_arguments_are_classified_as_rpc_refusal(self):
        peer = object.__new__(text_matrix.MatrixPeer)
        for code, expected in [(-32602, text_matrix.RpcRefused),
                               (-32603, BenchError), (None, BenchError)]:
            peer.calls = [{"rpc_error_code": code}]
            peer.healthy = False
            with patch.object(text_matrix.Peer, "tool", side_effect=BenchError("rpc_error")):
                with self.assertRaises(expected):
                    peer.tool("handkite_text", {})
            self.assertEqual(peer.healthy, code == -32602)

    def test_default_keyboard_label_is_accepted_before_transport_start(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            argv = ["text_matrix.py", "run", "--keyboard", "english", "--build", "installed",
                    "--note", "1", "--output", str(output), "--", "unused"]
            with patch("sys.argv", argv), patch.object(text_matrix, "MatrixPeer",
                                                       side_effect=BenchError("transport_probe")):
                with patch("builtins.print") as printed:
                    self.assertEqual(text_matrix.main(), 1)
                printed.assert_called_once_with("text_matrix=transport_probe")

    def test_transport_failure_observes_once_and_never_sends_next_case(self):
        class BrokenTransport(FakeEditor):
            def tool(self, name, arguments):
                result = super().tool(name, arguments)
                if name == "handkite_text":
                    raise BenchError("transport_eof")
                return result
        peer = BrokenTransport()
        rows = run(peer, only=["ascii_sentence", "simplified_chinese"])
        self.assertEqual(rows["ascii_sentence"]["outcome"], "uncertain")
        self.assertTrue(rows["ascii_sentence"]["editor_changed"])
        self.assertEqual(rows["simplified_chinese"]["outcome"], "not_run")
        self.assertEqual([call["tool"] for call in peer.calls].count("handkite_observe"), 3)

    def test_keyboard_is_required_before_each_case(self):
        class HiddenKeyboard(FakeEditor):
            def tool(self, name, arguments):
                result = super().tool(name, arguments)
                if name == "handkite_key":
                    self.keyboard = False
                return result
        peer = HiddenKeyboard()
        rows = run(peer, only=["ascii_sentence", "simplified_chinese"])
        self.assertEqual(rows["ascii_sentence"]["outcome"], "pass")
        self.assertEqual(rows["simplified_chinese"]["detail"], "keyboard_not_visible")
        self.assertEqual(len([text for text in peer.sent if text != "\n"]), 1)

    def test_separator_failure_preserves_stage_and_stops_without_replay(self):
        class FailedSeparator(FakeEditor):
            def tool(self, name, arguments):
                if name == "handkite_key":
                    call = {"tool": name, "elapsed_ms": 5000.0,
                            "error_code": "AdapterNativeBefore"}
                    self.calls.append(call)
                    return {"isError": True}, call
                return super().tool(name, arguments)
        peer = FailedSeparator()
        rows = run(peer, only=["ascii_sentence", "simplified_chinese"])
        self.assertEqual(rows["ascii_sentence"]["outcome"], "pass")
        separator = rows["ascii_sentence"]["separator"]
        self.assertEqual(separator["outcome"], "stop_pre_input")
        self.assertEqual(separator["error_code"], "AdapterNativeBefore")
        self.assertFalse(separator["editor_changed"])
        self.assertEqual(rows["simplified_chinese"]["outcome"], "not_run")
        self.assertEqual(sum(c["tool"] == "handkite_key" for c in peer.calls), 1)
        report = text_matrix.report(list(rows.values()), "english", "", "installed", "current", None)
        self.assertEqual(report["separator_stops"], {"separator_stop_pre_input": 1})

    def test_separator_refresh_refuses_changed_editor_before_input(self):
        class ChangedEditor(FakeEditor):
            def tool(self, name, arguments):
                if name == "handkite_observe" and self.sent:
                    self.value += "x"
                return super().tool(name, arguments)
        peer = ChangedEditor()
        rows = run(peer, only=["ascii_sentence", "simplified_chinese"])
        self.assertEqual(rows["ascii_sentence"]["separator"],
                         {"outcome": "stop_pre_input", "diagnosis": "separator_editor_changed"})
        self.assertFalse(any(call["tool"] == "handkite_key" for call in peer.calls))
        self.assertEqual(rows["simplified_chinese"]["outcome"], "not_run")

    def test_faithful_editor_passes_every_case_and_sends_each_text_once(self):
        peer = FakeEditor()
        rows = run(peer)
        self.assertEqual({row["outcome"] for row in rows.values()}, {"pass", "refused"})
        self.assertFalse([name for name, row in rows.items() if row["as_expected"] is False
                          and name != "ascii_rewrite_bait"])
        texts = [text for text in peer.sent if text != "\n"]
        self.assertEqual(len(texts), len(set(texts)))
        self.assertFalse(peer.controlled)
        report = text_matrix.report(list(rows.values()), "english", "", "branch", "current", None)
        self.assertEqual(report["typing_rate"]["ms_per_utf16_unit"], 5.0)
        self.assertEqual(report["typing_rate"]["fixed_ms_per_action"], 100.0)
        serialized = json.dumps(report)
        for case in text_matrix.load_spec()["cases"]:
            if len(case.get("input") or "") > 12:
                self.assertNotIn(json.dumps(case["input"])[1:-1], serialized)
        self.assertNotIn("fixture", serialized)

    def test_rewrites_are_reported_without_replay(self):
        smart = lambda text: text.replace('"', "\u201c").replace("--", "\u2014")
        peer = FakeEditor(rewrite=smart)
        rows = run(peer, only=["ascii_sentence", "ascii_rewrite_bait", "simplified_chinese"])
        self.assertEqual(rows["ascii_sentence"]["outcome"], "pass")
        self.assertEqual(rows["ascii_rewrite_bait"]["outcome"], "mismatch")
        # English passed this case live, so a rewrite there is now unexpected.
        self.assertIs(rows["ascii_rewrite_bait"]["as_expected"], False)
        self.assertEqual(rows["simplified_chinese"]["outcome"], "pass")
        self.assertNotIn("separator", rows["ascii_rewrite_bait"])
        composed = FakeEditor(rewrite=lambda text: unicodedata.normalize("NFC", text))
        rows = run(composed, only=["japanese_dakuten_decomposed"])
        self.assertEqual(rows["japanese_dakuten_decomposed"]["outcome"], "nfc_equal")
        self.assertEqual(sum(text != "\n" for text in composed.sent), 1)

    def test_single_case_never_sends_a_trailing_enter(self):
        for rewrite, expected in [(lambda text: text, "pass"),
                                  (lambda text: text[:-1], "mismatch")]:
            with self.subTest(expected=expected):
                peer = FakeEditor(rewrite=rewrite)
                rows = run(peer, only=["simplified_chinese"])
                self.assertEqual(rows["simplified_chinese"]["outcome"], expected)
                self.assertNotIn("separator", rows["simplified_chinese"])
                self.assertEqual(len(peer.sent), 1)
                self.assertNotIn("\n", peer.sent)
                self.assertFalse(peer.controlled)

    def test_mismatch_stops_the_note_before_separator_or_next_case(self):
        cases = text_matrix.select_cases(text_matrix.load_spec(), "english", "branch",
                                         ["ascii_sentence", "simplified_chinese"])
        peer = FakeEditor(rewrite=lambda text: text[:-1])
        rows = text_matrix.Matrix(peer, cases, "english").run()
        self.assertEqual([row["outcome"] for row in rows], ["mismatch", "not_run"])
        self.assertNotIn("separator", rows[0])
        self.assertEqual(len(peer.sent), 1)
        self.assertFalse(peer.controlled)

    def test_pre_input_stop_continues_and_uncertain_input_stops_the_run(self):
        spec = {case["id"]: case for case in text_matrix.load_spec()["cases"]}
        only = ["ascii_sentence", "simplified_chinese", "thai", "single_emoji"]
        peer = FakeEditor(errors={spec["simplified_chinese"]["input"]: "AdapterNativeBefore"})
        rows = run(peer, only=only)
        self.assertEqual([rows[name]["outcome"] for name in only],
                         ["pass", "stop_pre_input", "pass", "pass"])
        self.assertIs(rows["simplified_chinese"]["editor_changed"], False)
        peer = FakeEditor(errors={spec["simplified_chinese"]["input"]: "AdapterInputAfterSend"})
        rows = run(peer, only=only)
        self.assertEqual([rows[name]["outcome"] for name in only],
                         ["pass", "uncertain", "not_run", "not_run"])
        self.assertIs(rows["simplified_chinese"]["editor_changed"], True)
        self.assertEqual(peer.sent.count(spec["simplified_chinese"]["input"]), 1)
        self.assertNotIn(spec["thai"]["input"], peer.sent)

    def test_text_typed_by_a_build_without_the_rule_is_not_reported_as_refused(self):
        leaky = FakeEditor(refuse=lambda text: False)
        rows = run(leaky, build="installed", only=["refuse_tab", "ascii_sentence"])
        self.assertEqual(rows["refuse_tab"]["outcome"], "mismatch")
        self.assertIs(rows["refuse_tab"]["as_expected"], False)

    def test_refuses_to_start_outside_an_empty_unlocked_notes_editor(self):
        for peer, code in [
            (FakeEditor(initial="AlphaX\n"), "scratch_note_not_empty"),
            (FakeEditor(lock="locked"), "not_unlocked"),
            (FakeEditor(app="com.apple.Preferences"), "not_notes"),
            (FakeEditor(keyboard=False), "keyboard_not_visible"),
        ]:
            with self.assertRaises(BenchError) as raised:
                run(peer, only=["ascii_sentence"])
            self.assertEqual(raised.exception.args, (code,))
            self.assertEqual(peer.sent, [])
            self.assertFalse(peer.controlled)


class CapAndSequenceTests(unittest.TestCase):
    def spec(self):
        return {case["id"]: case for case in text_matrix.load_spec()["cases"]}

    def test_notes_keep_every_value_under_the_tree_cap(self):
        spec = text_matrix.load_spec()
        for keyboard in spec["keyboards"]:
            cases = text_matrix.select_cases(spec, keyboard, "branch")
            notes = text_matrix.batches(cases)
            self.assertEqual([case["id"] for note in notes for case in note],
                             [case["id"] for case in cases])
            for note in notes:
                typed = [case for case in note if case["comparison"] == "exact"]
                total = sum(text_matrix.units(case["expected_readback"]) + 1 for case in typed)
                self.assertTrue(total <= text_matrix.NOTE_BUDGET or len(typed) == 1, keyboard)
        english = text_matrix.batches(text_matrix.select_cases(spec, "english", "branch"))
        alone = [note for note in english if note[0]["id"] == "time_ascii_500"]
        self.assertEqual([case["comparison"] for case in alone[0][1:]], [])
        with_deadline = text_matrix.batches(text_matrix.select_cases(
            spec, "english", "branch", None, ("--include-deadline-case",)))
        self.assertEqual([case["id"] for case in with_deadline[-1]], ["deadline_ascii_2000"])

    def test_multiline_observes_before_each_input_and_passes_with_view_changes(self):
        peer = FakeEditor()
        rows = run(peer, only=["ascii_sentence", "multiline_enter_key", "thai"])
        self.assertEqual([row["outcome"] for row in rows.values()], ["pass"] * 3)
        self.assertEqual(rows["multiline_enter_key"]["inputs_sent"], 5)
        self.assertFalse([call for call in peer.calls if call.get("error_code")])

    def test_stop_inside_a_multiline_case_is_partial_and_never_resumed(self):
        second = self.spec()["multiline_enter_key"]["segments"][1]
        peer = FakeEditor(errors={second: "ViewChanged"})
        rows = run(peer, only=["multiline_enter_key", "time_ascii_1"])
        row = rows["multiline_enter_key"]
        self.assertEqual((row["outcome"], row["inputs_sent"], row["as_expected"]),
                         ("partial", 2, False))
        self.assertEqual(rows["time_ascii_1"]["outcome"], "not_run")
        self.assertEqual(peer.sent.count(second), 1)
        self.assertEqual(peer.sent.count(self.spec()["multiline_enter_key"]["segments"][0]), 1)

    def test_long_cases_verify_fully_in_their_own_note(self):
        peer = FakeEditor()
        rows = run(peer, only=["time_ascii_20", "time_ascii_500", "time_mixed_units_500"])
        self.assertEqual([row["outcome"] for row in rows.values()], ["pass"] * 3)
        dropped = FakeEditor(rewrite=lambda text: text[:-3] if len(text) > 100 else text)
        rows = run(dropped, only=["time_ascii_500"])
        self.assertEqual(rows["time_ascii_500"]["outcome"], "mismatch")
        self.assertEqual(rows["time_ascii_500"]["detail"]["actual_scalars"], 497)

    def test_value_past_the_tree_cap_proves_a_prefix_only(self):
        peer = FakeEditor()
        rows = run(peer, only=["deadline_ascii_2000"], optional=("--include-deadline-case",))
        row = rows["deadline_ascii_2000"]
        self.assertEqual(row["outcome"], "tree_capped")
        self.assertEqual(row["detail"]["actual_scalars"], text_matrix.TREE_VALUE_CAP)
        self.assertEqual(row["detail"]["expected_utf16_units"], 2000)
        self.assertIs(row["detail"]["direct_read_required"], True)
        self.assertNotIn("separator", row)
        wrong = FakeEditor(rewrite=lambda text: "X" + text[1:])
        rows = run(wrong, only=["deadline_ascii_2000"], optional=("--include-deadline-case",))
        self.assertEqual(rows["deadline_ascii_2000"]["outcome"], "mismatch")

    def test_timing_cases_use_image_only_capture_and_a_separate_read_back(self):
        peer = FakeEditor()
        rows = run(peer, only=["ascii_sentence", "time_ascii_20"])
        self.assertEqual(rows["time_ascii_20"]["capture"], "image_only")
        self.assertIn("verify_ms", rows["time_ascii_20"])
        self.assertEqual(rows["ascii_sentence"]["capture"], "paired")
        self.assertNotIn("verify_ms", rows["ascii_sentence"])
        self.assertEqual(sum(bool(call.get("image_only")) for call in peer.calls), 1)
        spec = text_matrix.load_spec()
        cases = text_matrix.select_cases(spec, "english", "branch", ["time_ascii_20"])
        paired = FakeEditor()
        rows = text_matrix.Matrix(paired, cases, "english", timing_capture="paired").run()
        self.assertEqual((rows[0]["outcome"], rows[0]["capture"]), ("pass", "paired"))
        self.assertFalse(any(call.get("image_only") for call in paired.calls))


if __name__ == "__main__":
    unittest.main()
