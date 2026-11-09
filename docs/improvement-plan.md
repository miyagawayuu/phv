# Improvement Plan

English | [日本語](improvement-plan.ja.md)

What the 1.2.0 work left for later, found while measuring and assessing the implementation (the benchmark harness in
`bench/`, the sixteenth and seventeenth assessments in [security-audit.md](security-audit.md)). Each item names the
code it touches, what changes, what could go wrong, how the gain is measured, and its size (S: hours, M: a day or
two, L: a week or more). The roadmap in the [specification](spec.md) §19 says which release each group is meant
for; this file is the working list, and an item is removed when it lands or is dropped.

Rules that hold for every item: the behaviour the specification promises does not change unless the specification
changes in the same commit (§18 says what may change); a change that touches performance comes with its
before/after `bench/results/*.json`; and the security-critical items are done first in a cycle and assessed again
before the release.

## 1.3 (the next minor release)

| Item | Code | Change | Risk | Measure | Size |
|---|---|---|---|---|---|
| Generators, traits, i18n, images, an ORM | the language and `crates/phv_std` | As the roadmap says: additions only, each with its spec section | Each is a new surface for the assessment | — | L each |

## Later

| Item | Code | Change | Risk | Measure | Size |
|---|---|---|---|---|---|
| Cheaper native object operations | `crates/phv_jit`, `crates/phv_vm/src/jit.rs` | Since 1.3 objects, methods and `echo` are compiled, but each property read or write and each `new` is a call into the runtime that costs what the interpreter's instruction does; read and write properties by slot in native code | The JIT is part of the trust base; the slot layout is part of the unit key (§28, R21-4) | A method that walks objects (a tree's sum); the `new` row of the README table | L |

Closures were measured and left out of the JIT: their bodies are short and called by builtins, and entering native
code costs more than such a body, so `map()`, `filter()` and `sortBy()` would not gain.

## Measured already

The harness measured these in 1.2.0 (`bench/results/6da479b.json` against `ae06940.json`): the special-page cache
took a page three directories deep from 4,589 to 67,918 requests per second; the teardown after the response took
the PostgreSQL page's p50 from 1.61 to 0.90 ms; kept prepared statements made the SQLite page about 18% faster; and
fat LTO made the binary 1 MB smaller and the micro-benchmarks 3–9% faster.

In the 1.3 cycle, reusing VMs between requests was measured and dropped: making a VM and dropping its empty shell
take about 0.43 µs, 0.2% of the home page's p50 of 230 µs, and the whole run of the page is 2.4% of it. A page that
echoes one word answers as fast as the home page, so the time of a request is in the HTTP path, not in the VM; a
reset would have added the burden of proving that nothing of a request survives into the next. Profiled instead,
the trivial page's time was about three quarters the client, the loopback and hyper; the server's own share went
mostly to two thread hand-offs and a wake of the log thread. A pool of page threads and batched log writes took the
p50 down 8–9% and the requests per second up 10–12%.

A cache of type-checked modules was measured and dropped too: the bodies of imported modules and all declarations
take 1–2.5% of `phv check` of the test corpus, 3–4% of a page's first compile, and under 1% of the editor's
diagnostics. The checker numbers functions and classes and counts its budgets over the whole program, so a correct
cache would have meant rebuilding it. The profile found the time elsewhere: parsing the builtin signatures (0.57 ms
per process, about 4% of a `phv check`) and the loader's file-system work (2–5 ms per analysis, about half of a
page's first compile). In 1.3 the builtin signatures are read by a parser of their own (85 µs instead of 450),
the loader keeps the text and hash of an unchanged file, and `phv check` takes several files in one process.
