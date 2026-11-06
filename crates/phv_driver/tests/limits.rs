//! Builtins that build large results in one call stay within the memory limit
//! (they account for the result before building it), and a regular expression
//! search that could not end within the time limit is refused before it starts.
//! Regression tests for the v0.8.0 audit (security-audit.md §17).

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
    let dir = std::env::temp_dir().join(format!("phv-limits-test-{}-{name}", std::process::id()));
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

fn assert_stops_at_limit(name: &str, src: &str) {
    let limits = Limits { max_memory: Some(LIMIT), ..Limits::default() };
    let (out, peak) = run_limited(name, src, limits);
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{name}: {out}");
    // Without the checks, each of these allocated hundreds of MB or more at once.
    assert!(peak < 4 * LIMIT, "{name} held {} MB", peak >> 20);
}

#[test]
fn json_and_list_builders_stay_within_the_memory_limit() {
    // 200,000 zeros 120 levels deep: 400 KB compact, about 50 MB indented.
    assert_stops_at_limit(
        "pretty",
        r#"<?phv
any $v = range(0, 199999)->map(fn(int $i): int => 0);
for ($i = 0; $i < 120; $i++) { $v = [$v]; }
echo strlen(json_encode_pretty($v));
"#,
    );
    // One shared list of 100,000 items, a hundred times over.
    let shared = r#"<?phv
$big = range(0, 99999);
list<list<int>> $outer = [];
for ($i = 0; $i < 100; $i++) { $outer[] = $big; }
"#;
    assert_stops_at_limit("flatten", &format!("{shared}echo $outer->flatten()->len();"));
    assert_stops_at_limit(
        "flat_map",
        &format!("{shared}echo range(1, 100)->flatMap(fn(int $i): list<int> => $big)->len();"),
    );
    assert_stops_at_limit(
        "csv_encode",
        r#"<?phv
$row = [str_repeat("x", 100000)];
list<list<string>> $rows = [];
for ($i = 0; $i < 1000; $i++) { $rows[] = $row; }
echo strlen(csv_encode($rows));
"#,
    );
    assert_stops_at_limit("chunk", "<?phv echo range(0, 299999)->chunk(1)->len();");
    // 3 MB of empty lists decode to far more than 16 MB of values (v0.9.1).
    assert_stops_at_limit(
        "json_decode",
        r#"<?phv
$text = "[" . str_repeat("[],", 1000000) . "[]]";
any $v = json_decode($text);
echo $v is list ? count($v) : 0;
"#,
    );
    // A query of 600,000 short fields builds far more than 16 MB (v0.9.1).
    assert_stops_at_limit(
        "parse_query",
        r#"<?phv
list<string> $parts = [];
for ($j = 0; $j < 600; $j++) {
    $parts[] = implode("&", range(0, 999)->map(fn(int $i): string => "k{$j}x$i"));
}
echo parse_query(implode("&", $parts))->len();
"#,
    );
    // 900 KB of line breaks are 900,000 rows (v0.9.1).
    assert_stops_at_limit("csv_parse", r#"<?phv echo count(csv_parse(str_repeat("\n", 900000)));"#);
    // A token signed with the site's key may still carry claims of many values (v0.9.1).
    assert_stops_at_limit(
        "jwt_decode",
        r#"<?phv
$k = str_repeat("k", 32);
// Each level holds the one below twice: 19 lists here, a million once decoded.
list<int> $empty = [];
any $x = $empty;
for ($i = 0; $i < 19; $i++) {
    $x = [$x, $x];
}
$token = jwt_encode(["d" => $x], $k);
any $c = jwt_decode($token, $k);
echo $c is map ? 1 : 0;
"#,
    );
    assert_stops_at_limit("group_by", "<?phv echo range(0, 299999)->groupBy(fn(int $i): int => $i)->len();");
}

#[test]
fn regular_expressions_stay_within_the_memory_limit() {
    let cases = [
        ("match_all", r#"echo (new Regex(str_repeat("(a)?", 100)))->matchAll(str_repeat("a", 2000000))->len();"#),
        ("replace", r#"echo strlen((new Regex("a"))->replace(str_repeat("a", 100000), str_repeat('$0', 500)));"#),
        ("split", r#"echo (new Regex(""))->split(str_repeat("a", 2000000))->len();"#),
        (
            "replace_with",
            r#"$big = str_repeat("x", 1000000);
echo strlen((new Regex("a"))->replaceWith(str_repeat("a", 100), fn(list<string> $m): string => $big));"#,
        ),
    ];
    for (name, body) in cases {
        assert_stops_at_limit(name, &format!("<?phv\nimport std.regex.Regex;\n{body}\n"));
    } // The regex crate's table for groups grows with states × groups: a pattern
      // that would need more than 16 MB for it is refused when compiled.
    let src = "<?phv\nimport std.regex.Regex;\nnew Regex(str_repeat(\"(a)?\", 1000));\n";
    let (out, peak) = run_limited("many_groups", src, Limits { max_memory: Some(LIMIT), ..Limits::default() });
    assert!(out.contains("too many groups for its size"), "{out}");
    assert!(peak < 4 * LIMIT, "many_groups held {} MB", peak >> 20);
}

#[test]
fn a_regular_expression_search_that_could_outlast_the_time_limit_is_refused() {
    // Compiling the pattern counts toward the limit: in a debug build on a
    // busy runner it took up to 2 seconds, so the limit leaves room for it.
    let limits = Limits { max_time: Some(Duration::from_secs(4)), ..Limits::default() };
    // About 4,000 states: searching 300,000 bytes could take 10 seconds.
    let src = r#"<?phv
import std.regex.Regex;
$r = new Regex('(?-u)(\w{1,100}){20}b');
try {
    $r->match(str_repeat("a", 300000));
} catch (RuntimeException $e) {
    echo $e->getMessage(), "\n";
}
echo $r->test(str_repeat("a", 1000)) ? "match" : "small texts are searched", "\n";
"#;
    let start = Instant::now();
    let (out, _) = run_limited("regex_time", src, limits);
    // Compiling counts too (slow on a busy runner); the search alone would take 10 s.
    assert!(start.elapsed() < Duration::from_secs(8), "took {:?}", start.elapsed());
    assert_eq!(
        out,
        "the regular expression is too large to search 300000 bytes of text in the time left (use a smaller pattern or a shorter text)\nsmall texts are searched\n"
    );
}

/// Comparing long strings, and callbacks of builtins, stop at the time limit
/// (one `==`, `contains()` or `sorted()` could otherwise run for minutes).
#[test]
fn long_comparisons_and_callbacks_stop_at_the_time_limit() {
    let cases = [
        ("eq", "$a == $b ? 1 : 0"),
        ("contains", "$a->contains($u) ? 1 : 0"),
        ("sorted", "$a->sorted()->len()"),
        ("sort_by", "range(1, 3000000)->sortBy(fn(int $x, int $y): int => $y <=> $x)->len()"),
    ];
    for (name, expr) in cases {
        let src = format!(
            r#"<?phv
$s = str_repeat("a", 10000000);
$t = str_repeat("a", 9999999) . "b";
$u = str_repeat("a", 9999999) . "c";
list<string> $a = [];
list<string> $b = [];
// Equal to $s and $t, but other strings: each comparison reads them whole.
$s2 = $s . "";
$t2 = $t . "";
for ($i = 0; $i < 4000; $i++) {{ $a[] = $i % 2 == 0 ? $s : $t; $b[] = $i % 2 == 0 ? $s2 : $t2; }}
echo {expr};
"#
        );
        let limits =
            Limits { max_time: Some(Duration::from_secs(1)), max_memory: Some(256 << 20), ..Limits::default() };
        let start = Instant::now();
        let (out, _) = run_limited(name, &src, limits);
        // Building the inputs counts too, and tests run side by side; without
        // the checks each of these ran for minutes.
        assert!(start.elapsed() < Duration::from_secs(20), "{name} took {:?}", start.elapsed());
        assert!(out.contains("maximum execution time"), "{name}: {out}");
    }
}

/// Found by the 1.0 audit: encoders that did not account their output, and
/// a set's strings written to JSON.
#[test]
fn encoders_and_sets_stay_within_the_memory_limit() {
    for (name, expr) in [
        ("rawurlencode", "strlen(rawurlencode(str_repeat(\"!\", 8 << 20)))"),
        ("bin2hex", "strlen(bin2hex(str_repeat(\"!\", 8 << 20)))"),
        ("http_build_query", "strlen(http_build_query([\"a\" => str_repeat(\"!\", 8 << 20)]))"),
        ("json_set", "strlen(json_encode(set([str_repeat(\"\\x01\", 3 << 20), str_repeat(\"\\x02\", 3 << 20)])))"),
        // Found by the 1.0.0 audit: these made several copies before the limit was checked.
        ("encrypt", "strlen(encrypt(str_repeat(\"é\", 5 << 20), str_repeat(\"k\", 32)))"),
        ("sign_token", "strlen(sign_token(str_repeat(\"é\", 5 << 20), str_repeat(\"k\", 32)))"),
        ("decrypt", "strlen(decrypt(str_repeat(\"A\", 10 << 20), str_repeat(\"k\", 32)) ?? \"\")"),
        ("verify_token", "strlen(verify_token(str_repeat(\"A\", 10 << 20) . \".x\", str_repeat(\"k\", 32)) ?? \"\")"),
    ] {
        assert_stops_at_limit(name, &format!("<?phv\necho {expr};\n"));
    }
}

/// Found by the 1.0 audit: copying a shared string for `.=`, and hashing a
/// long map key many times in one loop step, were not timed.
#[test]
fn copies_and_long_keys_stop_at_the_time_limit() {
    let cases = [
        ("append_shared", "$s = str_repeat(\"a\", 20 << 20);\nwhile (true) { $t = $s; $s .= \"x\"; }\n"),
        (
            "long_key",
            "$k = str_repeat(\"k\", 8 << 20);\nmap<string, int> $m = [$k => 1];\n$n = 0;\n\
             while (true) { $n += $m[$k] + $m[$k] + $m[$k] + $m[$k] + $m[$k] + $m[$k] + $m[$k] + $m[$k]; }\n",
        ),
        // Found by the 1.0.0 audit: `==` on lists, building ranges and map
        // literals with long keys were not timed.
        (
            "list_equal",
            "$a = range(0, 999999);\n$b = range(0, 999999);\n$n = 0;\nwhile (true) { if ($a == $b) { $n++; } }\n",
        ),
        ("range", "$n = 0;\nwhile (true) { $r = 0..3000000; $n += count($r); }\n"),
        (
            "map_literal",
            "$k = str_repeat(\"k\", 8 << 20);\n$n = 0;\n\
             while (true) { $m = [$k => 1, $k => 2, $k => 3, $k => 4, $k => 5, $k => 6]; $n += count($m); }\n",
        ),
    ];
    for (name, body) in cases {
        let limits =
            Limits { max_time: Some(Duration::from_secs(1)), max_memory: Some(256 << 20), ..Limits::default() };
        let start = Instant::now();
        let (out, _) = run_limited(name, &format!("<?phv\n{body}"), limits);
        assert!(out.contains("maximum execution time"), "{name}: {out}");
        assert!(start.elapsed() < Duration::from_secs(5), "{name} took {:?}", start.elapsed());
    }
}

/// Found by the 1.1 audit: literal HTML written in a loop was never counted
/// (4 GB under 128 MB).
#[test]
fn literal_html_in_a_loop_stops_at_the_memory_limit() {
    let piece = "x".repeat(1 << 20);
    let src = format!("<?phv for ($i = 0; $i < 3000; $i++) {{ ?>{piece}<?phv }} ?>");
    assert_stops_at_limit("echo_html", &src);
    // Many small pieces too: the bytes written since the last check count.
    let small = "y".repeat(60 << 10);
    let src = format!("<?phv for ($i = 0; $i < 100000; $i++) {{ ?>{small}<?phv }} ?>");
    assert_stops_at_limit("echo_html_small", &src);
}

/// Found by the 1.1 audit: builtins that compare or hash long values once
/// per element ran far past the time limit. Each case would take tens of
/// seconds without the checks, even on a fast machine.
#[test]
fn builtins_over_long_values_stop_at_the_time_limit() {
    let cases = [
        // `contains()` on maps that differ only at their end (C-3).
        (
            "contains_maps",
            "map<int, int> $m = [];\nfor ($i = 0; $i < 400000; $i++) { $m[$i] = $i; }\n$n = $m;\n$n[399999] = -1;\nlist<map<int, int>> $a = [];\nfor ($i = 0; $i < 2048; $i++) { $a[] = $m; }\necho $a->contains($n) ? 1 : 0;\n",
        ),
        // `min()` and `max()` of two long strings alternating (C-2).
        (
            "min",
            "$s = str_repeat(\"a\", 20000000);\n$t = str_repeat(\"a\", 19999999) . \"b\";\nlist<string> $a = [];\nfor ($i = 0; $i < 40000; $i++) { $a[] = $i % 2 == 0 ? $t : $s; }\necho strlen($a->min() ?? \"\");\n",
        ),
        (
            "max",
            "$s = str_repeat(\"a\", 20000000);\n$t = str_repeat(\"a\", 19999999) . \"b\";\nlist<string> $a = [];\nfor ($i = 0; $i < 40000; $i++) { $a[] = $i % 2 == 0 ? $s : $t; }\necho strlen($a->max() ?? \"\");\n",
        ),
        // `groupBy()` with one long key for every item (C-4).
        (
            "group_by",
            "$k = str_repeat(\"k\", 100000000);\necho count(range(1, 100000)->groupBy(fn(int $x): string => $k));\n",
        ),
    ];
    for (name, body) in cases {
        let limits =
            Limits { max_time: Some(Duration::from_secs(1)), max_memory: Some(256 << 20), ..Limits::default() };
        let start = Instant::now();
        let (out, _) = run_limited(name, &format!("<?phv\n{body}"), limits);
        assert!(out.contains("maximum execution time"), "{name}: {out}");
        assert!(start.elapsed() < Duration::from_secs(8), "{name} took {:?}", start.elapsed());
    }
}

/// Found by the 1.1 audit: `unique()` compared each value that is not hashed
/// (here `null`) with every int and string kept.
#[test]
fn unique_compares_only_what_it_cannot_hash() {
    // Compared with every value kept, the 20,000 nulls would take 20 billion
    // comparisons: far past the limit on any machine. Without that, building
    // the list is all the time it takes.
    let src = "<?phv\nlist<?int> $a = [];\nfor ($i = 0; $i < 1000000; $i++) { $a[] = $i; }\nfor ($i = 0; $i < 20000; $i++) { $a[] = null; }\necho $a->unique()->len();\n";
    let limits = Limits { max_time: Some(Duration::from_secs(10)), max_memory: Some(256 << 20), ..Limits::default() };
    let (out, _) = run_limited("unique_nulls", src, limits);
    assert_eq!(out, "1000001");
}

/// Found by the 1.1 audit: `E::cases()` of a huge enum was one untimed step.
#[test]
fn enum_cases_are_timed() {
    let mut src = String::from("<?phv\nenum E {\n");
    for i in 0..200_000 {
        src.push_str(&format!("    case C{i};\n"));
    }
    src.push_str("}\n$n = 0;\nwhile (true) { $n = $n + count(E::cases()); }\n");
    let limits = Limits { max_time: Some(Duration::from_secs(1)), max_memory: Some(256 << 20), ..Limits::default() };
    let start = Instant::now();
    let (out, _) = run_limited("enum_cases", &src, limits);
    assert!(out.contains("maximum execution time"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

/// Found by the 1.1 audit: `E::tryFrom()` looked at every case of a huge
/// backed enum in one untimed step.
#[test]
fn enum_lookups_are_timed() {
    let mut src = String::from("<?phv\nenum E: int {\n");
    for i in 0..200_000 {
        src.push_str(&format!("    case C{i} = {i};\n"));
    }
    src.push_str("}\n$n = 0;\nwhile (true) { $n = $n + (E::tryFrom(-1) == null ? 1 : 0); }\n");
    let limits = Limits { max_time: Some(Duration::from_secs(1)), max_memory: Some(256 << 20), ..Limits::default() };
    let start = Instant::now();
    let (out, _) = run_limited("enum_from", &src, limits);
    assert!(out.contains("maximum execution time"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}
