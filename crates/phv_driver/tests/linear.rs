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

/// A loop that passes a list, a string or a map to a helper and then grows
/// it: the interpreter kept the helper's parameter after it returned, so the
/// caller's value stayed shared and each append copied all of it (audit 20:
/// 40,000 passes took 12 to 22 s, and a 70-byte request held a worker for
/// 16 s). A returned call now lets its strings, collections and objects go.
#[test]
fn growing_a_value_passed_to_a_helper_takes_linear_time() {
    let src = r#"<?phv
function first(list<int> $xs): int {
    return $xs[0];
}
function size(string $s): int {
    $t = $s;
    return strlen($t);
}
function has(map<string, int> $m, string $k): bool {
    return $m->has($k);
}
function kept(list<int> $xs): list<int> {
    return $xs;
}
$n = 40000;
$xs = [1];
$t = 0;
for ($i = 0; $i < $n; $i++) {
    $t += first($xs);
    $xs[] = $i;
}
$s = "";
for ($i = 0; $i < $n; $i++) {
    $t += size($s);
    $s .= "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
}
map<string, int> $m = [];
for ($i = 0; $i < $n; $i++) {
    if (!has($m, "k" . $i)) {
        $m["k" . $i] = $i;
    }
}
$ys = [0];
for ($i = 0; $i < $n; $i++) {
    $ys = kept($ys);
    $ys[] = $i;
}
echo count($xs), " ", strlen($s), " ", count($m), " ", count($ys), " ", $t;
"#;
    let start = Instant::now();
    let limits = Limits { max_time: Some(Duration::from_secs(60)), ..Limits::default() };
    let (out, _) = run_src("helper", src, limits);
    assert_eq!(out, "40001 2560000 40000 40001 51198760000");
    // Copying at every pass copies more than 50 GB in all.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

/// A loop that destructures a list or a map and then changes it: the value
/// was kept in a hidden variable for the rest of the function, so it stayed
/// shared and every later write copied all of it (21st assessment: 80,000
/// passes took 37 s). The hidden variable is emptied once the targets are
/// assigned, in nested patterns, map patterns and patterns with a rest too,
/// and the results stay what they were.
#[test]
fn changing_a_value_after_destructuring_it_takes_linear_time() {
    let src = r#"<?phv
function lists(int $n): int {
    list<int> $xs = [1, 2, 3];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        [$a, $b] = $xs;
        $xs[] = $i;
        $xs[2] = $a + $b;
        $t += $a + $b;
    }
    return $t + count($xs);
}
function rest(int $n): int {
    list<int> $xs = [1, 2, 3];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        [$first, ...$others] = $xs->slice(0, 3);
        [$p] = $xs;
        $xs[] = $first + count($others);
        $t += $p;
    }
    return $t + count($xs);
}
function nested(int $n): int {
    list<list<int>> $rows = [[1, 2], [3, 4]];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        [[$a, $b], [$c]] = $rows;
        $rows[] = [$i, $a];
        $rows[1][] = $i;
        $t += $a + $b + $c;
    }
    return $t + count($rows) + count($rows[1]);
}
function maps(int $n): int {
    map<string, int> $m = ["id" => 7, "n" => 0];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        ["id" => $id, "n" => $seen] = $m;
        $m["k" . $i] = $i;
        $m["n"] = $seen + 1;
        $t += $id;
    }
    return $t + count($m) + $m["n"];
}
// The value destructured is the one from before the targets are assigned.
[$x, $y] = [1, 2];
[$x, $y] = [$y, $x];
list<list<int>> $pair = [[5, 6], [9]];
[$q, $r] = $pair;
$pair[] = [0];
echo lists(60000), " ", rest(60000), " ", nested(60000), " ", maps(60000), " ", $x, $y, " ", json_encode($q), json_encode($r), count($pair);
"#;
    let start = Instant::now();
    let limits = Limits { max_time: Some(Duration::from_secs(60)), ..Limits::default() };
    let (out, _) = run_src("destructure", src, limits);
    assert_eq!(out, "240003 120003 480004 540002 21 [5,6][9]3");
    // Copying at every pass copies tens of GB in all.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

/// A helper that puts the caller's list, string or map in an object: for a
/// while a returning call left objects in its registers (they are shared by
/// reference), but the object left there held the caller's value, which
/// stayed shared, so the caller's next append copied all of it (21st
/// assessment: 40,000 passes took 5 to 16 s against 0.04 s, and a request
/// held a worker for 9 s). The object made by `new`, the object passed as
/// an argument, the receiver of a method and the object a callee returned
/// are all let go when the call returns.
#[test]
fn growing_a_value_a_helper_put_in_an_object_takes_linear_time() {
    let src = r#"<?phv
class View {
    public function __construct(public list<int> $rows, public string $title) {}
    public function size(): int { return count($this->rows); }
}
class Page {
    public string $html = "";
    public map<string, int> $seen = [];
}
function summary(list<int> $rows): string {
    $v = new View($rows, "report");
    return $v->title->upper() . " " . $v->size();
}
function size(View $v): int {
    return count($v->rows) + strlen($v->title);
}
function wrap(list<int> $rows): View {
    return new View($rows, "t");
}
function wrapped(list<int> $rows): int {
    $v = wrap($rows);
    return $v->size();
}
function page(string $html, map<string, int> $seen): int {
    $p = new Page();
    $p->html = $html;
    $p->seen = $seen;
    return strlen($p->html) + count($p->seen);
}
$n = 100000;
list<int> $rows = [];
$out = "";
for ($i = 0; $i < $n; $i++) {
    $rows[] = $i;
    $out = summary($rows);
}
$t = 0;
list<int> $as = [];
for ($i = 0; $i < $n; $i++) {
    $as[] = $i;
    $t += size(new View($as, "x"));
}
list<int> $bs = [];
for ($i = 0; $i < $n; $i++) {
    $bs[] = $i;
    $t += wrapped($bs) + (new View($bs, "m"))->size();
}
$html = "";
map<string, int> $seen = [];
for ($i = 0; $i < $n; $i++) {
    $html .= "<li>item</li>";
    $seen["k" . $i] = $i;
    $t += page($html, $seen) % 7;
}
echo $out, " ", $t, " ", count($rows), " ", count($as), " ", count($bs), " ", strlen($html), " ", count($seen);
"#;
    let start = Instant::now();
    let limits = Limits { max_time: Some(Duration::from_secs(60)), ..Limits::default() };
    let (out, _) = run_src("wrapped", src, limits);
    assert_eq!(out, "REPORT 100000 15000250000 100000 100000 100000 1300000 100000");
    // Copying at every pass copies more than 200 GB in all.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

/// What a returned call built is freed when its caller lets it go: an object
/// left in the dead call's register kept the whole graph alive (21st
/// assessment: a script that loaded a report twice ran out of memory where
/// 1.3.2 did not, and a recursion kept one list per level).
#[test]
fn what_a_returned_call_built_is_freed() {
    let src = r#"<?phv
class Row {
    public ?Row $next = null;
    public int $v = 0;
}
class Box {
    public list<int> $xs = [];
}
function load(int $n): ?Row {
    ?Row $head = null;
    for ($i = 0; $i < $n; $i++) {
        $row = new Row();
        $row->next = $head;
        $row->v = $i;
        $head = $row;
    }
    return $head;
}
function total(?Row $row): int {
    $t = 0;
    while ($row != null) {
        $t += $row->v;
        $row = $row->next;
    }
    return $t;
}
function first(int $n): int {
    $rows = load($n);
    return total($rows);
}
function walk(int $depth, int $k): int {
    if ($depth == 0) {
        return 0;
    }
    $t = walk($depth - 1, $k);
    $xs = range(1, $k);
    $t += $k;
    $b = new Box();
    $b->xs = $xs;
    return $t;
}
echo first(160000), " ";
$again = load(160000);
echo total($again), " ", walk(200, 25000);
"#;
    // One chain of 160,000 rows fits (about 20 MB); two do not, nor do the
    // 200 lists of 25,000 ints (80 MB) of the recursion.
    const LIMIT: usize = 32 << 20;
    let limits = Limits { max_memory: Some(LIMIT), ..Limits::default() };
    let (out, peak) = run_src("freed", src, limits);
    assert_eq!(out, "12799920000 12799920000 5000000");
    assert!(peak < LIMIT, "held {} MB", peak >> 20);
}

/// Reading an element of a property (or of a row) and then growing the
/// property: the copy of the collection read for the element stayed in a
/// temporary register until that register was used again, so the property
/// stayed shared and `$o->xs[] = ...` copied all of it; the same for a string
/// property compared and then appended to (21st assessment: 40,000 passes of
/// `$x = $o->xs[0]; $o->xs[] = $x;` took 4 to 12 s in both engines, also in
/// 1.3.2). The temporary is emptied once the element is read.
#[test]
fn growing_a_property_after_reading_an_element_of_it_takes_linear_time() {
    let src = r#"<?phv
class A {
    public list<int> $xs = [];
    public list<string> $names = [];
    public map<int, int> $ids = [];
    public map<string, int> $byName = [];
    public string $text = "a";
    public ?string $note = "n";
    public list<list<int>> $rows = [];

    public function __construct() {
        $this->xs = [1];
        $this->names = ["p"];
        $this->ids = [0 => 5];
        $this->byName = ["k" => 5];
        $this->rows = [[1], [2]];
    }
    public function fill(int $n): int {
        for ($i = 0; $i < $n; $i++) {
            $x = $this->xs[0];
            $this->xs[] = $x;
        }
        return count($this->xs);
    }
}
function lists(A $a, int $n): int {
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $x = $a->xs[0];
        $a->xs[] = $x;
        $t += $i >= 0 ? $a->xs[1] : 3;
        $a->xs[] = $t % 7;
    }
    return count($a->xs) + $t;
}
function strings(A $a, int $n): int {
    for ($i = 0; $i < $n; $i++) {
        $c = $a->names[0];
        $a->names[] = $c;
    }
    return count($a->names);
}
function maps(A $a, int $n): int {
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $x = $a->ids[0];
        $a->ids[$i + 1] = $x;
        $t += $a->xs[0] + ($a->byName["k"] ?? 1) + ($a->byName["none"] ?? 2);
        $a->byName["k" . $i] = $i;
    }
    return count($a->ids) + count($a->byName) + $t;
}
function compared(A $a, int $n): int {
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $t += $i % 3 + ($a->text == "q" ? 1 : 0) + ($a->text < "b" ? 1 : 0);
        $a->text .= "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        if ($a->note == "n" && $a->note != null) {
            $t += 1;
        }
    }
    return strlen($a->text) + strlen($a->note ?? "") + $t;
}
function nested(A $a, int $n): int {
    list<list<int>> $grid = [[1], [2]];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $x = $grid[0][0];
        $grid[0][] = $x;
        $t += $a->rows[1][0];
        $a->rows[1][] = $i;
    }
    return count($grid[0]) + count($a->rows[1]) + $t;
}
$n = 100000;
$a = new A();
echo lists($a, $n), " ", $a->fill($n), " ", strings($a, $n), " ", maps($a, $n), " ", compared($a, $n), " ", nested($a, $n), " ";
$b = new A();
for ($i = 0; $i < $n; $i++) {
    $x = $b->xs[0];
    $b->xs[] = $x;
    if ($b->text == "a") {
        $b->text .= "b";
    }
}
echo count($b->xs), " ", $b->text;
"#;
    let start = Instant::now();
    let limits = Limits { max_time: Some(Duration::from_secs(60)), ..Limits::default() };
    let (out, _) = run_src("element", src, limits);
    assert_eq!(out, "300001 300001 100001 1000002 6700001 400002 100001 ab");
    // Copying at every pass copies more than 100 GB in each function.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

/// A value read into a temporary register and used once: the object a
/// call or `new` returned as the receiver of a property read or the
/// operand of `??`, a conditional, `match`, `?->`, `instanceof`, a
/// comparison, `echo`, `foreach`, a join, an assignment to a property, a
/// call whose result is not used. Nothing emptied the register, so the
/// object (and the caller's list it held) or the string stayed shared until
/// an instruction happened to use the register again, and the caller's next
/// `$rows[] = ...` or `.=` copied all of it (21st assessment:
/// `$rows[] = $i; $t += strlen(view($rows)->title);` took 8 to 10 s at
/// 40,000 passes in both engines, also in 1.3.2). A temporary that still
/// holds such a value is emptied when its expression or statement ends.
#[test]
fn growing_a_value_after_using_a_temporary_that_held_it_takes_linear_time() {
    let src = r#"<?phv
class View {
    public ?View $next = null;
    public int $n = 1;
    public function __construct(public list<int> $rows, public string $title) {}
    public function size(): int { return count($this->rows); }
    public function me(): View { return $this; }
}
function view(list<int> $rows): View {
    return new View($rows, "report");
}
function same(list<int> $rows): list<int> {
    return $rows;
}
function text(string $s): string {
    return $s;
}
function maybe(View $v, bool $some): ?View {
    return $some ? $v : null;
}
function receivers(int $n): int {
    list<int> $rows = [];
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $rows[] = $i;
        $t += strlen(view($rows)->title);
        $rows[] = $i;
        $t += count(view($rows)->rows);
        $rows[] = $i;
        $t += strlen((new View($rows, "r"))->title);
        $rows[] = $i;
        $t += view($rows)->me()->me()->n + view($rows)->size();
    }
    return $t + count($rows);
}
function choices(int $n): int {
    list<int> $rows = [];
    $keep = new View([], "k");
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $rows[] = $i;
        $t += (view($rows)->next ?? $keep)->n;
        $rows[] = $i;
        $t += ($i >= 0 ? view($rows) : $keep)->n;
        $rows[] = $i;
        $t += (match ($i % 2) { 0 => view($rows), default => $keep })->n;
        $rows[] = $i;
        $t += maybe(view($rows), $i > 0)?->n ?? 0;
        $rows[] = $i;
        $t += match (view($rows)->title) { "a" => 1, "b" => 3, default => 2 };
    }
    return $t + count($rows);
}
function tests(int $n): int {
    list<int> $rows = [];
    list<int> $other = [];
    $keep = new View([], "k");
    $s = "";
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $rows[] = $i;
        if (view($rows) instanceof View) {
            $t += 1;
        }
        $rows[] = $i;
        $t += same($rows) == $other ? 1 : 2;
        $rows[] = $i;
        if (view($rows)->next == null && !(view($rows) == $keep)) {
            $t += 1;
        }
        $rows[] = $i;
        $k = 0;
        while (view($rows)->n > $k) {
            $k++;
        }
        $s .= "0123456789abcdef0123456789abcdef";
        if (text($s) == "zz" || text($s) < "0") {
            $t += 1;
        }
        $s .= "0123456789abcdef0123456789abcdef";
        $t += text($s) <=> "a";
    }
    return $t + count($rows) + strlen($s);
}
function uses(int $n): int {
    list<int> $rows = [];
    $keep = new View([], "k");
    $s = "";
    $t = 0;
    for ($i = 0; $i < $n; $i++) {
        $rows[] = $i;
        echo view($rows)->title == "x" ? "x" : "";
        $rows[] = $i;
        foreach (view($rows)->rows as $v) {
            $t += $v;
            break;
        }
        $rows[] = $i;
        $t += strlen("n" . view($rows)->title . $i);
        $rows[] = $i;
        $keep->next = view($rows);
        $keep->next = null;
        $rows[] = $i;
        view($rows);
        same($rows);
        $rows[] = $i;
        $keep->rows = same($rows);
        $keep->rows = [];
        $s .= "0123456789abcdef0123456789abcdef";
        $keep->title = text($s);
        $keep->title = "";
        $s .= "0123456789abcdef0123456789abcdef";
        $t += strlen((string) text($s));
    }
    return $t + count($rows) + strlen($s);
}
$n = 30000;
echo receivers($n), " ", choices($n), " ", tests($n), " ", uses($n), " ";
list<int> $rows = [];
$t = 0;
for ($i = 0; $i < $n; $i++) {
    $rows[] = $i;
    $t += strlen(view($rows)->title);
    $rows[] = $i;
    $t += view($rows)->next == null ? 1 : 0;
}
echo $t, " ", count($rows);
"#;
    let start = Instant::now();
    let limits = Limits { max_time: Some(Duration::from_secs(60)), ..Limits::default() };
    let (out, _) = run_src("temporary", src, limits);
    assert_eq!(out, "3600420000 329999 2130000 28803408890 210000 60000");
    // Copying at every pass copies some 7 GB for each of the 25 uses.
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}
