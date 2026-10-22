#!/usr/bin/env python3
"""Run cargo and, after a successful run, prune build artifacts that only take up disk space.

Usage: scripts/cargo.py <cargo arguments>, e.g. scripts/cargo.py test --features sqlite

After cargo succeeds, every profile directory in the target directory loses its
incremental/ cache and the hashed executables in deps/ and examples/ (test
binaries and leftovers of earlier builds). Executables still hard-linked to an
uplifted copy, such as target/debug/vaultwarden, are kept because removing them
frees nothing. Final binaries, dependency libraries and build script outputs
stay. A failed run leaves everything in place for diagnosis.
"""

import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

HASHED_EXECUTABLE = re.compile(r".+-[0-9a-f]{16}(\.exe)?", re.ASCII)


def target_directory():
    metadata = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], check=True, text=True, capture_output=True
    ).stdout
    return Path(json.loads(metadata)["target_directory"])


def profile_directories(target):
    # target/<profile>/ and, for cross builds, target/<triple>/<profile>/
    return {deps.parent for pattern in ("*/deps", "*/*/deps") for deps in target.glob(pattern) if deps.is_dir()}


def prune(profile):
    freed = 0
    incremental = profile / "incremental"
    if incremental.is_dir():
        freed += sum(path.stat().st_size for path in incremental.rglob("*") if path.is_file() and not path.is_symlink())
        shutil.rmtree(incremental)
    for directory in (profile / "deps", profile / "examples"):
        if not directory.is_dir():
            continue
        for path in directory.iterdir():
            if not HASHED_EXECUTABLE.fullmatch(path.name) or path.is_symlink() or not path.is_file():
                continue
            stat = path.stat()
            if stat.st_mode & 0o111 and stat.st_nlink == 1:
                path.unlink()
                freed += stat.st_size
    return freed


def main():
    status = subprocess.run(["cargo", *sys.argv[1:]]).returncode
    if status != 0:
        return status
    try:
        freed = sum(prune(profile) for profile in profile_directories(target_directory()))
    except (subprocess.CalledProcessError, OSError) as error:
        print(f"cargo.py: pruning failed: {error}", file=sys.stderr)
        return 1
    if freed:
        print(f"cargo.py: pruned {freed / 2**20:.0f} MiB of build artifacts", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
