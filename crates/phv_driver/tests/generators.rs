//! Generators (`yield`) keep the script's limits: the frames of generators
//! count against the memory limit, a long generator stops at the time limit,
//! recursion through `yield from` at the call-depth limit, and a generator
//! whose frame refers to an object holding it is freed by the cycle collector.

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak, CountingAlloc};
use phv_vm::Limits;
use std::time::{Duration, Instant};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const LIMIT: usize = 16 << 20;

/// Runs `src`; returns its output and first error line, and the most memory
/// the run held at once.
fn run_limited(name: &str, src: &str, limits: Limits) -> (String, usize) {
    let dir = std::env::temp_dir().join(format!("phv-generators-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let base = thread_allocated();
    reset_thread_peak();
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
    let peak = (thread_peak() - base).max(0) as usize;
    let _ = std::fs::remove_dir_all(&dir);
    let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
    (format!("{}{}", outcome.output, err.unwrap_or_default()), peak)
}

fn memory_limited() -> Limits {
    Limits { max_memory: Some(LIMIT), ..Limits::default() }
}

/// Generators that were made and not run yet keep their frames: a script
/// making millions of them stops at its memory limit.
#[test]
fn the_frames_of_generators_count_against_the_memory_limit() {
    let src = r#"<?phv
function rows(int $from, string $label): Generator<string> {
    for ($i = $from; $i < $from + 3; $i++) {
        yield "$label $i";
    }
}
list<Generator<string>> $waiting = [];
for ($i = 0; $i < 5000000; $i++) {
    $waiting[] = rows($i, "row");
}
echo count($waiting);
"#;
    let (out, peak) = run_limited("many", src, memory_limited());
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
    assert!(peak < 2 * LIMIT, "held {} MB", peak >> 20);
}

/// Generators waiting at a `yield`, nested through `yield from`, count too.
#[test]
fn waiting_generators_count_against_the_memory_limit() {
    let src = r#"<?phv
function nest(int $n, list<int> $pad): Generator<int> {
    $mine = $pad;
    $mine[] = $n;
    yield $n;
    if ($n < 9000) {
        yield from nest($n + 1, $mine);
    }
}
$total = 0;
foreach (nest(0, range(1, 200)) as $v) {
    $total += $v;
}
echo $total;
"#;
    let (out, peak) = run_limited("nested", src, memory_limited());
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
    assert!(peak < 2 * LIMIT, "held {} MB", peak >> 20);
}

/// A generator that never ends stops at the time limit, whether the loop
/// over it or the generator itself does the work.
#[test]
fn a_long_generator_stops_at_the_time_limit() {
    let cases = [
        (
            "endless",
            "function g(): Generator<int> { $i = 0; while (true) { yield $i; $i++; } }\nforeach (g() as $v) {}\n",
        ),
        (
            "busy",
            "function g(): Generator<int> { $n = 0; while (true) { $n++; if ($n < 0) { yield $n; } } }\n\
             foreach (g() as $v) { echo $v; }\n",
        ),
        (
            "list",
            "function g(): Generator<int> { $i = 0; while (true) { yield $i; $i++; } }\necho count(g()->toList());\n",
        ),
    ];
    for (name, body) in cases {
        let src = format!("<?phv\n{body}");
        let limits =
            Limits { max_time: Some(Duration::from_millis(500)), max_memory: Some(64 << 20), ..Limits::default() };
        let start = Instant::now();
        let (out, _) = run_limited(name, &src, limits);
        assert!(start.elapsed() < Duration::from_secs(20), "{name} took {:?}", start.elapsed());
        assert!(
            out.contains("maximum execution time") || (name == "list" && out.contains("allowed memory size")),
            "{name}: {out}"
        );
    }
}

/// Recursion through `yield from` keeps the call-depth limit (each level is a
/// frame while it runs).
#[test]
fn recursion_through_yield_from_keeps_the_depth_limit() {
    let src = r#"<?phv
function deep(int $n): Generator<int> {
    if ($n == 0) {
        yield 0;
        return;
    }
    yield from deep($n - 1);
}
foreach (deep(5000) as $v) {
    echo "ok $v\n";
}
foreach (deep(50000) as $v) {
    echo "never $v\n";
}
"#;
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let (out, _) = worker.spawn(move || run_limited("deep", src, Limits::default())).unwrap().join().unwrap();
    assert_eq!(out, "ok 0\nFatal error: maximum call stack depth exceeded", "{out}");
}

#[test]
fn nested_delegation_preserves_keys_and_unwinds_after_resuming() {
    let src = r#"<?phv
function leaf(bool $fail): Generator<int> {
    try {
        yield 5 => 10;
        yield 20;
        if ($fail) {
            throw new RuntimeException("leaf failed");
        }
        yield 2 => 30;
        yield 40;
    } finally {
        echo "leaf finally\n";
    }
}

function relay(int $depth, bool $fail): Generator<int> {
    try {
        if ($depth == 0) {
            yield from leaf($fail);
        } else {
            yield from relay($depth - 1, $fail);
        }
        yield 99;
    } finally {
        echo "relay $depth finally\n";
    }
}

function item(int $key, int $value): void {
    echo "$key:$value\n";
}

foreach (relay(1, false) as $key => $value) {
    item($key, $value);
}
foreach (relay(1, false) as $key => $value) {
    item($key, $value);
    if ($value == 20) {
        break;
    }
}
try {
    foreach (relay(1, true) as $key => $value) {
        item($key, $value);
    }
} catch (RuntimeException $e) {
    echo $e->getMessage(), "\n";
}
"#;
    let (out, _) = run_limited("forward", src, Limits::default());
    assert_eq!(
        out,
        concat!(
            "5:10\n6:20\n2:30\n7:40\nleaf finally\n0:99\nrelay 0 finally\n0:99\nrelay 1 finally\n",
            "5:10\n6:20\nleaf finally\nrelay 0 finally\nrelay 1 finally\n",
            "5:10\n6:20\nleaf finally\nrelay 0 finally\nrelay 1 finally\nleaf failed\n",
        ),
    );
}

#[test]
fn suspended_handlers_follow_the_current_try_nesting() {
    let src = r#"<?phv
function changing_handlers(): Generator<int> {
    try {
        yield 1;
        try {
            yield 2;
            throw new ValueError("inner");
        } catch (ValueError $e) {
            echo $e->getMessage(), "\n";
            yield 3;
        } finally {
            echo "inner finally\n";
        }
        yield 4;
    } finally {
        echo "outer finally\n";
    }
    yield 5;
    throw new ValueError("outside");
}

try {
    foreach (changing_handlers() as $value) {
        echo $value, "\n";
    }
} catch (ValueError $e) {
    echo $e->getMessage(), "\n";
}
foreach (changing_handlers() as $value) {
    echo $value, "\n";
    if ($value == 3) {
        break;
    }
}
"#;
    let (out, _) = run_limited("changing_handlers", src, Limits::default());
    assert_eq!(
        out,
        concat!(
            "1\n2\ninner\n3\ninner finally\n4\nouter finally\n5\noutside\n",
            "1\n2\ninner\n3\ninner finally\nouter finally\n",
        ),
    );
}

#[test]
fn wide_generator_keeps_values_needed_by_close_and_catch() {
    let src = r#"<?phv
function wide(): Generator<int> {
    $a = 1;
    $b = 2;
    $c = 3;
    $d = 4;
    $e = 5;
    $f = 6;
    $g = 7;
    $h = 8;
    echo $a + $b + $c + $d + $e + $f + $g + $h, "\n";
    $close = 91;
    $caught = 72;
    try {
        yield 10;
        throw new ValueError("leaf");
    } catch (ValueError) {
        echo $caught, "\n";
    } finally {
        echo $close, "\n";
    }
}
foreach (wide() as $value) {
    echo $value, "\n";
}
foreach (wide() as $value) {
    echo $value, "\n";
    break;
}
"#;
    let (out, _) = run_limited("wide_close_catch", src, Limits::default());
    assert_eq!(out, "36\n10\n72\n91\n36\n10\n91\n");
}

/// An object holding a generator whose frame holds the object is a cycle:
/// the cycle collector frees it while the script runs.
#[test]
fn a_generator_in_a_cycle_is_freed() {
    let src = r#"<?phv
class Feed {
    public ?Generator<int> $items = null;
    public list<int> $pad = [];

    public function items(): Generator<int> {
        foreach ($this->pad as $x) {
            yield $x;
        }
    }
}
$sum = 0;
for ($i = 0; $i < 100000; $i++) {
    $f = new Feed();
    $f->pad = range(0, 100);
    // Not iterated: its frame keeps `$this`, which keeps it.
    $f->items = $f->items();
    if ($i % 1000 == 0) {
        $g = (new Feed())->items();
        foreach ($g as $x) {
            $sum += $x;
        }
    }
}
echo "done";
"#;
    let (out, _) = run_limited("cycle", src, memory_limited());
    assert_eq!(out, "done");
}
