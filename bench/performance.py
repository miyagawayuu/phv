"""Compare two PHV binaries on the same runtime and local HTTP workloads."""

import argparse
import concurrent.futures
import ctypes
import http.client
import json
import math
import os
import re
import shutil
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PAGES = ("/", "/archive/2026/11", "/guestbook", "/bench-db", "/bench-object")
COLD_PAGES = tuple(f"/bench-cold-{i}" for i in range(16))
EXISTING_ROWS = ("fib", "loop", "append", "mandelbrot", "concat", "map_filter", "map_updates", "sieve", "list_floats",
                 "prop_append", "string_eq", "escape", "new_objects", "throw_catch", "sort_by")
RUNTIME_ROWS = {"property_reads", "generator_yield", "generator_wide_frame", "generator_active_handler",
                "generator_delegated", "generator_close"}


def environment(jit=True, scalar=False, stats=False):
    env = os.environ.copy()
    for key, enabled, value in (
        ("PHV_JIT", not jit, "0"),
        ("PHV_JIT_SCALAR_PROPERTIES", scalar, "1"),
        ("PHV_JIT_STATS", stats, "1"),
    ):
        if enabled:
            env[key] = value
        else:
            env.pop(key, None)
    return env


def command(args, cwd, env):
    result = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=180)
    if result.returncode:
        raise RuntimeError(f"{args[0]} exited {result.returncode}\n{result.stdout}{result.stderr}")
    return result.stdout


def runtime_rows(output):
    rows = {}
    for key, elapsed, checksum in re.findall(r"^(\w+): ([\d,.]+) ms \(([^)]+)\)$", output, re.M):
        rows[key] = {"ms": float(elapsed.replace(",", "")), "checksum": checksum}
    if rows.keys() != RUNTIME_ROWS:
        raise RuntimeError(output)
    return rows


def runtime(binary, root, scalar, existing):
    result = {}
    for mode, jit in (("jit", True), ("interp", False)):
        env = environment(jit, scalar)
        output = command([binary, "run", "runtime.phv"], root, env)
        rows = runtime_rows(output)
        if existing:
            output = command([binary, "run", "bench.phv"], root, env)
            times = re.findall(r"([\d,.]+) ms\s*$", output, re.M)
            if len(times) != len(EXISTING_ROWS):
                raise RuntimeError(output)
            rows.update({key: {"ms": float(elapsed.replace(",", ""))}
                         for key, elapsed in zip(EXISTING_ROWS, times)})
        result[mode] = rows
    if any(row.get("checksum") != result["interp"][key].get("checksum") for key, row in result["jit"].items()):
        raise RuntimeError("JIT and interpreter checksums differ")
    return result


def save_generator_profile(binary, root, output):
    with output.open("w", encoding="utf-8") as log:
        subprocess.run([binary, "run", "--profile", "runtime.phv"], cwd=root, env=environment(False),
                       stdout=log, stderr=subprocess.STDOUT, check=True, timeout=180)


def png_fixture():
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    width, height = 1024, 768
    rows = b"".join(b"\0" + bytes((x + y) % 256 for x in range(width * 3)) for y in range(height))
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def request(port, path):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
    try:
        conn.request("GET", path)
        response = conn.getresponse()
        body = response.read()
        if response.status != 200:
            raise RuntimeError(f"{path}: HTTP {response.status}")
        return body
    finally:
        conn.close()


def peak_rss(pid):
    if sys.platform == "linux":
        text = Path(f"/proc/{pid}/status").read_text()
        return int(re.search(r"^VmHWM:\s+(\d+) kB", text, re.M)[1]) * 1024
    if sys.platform == "win32":
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [("size", wintypes.DWORD), ("faults", wintypes.DWORD)] + [
                (name, ctypes.c_size_t) for name in ("peak_rss", "rss", "a", "b", "c", "d", "e", "f")
            ]

        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.CloseHandle.argtypes = (wintypes.HANDLE,)
        handle = kernel.OpenProcess(0x0410, False, pid)
        if not handle:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            memory = Counters()
            memory.size = ctypes.sizeof(memory)
            read = kernel.K32GetProcessMemoryInfo
            read.argtypes = (wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD)
            if not read(handle, ctypes.byref(memory), memory.size):
                raise ctypes.WinError(ctypes.get_last_error())
            return memory.peak_rss
        finally:
            kernel.CloseHandle(handle)
    return None


def prepare_site(site, port, workers, image):
    shutil.copytree(ROOT / "examples/site", site, ignore=shutil.ignore_patterns(".phv-cache", "*.db*"))
    config = site / "phv.toml"
    config.write_text(config.read_text().replace('write = ["data/"]', 'read = ["data/"]\nwrite = ["data/"]')
                      + f'\n[server]\nworkers = {workers}\n\n[admin]\nlisten = "127.0.0.1:{port}"\n', encoding="utf-8")
    (site / "data").mkdir(exist_ok=True)
    (site / "data/bench.png").write_bytes(image)
    (site / "seed.phv").write_text('''<?phv
import lib.guestbook.{guestbook_db, add_entry};
$db = guestbook_db();
foreach (range(1, 20) as $i) {
    add_entry($db, "Visitor " . $i, "Benchmark entry");
}
''', encoding="utf-8")
    (site / "public/bench-db.phv").write_text('''<?phv
import std.db.Database;
$db = Database::open(":memory:");
$db->execute(sql"CREATE TABLE bench_count (value INTEGER NOT NULL)");
$db->execute(sql"INSERT INTO bench_count VALUES (0)");
$db->execute(sql"UPDATE bench_count SET value = value + 1");
echo "ok";
''', encoding="utf-8")
    (site / "public/bench-image.phv").write_text('''<?phv
import std.http.response;
import std.image.Image;
try {
    Image::open("data/bench.png")->fit(512, 512)->send("jpeg");
} catch (RuntimeException $e) {
    if ($e->getMessage() != "all image conversion slots are busy") {
        throw $e;
    }
    response()->status(429);
    echo "image conversion busy";
}
''', encoding="utf-8")
    runtime_source = (ROOT / "bench/runtime.phv").read_text(encoding="utf-8")
    object_page = runtime_source.split("function numbers(", 1)[0] + "echo read_properties(new BenchPoint(3, 1.5, true), 10000);\n"
    for path in ("/bench-object", *COLD_PAGES):
        (site / f"public{path}.phv").write_text(object_page, encoding="utf-8")


def server(binary, load, root, label, args, scalar, image, jit=True):
    port, admin = free_port(), free_port()
    while admin == port:
        admin = free_port()
    site = root / label
    prepare_site(site, admin, args.workers, image)
    env = environment(jit, scalar, stats=True)
    command([binary, "run", "seed.phv"], site, env)
    log_path = args.output.with_suffix(f".{label}.log")
    with log_path.open("wb") as log:
        process = subprocess.Popen([binary, "serve", str(site), "--prod", "--host", "127.0.0.1", "--port", str(port)],
                                   stdout=log, stderr=log, env=env)
        try:
            for _ in range(200):
                if process.poll() is not None:
                    raise RuntimeError(f"server exited, see {log_path}")
                try:
                    request(admin, "/ready")
                    break
                except (OSError, RuntimeError) as error:
                    last_error = error
                    time.sleep(0.05)
            else:
                raise RuntimeError(f"server did not become ready, see {log_path}") from last_error

            ready = threading.Barrier(len(COLD_PAGES))

            def cold(path):
                ready.wait(timeout=20)
                start = time.perf_counter()
                request(port, path)
                return (time.perf_counter() - start) * 1000

            with concurrent.futures.ThreadPoolExecutor(max_workers=len(COLD_PAGES)) as pool:
                cold_ms = sorted(pool.map(cold, COLD_PAGES))
            result = {"cold_ms": cold_ms, "cold_p99_ms": cold_ms[math.ceil(len(cold_ms) * .99) - 1]}
            if jit:
                result["http"] = {}
                for count in sorted({16, 2 * args.workers}):
                    for path, mix in [*((path, False) for path in PAGES), ("/", True)]:
                        cmd = [load, "--url", f"http://127.0.0.1:{port}", "--path", path,
                               "--connections", str(count), "--requests", str(args.requests), "--json"]
                        if mix:
                            cmd += ["--image-path", "/bench-image"]
                        result["http"][f"{count}:{path}{':mixed' if mix else ''}"] = json.loads(command(cmd, site, env))
                metrics = request(admin, "/metrics").decode()
                result["worker_wait"] = [line for line in metrics.splitlines() if line.startswith("phv_worker_wait_seconds_")]
                result["peak_rss_bytes"] = peak_rss(process.pid)
        finally:
            process.terminate()
            process.wait(timeout=20)
    jit_log = log_path.read_text(encoding="utf-8", errors="replace")
    result["jit_compile"] = [line for line in jit_log.splitlines() if line.startswith("phv jit: read_properties:")]
    return result


def median_fields(rows, fields):
    return {field: statistics.median(row[field] for row in rows) for field in fields if rows[0][field] is not None}


def summarize(runs):
    result = {mode: {key: statistics.median(row["runtime"][mode][key]["ms"] for row in runs)
                     for key in runs[0]["runtime"][mode]} for mode in ("jit", "interp")}
    if "server" in runs[0]:
        result["server"] = median_fields([r["server"] for r in runs], ("cold_p99_ms", "peak_rss_bytes"))
        result["server"]["cold_interp_p99_ms"] = statistics.median(r["server_interp"]["cold_p99_ms"] for r in runs)
        result["http"] = {key: median_fields([r["server"]["http"][key] for r in runs],
                                              ("p50_ms", "p99_ms", "successful_requests_per_s"))
                          for key in runs[0]["server"]["http"]}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--load", type=Path, help="include HTTP using the phv_server load example")
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--workers", type=int, default=16)
    parser.add_argument("--requests", type=int, default=500)
    parser.add_argument("--scalar-properties", action="store_true",
                        help="enable scalar property reads in older candidate binaries that required the opt-in")
    parser.add_argument("--profile-generators", action="store_true", help="save a separate interpreter profile")
    parser.add_argument("--existing-runtime", action="store_true", help="also run the existing runtime benchmark rows")
    args = parser.parse_args()
    if min(args.runs, args.workers, args.requests) < 1:
        parser.error("runs, workers and requests must be positive")
    args.output = args.output.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    if args.load:
        args.load = args.load.resolve()
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    results = {"runs": {name: [] for name in binaries}, "workers": args.workers, "requests": args.requests,
               "scalar_properties": args.scalar_properties}
    image = png_fixture() if args.load else None
    with tempfile.TemporaryDirectory(prefix="phv-perf-", dir=args.output.parent) as work:
        root = Path(work)
        shutil.copy(ROOT / "bench/runtime.phv", root)
        if args.existing_runtime:
            shutil.copy(ROOT / "examples/bench/bench.phv", root)
        for number in range(args.runs):
            names = tuple(binaries) if number % 2 == 0 else tuple(reversed(binaries))
            for name in names:
                binary = binaries[name]
                scalar = args.scalar_properties and name == "candidate"
                run = {"runtime": runtime(binary, root, scalar, args.existing_runtime)}
                if args.load:
                    run["server"] = server(binary, args.load, root, f"{name}-{number + 1}", args, scalar, image)
                    run["server_interp"] = server(binary, args.load, root, f"{name}-interp-{number + 1}", args, False, image, jit=False)
                results["runs"][name].append(run)
                args.output.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
            previous, current = (results["runs"][name][-1]["runtime"]["jit"] for name in binaries)
            if any(row.get("checksum") != current[key].get("checksum") for key, row in previous.items()):
                raise RuntimeError("baseline and candidate checksums differ")
            print(f"{number + 1}/{args.runs} pairs complete", flush=True)
        if args.profile_generators:
            for name, binary in binaries.items():
                save_generator_profile(binary, root, args.output.with_suffix(f".{name}-generator-profile.log"))
    results["median"] = {name: summarize(runs) for name, runs in results["runs"].items()}
    args.output.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
