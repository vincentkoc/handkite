"""Checks for the text matrix spec, its loader and the single-dispatch runner."""

import base64
import json
import re
import tempfile
import unittest
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
                 lock="unlocked", initial="", keyboard=True, refuse=None):
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
        value = f" value={quoteattr(self.value)}" if self.value else ""
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
            return self.screen(), call
        assert self.controlled and arguments["expected_revision"] == self.revision
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
        return self.screen(), call


def text_matrix_refusal(text):
    return (len(text.encode()) > 4096 or any(unicodedata.category(c) == "Cc" for c in text)
            or any(c in "\u2028\u2029" or "\uf700" <= c <= "\uf8ff" for c in text))


def run(peer, keyboard="english", build="branch", only=None, optional=()):
    spec = text_matrix.load_spec()
    cases = text_matrix.select_cases(spec, keyboard, build, only, optional)
    rows = text_matrix.Matrix(peer, cases, keyboard).run()
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
                         "time_mixed_500"):
            self.assertIn(required, by_id)
        self.assertEqual(len(by_id["time_ascii_500"]["input"]), 500)
        self.assertGreaterEqual(len(by_id["time_mixed_500"]["input"]), 500)

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
        self.assertIs(rows["ascii_rewrite_bait"]["as_expected"], True)
        self.assertEqual(rows["simplified_chinese"]["outcome"], "pass")
        composed = FakeEditor(rewrite=lambda text: unicodedata.normalize("NFC", text))
        rows = run(composed, only=["japanese_dakuten_decomposed"])
        self.assertEqual(rows["japanese_dakuten_decomposed"]["outcome"], "nfc_equal")
        self.assertEqual(sum(text != "\n" for text in composed.sent), 1)

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


if __name__ == "__main__":
    unittest.main()
