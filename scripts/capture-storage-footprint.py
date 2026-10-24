#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# ///
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


def run_text(*args: str) -> str:
    proc = subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False)
    return proc.stdout.strip() if proc.returncode == 0 else ""


def parse_int(value: str) -> int | None:
    try:
        return int(value)
    except ValueError:
        return None


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: capture-storage-footprint.py PATH")
    root = Path(sys.argv[1]).resolve()
    if not root.exists():
        raise SystemExit(f"path does not exist: {root}")

    apparent = allocated = files = directories = symlinks = 0
    for current, dirnames, filenames in os.walk(root, followlinks=False):
        directories += len(dirnames)
        for name in filenames:
            path = Path(current) / name
            try:
                st = path.lstat()
            except OSError:
                continue
            if path.is_symlink():
                symlinks += 1
                continue
            if path.is_file():
                files += 1
                apparent += st.st_size
                allocated += st.st_blocks * 512

    source = run_text("findmnt", "-T", str(root), "-n", "-o", "SOURCE")
    filesystem = run_text("findmnt", "-T", str(root), "-n", "-o", "FSTYPE")
    target = run_text("findmnt", "-T", str(root), "-n", "-o", "TARGET")
    payload: dict[str, object] = {
        "path": str(root),
        "filesystem": filesystem,
        "source": source,
        "mount_target": target,
        "apparent_bytes": apparent,
        "allocated_block_bytes": allocated,
        "file_count": files,
        "directory_count": directories,
        "symlink_count": symlinks,
    }

    if filesystem == "zfs" and source:
        props = [
            "compression", "compressratio", "recordsize", "primarycache", "secondarycache",
            "sync", "logbias", "used", "logicalused", "referenced", "logicalreferenced",
        ]
        zfs: dict[str, object] = {}
        for prop in props:
            raw = run_text("zfs", "get", "-Hp", "-o", "value", prop, source)
            parsed = parse_int(raw)
            zfs[prop] = parsed if parsed is not None else raw
        payload["zfs_dataset"] = zfs

    print(json.dumps(payload, sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
