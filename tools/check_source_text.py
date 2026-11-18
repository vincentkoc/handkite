#!/usr/bin/env python3
"""Check English source and prose while preserving Unicode fixture values."""

import json
from pathlib import Path
import subprocess
import unicodedata


TEXT_SUFFIXES = {
    ".css", ".html", ".js", ".json", ".lock", ".md", ".py", ".rs",
    ".sh", ".svg", ".toml", ".ts", ".txt", ".yaml", ".yml",
}
TEXT_NAMES = {".gitignore", "Caddyfile"}
UNICODE_FIXTURES = {
    "tools/benchmarks/notes-scroll.json",
    "tools/benchmarks/notes-typing.json",
}


def has_han(value: str) -> bool:
    return any(
        unicodedata.name(char, "").startswith(
            ("CJK UNIFIED IDEOGRAPH", "CJK COMPATIBILITY IDEOGRAPH")
        )
        for char in value
    )


def unique_object(pairs: list[tuple[str, object]]) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate fixture key")
        result[key] = value
    return result


def invalid_constant(_value: str) -> None:
    raise ValueError("invalid JSON constant")


def fixture_value(path: tuple) -> bool:
    if len(path) < 5 or path[:1] != ("cases",) or not isinstance(path[1], int):
        return False
    if path[2] == "measure":
        return path[3:] == ("expect", "text", "value")
    return (
        path[2] in ("prepare", "cleanup")
        and isinstance(path[3], int)
        and path[4:] == ("expect", "text", "value")
    )


def forbidden_han(relative: Path, content: str) -> bool:
    if relative.as_posix() not in UNICODE_FIXTURES:
        return has_han(content)
    try:
        document = json.loads(content, object_pairs_hook=unique_object,
                              parse_constant=invalid_constant)
    except ValueError:
        return True

    def visit(value: object, path: tuple = ()) -> bool:
        if isinstance(value, dict):
            return any(has_han(key) or visit(child, (*path, key))
                       for key, child in value.items())
        if isinstance(value, list):
            return any(visit(child, (*path, index)) for index, child in enumerate(value))
        return isinstance(value, str) and not fixture_value(path) and has_han(value)

    return visit(document)


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
        if forbidden_han(relative, staged) or forbidden_han(relative, content):
            violations.append(relative)
    for path in violations:
        print(f"source_han=found path={path}")
    if violations:
        return 1
    print(f"source_han=none checked={checked}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
