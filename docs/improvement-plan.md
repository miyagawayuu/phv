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

## 1.2.x (patch releases)

### Databases and sessions

| Item | Code | Change | Risk | Measure | Size |
|---|---|---|---|---|---|
| Lazy session values | `crates/phv_server/src/session.rs` | Read and promote the session record as now, but decode its values only at the first `session()` call of the page | The `last` update, the promotion and the cookie must stay eager, or sessions expire differently | Home page p50 with a session cookie | M |
| Redis pipelining | `crates/phv_std/src/redisclient.rs`, `netdb.rs`, the spec | A `pipeline()` on the Redis client that sends several commands and reads their replies together; the job and session stores use it where they send more than one command | A new API: needs its section in the spec and the reference, and the reply limits apply per command | Jobs added per second in `databases.rs` | M |
| `phv jobs adopt` | `crates/phv_cli/src/jobs.rs`, `netdb.rs`, the spec | Move the jobs a site left in the shared store before it had a namespace (`phv jobs list` counts them) into the site's own table or keys | Must not take another site's jobs: only rows whose site name matches, with a dry run first | The test of `left_in_shared` extended | M |

### Runtime and compiler

| Item | Code | Change | Risk | Measure | Size |
|---|---|---|---|---|---|
| mimalloc under the counting allocator | `crates/phv_vm/src/alloc.rs`, `Cargo.toml` | Put mimalloc under `CountingAlloc` as the system allocator; the counting and the per-thread charging stay | A new C dependency in the supply chain (pinned, audited); Windows and musl builds must link it | `bench/run.*` rows (object creation, strings) | S |
| Lazy JIT | `crates/phv_jit/src/lib.rs`, `phv_vm` | Compile a function at its Nth call instead of at load, and share the compiled code of the standard library between scripts of a process | The eligibility test and `step()` do not change; a function must run identically before and after it is compiled (the differential test) | Compile time of a page (`phv_workers_busy` at the first requests), the micro-benchmarks unchanged | M |
| Callback fast path | `crates/phv_std/src/lists.rs`, `phv_vm` | `map()`, `filter()` and their kind call a closure without a full frame when it has no `finally` and no captures by the VM | Error positions and traces must not change | "map / filter / sum" | M |

### Tools

| Item | Code | Change | Risk | Measure | Size |
|---|---|---|---|---|---|
| Split `netdb.rs` | `crates/phv_std/src/netdb.rs` (5,767 lines) → `netdb/{pool,pg,my,redis,jobs,publish,ratelimit,migrate}.rs` | A mechanical split: no code changes, the tests stay where they are | None if mechanical; review the diff with `--color-moved` | The tests pass; `git log --follow` works per file | S |

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
