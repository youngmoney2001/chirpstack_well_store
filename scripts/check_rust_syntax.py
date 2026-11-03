#!/usr/bin/env python3
"""Lightweight fallback syntax check for Rust files (no cargo required).

This is a sandbox-time helper, not a replacement for `cargo check` / `clippy`:
it parses the given files with tree-sitter-rust and reports ERROR / MISSING
nodes, with their line numbers. It does NOT type-check the code.

Usage:
    python3 -m venv /tmp/tsvenv
    /tmp/tsvenv/bin/pip install tree-sitter tree-sitter-rust
    /tmp/tsvenv/bin/python scripts/check_rust_syntax.py <file.rs> [...]

Exits with code 1 when at least one file contains a syntax error.
"""

from __future__ import annotations

import sys

try:
    from tree_sitter import Language, Parser
    import tree_sitter_rust
except ImportError:
    sys.exit(
        "tree-sitter is missing. Install it with:\n"
        "  python3 -m venv /tmp/tsvenv && /tmp/tsvenv/bin/pip install "
        "tree-sitter tree-sitter-rust"
    )


def scan(path: str, parser: Parser) -> list[tuple[int, str, str]]:
    with open(path, "rb") as fh:
        data = fh.read()
    tree = parser.parse(data)
    issues: list[tuple[int, str, str]] = []

    def walk(node) -> None:
        if node.type == "ERROR" or node.is_missing:
            snippet = data[node.start_byte : node.end_byte][:60].decode(
                "utf-8", "replace"
            )
            issues.append((node.start_point[0] + 1, node.type, snippet))
        for child in node.children:
            walk(child)

    walk(tree.root_node)
    return issues


def main(paths: list[str]) -> int:
    parser = Parser(Language(tree_sitter_rust.language()))
    failed = False
    for path in paths:
        try:
            issues = scan(path, parser)
        except OSError as e:
            print(f"{path}: cannot read ({e})")
            failed = True
            continue

        if not issues:
            print(f"{path}: OK")
            continue

        failed = True
        print(f"{path}: {len(issues)} syntax issue(s)")
        for line, kind, snippet in issues[:10]:
            print(f"    line {line}: {kind} near {snippet!r}")
    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    sys.exit(main(sys.argv[1:]))
