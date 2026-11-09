#!/usr/bin/env python3
"""Writes THIRD-PARTY-LICENSES.txt: the licence notices of every crate built
into the `phv` executable for a target, from the crates' own licence files.
Used by .github/workflows/release.yml (each archive and the Docker image get
the file of their target) and checked by ci.yml:

    python3 packaging/licenses.py x86_64-unknown-linux-musl THIRD-PARTY-LICENSES.txt

The crates are those `cargo metadata --locked` resolves for the target as
normal dependencies of phv_cli (not build or dev dependencies), read from
Cargo's registry cache, which the build has filled. The licence files of a
crate are the files of its package named LICENSE*, LICENCE*, COPYING*,
NOTICE*, COPYRIGHT* or UNLICENSE*, also those of the C sources a crate
vendors (mimalloc, SQLite, ring). Identical texts are written once, with the
crates that carry them. Fails when a crate has no licence file, unless its
licence is one whose text does not have to go with the binary (public domain).
"""

import json
import os
import subprocess
import sys
from pathlib import Path

PREFIXES = ("licen", "copying", "notice", "copyright", "unlicense")
# Source files and folders of tests are not licence notices.
NOT_TEXT = (".rs", ".toml", ".py", ".sh", ".c", ".h", ".json", ".yml", ".yaml", ".html")
SKIP_DIRS = {".git", "tests", "test", "testdata", "benches", "examples", "fuzz"}
# Licences that ask for no notice in a binary (zlib's notice goes with the
# source only). A crate without a licence file that offers one of them is
# taken under it.
NO_NOTICE = {"Unlicense", "CC0-1.0", "0BSD", "Zlib"}
# Crates published without a licence file, and the crate of the same project
# (the same repository, licence and authors) whose licence file is theirs.
SAME_PROJECT = {
    "cranelift-": "cranelift-codegen",
    "wasmtime-internal-": "cranelift-codegen",
}


def licence_files(root: Path) -> list[Path]:
    found = []
    for path, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d.lower() not in SKIP_DIRS)
        for f in sorted(files):
            low = f.lower()
            if low.startswith(PREFIXES) and not low.endswith(NOT_TEXT):
                found.append(Path(path) / f)
    return found


def crates(target: str) -> list[dict]:
    out = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
        check=True,
        capture_output=True,
    ).stdout
    meta = json.loads(out)
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == "phv_cli" and p["source"] is None)
    seen, stack = set(), [root]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    # The workspace's own crates are PHV (LICENSE).
    found = [packages[p] for p in seen if packages[p]["source"] is not None]
    return sorted(found, key=lambda p: (p["name"], p["version"]))


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: licenses.py <target> <output file>", file=sys.stderr)
        return 2
    target, output = sys.argv[1], Path(sys.argv[2])
    texts: dict[str, list[str]] = {}
    listed, missing = [], []
    found = crates(target)
    by_name = {p["name"]: p for p in found}
    for p in found:
        name = f"{p['name']} {p['version']}"
        licence = p.get("license") or "(see the licence file)"
        listed.append(f"  {name:<40} {licence}")
        files = licence_files(Path(p["manifest_path"]).parent)
        if p.get("license_file"):
            extra = Path(p["manifest_path"]).parent / p["license_file"]
            if extra.is_file() and extra not in files:
                files.append(extra)
        root = Path(p["manifest_path"]).parent
        note = ""
        if not files:
            kin = next((by_name.get(k) for pre, k in SAME_PROJECT.items() if p["name"].startswith(pre)), None)
            if kin is not None and kin.get("license") == p.get("license"):
                root = Path(kin["manifest_path"]).parent
                files = licence_files(root)
                note = f" (the package has none: that of {kin['name']} {kin['version']}, the same project)"
        if not files:
            # One of the licences offered asks for no notice in a binary.
            words = licence.replace("(", " ").replace(")", " ").replace("/", " OR ").split()
            if "AND" in words or "WITH" in words or not set(words) & NO_NOTICE:
                missing.append(f"{name} ({licence})")
            continue
        for f in files:
            text = f.read_text(encoding="utf-8", errors="replace").strip().replace("\r\n", "\n")
            texts.setdefault(text, []).append(f"{name}: {f.relative_to(root).as_posix()}{note}")
    if missing:
        print("error: no licence file in these crates:", file=sys.stderr)
        for m in missing:
            print(f"  {m}", file=sys.stderr)
        return 1
    rule = "=" * 78
    parts = [
        f"Third-party software in phv ({target})",
        rule,
        "",
        "PHV itself is under the MIT licence (LICENSE). The phv executable also contains",
        "the following crates from crates.io (and the C code that some of them include),",
        "each under its own licence, whose notices follow.",
        "",
        *listed,
        "",
    ]
    for text, users in sorted(texts.items(), key=lambda t: t[1][0]):
        parts += [rule, *users, rule, "", text, ""]
    output.write_text("\n".join(parts) + "\n", encoding="utf-8", newline="\n")
    print(f"{output}: {len(listed)} crates, {len(texts)} licence texts")
    return 0


if __name__ == "__main__":
    sys.exit(main())
