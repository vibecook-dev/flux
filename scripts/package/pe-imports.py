#!/usr/bin/env python3
"""Lists the DLLs a Windows executable imports, and checks the release build
needs none from the Visual C++ Redistributable.

flux-server.exe is built with the C runtime linked in (`-C
target-feature=+crt-static`, set in build.yml and ci.yml), because Windows
doesn't come with vcruntime140.dll: it is part of the Visual C++
Redistributable, which GitHub's runners and most developer machines have
but a fresh Windows doesn't. An exe that imports it fails to start there
("VCRUNTIME140.dll was not found") before flux runs any code, and a test on
a machine that has the DLL can't notice. So smoke-test.sh and CI read the
import tables instead. The Universal CRT (ucrtbase.dll, api-ms-win-crt-*)
is part of Windows 10 and later and is allowed.

  pe-imports.py [--no-vc-redist] FILE...
      Prints each FILE's imported DLLs (import table and delay-load table),
      one line per file. With --no-vc-redist, exits 1 when a FILE imports a
      DLL of the Visual C++ Redistributable (vcruntime*, msvcp*, concrt*,
      vccorlib*, vcomp*, msvcr<N>*).

It reads the PE headers itself, so it runs anywhere Python 3.8 does, with no
objdump or dumpbin.
"""
from __future__ import annotations

import argparse
import re
import struct
import sys

# The DLLs the Visual C++ 2015-2022 Redistributable installs (and older
# ones' msvcr<N>), debug builds' *d.dll included. Not ucrtbase.dll or
# api-ms-win-crt-*: the Universal CRT ships with Windows 10 and later.
VC_REDIST = re.compile(r"^(vcruntime\d|msvcp\d|concrt\d|vccorlib\d|vcomp\d|msvcr\d).*\.dll$", re.IGNORECASE)

IMPORT_DIR = 1  # IMAGE_DIRECTORY_ENTRY_IMPORT
DELAY_IMPORT_DIR = 13  # IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT
MAX_DESCRIPTORS = 4096  # a real exe has a few dozen; stops a looping table


class PEError(Exception):
    pass


class PE:
    def __init__(self, data: bytes):
        self.data = data
        if data[:2] != b"MZ":
            raise PEError("not a PE file (no MZ header)")
        (pe,) = self.unpack("<I", 0x3C)
        if data[pe : pe + 4] != b"PE\0\0":
            raise PEError("not a PE file (no PE signature)")
        coff = pe + 4
        nsections, opt_size = self.unpack("<H", coff + 2)[0], self.unpack("<H", coff + 16)[0]
        opt = coff + 20
        (magic,) = self.unpack("<H", opt)
        if magic == 0x10B:  # PE32
            (self.image_base,) = self.unpack("<I", opt + 28)
            ndirs_at, dirs_at = opt + 92, opt + 96
        elif magic == 0x20B:  # PE32+
            (self.image_base,) = self.unpack("<Q", opt + 24)
            ndirs_at, dirs_at = opt + 108, opt + 112
        else:
            raise PEError(f"unknown optional header magic {magic:#x}")
        (ndirs,) = self.unpack("<I", ndirs_at)
        self.dirs = [self.unpack("<II", dirs_at + 8 * i) for i in range(min(ndirs, 16))]
        sections = opt + opt_size
        self.sections = []
        for i in range(nsections):
            vsize, va, raw_size, raw_at = self.unpack("<IIII", sections + 40 * i + 8)
            self.sections.append((va, max(vsize, raw_size), raw_at, raw_size))

    def unpack(self, fmt: str, at: int) -> tuple:
        end = at + struct.calcsize(fmt)
        if at < 0 or end > len(self.data):
            raise PEError(f"truncated: needs bytes {at}..{end} of {len(self.data)}")
        return struct.unpack_from(fmt, self.data, at)

    def offset(self, rva: int) -> int:
        for va, size, raw_at, raw_size in self.sections:
            if va <= rva < va + size:
                if rva - va >= raw_size:
                    raise PEError(f"RVA {rva:#x} is not backed by file data")
                return raw_at + rva - va
        raise PEError(f"RVA {rva:#x} is in no section")

    def cstring(self, rva: int) -> str:
        at = self.offset(rva)
        end = self.data.find(b"\0", at, at + 512)
        if end < 0:
            raise PEError(f"unterminated name at RVA {rva:#x}")
        return self.data[at:end].decode("ascii", "replace")

    def directory(self, index: int) -> tuple[int, int]:
        return self.dirs[index] if index < len(self.dirs) else (0, 0)

    def imports(self) -> list[str]:
        """The DLL names in the import table, then the delay-load table."""
        names: list[str] = []
        rva, _ = self.directory(IMPORT_DIR)
        if rva:
            at = self.offset(rva)
            for i in range(MAX_DESCRIPTORS):
                desc = self.unpack("<IIIII", at + 20 * i)
                if desc == (0, 0, 0, 0, 0):
                    break
                names.append(self.cstring(desc[3]))
            else:
                raise PEError("import table has no end")
        rva, _ = self.directory(DELAY_IMPORT_DIR)
        if rva:
            at = self.offset(rva)
            for i in range(MAX_DESCRIPTORS):
                desc = self.unpack("<IIIIIIII", at + 32 * i)
                if desc == (0,) * 8:
                    break
                attributes, name = desc[0], desc[1]
                # Bit 0 clear: Visual C++ 6's format, with VAs instead of RVAs.
                names.append(self.cstring(name if attributes & 1 else name - self.image_base))
            else:
                raise PEError("delay-load import table has no end")
        return names


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--no-vc-redist", action="store_true", help="fail when a file imports a Visual C++ Redistributable DLL")
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()
    status = 0
    for path in args.files:
        try:
            with open(path, "rb") as f:
                dlls = PE(f.read()).imports()
        except (OSError, PEError) as e:
            print(f"pe-imports.py: {path}: {e}", file=sys.stderr)
            return 2
        print(f"{path}: {' '.join(dlls) if dlls else '(no imports)'}")
        redist = [d for d in dlls if VC_REDIST.match(d)]
        if args.no_vc_redist and redist:
            print(
                f"pe-imports.py: {path} imports {', '.join(redist)} from the Visual C++ Redistributable, "
                "which Windows doesn't include; link the C runtime statically (-C target-feature=+crt-static)",
                file=sys.stderr,
            )
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
