#!/usr/bin/env python3
"""Check tracked source and documentation for Han characters."""

from pathlib import Path
import subprocess
import unicodedata


TEXT_SUFFIXES = {".rs", ".md", ".py", ".toml", ".yaml", ".lock"}
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
        ["git", "-C", str(root), "ls-files", "-z"],
        check=True,
        capture_output=True,
    )
    checked = 0
    violations = []
    for name in result.stdout.split(b"\0"):
        if not name:
            continue
        relative = Path(name.decode("utf-8"))
        if relative.suffix not in TEXT_SUFFIXES and relative.name not in TEXT_NAMES:
            continue
        path = root / relative
        if path.is_symlink():
            violations.append(relative)
            continue
        checked += 1
        if has_han(path.read_text(encoding="utf-8")):
            violations.append(relative)
    for path in violations:
        print(f"source_han=found path={path}")
    if violations:
        return 1
    print(f"source_han=none checked={checked}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
