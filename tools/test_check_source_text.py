"""Regression checks for staged and working source content."""

from pathlib import Path
import json
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_source_text.py")


class SourceTextTests(unittest.TestCase):
    def fixture_scan(self, value, *, path="tools/benchmarks/notes-typing.json", raw=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "tools").mkdir()
            shutil.copyfile(SCRIPT, root / "tools" / SCRIPT.name)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            fixture = root / path
            fixture.parent.mkdir(parents=True, exist_ok=True)
            fixture.write_text(value if raw else json.dumps(value, ensure_ascii=False))
            subprocess.run(["git", "-C", str(root), "add", path], check=True)
            return self.scan(root)

    def test_cjk_expectations_are_fixture_data(self):
        expectation = {"expect": {"text": {"value": chr(0x6C49)}}}
        fixture = {"cases": [{"prepare": [expectation], "measure": expectation,
                              "cleanup": [expectation]}]}
        for path in ("tools/benchmarks/notes-typing.json", "tools/benchmarks/notes-scroll.json"):
            with self.subTest(path=path):
                self.assertEqual(self.fixture_scan(fixture, path=path).returncode, 0)
        self.assertEqual(self.fixture_scan(fixture, path="other.json").returncode, 1)

    def test_fixture_metadata_and_keys_still_require_english(self):
        han = chr(0x6C49)
        for fixture in ({"description": han}, {han: "text"},
                        {"cases": [{"measure": {"expect": {"text": {"label": han}}}}]},
                        {"cases": [{"measure": {"arguments": {"text": han}}}]}):
            with self.subTest(fixture=fixture):
                self.assertEqual(self.fixture_scan(fixture).returncode, 1)

    def test_invalid_or_duplicate_key_fixtures_fail(self):
        for value in ('{', '{"description":"\\u6c49","description":"English"}',
                      '{"version":NaN}', '{"version":Infinity}', '{"version":-Infinity}'):
            with self.subTest(value=value):
                self.assertEqual(self.fixture_scan(value, raw=True).returncode, 1)

    def test_fixture_staged_and_working_metadata_are_both_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "tools" / "benchmarks").mkdir(parents=True)
            shutil.copyfile(SCRIPT, root / "tools" / SCRIPT.name)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            fixture = root / "tools/benchmarks/notes-typing.json"
            for staged, working in (("English", chr(0x6C49)), (chr(0x6C49), "English")):
                fixture.write_text(json.dumps({"description": staged}, ensure_ascii=False))
                subprocess.run(["git", "-C", str(root), "add", str(fixture)], check=True)
                fixture.write_text(json.dumps({"description": working}, ensure_ascii=False))
                self.assertEqual(self.scan(root).returncode, 1)

    def test_staged_and_working_content_are_both_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "tools").mkdir()
            shutil.copyfile(SCRIPT, root / "tools" / SCRIPT.name)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            source = root / "sample.md"
            source.write_text("English text\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", "sample.md"], check=True)

            source.write_text(chr(0x6C49) + "\n", encoding="utf-8")
            self.assertEqual(self.scan(root).returncode, 1)

            subprocess.run(["git", "-C", str(root), "add", "sample.md"], check=True)
            source.write_text("English text\n", encoding="utf-8")
            self.assertEqual(self.scan(root).returncode, 1)

            subprocess.run(["git", "-C", str(root), "add", "sample.md"], check=True)
            result = self.scan(root)
            self.assertEqual(result.returncode, 0)
            self.assertIn("source_han=none checked=1", result.stdout)

    @staticmethod
    def scan(root: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["python3", str(root / "tools" / SCRIPT.name)],
            check=False,
            capture_output=True,
            text=True,
        )


if __name__ == "__main__":
    unittest.main()
