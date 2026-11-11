"""Compare PHV and PHP 8.5 CLI workloads with and without JIT."""

import argparse
import json
import os
import platform
import shutil
import statistics
import tempfile
from decimal import Decimal
from pathlib import Path

from performance import ROOT, command, environment, runtime_rows


PHP_CHECK = """
$status = function_exists('opcache_get_status') ? opcache_get_status(false) : false;
echo json_encode([
    'version' => PHP_VERSION,
    'version_id' => PHP_VERSION_ID,
    'opcache' => $status['opcache_enabled'] ?? false,
    'jit' => $status['jit']['on'] ?? false,
]);
"""


def program(name):
    """The executable `name` names: a path with a folder (`./phv`,
    `target/release/phv.exe`) as given; a bare name only in the folders of PATH
    that are absolute. Never the current folder, which Windows searches first for
    a bare name, and so does `shutil.which`: a `php.bat` there ran
    (20th assessment, R23-24)."""
    seps = [os.sep] + ([os.altsep] if os.altsep else [])
    if any(s in name for s in seps):
        found = shutil.which(str(Path(name).resolve()))
        return str(Path(found).resolve()) if found else None
    # Looked up here: `shutil.which(name, path=folder)` on Windows tries the
    # current folder first, even with a folder given.
    suffixes = [""]
    if os.name == "nt":
        exts = [e for e in os.environ.get("PATHEXT", ".COM;.EXE;.BAT;.CMD").split(os.pathsep) if e]
        if not any(name.lower().endswith(e.lower()) for e in exts):
            suffixes = exts
    for folder in os.environ.get("PATH", "").split(os.pathsep):
        if not folder or not os.path.isabs(folder):
            continue
        for suffix in suffixes:
            candidate = os.path.join(folder, name + suffix)
            if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                return str(Path(candidate).resolve())
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phv", required=True, help="path to a PHV release binary")
    parser.add_argument("--php", default="php", help="PHP 8.5 executable with OPcache loaded")
    parser.add_argument("--runs", type=int, default=7, help="samples per mode (default: 7)")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    phv, php = program(args.phv), program(args.php)
    if not phv or not php:
        parser.error("--phv and --php must identify executable CLI binaries (a path, or a name found in PATH)")

    source = ROOT / "bench/runtime.phv"
    # Keep the workloads and measurement boundaries identical across the two languages.
    php_source = (source.read_text(encoding="utf-8")
                  .replace("<?phv", "<?php", 1)
                  .replace("Generator<int, int>", "Generator")
                  .replace("microtime()", "microtime(true)"))
    samples = {mode: [] for mode in ("phv_jit", "php_jit", "phv_interp", "php_opcache")}
    with tempfile.TemporaryDirectory(prefix="phv-php-bench-") as work:
        root = Path(work)
        shutil.copyfile(source, root / "runtime.phv")
        (root / "runtime.php").write_text(php_source, encoding="utf-8")
        commands = {}
        versions = {"phv": command([phv, "--version"], root, environment()).strip()}
        for jit in (True, False):
            env = environment(jit)
            settings = [
                "-d", "opcache.enable=1", "-d", "opcache.enable_cli=1",
                "-d", "opcache.file_update_protection=0", "-d", "opcache.file_cache=",
                "-d", "opcache.jit_buffer_size=64M",
                "-d", f"opcache.jit={'tracing' if jit else 'disable'}",
            ]
            php_command = [php, *settings]
            info = json.loads(command([*php_command, "-r", PHP_CHECK], root, env))
            if not 80500 <= info["version_id"] < 80600:
                parser.error(f"PHP 8.5 is required, found {info['version']}")
            if not info["opcache"] or info["jit"] != jit:
                parser.error(f"PHP CLI must have OPcache active and JIT {'on' if jit else 'off'}")
            versions["php"] = info["version"]
            commands["phv_jit" if jit else "phv_interp"] = ([phv, "run", "runtime.phv"], env)
            commands["php_jit" if jit else "php_opcache"] = ([*php_command, "runtime.php"], env)

        modes = tuple(samples)
        expected = None
        for number in range(args.runs):
            offset = number % len(modes)
            for mode in modes[offset:] + modes[:offset]:
                argv, env = commands[mode]
                rows = runtime_rows(command(argv, root, env))
                checksums = {key: Decimal(row["checksum"]) for key, row in rows.items()}
                if expected is None:
                    expected = checksums
                elif checksums != expected:
                    different = ", ".join(key for key in rows if checksums[key] != expected[key])
                    raise RuntimeError(f"{mode} run {number + 1}: checksums differ for {different}")
                samples[mode].append(rows)

    medians = {mode: {key: statistics.median(row[key]["ms"] for row in runs) for key in runs[0]}
               for mode, runs in samples.items()}
    output = {"versions": versions, "platform": platform.platform(), "machine": platform.machine(),
              "samples": samples, "median_ms": medians}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2) + "\n", encoding="utf-8")
    print(f"Median ms ({args.runs} runs per mode)")
    print(f"{'Workload':<27}" + "".join(f"{mode:>14}" for mode in modes))
    for key in medians["phv_jit"]:
        print(f"{key:<27}" + "".join(f"{medians[mode][key]:>14.3f}" for mode in modes))


if __name__ == "__main__":
    main()
