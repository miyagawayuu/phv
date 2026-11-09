#!/usr/bin/env python3
"""Latency of `phv lsp` in a scripted editing session.

The session: initialize, didOpen of the largest page of examples/site, then
--edits times a didChange (one line appended to the page) followed at once by a
completion request at the end of that line (after `$req->`), as an editor sends
them while someone types. Two latencies are taken per edit, as the editor sees
them: the completion answer (from sending the request) and the diagnostics of
the page (from sending the edit; the server answers the completion first, so
this includes it). Their median and p99 (nearest rank: with 20 edits, the
slowest one) go under "lsp" in bench/results/<short-sha>.json.

With --keystrokes K (more than 1), each edit types the line's start (`$editN =
$req->`) in K didChange notifications --interval-ms apart, as someone typing
does, and the completion request follows the last keystroke. The diagnostics
latency is then taken from the last keystroke to the last publishDiagnostics
the edit caused (the server is given a quiet moment to show there are no
more), and "checks" counts those publishDiagnostics per edit: one when the
server waits for the typing to pause, up to K when it checks every keystroke.

With --pages N, the session instead opens the N largest .phv files of examples/
one after the other (each waits for its diagnostics), as an editor with N
tabs does, and then reads the memory of the server: its working set (resident
set) and the peak of it. These go under "lsp_pages".

    python bench/lsp.py [--edits 20] [--keystrokes 1] [--interval-ms 50] [--pages N] [--phv target/release/phv] [--out bench/results/<sha>.json]
"""

import argparse
import json
import os
import queue
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "examples" / "site"


def short_sha() -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"], text=True).strip()


def default_phv() -> Path:
    target = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
    return target / "release" / ("phv.exe" if os.name == "nt" else "phv")


def largest_page() -> Path:
    pages = sorted((SITE / "public").rglob("*.phv"), key=lambda p: (p.stat().st_size, str(p)))
    return pages[-1]


def largest_files(n: int) -> list[Path]:
    # Not templates (`.phv.html`): the pages that render them publish their diagnostics.
    files = list((ROOT / "examples").rglob("*.phv"))
    return sorted(files, key=lambda p: (-p.stat().st_size, str(p)))[:n]


def memory(pid: int) -> tuple[int, int]:
    """The working set (resident set) of a process and its peak, in bytes."""
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)
        kernel32.OpenProcess.restype = wintypes.HANDLE
        handle = kernel32.OpenProcess(0x0400 | 0x0010, False, pid)  # query information, read
        if not handle:
            raise OSError(ctypes.get_last_error(), "OpenProcess failed")
        try:
            c = Counters()
            c.cb = ctypes.sizeof(c)
            if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(c), c.cb):
                raise OSError(ctypes.get_last_error(), "GetProcessMemoryInfo failed")
            return c.WorkingSetSize, c.PeakWorkingSetSize
        finally:
            kernel32.CloseHandle(handle)
    status = Path(f"/proc/{pid}/status")
    if status.exists():
        fields = {}
        for line in status.read_text().splitlines():
            name, _, value = line.partition(":")
            fields[name] = value.split()
        return int(fields["VmRSS"][0]) * 1024, int(fields["VmHWM"][0]) * 1024
    # macOS: ps knows the resident set, not its peak.
    rss = int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True).strip()) * 1024
    return rss, rss


def open_pages(client: "Client", n: int) -> dict:
    files = largest_files(n)
    if len(files) < n:
        print(f"warning: examples/ has only {len(files)} files to open", file=sys.stderr)
    start = time.perf_counter()
    for i, path in enumerate(files):
        uri = path.as_uri()
        client.notify(
            "textDocument/didOpen",
            {"textDocument": {"uri": uri, "languageId": "phv", "version": 0, "text": path.read_text(encoding="utf-8")}},
        )
        client.wait(is_diagnostics_for(uri))
    open_ms = (time.perf_counter() - start) * 1000
    # Work that follows the last diagnostics (keeping the analysis) ends first.
    time.sleep(1.0)
    rss, peak = memory(client.proc.pid)
    return {
        "pages": len(files),
        "bytes": sum(p.stat().st_size for p in files),
        "open_ms": round(open_ms, 3),
        "rss_bytes": rss,
        "peak_rss_bytes": peak,
    }


def percentile(values: list[float], q: float) -> float:
    ordered = sorted(values)
    rank = min(len(ordered), max(1, int(-(-q * len(ordered) // 1))))  # ceil
    return ordered[rank - 1]


class Client:
    """A JSON-RPC client over the server's stdin/stdout (Content-Length framing)."""

    def __init__(self, phv: Path):
        self.proc = subprocess.Popen(
            [str(phv), "lsp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=str(ROOT)
        )
        self.inbox: queue.Queue = queue.Queue()
        self.pending: list[tuple[float, dict]] = []  # arrived, not yet asked for
        self.next_id = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        out = self.proc.stdout
        while True:
            length = None
            while True:
                line = out.readline()
                if not line:
                    self.inbox.put((time.perf_counter(), None))
                    return
                if line in (b"\r\n", b"\n"):
                    break
                name, _, value = line.decode("ascii", "replace").partition(":")
                if name.strip().lower() == "content-length":
                    length = int(value.strip())
            if length is None:
                continue
            body = out.read(length)
            self.inbox.put((time.perf_counter(), json.loads(body)))

    def send(self, msg: dict) -> float:
        body = json.dumps(msg).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.proc.stdin.flush()
        return time.perf_counter()

    def notify(self, method: str, params: dict) -> float:
        return self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method: str, params: dict) -> tuple[int, float]:
        self.next_id += 1
        return self.next_id, self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})

    def wait(self, matches, timeout: float = 60.0) -> tuple[float, dict]:
        """The first message `matches` accepts, with the time it arrived; others are kept
        for a later wait (the server may publish diagnostics before answering a request)."""
        for i, (at, msg) in enumerate(self.pending):
            if matches(msg):
                del self.pending[i]
                return at, msg
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError("no answer from phv lsp within %.0f s" % timeout)
            try:
                at, msg = self.inbox.get(timeout=left)
            except queue.Empty:
                raise TimeoutError("no answer from phv lsp within %.0f s" % timeout) from None
            if msg is None:
                err = self.proc.stderr.read().decode("utf-8", "replace")
                raise RuntimeError("phv lsp ended: " + err)
            if matches(msg):
                return at, msg
            self.pending.append((at, msg))

    def wait_until_quiet(self, matches, quiet: float, timeout: float = 60.0) -> tuple[float, dict, int]:
        """The last message `matches` accepts (the first within `timeout`), once none
        came for `quiet` seconds, with the time it arrived and how many were accepted."""
        at, msg = self.wait(matches, timeout)
        count = 1
        while True:
            try:
                at, msg = self.wait(matches, quiet)
            except TimeoutError:
                return at, msg, count
            count += 1

    def close(self) -> None:
        try:
            rid, _ = self.request("shutdown", {})
            self.wait(lambda m: m.get("id") == rid, timeout=10)
            self.notify("exit", {})
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def is_diagnostics_for(uri: str):
    return lambda m: m.get("method") == "textDocument/publishDiagnostics" and m["params"]["uri"] == uri


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--edits", type=int, default=20)
    ap.add_argument("--keystrokes", type=int, default=1, help="didChange notifications per edit (typing)")
    ap.add_argument("--interval-ms", type=float, default=50.0, help="between keystrokes")
    ap.add_argument("--pages", type=int, default=0, help="open this many files of examples/ and read the memory")
    ap.add_argument("--phv", type=Path, default=None)
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()
    if args.keystrokes < 1:
        print("error: --keystrokes must be at least 1", file=sys.stderr)
        return 1
    phv = args.phv or default_phv()
    if not phv.exists():
        print(f"error: {phv} does not exist; build it with: cargo build --release -p phv_cli", file=sys.stderr)
        return 1
    sha = short_sha()
    out = args.out or ROOT / "bench" / "results" / f"{sha}.json"

    if args.pages > 0:
        client = Client(phv)
        try:
            rid, _ = client.request(
                "initialize",
                {"processId": os.getpid(), "rootUri": SITE.as_uri(), "capabilities": {}, "clientInfo": {"name": "bench"}},
            )
            client.wait(lambda m: m.get("id") == rid)
            client.notify("initialized", {})
            section = open_pages(client, args.pages)
        finally:
            client.close()
        doc = json.loads(out.read_text(encoding="utf-8")) if out.exists() else {"commit": sha}
        doc["lsp_pages"] = section
        out.parent.mkdir(parents=True, exist_ok=True)
        with open(out, "w", encoding="utf-8", newline="") as f:
            json.dump(doc, f, indent=2)
            f.write("\n")
        print(f"{section['pages']} files of examples/ ({section['bytes']} bytes) opened in {section['open_ms']:.1f} ms")
        print(f"  memory  {section['rss_bytes'] / 2**20:.1f} MiB  peak {section['peak_rss_bytes'] / 2**20:.1f} MiB")
        print(f"Wrote the lsp_pages section of {out}")
        return 0

    page = largest_page()
    uri = page.as_uri()
    text = page.read_text(encoding="utf-8")
    base = text.rstrip("\n") + "\n"
    if "$req = request();" not in text:
        print(f"warning: {page} does not define $req; completions after `$req->` will be empty", file=sys.stderr)

    client = Client(phv)
    try:
        rid, _ = client.request(
            "initialize",
            {"processId": os.getpid(), "rootUri": SITE.as_uri(), "capabilities": {}, "clientInfo": {"name": "bench"}},
        )
        client.wait(lambda m: m.get("id") == rid)
        client.notify("initialized", {})

        sent = client.notify(
            "textDocument/didOpen", {"textDocument": {"uri": uri, "languageId": "phv", "version": 0, "text": text}}
        )
        at, _ = client.wait(is_diagnostics_for(uri))
        open_ms = (at - sent) * 1000

        diagnostics_ms: list[float] = []
        completion_ms: list[float] = []
        checks: list[int] = []
        items = 0
        keystrokes = args.keystrokes
        # Long enough for a check that is under way to end: a few times one check.
        quiet = max(0.3, 3 * open_ms / 1000)
        version = 0
        for i in range(1, args.edits + 1):
            prefix = f"$edit{i} = $req->"
            if keystrokes == 1:
                typed = [f'{prefix}query("q{i}");\n']
            else:
                # The line's start, a few characters per keystroke; the last one types `>`.
                typed = [prefix[: -(-len(prefix) * j // keystrokes)] + "\n" for j in range(1, keystrokes + 1)]
            for j, text_now in enumerate(typed):
                if j:
                    time.sleep(args.interval_ms / 1000)
                version += 1
                sent_edit = client.notify(
                    "textDocument/didChange",
                    {"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": base + text_now}]},
                )
            rid, sent_completion = client.request(
                "textDocument/completion",
                {
                    "textDocument": {"uri": uri},
                    "position": {"line": base.count("\n"), "character": len(prefix)},
                    "context": {"triggerKind": 2, "triggerCharacter": ">"},
                },
            )
            at, reply = client.wait(lambda m, rid=rid: m.get("id") == rid)
            completion_ms.append((at - sent_completion) * 1000)
            items += len(reply.get("result") or [])
            if keystrokes == 1:
                at, _ = client.wait(is_diagnostics_for(uri))
            else:
                at, _, count = client.wait_until_quiet(is_diagnostics_for(uri), quiet)
                checks.append(count)
            diagnostics_ms.append((at - sent_edit) * 1000)
    finally:
        client.close()

    if items == 0:
        print("warning: every completion request returned no items", file=sys.stderr)
    section = {
        "document": page.relative_to(ROOT).as_posix(),
        "document_bytes": len(text.encode("utf-8")),
        "edits": args.edits,
        "open_ms": round(open_ms, 3),
        "diagnostics_ms": {
            "median": round(statistics.median(diagnostics_ms), 3),
            "p99": round(percentile(diagnostics_ms, 0.99), 3),
        },
        "completion_ms": {
            "median": round(statistics.median(completion_ms), 3),
            "p99": round(percentile(completion_ms, 0.99), 3),
        },
    }
    if keystrokes > 1:
        section["keystrokes"] = keystrokes
        section["interval_ms"] = args.interval_ms
        section["checks_per_edit"] = round(statistics.mean(checks), 2)
    doc = json.loads(out.read_text(encoding="utf-8")) if out.exists() else {"commit": sha}
    # A typing session is a measurement of its own, beside the plain one.
    doc["lsp_typing" if keystrokes > 1 else "lsp"] = section
    out.parent.mkdir(parents=True, exist_ok=True)
    with open(out, "w", encoding="utf-8", newline="") as f:
        json.dump(doc, f, indent=2)
        f.write("\n")
    typing = f", {keystrokes} keystrokes {args.interval_ms:g} ms apart" if keystrokes > 1 else ""
    print(f"{section['document']}: open {open_ms:.1f} ms; over {args.edits} edits{typing}:")
    print(f"  diagnostics  median {section['diagnostics_ms']['median']:.2f} ms  p99 {section['diagnostics_ms']['p99']:.2f} ms")
    print(f"  completion   median {section['completion_ms']['median']:.2f} ms  p99 {section['completion_ms']['p99']:.2f} ms")
    if keystrokes > 1:
        print(f"  checks       {section['checks_per_edit']:.2f} per edit")
    print(f"Wrote the {'lsp_typing' if keystrokes > 1 else 'lsp'} section of {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
