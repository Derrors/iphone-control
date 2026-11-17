"""Checks that the tests docs/dev/regression-cases.md names still exist.

    python3 tests/check_regression_cases.py

The list says for each problem how it is tested, often by a unit test's name. A test that is renamed or removed
leaves the list pointing at nothing, and the case looks covered when it no longer is. Every snake_case name with
at least three underscores in the list has to appear somewhere in the sources (crates/, apps/, tests/, packaging/):
that covers Rust tests, the Python ones and identifiers the list quotes from the code. Exit code 1 names the rows
whose test is gone. No dependencies beyond the standard library.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LIST = os.path.join(ROOT, "docs", "dev", "regression-cases.md")
SOURCES = ("crates", "apps", "tests", "packaging")
KINDS = (".rs", ".py", ".mjs", ".js", ".html", ".sh", ".toml")
NAME = re.compile(r"(?<![A-Za-z0-9_])[a-z][a-z0-9]*(?:_[a-z0-9]+){3,}(?![A-Za-z0-9_])")
ROW = re.compile(r"^\| (\d+) \|")


def sources() -> str:
    out = []
    for top in SOURCES:
        for folder, dirs, files in os.walk(os.path.join(ROOT, top)):
            dirs[:] = [d for d in dirs if d not in ("target", "node_modules", "__pycache__", "fixtures")]
            for name in files:
                path = os.path.join(folder, name)
                if name.endswith(KINDS) and os.path.abspath(path) != os.path.abspath(__file__):
                    with open(path, encoding="utf-8", errors="replace") as f:
                        out.append(f.read())
    return "\n".join(out)


def main() -> int:
    code = sources()
    rows = names = 0
    missing = []
    with open(LIST, encoding="utf-8") as f:
        for line in f:
            m = ROW.match(line)
            if not m:
                continue
            rows += 1
            for name in sorted(set(NAME.findall(line))):
                names += 1
                if name not in code:
                    missing.append((m[1], name))
    for row, name in missing:
        print(f"#{row}: `{name}` is not in the sources (renamed or removed? fix the row, or restore the test)")
    print(f"{rows} cases, {names} names, {len(missing)} missing")
    return 1 if missing or not rows else 0


if __name__ == "__main__":
    sys.exit(main())
