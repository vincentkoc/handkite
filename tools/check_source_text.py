#!/usr/bin/env python3
"""Check tracked source and documentation for Han characters."""

from pathlib import Path
import subprocess
import unicodedata


TEXT_SUFFIXES = {
    ".css", ".html", ".js", ".json", ".lock", ".md", ".py", ".rs",
    ".sh", ".svg", ".toml", ".ts", ".txt", ".yaml", ".yml",
}
TEXT_NAMES = {".gitignore", "Caddyfile"}


def has_han(value: str) -> bool:
    return any(
        unicodedata.name(char, "").startswith(
            ("CJK UNIFIED IDEOGRAPH", "CJK COMPATIBILITY IDEOGRAPH")
        )
        for char in value
    )


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    result = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--stage", "-z"],
        check=True,
        capture_output=True,
    )
    checked = 0
    violations = []
    for entry in result.stdout.split(b"\0"):
        if not entry:
            continue
        metadata, name = entry.split(b"\t", 1)
        mode, object_id, stage = metadata.split(b" ")
        relative = Path(name.decode("utf-8"))
        if stage != b"0" or has_han(relative.as_posix()):
            violations.append(relative)
            continue
        if relative.suffix not in TEXT_SUFFIXES and relative.name not in TEXT_NAMES:
            continue
        path = root / relative
        if mode not in (b"100644", b"100755") or path.is_symlink():
            violations.append(relative)
            continue
        checked += 1
        try:
            staged = subprocess.run(
                ["git", "-C", str(root), "cat-file", "blob", object_id.decode("ascii")],
                check=True,
                capture_output=True,
            ).stdout.decode("utf-8")
            content = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError, subprocess.CalledProcessError):
            violations.append(relative)
            continue
        if has_han(staged) or has_han(content):
            violations.append(relative)
    for path in violations:
        print(f"source_han=found path={path}")
    if violations:
        return 1
    print(f"source_han=none checked={checked}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
