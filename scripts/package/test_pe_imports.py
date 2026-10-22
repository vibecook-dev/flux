"""Tests for pe-imports.py on synthetic PE files built here.

Run: python3 -m unittest discover -s scripts/package -p 'test_*.py'
"""
from __future__ import annotations

import importlib.util
import os
import struct
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "pe-imports.py")
spec = importlib.util.spec_from_file_location("pe_imports", SCRIPT)
pe_imports = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pe_imports)

IMAGE_BASE = 0x140000000
IMAGE_BASE_PE32 = 0x400000
SECTION_RVA = 0x1000
SECTION_AT = 0x400  # file offset of the one section


def make_pe(imports=(), delay=(), delay_old_format=False, pe32=False) -> bytes:
    """A minimal x64 (or, with pe32, x86) PE image whose one section holds
    an import table for `imports` and a delay-load table for `delay`.
    delay_old_format writes Visual C++ 6's delay-load entries (VAs; PE32
    only)."""
    base = IMAGE_BASE_PE32 if pe32 else IMAGE_BASE
    names = list(imports) + list(delay)
    descs_size = 20 * (len(imports) + 1)
    delay_at = descs_size
    delay_size = 32 * (len(delay) + 1)
    names_at = delay_at + delay_size
    name_rvas, blob = [], b""
    for n in names:
        name_rvas.append(SECTION_RVA + names_at + len(blob))
        blob += n.encode() + b"\0"
    body = b""
    for i in range(len(imports)):
        body += struct.pack("<IIIII", 0, 0, 0, name_rvas[i], 0)
    body += b"\0" * 20
    for i in range(len(delay)):
        rva = name_rvas[len(imports) + i]
        if delay_old_format:
            body += struct.pack("<IIIIIIII", 0, base + rva, 0, 0, 0, 0, 0, 0)
        else:
            body += struct.pack("<IIIIIIII", 1, rva, 0, 0, 0, 0, 0, 0)
    body += b"\0" * 32
    body += blob
    raw_size = (len(body) + 0x1FF) & ~0x1FF
    section = body.ljust(raw_size, b"\0")

    dirs = [(0, 0)] * 16
    if imports:
        dirs[1] = (SECTION_RVA, descs_size)
    if delay:
        dirs[13] = (SECTION_RVA + delay_at, delay_size)
    dir_bytes = b"".join(struct.pack("<II", *d) for d in dirs)
    if pe32:
        opt = struct.pack("<H", 0x10B) + b"\0" * 26 + struct.pack("<I", base)
        opt = opt.ljust(92, b"\0") + struct.pack("<I", 16) + dir_bytes
        machine = 0x14C
    else:
        opt = struct.pack("<H", 0x20B) + b"\0" * 22 + struct.pack("<Q", base)
        opt = opt.ljust(108, b"\0") + struct.pack("<I", 16) + dir_bytes
        machine = 0x8664
    coff = struct.pack("<HHIIIHH", machine, 1, 0, 0, 0, len(opt), 0x22)
    sect = b".idata\0\0" + struct.pack("<IIIIIIHHI", raw_size, SECTION_RVA, raw_size, SECTION_AT, 0, 0, 0, 0, 0xC0000040)
    dos = b"MZ" + b"\0" * 58 + struct.pack("<I", 0x40)
    headers = dos + b"PE\0\0" + coff + opt + sect
    assert len(headers) <= SECTION_AT
    return headers.ljust(SECTION_AT, b"\0") + section


def run(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, "-I", SCRIPT, *args], capture_output=True, text=True)


class PEImportsTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def write(self, name: str, data: bytes) -> str:
        path = os.path.join(self.tmp.name, name)
        with open(path, "wb") as f:
            f.write(data)
        return path

    def test_reads_import_and_delay_load_tables(self):
        pe = pe_imports.PE(make_pe(["KERNEL32.dll", "VCRUNTIME140.dll"], ["msvcp140.dll"]))
        self.assertEqual(pe.imports(), ["KERNEL32.dll", "VCRUNTIME140.dll", "msvcp140.dll"])

    def test_reads_pe32_and_old_delay_load_format(self):
        pe = pe_imports.PE(make_pe(["kernel32.dll"], ["vcruntime140.dll"], delay_old_format=True, pe32=True))
        self.assertEqual(pe.imports(), ["kernel32.dll", "vcruntime140.dll"])

    def test_dynamic_crt_fails_the_check(self):
        # What a default (dynamic CRT) MSVC Rust build imports.
        path = self.write("dynamic.exe", make_pe(
            ["KERNEL32.dll", "VCRUNTIME140.dll", "api-ms-win-crt-runtime-l1-1-0.dll", "api-ms-win-crt-heap-l1-1-0.dll"]))
        r = run("--no-vc-redist", path)
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("VCRUNTIME140.dll", r.stderr)
        self.assertIn("+crt-static", r.stderr)
        # Without --no-vc-redist it only lists.
        r = run(path)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("VCRUNTIME140.dll api-ms-win-crt-runtime-l1-1-0.dll", r.stdout)

    def test_delay_loaded_redist_dll_fails_the_check(self):
        path = self.write("delay.exe", make_pe(["KERNEL32.dll"], ["MSVCP140_1.dll"]))
        r = run("--no-vc-redist", path)
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("MSVCP140_1.dll", r.stderr)

    def test_static_crt_and_system_dlls_pass(self):
        # What the +crt-static build imports, plus Windows' own CRTs.
        static = self.write("static.exe", make_pe(
            ["advapi32.dll", "bcrypt.dll", "KERNEL32.dll", "ntdll.dll", "WS2_32.dll", "msvcrt.dll"],
            ["ucrtbase.dll", "api-ms-win-crt-runtime-l1-1-0.dll"]))
        none = self.write("none.exe", make_pe())
        r = run("--no-vc-redist", static, none)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("none.exe: (no imports)", r.stdout)

    def test_every_file_is_checked(self):
        ok = self.write("ok.exe", make_pe(["KERNEL32.dll"]))
        bad = self.write("bad.exe", make_pe(["vcruntime140_1.dll"]))
        r = run("--no-vc-redist", bad, ok)
        self.assertEqual(r.returncode, 1)
        self.assertIn("ok.exe: KERNEL32.dll", r.stdout)

    def test_redist_names(self):
        match = pe_imports.VC_REDIST.match
        for name in ["vcruntime140.dll", "VCRUNTIME140_1.dll", "vcruntime140d.dll", "msvcp140.dll",
                     "msvcp140_atomic_wait.dll", "concrt140.dll", "vccorlib140.dll", "vcomp140.dll", "msvcr120.dll"]:
            self.assertTrue(match(name), name)
        for name in ["msvcrt.dll", "ucrtbase.dll", "api-ms-win-crt-stdio-l1-1-0.dll", "kernel32.dll", "vcruntime.txt"]:
            self.assertFalse(match(name), name)

    def test_not_a_pe_file(self):
        for name, data in [("text.exe", b"#!/bin/sh\n"), ("short.exe", b"MZ"), ("cut.exe", make_pe(["KERNEL32.dll"])[:0x300])]:
            r = run("--no-vc-redist", self.write(name, data))
            self.assertEqual(r.returncode, 2, name)
            self.assertIn(name, r.stderr)


if __name__ == "__main__":
    unittest.main()
