//! Checking and compiling take time and memory in proportion to the source:
//! nested `finally` blocks, chains of classes and many branches over many
//! variables each used to grow far faster (exponentially for the first).

use phv_driver::{compile, run, CompileError, Compiled, RunOptions};
use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak, CountingAlloc};
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Checks and compiles `src` on a thread with a large stack, as `phv` does.
/// Returns the result (output or first error), the most memory held at once
/// and the time taken.
fn compile_src(name: &str, src: String) -> (String, usize, Duration) {
    compile_on(name, src, 64 << 20)
}

/// `compile_src` on a thread with a stack of `stack` bytes.
fn compile_on(name: &str, src: String, stack: usize) -> (String, usize, Duration) {
    let dir = std::env::temp_dir().join(format!("phv-compile-cost-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let (d, e) = (dir.clone(), entry.clone());
    let worker = std::thread::Builder::new().stack_size(stack);
    let out = worker
        .spawn(move || {
            let base = thread_allocated();
            reset_thread_peak();
            let start = Instant::now();
            let c: Result<Compiled, CompileError> = compile(&d, &e);
            let took = start.elapsed();
            let peak = (thread_peak() - base).max(0) as usize;
            let out = match c {
                Ok(c) => {
                    let o = run(&c, RunOptions { html: false, limits: Default::default(), http: None, host: None });
                    o.output + &o.error.map(|e| e.render()).unwrap_or_default()
                }
                Err(e) => e.render(false),
            };
            (out, peak, took)
        })
        .unwrap()
        .join()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// 30 nested `finally` blocks that each may `return`: each block's code is
/// emitted once. Inlining it at every way out took 2^30 copies (a 1 KB
/// script took 7.7 GB at 24 levels).
#[test]
fn nested_finally_blocks_are_compiled_once() {
    let n = 30;
    let mut f = String::from("function deep(int $at): int {\n");
    f += &"try { ".repeat(n);
    f += "\n    return 100;\n";
    for i in (0..n).rev() {
        f += &format!("}} finally {{ if ($at == {i}) {{ return {i}; }} }}\n");
    }
    f += "}\n";
    // The same with `break` and `continue` from the innermost block, in loops.
    let mut g = String::from("function loops(int $at): string {\n    $out = \"\";\n");
    g += "    foreach ([1, 2] as $a) {\n        for ($b = 0; $b < 2; $b++) {\n";
    g += &"try { ".repeat(n);
    g += "\n    if ($b == 0) { continue; }\n    break 2;\n";
    for i in (0..n).rev() {
        g += &format!("}} finally {{ if ($at == {i}) {{ $out .= \"{i}\"; }} }}\n");
    }
    g += "        }\n        $out .= \"/\";\n    }\n    return $out;\n}\n";
    let src = format!(
        "<?phv\n{f}{g}foreach ([-1, 0, 7, 29] as $at) {{\n    echo deep($at), \" \";\n}}\necho loops(29), \" \", loops(0);\n"
    );
    let (out, peak, took) = compile_src("finally", src);
    assert_eq!(out, "100 0 7 29 2929 00");
    assert!(peak < 64 << 20, "held {} MB", peak >> 20);
    assert!(took < Duration::from_secs(60), "took {took:?}");
}

/// `return`, `break 80` and `continue 80` deep inside 80 `try` statements
/// and 80 `foreach` loops: each removes the handlers with one instruction and
/// clears the collections in code shared by all of them, instead of one
/// instruction per level at each (20,000 of them made 3 million).
#[test]
fn deep_exits_are_short() {
    let n = 80;
    let k = 20_000;
    let mut f = String::from("function f(int $x): int {\n$xs = [1, 2];\n");
    f += &"try { ".repeat(n);
    f += "\n";
    for i in 0..n {
        f += &format!("foreach ($xs as $i{i}) {{ ");
    }
    f += "\n";
    for i in 0..k {
        let jump = ["break 80;", "continue 80;", "return 1;"][i % 3];
        f += &format!("if ($x == {i}) {{ {jump} }}\n");
    }
    f += &"} ".repeat(n);
    f += "\n";
    f += &"} catch (Exception $e) { return 2; } ".repeat(n);
    f += "\nreturn 0;\n}\n";
    let src = format!("<?phv\n{f}echo f(0), f(1), f(2);\n");
    let dir = std::env::temp_dir().join(format!("phv-compile-cost-{}-exits", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let worker = std::thread::Builder::new().stack_size(64 * 1024 * 1024);
    let (d, e) = (dir.clone(), entry.clone());
    let (out, len) = worker
        .spawn(move || {
            let c = compile(&d, &e).unwrap_or_else(|e| panic!("{}", e.render(false)));
            let len = c.program.funcs.iter().find(|f| f.name == "f").unwrap().code.len();
            (run(&c, RunOptions { html: false, limits: Default::default(), http: None, host: None }).output, len)
        })
        .unwrap()
        .join()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out, "001");
    assert!(len < 10 * k, "{len} instructions");
}

/// Each class lists its ancestors: a chain of 20,000 classes held 200 million
/// entries (16,000 empty classes took 1.6 GB). Classes may be at most 256
/// levels deep, and all classes together hold a bounded number of entries.
#[test]
fn class_chains_are_limited() {
    let chain = |n: usize| -> String {
        let mut s = String::from("<?phv\nclass C0 { public int $x = 7; }\n");
        for i in 1..n {
            s += &format!("class C{i} extends C{} {{ }}\n", i - 1);
        }
        s
    };
    // 256 levels are allowed, and `instanceof` sees every ancestor.
    let ok = format!("{}$o = new C256();\necho $o instanceof C0 ? \"yes\" : \"no\", $o->x;\n", chain(257));
    let (out, _, _) = compile_src("chain_ok", ok);
    assert_eq!(out, "yes7");
    let (out, _, _) = compile_src("chain_deep", chain(258));
    assert!(out.contains("`C257` has more than 256 levels of classes and interfaces above it"), "{out}");
    let (out, peak, took) = compile_src("chain_long", chain(20_000));
    assert!(out.contains("more than 256 levels"), "{}", &out[..out.len().min(500)]);
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
    assert!(took < Duration::from_secs(120), "took {took:?}");
    // An interface that extends many others, implemented by many classes.
    let mut fan = String::from("<?phv\n");
    for i in 0..3000 {
        fan += &format!("interface I{i} {{ }}\n");
    }
    fan +=
        &format!("interface J extends {} {{ }}\n", (0..3000).map(|i| format!("I{i}")).collect::<Vec<_>>().join(", "));
    for i in 0..3000 {
        fan += &format!("class K{i} implements J {{ }}\n");
    }
    let (out, peak, _) = compile_src("fan", fan);
    assert!(out.contains("ancestors, interfaces and their type arguments in total"), "{}", &out[..out.len().min(500)]);
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
}

/// A trait's members are copied into each class that uses it, and its
/// methods are checked and compiled there: the members copied (with those
/// inherited) and the source of the methods copied are limited, and traits
/// that reach one trait along many paths (doubling at each level) flatten it
/// once or twice, not once per path.
#[test]
fn trait_copies_are_limited() {
    // 1,000 methods in 300 classes.
    let mut wide = String::from("<?phv\ntrait Wide {\n");
    for i in 0..1000 {
        wide += &format!("    public function m{i}(): int {{ return {i}; }}\n");
    }
    wide += "}\n";
    for i in 0..300 {
        wide += &format!("class C{i} {{ use Wide; }}\n");
    }
    let (out, peak, _) = compile_src("trait_wide", wide);
    assert!(out.contains("inherit more than 200000 members in total"), "{}", &out[..out.len().min(500)]);
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
    // A method of 300 KB in 120 classes: 36 MB to check.
    let big = format!(
        "<?phv\ntrait Big {{\n    public function text(): string {{\n        return \"{}\";\n    }}\n}}\n{}",
        "x".repeat(300_000),
        (0..120).map(|i| format!("class B{i} {{ use Big; }}\n")).collect::<String>()
    );
    let (out, _, took) = compile_src("trait_big", big);
    assert!(out.contains("copy more than 32 MB of methods"), "{}", &out[..out.len().min(500)]);
    assert!(took < Duration::from_secs(60), "took {took:?}");
    // T30 reaches T0 along 2^30 paths.
    let mut paths = String::from("<?phv\ntrait T0 {\n    public function f(): int { return 1; }\n}\n");
    for i in 1..=30 {
        let p = i - 1;
        paths += &format!("trait L{i} {{ use T{p}; }}\ntrait R{i} {{ use T{p}; }}\ntrait T{i} {{ use L{i}, R{i}; }}\n");
    }
    paths += "class D { use T30; }\necho (new D())->f();\n";
    let (out, peak, took) = compile_src("trait_paths", paths);
    assert_eq!(out, "1");
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
    assert!(took < Duration::from_secs(10), "took {took:?}");
    // A few levels are fine: the same method reached twice is one method.
    let mut few_src = String::from("<?phv\ntrait T0 {\n    public function f(): int { return 1; }\n}\n");
    for i in 1..=3 {
        let p = i - 1;
        few_src +=
            &format!("trait L{i} {{ use T{p}; }}\ntrait R{i} {{ use T{p}; }}\ntrait T{i} {{ use L{i}, R{i}; }}\n");
    }
    few_src += "class D { use T3; }\necho (new D())->f();\n";
    let (out, _, _) = compile_src("trait_few_paths", few_src);
    assert_eq!(out, "1");
    // Traits used inside each other: 256 levels, as classes.
    let chain = |n: usize| -> String {
        let mut s = String::from("<?phv\ntrait U0 {\n    public function f(): int { return 7; }\n}\n");
        for i in 1..n {
            s += &format!("trait U{i} {{ use U{}; }}\n", i - 1);
        }
        s + &format!("class E {{ use U{}; }}\necho (new E())->f();\n", n - 1)
    };
    let (out, _, _) = compile_src("trait_chain_ok", chain(256));
    assert_eq!(out, "7");
    let (out, peak, _) = compile_src("trait_chain_long", chain(5000));
    assert!(
        out.contains("traits are used inside each other more than 256 levels deep"),
        "{}",
        &out[..out.len().min(500)]
    );
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
}

/// 40 levels of `trait Ai { use Ai-1, Bi-1; } trait Bi { use Ai-1, Bi-1; }`
/// (1.3 KB) reach `A0` along 2^40 paths. Each path was walked, and no limit
/// counted a trait without members: 24 levels took a minute, 40 days. Now a
/// trait reached again is flattened from what it gave before, and every
/// flatten counts against the limit, so such traits check in a moment, with
/// a class that uses them or without, with members or without; and traits
/// whose type arguments differ along each path (so that nothing is reached
/// twice) stop at the limit.
#[test]
fn diamonds_of_traits_check_at_once() {
    let diamond = |levels: usize, bottom: &str, class: &str| -> String {
        let mut s = format!("<?phv\ntrait A0 {{ {bottom} }}\ntrait B0 {{ }}\n");
        for i in 1..=levels {
            let p = i - 1;
            s += &format!("trait A{i} {{ use A{p}, B{p}; }}\ntrait B{i} {{ use A{p}, B{p}; }}\n");
        }
        s + class
    };
    for (name, src, want) in [
        ("diamond_class", diamond(40, "", "class K { use A40; }\necho \"done\";\n"), "done"),
        ("diamond_traits", diamond(40, "", "echo \"done\";\n"), "done"),
        (
            "diamond_members",
            diamond(
                40,
                "public int $n = 4; const C = 2; public function f(): int { return $this->n * self::C; } abstract public function g(): int;",
                "class K { use A40; public function g(): int { return 1; } }\necho (new K())->f() + (new K())->g();\n",
            ),
            "9",
        ),
    ] {
        let (out, peak, took) = compile_src(name, src);
        assert_eq!(out, want, "{name}");
        assert!(took < Duration::from_secs(1), "{name} took {took:?}");
        assert!(peak < 64 << 20, "{name} held {} MB", peak >> 20);
    }
    // Different type arguments along each path: 2^40 different traits to
    // flatten, which stop at the limit.
    let mut generic = String::from("<?phv\ntrait A0<T> { }\ntrait B0<T> { }\n");
    for i in 1..=40 {
        let p = i - 1;
        generic += &format!(
            "trait A{i}<T> {{ use A{p}<list<T>>, B{p}<map<string, T>>; }}\ntrait B{i}<T> {{ use A{p}<list<T>>, B{p}<map<string, T>>; }}\n"
        );
    }
    generic += "class K { use A40<int>; }\necho \"done\";\n";
    let (out, peak, took) = compile_src("diamond_generic", generic);
    assert!(out.contains("inherit more than 200000 members in total"), "{}", &out[..out.len().min(500)]);
    assert!(took < Duration::from_secs(20), "took {took:?}");
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
}

/// A `use` with one alias per method of the trait (`m0 as a0; ...`): each
/// rule looked through the trait's methods, so `N` aliases took O(N²) (40,000
/// took 13 s to check, and the language server answered nothing meanwhile),
/// and 17 levels of traits that each alias every method of the one below
/// (7 MB) took 43 s (audit 20). Rules now find a method by its name, and
/// count against the limit of members copied.
#[test]
fn many_trait_aliases_check_at_once() {
    // (`T::m as a;` is one token more: 60,000 of them pass the token limit.)
    for (name, from, n) in [("aliases", "", 60_000), ("aliases_named", "T::", 40_000)] {
        let methods: String = (0..n).map(|i| format!("public function m{i}(): int {{ return {i}; }}\n")).collect();
        let rules: String = (0..n).map(|i| format!("{from}m{i} as a{i};\n")).collect();
        let src = format!(
            "<?phv\ntrait T {{\n{methods}}}\nfinal class C {{ use T {{\n{rules}}} }}\necho (new C())->a{}();\n",
            n - 1
        );
        let (out, _, took) = compile_src(name, src);
        assert_eq!(out, format!("{}", n - 1), "{name}");
        // About 20 s in a debug build before (13 s for 40,000 in release).
        assert!(took < Duration::from_secs(8), "{name} took {took:?}");
    }
    // Each level aliases every method of the one below: 2^17 methods at the top.
    let mut names = vec!["m0".to_string()];
    let mut src = String::from("<?phv\ntrait T0 { public function m0(): int { return 1; } }\n");
    for i in 1..=17 {
        let mut rules = String::new();
        let mut next = Vec::new();
        for m in &names {
            rules += &format!("T{}::{m} as {m}a{i}; ", i - 1);
            next.push(m.clone());
            next.push(format!("{m}a{i}"));
        }
        src += &format!("trait T{i} {{ use T{} {{ {rules}}} }}\n", i - 1);
        names = next;
    }
    src += "final class C { use T17; }\necho (new C())->m0();\n";
    let (out, peak, took) = compile_src("aliases_doubling", src);
    assert!(out.contains("inherit more than 200000 members in total"), "{}", &out[..out.len().min(500)]);
    assert!(took < Duration::from_secs(8), "took {took:?}");
    assert!(peak < 1 << 30, "held {} MB", peak >> 20);
}

/// Errors found in traits are reported once, also when the trait is checked
/// again for each class that uses it: each one was looked for among all the
/// diagnostics before it, so two traits whose 20,000 rules each name a
/// missing method took 10 s to check, and a trait of 20,000 wrong methods
/// used by two classes 3 s (audit 20).
#[test]
fn many_errors_in_traits_are_reported_at_once() {
    let n = 20_000;
    let mut src = String::from("<?phv\ntrait T { public function m(): int { return 1; } }\n");
    for k in 0..2 {
        let rules: String = (0..n).map(|i| format!("x{i} as y{k}_{i}; ")).collect();
        src += &format!("trait U{k} {{ use T {{ {rules}}} }}\n");
    }
    src += "final class C { use U0; use U1; }\n";
    let (out, _, took) = compile_src("rule_errors", src);
    assert!(out.contains("no trait used here has a method `x0`"), "{}", &out[..out.len().min(500)]);
    assert!(
        out.ends_with(
            "40000 errors found
"
        ),
        "{}",
        &out[out.len().saturating_sub(300)..]
    );
    assert!(took < Duration::from_secs(5), "took {took:?}");
    let methods: String = (0..n).map(|i| format!("public function m{i}(): int {{ return \"s\"; }}\n")).collect();
    let src = format!("<?phv\ntrait T {{\n{methods}}}\nfinal class C {{ use T; }}\nfinal class D {{ use T; }}\n");
    let (out, _, took) = compile_src("method_errors", src);
    assert!(out.contains("in trait `T`, used by `C`"), "{}", &out[..out.len().min(500)]);
    assert!(!out.contains("used by `D`"), "reported once: {}", &out[..out.len().min(500)]);
    assert!(
        out.ends_with(
            "20000 errors found
"
        ),
        "{}",
        &out[out.len().saturating_sub(300)..]
    );
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// The type arguments a class gives its ancestors double with each level of
/// `class C<T> extends B<fn(T): T>` (25 lines took 7 GB): they are measured
/// before they are built.
#[test]
fn type_arguments_of_ancestors_stay_small() {
    let mut src = String::from("<?phv\nclass C0<T> { }\n");
    for i in 1..40 {
        src += &format!("class C{i}<T> extends C{}<fn(T): T> {{ }}\n", i - 1);
    }
    let (out, peak, _) = compile_src("doubling", src);
    assert!(out.contains("are too large (more than 10000 type nodes)"), "{out}");
    assert!(peak < 64 << 20, "held {} MB", peak >> 20);
    // Growing by one level at a time is fine.
    let mut src = String::from("<?phv\nclass C0<T> { public ?T $v = null; }\n");
    for i in 1..200 {
        src += &format!("class C{i}<T> extends C{}<list<T>> {{ }}\n", i - 1);
    }
    src += "$c = new C199<int>();\necho $c instanceof C0 ? \"ok\" : \"no\";\n";
    let (out, _, _) = compile_src("growing", src);
    assert_eq!(out, "ok");
}

/// 10,000 narrowed variables and 20,000 branches: each branch used to copy
/// and join the narrowings of every variable (8,000 x 16,000 took 30 s).
#[test]
fn narrowing_costs_what_branches_change() {
    let mut src = String::from("<?phv\n$c = strlen(\"x\") == 1;\n");
    for i in 0..10_000 {
        src += &format!("?int $v{i} = {i};\n");
    }
    for i in 0..20_000 {
        src += &format!("if ($c) {{ $w = {i}; }} else {{ $v{} = null; }}\n", i % 10_000);
    }
    src += "echo $v0 ?? -1, \" \", $v9999 ?? -1;\n";
    let (out, peak, took) = compile_src("narrow", src);
    assert_eq!(out, "0 9999");
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
    assert!(took < Duration::from_secs(120), "took {took:?}");
}

/// A generic call can double the type of its argument (`function dbl<T>(T
/// $x): fn(T): T`; 24 lines took 4.5 GB and 15 s), and closures that return
/// one of the two before grow like Fibonacci numbers: inferred types are
/// measured before they are built and refused past 10,000 nodes. Each use of a
/// value copies its type, so large ones also count against a total.
#[test]
fn inferred_types_stay_small() {
    let dbl = "function dbl<T>(T $x): fn(T): T { return fn(T $y): T => $x; }\n$a0 = 1;\n";
    let mut src = format!("<?phv\n{dbl}");
    for i in 1..30 {
        src += &format!("$a{i} = dbl($a{});\n", i - 1);
    }
    src += "echo 1;\n";
    let (out, peak, _) = compile_src("dbl", src);
    assert!(out.contains("the type of this expression is too large (more than 10000 type nodes)"), "{out}");
    assert!(peak < 64 << 20, "held {} MB", peak >> 20);

    let mut src = String::from("<?phv\n$c = strlen(\"x\") == 1;\n$f0 = fn(): int => 1;\n$f1 = fn(): string => \"\";\n");
    for i in 2..40 {
        src += &format!("$f{i} = fn() => $c ? $f{} : $f{};\n", i - 1, i - 2);
    }
    src += "echo 1;\n";
    let (out, peak, _) = compile_src("fib", src);
    assert!(out.contains("is too large (more than 10000 type nodes)"), "{out}");
    assert!(peak < 64 << 20, "held {} MB", peak >> 20);

    // 2,000 uses of a value whose type has 8,191 nodes.
    let mut src = format!("<?phv\n{dbl}");
    for i in 1..13 {
        src += &format!("$a{i} = dbl($a{});\n", i - 1);
    }
    src += &format!("$all = [{}];\necho 1;\n", vec!["$a12"; 2000].join(", "));
    let (out, peak, _) = compile_src("uses", src);
    assert!(out.contains("are too large in total (more than 2000000 type nodes)"), "{out}");
    assert!(peak < 200 << 20, "held {} MB", peak >> 20);
    // A generic call that wraps its argument in one more list each time: 30,000
    // lines took 60 GB (each variable held its own copy of a longer type).
    let wrap = "function wrap<T>(T $x): list<T> { return [$x]; }\n$a0 = 1;\n";
    let mut src = format!("<?phv\n{wrap}");
    for i in 1..30_000 {
        src += &format!("$a{i} = wrap($a{});\n", i - 1);
    }
    let (out, peak, _) = compile_src("wrap", src + "echo 1;\n");
    assert!(out.contains("are too large in total (more than 2000000 type nodes)"), "{}", &out[..out.len().min(300)]);
    assert!(peak < 400 << 20, "held {} MB", peak >> 20);
    // A type 9,900 levels deep (as deep as 10,000 nodes allow) is shown in a
    // message, on the 16 MB stack of a `phv serve` worker.
    let ty = format!("{}T{}", "list<".repeat(450), ">".repeat(450));
    let mut src = format!("<?phv\nfunction w<T>(T $x): {ty} {{ return []; }}\n$a0 = 1;\n");
    for i in 1..=22 {
        src += &format!("$a{i} = w($a{});\n", i - 1);
    }
    let (out, _, _) = compile_on("deep", src + "$b = $a22 + 1;\n", 16 << 20);
    assert!(out.contains("cannot apply `+` to `list<list<"), "{}", &out[..out.len().min(300)]);

    // A few levels are fine.
    let mut src = format!("<?phv\n{dbl}");
    for i in 1..8 {
        src += &format!("$a{i} = dbl($a{});\n", i - 1);
    }
    src += "echo $a7($a6)($a5)($a4)($a3)($a2)($a1)($a0);\n";
    let (out, _, _) = compile_src("dbl_ok", src);
    assert_eq!(out, "1");
}

/// A long constant string is held once: as a property's default it was copied
/// for each class that has the property (1,000 subclasses of a class with a
/// 1 MiB default took 2.5 GB, and a 1 GB cache entry), and each use of it
/// hashed the whole text again (100,000 uses took 21 s).
#[test]
fn long_constant_strings_are_not_copied() {
    let mut consts = String::from("<?phv\nconst C0 = \"xxxxxxxxxxxxxxxx\";\n");
    for i in 1..17 {
        consts += &format!("const C{i} = C{} . C{};\n", i - 1, i - 1);
    }
    let classes = |n: usize| -> String {
        let mut s = format!("{consts}class A {{ public string $s = C16; }}\n");
        for i in 0..n {
            s += &format!("class K{i} extends A {{ }}\n");
        }
        s + "echo strlen((new K0())->s);\n"
    };
    let (out, peak, _) = compile_src("fields", classes(100));
    assert!(out.contains("the default values of the properties of all classes are too long in total"), "{out}");
    assert!(peak < 64 << 20, "held {} MB", peak >> 20);
    let (out, _, _) = compile_src("fields_ok", classes(10));
    assert_eq!(out, "1048576");

    let uses = |c: &str| {
        format!("{consts}function f(): int {{\n{}return 0;\n}}\necho f();\n", format!("$s = {c};\n").repeat(2000))
    };
    let (out, _, short) = compile_src("uses_short", uses("C0"));
    assert_eq!(out, "0");
    let (out, _, long) = compile_src("uses_long", uses("C16"));
    assert_eq!(out, "0");
    assert!(long < short * 3 + Duration::from_secs(2), "{long:?} for 1 MiB, {short:?} for 16 bytes");
}

/// Long `elseif` chains, statements nested without braces and types nested in
/// types count against the nesting limit: 60,000 `elseif`s, or 75,000 nested
/// types, overflowed the stack and ended the process. They are checked on a
/// thread with the 16 MB stack that `phv serve` gives its workers.
#[test]
fn long_chains_and_deep_types_are_limited() {
    type Shape = fn(usize) -> String;
    let shapes: Vec<(&str, Shape)> = vec![
        ("elseif", |n| {
            let arms: String = (1..n).map(|i| format!("elseif ($a == {i}) {{ echo {i}; }}\n")).collect();
            format!("<?phv\n$a = strlen(\"x\");\nif ($a == 0) {{ echo 0; }}\n{arms}")
        }),
        ("else_if", |n| {
            let arms: String = (1..n).map(|i| format!("else if ($a == {i}) {{ echo {i}; }}\n")).collect();
            format!("<?phv\n$a = strlen(\"x\");\nif ($a == 0) {{ echo 0; }}\n{arms}")
        }),
        ("template_elseif", |n| {
            let arms: String = (1..n).map(|i| format!("elseif ($a == {i}):\necho {i};\n")).collect();
            format!("<?phv\n$a = strlen(\"x\");\nif ($a == 0):\necho 0;\n{arms}endif;\n")
        }),
        ("unbraced", |n| format!("<?phv\n$a = true;\n{}echo 1;\n", "if ($a) ".repeat(n))),
        ("unbraced_loops", |n| format!("<?phv\n{}echo 1;\n", "foreach ([1] as $x) ".repeat(n))),
        ("generic_type", |n| {
            format!(
                "<?phv\nfunction f({}int{} $x): int {{ return 1; }}\necho f([]);\n",
                "list<".repeat(n),
                ">".repeat(n)
            )
        }),
        ("paren_type", |n| {
            format!("<?phv\nfunction f({}int{} $x): int {{ return $x; }}\necho f(1);\n", "(".repeat(n), ")".repeat(n))
        }),
        ("fn_type", |n| {
            format!("<?phv\nfunction f({}int $x): int {{ return 1; }}\necho f(null);\n", "?fn(): ".repeat(n))
        }),
    ];
    for (name, shape) in shapes {
        let (out, _, _) = compile_on(name, shape(50_000), 16 << 20);
        assert!(
            out.contains("code is nested too deeply (more than 500 levels)"),
            "{name}: {}",
            &out[..out.len().min(300)]
        );
        let (out, _, _) = compile_on(name, shape(240), 16 << 20);
        assert_eq!(out, "1", "{name}");
    }
}
