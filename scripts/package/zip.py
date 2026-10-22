#!/usr/bin/env python3
"""Writes, lists and unpacks the Windows release package (a .zip).

package.sh, smoke-test.sh and the release dry run use it instead of zip or
unzip, which Git for Windows and GitHub's Windows runners don't all have.

  zip.py create <dir> <out.zip> [--epoch SECONDS]
      Zips <dir> as one top-level folder of that name. Entries are sorted,
      every entry gets the same time (--epoch, default SOURCE_DATE_EPOCH or
      now), and Unix modes are recorded (0755 for folders and .exe files,
      0644 for the rest), so the same inputs give the same archive on any OS.
  zip.py list <file.zip>
      Prints the entry names, one per line.
  zip.py extract <file.zip> <dir>
      Unpacks into <dir>. Entries that would land outside it are refused.
"""
from __future__ import annotations

import argparse
import os
import sys
import time
import zipfile

# Zip times are DOS times, which start in 1980.
DOS_EPOCH = 315532800


def die(msg: str) -> None:
    sys.exit(f"zip.py: {msg}")


def create(src: str, out: str, epoch: int) -> None:
    src = os.path.abspath(src)
    if not os.path.isdir(src):
        die(f"not a folder: {src}")
    stamp = time.gmtime(max(epoch, DOS_EPOCH))[:6]
    names: list[str] = []
    for dirpath, dirnames, filenames in os.walk(src):
        rel = os.path.relpath(dirpath, os.path.dirname(src)).replace(os.sep, "/")
        names.append(rel + "/")
        names += [f"{rel}/{f}" for f in filenames]
        dirnames.sort()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for name in sorted(names):
            info = zipfile.ZipInfo(name, stamp)
            info.create_system = 3  # Unix, so external_attr holds a mode
            if name.endswith("/"):
                info.external_attr = (0o040755 << 16) | 0x10  # and MS-DOS's directory bit
                z.writestr(info, b"")
                continue
            mode = 0o755 if name.endswith(".exe") else 0o644
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            path = os.path.join(os.path.dirname(src), *name.split("/"))
            with open(path, "rb") as f:
                z.writestr(info, f.read(), compresslevel=9)


def extract(archive: str, dest: str) -> None:
    dest = os.path.realpath(dest)
    with zipfile.ZipFile(archive) as z:
        for name in z.namelist():
            target = os.path.realpath(os.path.join(dest, name))
            if os.path.commonpath([dest, target]) != dest:
                die(f"{archive}: entry {name!r} is outside the archive's folder")
        z.extractall(dest)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("create")
    c.add_argument("dir")
    c.add_argument("out")
    c.add_argument("--epoch", type=int, default=int(os.environ.get("SOURCE_DATE_EPOCH") or time.time()))
    lst = sub.add_parser("list")
    lst.add_argument("archive")
    x = sub.add_parser("extract")
    x.add_argument("archive")
    x.add_argument("dest")
    args = ap.parse_args()
    if args.cmd == "create":
        create(args.dir, args.out, args.epoch)
    elif args.cmd == "list":
        with zipfile.ZipFile(args.archive) as z:
            print("\n".join(z.namelist()))
    else:
        extract(args.archive, args.dest)


if __name__ == "__main__":
    main()
