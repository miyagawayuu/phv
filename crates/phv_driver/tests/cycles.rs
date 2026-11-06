//! Objects that reference each other are freed when a run ends (reference
//! counting alone would leak them, request after request).

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::{thread_allocated, CountingAlloc};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

#[test]
fn reference_cycles_do_not_leak() {
    let dir = std::env::temp_dir().join(format!("phv-cycles-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("cycle.phv");
    std::fs::write(
        &file,
        "<?phv\nclass Node {\n    public ?Node $next = null;\n    public string $data = \"\";\n}\n\
         $n = new Node();\n$n->data = str_repeat(\"a\", 1000000);\n$n->next = $n;\n\
         $a = new Node();\n$b = new Node();\n$a->next = $b;\n$b->next = $a;\necho \"ok\";\n",
    )
    .unwrap();
    let Ok(c) = compile(&dir, &file) else { panic!("cycle.phv does not compile") };
    let opts = || RunOptions { html: false, limits: Default::default(), http: None, host: None };
    // Warm up (lazy statics, caches), then measure.
    assert_eq!(run(&c, opts()).output, "ok");
    let before = thread_allocated();
    for _ in 0..20 {
        assert_eq!(run(&c, opts()).output, "ok");
    }
    let grown = thread_allocated() - before;
    assert!(grown < 1_000_000, "memory grew by {grown} bytes over 20 runs");
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_src(name: &str, src: &str, limits: phv_vm::Limits) -> (String, Option<String>) {
    let dir = std::env::temp_dir().join(format!("phv-cycles-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.phv");
    std::fs::write(&file, src).unwrap();
    let c = compile(&dir, &file).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
    let _ = std::fs::remove_dir_all(&dir);
    (outcome.output, outcome.error.map(|e| e.render()))
}

/// Cycles made in a loop are freed while the script runs: 200,000 of them,
/// half holding a list of 200 numbers, fit in 16 MB (they need over 300 MB
/// when they are freed only at the end of the run).
#[test]
fn reference_cycles_are_freed_while_the_script_runs() {
    let src = "<?phv\nclass Node {\n    public ?Node $next = null;\n    public list<int> $pad = [];\n    \
               public ?fn(): int $cb = null;\n}\n\
               for ($i = 0; $i < 100000; $i++) {\n    $a = new Node();\n    $b = new Node();\n    \
               $a->next = $b;\n    $b->next = $a;\n    $a->pad = range(0, 200);\n    $c = new Node();\n    \
               $c->cb = function () use ($c): int { return count($c->pad); };\n}\necho \"done\";\n";
    let limits = phv_vm::Limits { max_memory: Some(16 << 20), ..Default::default() };
    let (out, err) = run_src("churn", src, limits);
    assert_eq!((out.as_str(), err), ("done", None));
}

/// The collector never frees what the script can still reach: cycles held
/// from a local, from a list or map in a local, from a closure, and from an
/// object field, all survive many collections with their contents.
#[test]
fn reachable_cycles_survive_collections() {
    let src = r#"<?phv
class Node {
    public ?Node $next = null;
    public int $n = 0;
    public list<Node> $kids = [];
    public map<string, Node> $named = [];
    public ?fn(): int $cb = null;
}

function ring(int $size, int $start): Node {
    $first = new Node();
    $first->n = $start;
    $cur = $first;
    for ($i = 1; $i < $size; $i++) {
        $nx = new Node();
        $nx->n = $start + $i;
        $cur->next = $nx;
        $cur = $nx;
    }
    $cur->next = $first;
    return $first;
}

function sum(Node $r, int $size): int {
    $t = 0;
    $cur = $r;
    for ($i = 0; $i < $size; $i++) {
        $t += $cur->n;
        $cur = $cur->next ?? $cur;
    }
    return $t;
}

$local = ring(50, 0);
$inList = [ring(50, 100)];
$inMap = ["k" => ring(50, 200)];
$holder = new Node();
$holder->kids = [ring(50, 300)];
$holder->named = ["x" => ring(50, 400)];
$holder->next = $holder;
$viaClosure = ring(50, 500);
$f = function () use ($viaClosure): int { return sum($viaClosure, 50); };
$self = new Node();
$self->cb = function () use ($self): int { return $self->n; };
$self->n = 7;

for ($i = 0; $i < 60000; $i++) {
    $g = ring(3, $i);
}

echo sum($local, 50), " ", sum($inList[0], 50), " ", sum($inMap["k"], 50), " ";
echo sum($holder->kids[0], 50), " ", sum($holder->named["x"], 50), " ", $f(), " ";
$cb = $self->cb;
echo $cb === null ? -1 : $cb(), " ", $holder->next === null ? "lost" : "kept";
"#
    .replace("===", "==");
    let (out, err) = run_src("survive", &src, Default::default());
    assert_eq!(err, None);
    assert_eq!(out, "1225 6225 11225 16225 21225 26225 7 kept");
}

/// Found by the 1.0.0 audit: the collector ran on the number of objects, so a
/// few cycles holding large strings reached the memory limit although they
/// were garbage. It also runs when memory runs short.
#[test]
fn cycles_are_collected_when_memory_runs_short() {
    let limits = phv_vm::Limits { max_memory: Some(32 << 20), ..Default::default() };
    let node = "<?phv\nclass Node {\n    public ?Node $next = null;\n    public string $data = \"\";\n}\n\
                $big = str_repeat(\"x\", 1 << 20);\n";
    // 300 cycles of one object holding 1 MB each: 300 MB if none is freed.
    let src = format!(
        "{node}for ($i = 0; $i < 300; $i++) {{\n    $n = new Node();\n    $n->data = $big . $i;\n    \
         $n->next = $n;\n}}\necho \"done\";\n"
    );
    let (out, err) = run_src("pressure", &src, limits);
    assert_eq!((out.as_str(), err), ("done", None));
    // Cycles made before many strings are kept, with no object made after.
    let src = format!(
        "{node}for ($i = 0; $i < 20; $i++) {{\n    $n = new Node();\n    $n->data = $big . $i;\n    \
         $n->next = $n;\n}}\n$k = str_repeat(\"k\", 1000);\nlist<string> $parts = [];\n\
         for ($i = 0; $i < 20000; $i++) {{ $parts[] = $k . $i; }}\necho count($parts);\n"
    );
    let (out, err) = run_src("pressure_strings", &src, limits);
    assert_eq!((out.as_str(), err), ("20000", None));
    // Memory held by live objects still stops the script.
    let src = format!(
        "{node}list<Node> $keep = [];\nfor ($i = 0; $i < 300; $i++) {{\n    $n = new Node();\n    \
         $n->data = $big . $i;\n    $n->next = $n;\n    $keep[] = $n;\n}}\necho \"done\";\n"
    );
    let (out, err) = run_src("pressure_live", &src, limits);
    assert_eq!(out, "");
    assert!(err.as_deref().is_some_and(|e| e.contains("allowed memory size of 32 MB exhausted")), "{err:?}");
}
