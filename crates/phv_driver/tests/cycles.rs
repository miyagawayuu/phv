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
