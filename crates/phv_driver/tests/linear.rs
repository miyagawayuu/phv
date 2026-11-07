//! Work that used to grow with the square of the input: `.=` on a property
//! or an element copied the whole string at every step. Each test would take
//! minutes that way; done in place it takes well under a second. Also checks
//! that appending in place keeps the results, errors and limits of `.=`.

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak, CountingAlloc};
use phv_vm::Limits;
use std::time::{Duration, Instant};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Runs `src` with and without the JIT; both must give the same output and
/// first error line, which is returned with the most memory a run held.
fn run_src(name: &str, src: &str, limits: Limits) -> (String, usize) {
    let dir = std::env::temp_dir().join(format!("phv-linear-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let mut results = Vec::new();
    let mut peak = 0;
    for jit in [false, true] {
        let mut c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
        if jit {
            c.enable_jit_all();
        }
        let base = thread_allocated();
        reset_thread_peak();
        let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
        peak = peak.max((thread_peak() - base).max(0) as usize);
        let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
        results.push(format!("{}{}", outcome.output, err.unwrap_or_default()));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(results[0], results[1], "the JIT changed the result of {name}");
    (results.pop().unwrap(), peak)
}

#[test]
fn appending_to_properties_and_elements_takes_linear_time() {
    let src = r#"<?phv
class Builder {
    public string $text = "";
    public map<string, string> $parts = [];
    public map<string, map<string, string>> $nested = [];
}
function element(int $n): int {
    map<string, string> $m = ["k" => ""];
    for ($i = 0; $i < $n; $i++) {
        $m["k"] .= "x";
    }
    return strlen($m["k"]);
}
$b = new Builder();
$b->parts["a"] = "";
$b->nested["o"] = ["i" => ""];
for ($i = 0; $i < 400000; $i++) {
    $b->text .= "x";
    $b->parts["a"] .= "yz";
    $b->nested["o"]["i"] .= $i % 10;
}
echo strlen($b->text), " ", strlen($b->parts["a"]), " ", strlen($b->nested["o"]["i"]), " ", element(400000);
"#;
    let start = Instant::now();
    let limits = Limits { max_time: Some(Duration::from_secs(60)), ..Limits::default() };
    let (out, _) = run_src("append", src, limits);
    assert_eq!(out, "400000 800000 400000 400000");
    // Copying each string at every step copies more than 200 GB in all.
    assert!(start.elapsed() < Duration::from_secs(30), "took {:?}", start.elapsed());
}

#[test]
fn appending_to_a_property_stays_within_the_memory_limit() {
    let src = r#"<?phv
class Builder {
    public string $text = "";
}
$b = new Builder();
while (true) {
    $b->text .= "0123456789abcdef";
}
"#;
    const LIMIT: usize = 16 << 20;
    let limits = Limits { max_memory: Some(LIMIT), ..Limits::default() };
    let (out, peak) = run_src("memory", src, limits);
    assert_eq!(out, "Fatal error: allowed memory size of 16 MB exhausted");
    assert!(peak < 4 * LIMIT, "held {} MB", peak >> 20);
}

/// The value read before the right side was evaluated is what is appended
/// to, even when the right side changes the place, as before.
#[test]
fn appending_keeps_the_meaning_of_dot_equals() {
    let src = r#"<?phv
class Box {
    public string $s = "a";
    public map<string, string> $m = [];
    public list<string> $xs = [];

    public function change(): string {
        $this->s = "zz";
        return "x";
    }
    public function clearMap(): string {
        $this->m = [];
        return "y";
    }
    public function shrink(): string {
        $this->xs = [];
        return "z";
    }
    public function self(): string {
        return $this->s;
    }
}
$b = new Box();
$b->m = ["k" => "a"];
$b->xs = ["a", "b"];
$b->s .= "1";
$b->s .= $b->change();
echo $b->s, "\n";
$b->m["k"] .= "2";
$b->m["k"] .= $b->clearMap();
echo json_encode($b->m), "\n";
$b->s .= $b->self();
$t = $b->s;
$b->s .= "!";
echo $t, " ", $b->s, "\n";
try {
    $b->xs[1] .= $b->shrink();
} catch (IndexError $e) {
    echo $e->getMessage(), "\n";
}
map<string, string> $m = ["a" => "x"];
$m["a"] .= "y";
$copy = $m;
$m["a"] .= 3;
echo json_encode($m), json_encode($copy), "\n";
try {
    $m["missing"] .= "q";
} catch (KeyError $e) {
    echo $e->getMessage(), "\n";
}
"#;
    let (out, _) = run_src("meaning", src, Limits::default());
    assert_eq!(
        out,
        "a1x\n{\"k\":\"a2y\"}\na1xa1x a1xa1x!\nlist index 1 is out of range (length 0)\n\
         {\"a\":\"xy3\"}{\"a\":\"xy\"}\nundefined map key \"missing\"\n"
    );
}
