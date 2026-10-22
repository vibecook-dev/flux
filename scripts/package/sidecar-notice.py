#!/usr/bin/env python3
"""Writes sidecar-NOTICE.txt, the licences of the Go code in Truffle's sidecar
(bin/sidecar-slim, or sidecar-slim.exe on Windows), which package.sh appends
to THIRD_PARTY_LICENSES.

It takes the sidecars pinned for the truffle-core version in Cargo.lock
(fetch-sidecar.sh, all five release targets). Each one records the Go
modules it was built from, with their go.sum hashes (`go version -m`). For
every module, this downloads the module's zip from the Go module proxy,
checks it against that hash, and quotes each licence file in it. The Go
standard library's licence comes from the Go release the sidecars were built
with, and Truffle's from its repository at the commit they were built from.

Run it again whenever the pinned sidecar changes: package.sh refuses to
bundle a sidecar with a module that sidecar-NOTICE.txt doesn't list.

Usage: scripts/package/sidecar-notice.py [--version V] [--out FILE]
  --version V   Truffle version; default: truffle-core's in Cargo.lock
  --out FILE    default: scripts/package/sidecar-NOTICE.txt
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import io
import re
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
TARGETS = {
    "aarch64-apple-darwin": "macOS",
    "x86_64-apple-darwin": "macOS",
    "x86_64-unknown-linux-gnu": "Linux",
    "aarch64-unknown-linux-gnu": "Linux",
    "x86_64-pc-windows-msvc": "Windows",
}
PROXY = "https://proxy.golang.org"
RAW = "https://raw.githubusercontent.com"
TRUFFLE = "vibecook-dev/truffle"
# Licence files, at any depth of a module (testdata excepted).
LICENSE_FILE = re.compile(r"(?i)^(licen[cs]e|copying|notice)([-_][a-z0-9-]+)?(\.(txt|md|markdown))?$")
RULE = "-" * 80


def die(msg: str) -> None:
    sys.exit(f"sidecar-notice.py: {msg}")


def fetch(url: str) -> bytes:
    for attempt in range(4):
        try:
            with urllib.request.urlopen(url, timeout=120) as r:
                return r.read()
        except Exception as e:  # noqa: BLE001 - retried, then reported
            if attempt == 3:
                die(f"{url}: {e}")
            time.sleep(2 * (attempt + 1))
    raise AssertionError


def lock_version() -> str:
    lock = (ROOT / "Cargo.lock").read_text()
    m = re.search(r'\[\[package\]\]\nname = "truffle-core"\nversion = "([^"]+)"\nsource = "registry\+', lock)
    if not m:
        die("truffle-core in Cargo.lock is not a crates.io release; pass --version")
    return m.group(1)


def uvarint(data: bytes, i: int) -> tuple[int, int]:
    n = shift = 0
    while True:
        b = data[i]
        i += 1
        n |= (b & 0x7F) << shift
        if b < 0x80:
            return n, i
        shift += 7


def build_info(binary: bytes) -> tuple[str, list[str]]:
    """The Go version and the module info lines that `go version -m` prints."""
    at = binary.find(b"\xff Go buildinf:")
    if at < 0 or not binary[at + 15] & 2:
        die("no Go 1.18+ build info in the sidecar")
    n, i = uvarint(binary, at + 32)
    go = binary[i : i + n].decode()
    n, i = uvarint(binary, i + n)
    # The module info is framed by 16-byte sentinels.
    return go, binary[i + 16 : i + n - 16].decode().splitlines()


def escape(s: str) -> str:
    """Module path and version escaping of the Go module proxy protocol."""
    return re.sub(r"[A-Z]", lambda m: "!" + m.group(0).lower(), s)


def h1(z: zipfile.ZipFile) -> str:
    """The go.sum hash of a module zip (golang.org/x/mod/sumdb/dirhash.HashZip)."""
    summary = "".join(f"{hashlib.sha256(z.read(n)).hexdigest()}  {n}\n" for n in sorted(z.namelist()))
    return "h1:" + base64.b64encode(hashlib.sha256(summary.encode()).digest()).decode()


def text_of(data: bytes) -> str:
    return data.decode("utf-8", errors="replace").replace("\r\n", "\n").strip("\n")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--version")
    ap.add_argument("--out", default=str(HERE / "sidecar-NOTICE.txt"))
    args = ap.parse_args()
    version = args.version or lock_version()

    # module -> (version, h1, set of OS names); go version -> targets
    modules: dict[str, tuple[str, str, set[str]]] = {}
    gos: set[str] = set()
    revisions: set[str] = set()
    with tempfile.TemporaryDirectory() as tmp:
        for target, os_name in TARGETS.items():
            out = Path(tmp) / target
            subprocess.run(
                [str(HERE / "fetch-sidecar.sh"), "--target", target, "--out", str(out), "--version", version],
                check=True,
                stdout=subprocess.DEVNULL,
            )
            sidecar = out / ("sidecar-slim.exe" if os_name == "Windows" else "sidecar-slim")
            if not sidecar.is_file():
                die(f"no pinned Truffle {version} sidecar for {target} (fetch-sidecar.sh --pin {version})")
            go, lines = build_info(sidecar.read_bytes())
            gos.add(go)
            for line in lines:
                f = line.split("\t")
                if f[0] == "=>":
                    die(f"{target}: replaced module {line!r}: not handled")
                if f[0] == "dep":
                    path, ver, sum_ = f[1], f[2], f[3]
                    old = modules.get(path)
                    if old and old[:2] != (ver, sum_):
                        die(f"{path}: {old[0]} in one sidecar, {ver} in another")
                    modules.setdefault(path, (ver, sum_, set()))[2].add(os_name)
                if f[0] == "build" and f[1].startswith("vcs.revision="):
                    revisions.add(f[1].split("=", 1)[1])
    if len(revisions) != 1:
        die(f"the sidecars come from different Truffle commits: {sorted(revisions)}")
    revision = revisions.pop()

    # (source, file) -> text, in the order they appear in the notice.
    texts: list[tuple[str, str, str]] = []
    for go in sorted(gos):
        texts.append((f"Go standard library ({go})", "LICENSE", text_of(fetch(f"{RAW}/golang/go/{go}/LICENSE"))))
    texts.append(
        (f"Truffle (github.com/{TRUFFLE} {revision[:7]})", "LICENSE", text_of(fetch(f"{RAW}/{TRUFFLE}/{revision}/LICENSE")))
    )
    for path in sorted(modules):
        ver, want, _ = modules[path]
        data = fetch(f"{PROXY}/{escape(path)}/@v/{escape(ver)}.zip")
        z = zipfile.ZipFile(io.BytesIO(data))
        got = h1(z)
        if got != want:
            die(f"{path}@{ver}: the proxy's zip hashes to {got}, the sidecar records {want}")
        prefix = f"{path}@{ver}/"
        found = sorted(
            n
            for n in z.namelist()
            if LICENSE_FILE.match(n.rsplit("/", 1)[-1]) and "/testdata/" not in "/" + n[len(prefix) :]
        )
        if not any("/" not in n[len(prefix) :] for n in found):
            die(f"{path}@{ver} has no licence file at its root")
        for n in found:
            texts.append((f"{path} {ver}", n[len(prefix) :], text_of(z.read(n))))
        print(f"{path} {ver}: {len(found)} licence file(s)", file=sys.stderr)

    # One section per distinct text, headed by every file that has it.
    sections: dict[str, list[str]] = {}
    for source, file, text in texts:
        sections.setdefault(text, []).append(f"{source}: {file}")

    every_os = set(TARGETS.values())
    out = [
        "=" * 80,
        "sidecar-slim: Truffle's Tailscale sidecar",
        "=" * 80,
        "",
        "bin/sidecar-slim (sidecar-slim.exe in the Windows package) is the unmodified",
        f"sidecar binary from Truffle {version}",
        f"(https://github.com/{TRUFFLE}/releases/tag/truffle-v{version}), built",
        f"with {', '.join(sorted(gos))} from Truffle's commit {revision[:7]}. It is a Go program built",
        "with Tailscale's tsnet. The Go modules compiled into it, as the binary",
        "records them (`go version -m bin/sidecar-slim`), are listed below, followed by",
        "every licence file in their sources: each module's zip from the Go module",
        "proxy, checked against the go.sum hash the binary records.",
        "Generated by scripts/package/sidecar-notice.py.",
        "",
        "Go modules:",
    ]
    for path in sorted(modules):
        oses = modules[path][2]
        note = "" if oses == every_os else f" ({' and '.join(sorted(oses))} only)"
        out.append(f"  {path} {modules[path][0]}{note}")
    for text, heads in sections.items():
        out += ["", RULE, *heads, RULE, "", text]
    Path(args.out).write_text("\n".join(out) + "\n")
    print(f"{args.out}: {len(modules)} modules, {len(sections)} licence texts", file=sys.stderr)


if __name__ == "__main__":
    main()
