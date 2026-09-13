#!/usr/bin/env python3
"""Helper script to bump patch version in Cargo.toml and generate categorized release notes."""
import os
import pathlib
import re
import subprocess
import sys
import tomllib


def bump_version():
    cargo_path = pathlib.Path("Cargo.toml")
    content = cargo_path.read_text()
    manifest = tomllib.loads(content)
    v = manifest["workspace"]["package"]["version"]
    major, minor, patch = map(int, v.split("."))
    new_v = f"{major}.{minor}.{patch + 1}"
    new_content = re.sub(
        r'(version\s*=\s*")' + re.escape(v) + r'(")',
        r"\g<1>" + new_v + r"\g<2>",
        content,
        count=1,
    )
    cargo_path.write_text(new_content)
    print(new_v)


def generate_notes(tag: str, prev_tag: str, out_path: str):
    rev_range = f"{prev_tag}..{tag}" if prev_tag else tag
    out = subprocess.run(
        ["git", "log", rev_range, "--no-merges", "--pretty=format:%h %s"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip().splitlines()

    features, fixes, improvements, others = [], [], [], []
    for line in out:
        if not line.strip():
            continue
        sha, _, msg = line.partition(" ")
        lower = msg.lower()
        if lower.startswith("feat"):
            features.append(f"- **{msg}** (`{sha}`)")
        elif lower.startswith("fix"):
            fixes.append(f"- {msg} (`{sha}`)")
        elif (
            lower.startswith("perf")
            or lower.startswith("refactor")
            or lower.startswith("chore")
            or lower.startswith("ci")
        ):
            improvements.append(f"- {msg} (`{sha}`)")
        else:
            others.append(f"- {msg} (`{sha}`)")

    lines = [f"## DUM-E {tag}\n", "### 작업 요약 (Summary of Changes)\n"]
    if features:
        lines.append("#### ✨ 새로운 기능 (Features)")
        lines.append("\n".join(features) + "\n")
    if fixes:
        lines.append("#### 🐛 버그 수정 (Bug Fixes)")
        lines.append("\n".join(fixes) + "\n")
    if improvements:
        lines.append("#### ⚡ 개선 및 리팩토링 (Improvements)")
        lines.append("\n".join(improvements) + "\n")
    if others:
        lines.append("#### 📝 기타 변경 사항 (Other Changes)")
        lines.append("\n".join(others) + "\n")
    if not (features or fixes or improvements or others):
        lines.append("- Routine update and maintenance release.\n")

    pathlib.Path(out_path).write_text("\n".join(lines), encoding="utf-8")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit("Usage: bump | notes <tag> <prev_tag> <out_path>")
    cmd = sys.argv[1]
    if cmd == "bump":
        bump_version()
    elif cmd == "notes":
        tag = sys.argv[2]
        prev_tag = sys.argv[3] if len(sys.argv) > 3 else ""
        out_path = sys.argv[4] if len(sys.argv) > 4 else "RELEASE_NOTES.md"
        generate_notes(tag, prev_tag, out_path)
    else:
        sys.exit(f"Unknown command: {cmd}")
