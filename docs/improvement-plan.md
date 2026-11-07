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
| JIT for objects, closures, methods and `echo` | `crates/phv_jit` | Compile the operations the interpreter still runs for objects and closures, and the output path | The JIT is part of the trust base: it gets its own review in the assessment, and the differential test grows with it | The rows of the README table that still show the same time with and without the JIT | L |
| Cached type-checked modules | `crates/phv_types`, `crates/phv_compiler/src/cache.rs` | Cache the checked HIR of a module keyed by its source and its imports, so a page's check reuses what did not change | Also bounds the residual risk of the sixteenth assessment (a check of 415 MB): a cache entry has the same limits as the compile | `phv check` of the test corpus; the editor's diagnostics latency | L |
| Worker pool with reused VMs | `crates/phv_driver`, `crates/phv_server` | Keep a VM per worker and reset its heap between requests, instead of a new VM per request | Needs an argument that the reset leaves nothing of the previous request (the heap, the kept statements, the output buffer): a test that runs two requests on one VM and compares the heap to a fresh one | Home page p50 | L |
| Generators, traits, i18n, images, an ORM | the language and `crates/phv_std` | As the roadmap says: additions only, each with its spec section | Each is a new surface for the assessment | — | L each |
| A smaller AST | `crates/phv_syntax/src/ast.rs` | Box the rare variants so that the common nodes are small, with `size_of` assertions in the tests | None for behaviour | Parse time of the corpus; memory of the editor with 20 pages | M |

## Measured already

The harness measured these in 1.2.0 (`bench/results/6da479b.json` against `ae06940.json`): the special-page cache
took a page three directories deep from 4,589 to 67,918 requests per second; the teardown after the response took
the PostgreSQL page's p50 from 1.61 to 0.90 ms; kept prepared statements made the SQLite page about 18% faster; and
fat LTO made the binary 1 MB smaller and the micro-benchmarks 3–9% faster.
