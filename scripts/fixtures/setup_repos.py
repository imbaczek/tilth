#!/usr/bin/env python3
"""Clone four real-world repositories at pinned commits.

The README examples and the speed table were produced on these repositories,
and output-parity checks compare tilth builds against them. Pinning the
commits keeps those results reproducible.

Usage:
    python3 scripts/fixtures/setup_repos.py                 # all four
    python3 scripts/fixtures/setup_repos.py --repos gin     # a subset
    python3 scripts/fixtures/setup_repos.py --dir ~/fixtures
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

# The default sits outside the tilth checkout on purpose, so an agent working
# in a fixture never reads tilth's own CLAUDE.md.
DEFAULT_DIR = Path("/tmp/tilth_fixtures/repos")

REPOS = {
    "ripgrep": ("https://github.com/BurntSushi/ripgrep.git", "0a88cccd5188074de96f54a4b6b44a63971ac157"),
    "fastapi": ("https://github.com/tiangolo/fastapi.git", "6fa573ce0bc16fe445f93db413d20146dd9ff35d"),
    "gin": ("https://github.com/gin-gonic/gin.git", "d7776de7d444935ea4385999711bd6331a98fecb"),
    "express": ("https://github.com/expressjs/express.git", "1140301f6a0ed5a05bc1ef38d48294f75a49580c"),
}


def git(args: list[str], cwd: Path | None = None) -> str:
    result = subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True)
    return result.stdout.strip()


def setup_repo(name: str, root: Path) -> None:
    url, sha = REPOS[name]
    path = root / name

    if not path.exists():
        print(f"  {name}: cloning {url}")
        git(["clone", "--no-checkout", url, str(path)])
    elif git(["rev-parse", "HEAD"], cwd=path) == sha:
        print(f"  {name}: already at {sha[:8]}")
        return
    else:
        # An existing clone at another commit is moved, never deleted.
        print(f"  {name}: fetching {sha[:8]}")
        git(["fetch", "origin", sha], cwd=path)

    git(["checkout", "--detach", sha], cwd=path)
    print(f"  {name}: checked out {sha[:8]}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repos", default="all", help="comma-separated names, or 'all'")
    parser.add_argument("--dir", type=Path, default=DEFAULT_DIR, help=f"target directory (default: {DEFAULT_DIR})")
    args = parser.parse_args()

    names = list(REPOS) if args.repos.lower() == "all" else [n.strip() for n in args.repos.split(",") if n.strip()]
    unknown = [n for n in names if n not in REPOS]
    if unknown:
        print(f"unknown repos: {', '.join(unknown)}. Known: {', '.join(REPOS)}", file=sys.stderr)
        return 2

    root = args.dir.expanduser()
    root.mkdir(parents=True, exist_ok=True)
    print(f"Setting up fixture repositories in {root}")
    for name in names:
        setup_repo(name, root)
    return 0


if __name__ == "__main__":
    sys.exit(main())
