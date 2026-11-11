"""Regression checks for staged and working source content."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_source_text.py")


class SourceTextTests(unittest.TestCase):
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
