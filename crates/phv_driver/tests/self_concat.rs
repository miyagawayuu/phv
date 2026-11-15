//! `$s = $s . $x;` and `$s = "{$s}...";` on a `string` variable append in
//! place, as `$s .= $x;` does. They used to build a new string at every
//! pass, so a loop of them took quadratic time (21st assessment: 200,000
//! passes took 48 seconds, `.=` 0.04). The result must be the same in every
//! case: `tests/cases/self_concat.phv` (what 1.3.2 printed) is run here with
//! the JIT too. Also: a closure's captured values are not left in the
//! temporaries they were staged in.

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::CountingAlloc;
use phv_vm::Limits;
use std::path::Path;
use std::time::{Duration, Instant};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Runs `src` with and without the JIT; both must give the same output and
/// first error line, which is returned.
fn run_src(name: &str, src: &str) -> String {
    let dir = std::env::temp_dir().join(format!("phv-self-concat-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let mut results = Vec::new();
    for jit in [false, true] {
        let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
        if jit {
            c.enable_jit_all();
        }
        let limits =
            Limits { max_time: Some(Duration::from_secs(60)), max_memory: Some(128 << 20), ..Limits::default() };
        let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
        let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
        results.push(format!("{}{}", outcome.output, err.unwrap_or_default()));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(results[0], results[1], "the JIT changed the result of {name}");
    results.pop().unwrap()
}

#[test]
fn joining_a_string_to_itself_takes_linear_time() {
    let src = r#"<?phv
function dots(string $u, int $n): int {
    $s = "";
    for ($i = 0; $i < $n; $i++) {
        $s = $s . $u . $i;
    }
    return strlen($s);
}
function interpolated(string $u, int $n): int {
    $s = "";
    for ($i = 0; $i < $n; $i++) {
        $s = "{$s}{$u}";
    }
    return strlen($s);
}
function yielded(int $n): Generator<int> {
    $s = "";
    for ($i = 0; $i < $n; $i++) {
        $s = $s . "ab";
        if ($i % 1000 == 0) {
            yield strlen($s);
        }
    }
    yield strlen($s);
}
$n = 300000;
$t = "";
for ($i = 0; $i < $n; $i++) {
    $t = $t . "ab";
}
$last = 0;
foreach (yielded($n) as $v) {
    $last = $v;
}
echo dots("mk", $n), " ", interpolated("ab", $n), " ", strlen($t), " ", $last;
"#;
    let start = Instant::now();
    assert_eq!(run_src("linear", src), "2288890 600000 600000 600000");
    // A new string at every pass copies more than 300 GB in all.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

#[test]
fn joining_a_string_to_itself_gives_what_it_gave() {
    let cases = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let src = std::fs::read_to_string(cases.join("self_concat.phv")).unwrap();
    let expected = std::fs::read_to_string(cases.join("self_concat.out")).unwrap().replace("\r\n", "\n");
    assert_eq!(run_src("same", &src), expected);
}

/// `f(fn() => count($xs))` after `$xs[] = $i`: the list staged for the
/// closure stayed in a temporary, so the next append copied all of it
/// (21st assessment: 20,000 passes took 2.2 s, 40,000 took 14 s).
#[test]
fn a_closure_does_not_keep_what_it_captured_in_a_temporary() {
    let src = r#"<?phv
function call(fn(): int $f): int {
    return $f();
}
function lists(int $n): int {
    list<int> $xs = [];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $xs[] = $i;
        $t += call(fn(): int => count($xs));
    }
    return $t;
}
function strings(int $n): int {
    $s = "";
    map<string, int> $m = [];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $s .= "ab";
        $m["k" . $i] = $i;
        $t += call(function () use ($s, $m, $i): int { return strlen($s) + count($m) + $i; });
    }
    return $t;
}
// The closure still has what it captured, and the variable its own value.
$xs = [1, 2];
$f = fn(): int => count($xs);
$xs[] = 3;
echo lists(60000), " ", strings(60000), " ", $f(), count($xs);
"#;
    let start = Instant::now();
    assert_eq!(run_src("closure", src), "1800030000 7200060000 23");
    // Copying at every pass copies tens of GB in all.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

/// `$s = $s . $a . $b;` must not hold what it appends twice: joining `$a`
/// and `$b` into a temporary before appending it (the first form of this
/// change) made a 60 MB string built from two of 30 MB fail under 128 MB,
/// where building it anew fits (found by the re-attack of the 21st
/// assessment). The items are added to `$s` in a buffer grown once.
#[test]
fn joining_several_strings_to_a_string_needs_no_more_memory_than_before() {
    let src = r#"<?phv
function build(string $a, int $n): int {
    $s = "x";
    for ($i = 0; $i < $n; $i++) {
        $s = "y";
        $s = $s . $a . $a;
    }
    return strlen($s);
}
function grown(string $a): int {
    $s = "x" . strlen($a);
    $s = $s . $a . $a;
    return strlen($s);
}
function page(string $head, string $body, string $foot): int {
    $out = "";
    $out = "{$out}{$head}{$body}{$foot}";
    return strlen($out);
}
$a = str_repeat("a", 30 << 20);
$s = "x";
$s = $s . $a . $a;
echo strlen($s), " ";
$s = "";
echo build($a, 2), " ", grown($a), " ";
$a = "";
echo page("<h1>", str_repeat("b", 50 << 20), "</h1>"), " ";
// A result joined to the next piece on each of 9,900 returns: its buffer
// doubles while that fits, then grows by what is needed.
function back(string $s, int $n): string {
    if ($n == 0) { return $s; }
    return back($s, $n - 1) . $s;
}
echo strlen(back(str_repeat("a", 6000), 9900));
"#;
    assert_eq!(run_src("memory", src), "62914561 62914561 62914569 52428809 59406000");
}
