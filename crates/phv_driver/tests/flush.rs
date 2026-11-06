//! Pages that send their output while they run (`response()->flush()`), run
//! with a `FlushSink` as `phv serve` runs them. Regression tests for the 1.2
//! audit (security-audit.md §26).

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::{thread_allocated, CountingAlloc};
use phv_vm::{FlushSink, HttpCtx, Limits};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Keeps what is flushed, as a client that reads would get it.
struct Recording(Arc<Mutex<Vec<u8>>>);

impl FlushSink for Recording {
    fn flush(&mut self, _http: &mut HttpCtx, chunk: Vec<u8>) -> Result<(), Vec<u8>> {
        self.0.lock().unwrap().extend_from_slice(&chunk);
        Ok(())
    }

    fn finish(&mut self, chunk: Vec<u8>) {
        self.0.lock().unwrap().extend_from_slice(&chunk);
    }
}

/// A client that has gone away (or a HEAD request): nothing is sent.
/// It counts the flushes.
struct Gone(Arc<AtomicUsize>);

impl FlushSink for Gone {
    fn flush(&mut self, _http: &mut HttpCtx, chunk: Vec<u8>) -> Result<(), Vec<u8>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Err(chunk)
    }

    fn finish(&mut self, _chunk: Vec<u8>) {}
}

fn gone() -> Box<dyn FlushSink> {
    Box::new(Gone(Default::default()))
}

/// Runs `src` as a page with `sink`; its output (or error) and the query `q`.
fn page(name: &str, src: &str, q: &str, sink: Box<dyn FlushSink>, limits: Limits) -> (String, Option<String>) {
    let dir = std::env::temp_dir().join(format!("phv-flush-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, src).unwrap();
    let c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let http = HttpCtx {
        method: "GET".into(),
        path: "/".into(),
        query: vec![("q".into(), q.into())],
        stream: Some(sink),
        ..Default::default()
    };
    let outcome = run(&c, RunOptions { html: true, limits, http: Some(http), host: None });
    let _ = std::fs::remove_dir_all(&dir);
    (outcome.output, outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string()))
}

/// What a client gets from a page that flushes, and its error.
fn streamed(name: &str, src: &str, q: &str) -> (String, Option<String>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let (rest, err) = page(name, src, q, Box::new(Recording(sent.clone())), Limits::default());
    let mut all = String::from_utf8(sent.lock().unwrap().clone()).unwrap();
    if err.is_none() {
        all.push_str(&rest);
    }
    (all, err)
}

/// Output flush() could not send (the client is gone, or the request is
/// HEAD) is freed on the page's thread and stays counted until then (found
/// by the 1.2 audit: it was let go of either way, so each flush added room
/// of room and a 32 MB page held 400–794 MB).
#[test]
fn output_a_gone_client_did_not_take_stays_counted() {
    let limit = 16 << 20;
    let src = r#"<?phv
import std.http.response;
$chunk = str_repeat("x", 1000000);
$open = true;
for ($i = 0; $i < 200; $i++) {
    echo $chunk;
    $open = response()->flush() && $open;
}
echo $open ? "open" : "gone", "\n";
list<string> $kept = [];
for ($j = 0; $j < 8; $j++) {
    $kept[] = str_repeat("y", 4000000);
}
echo "held ", count($kept) * 4, " MB\n";
"#;
    let flushes = Arc::new(AtomicUsize::new(0));
    let sink = Box::new(Gone(flushes.clone()));
    let base = thread_allocated();
    let (out, err) = page("gone", src, "", sink, Limits { max_memory: Some(limit), ..Limits::default() });
    let drift = thread_allocated() - base;
    assert_eq!(flushes.load(Ordering::Relaxed), 200);
    assert!(!out.contains("held"), "{out}");
    // It then allocates twice its limit.
    let err = err.unwrap_or_default();
    assert!(err.contains("allowed memory size of 16 MB exhausted"), "{err}");
    // 200 flushes of 1 MB: the count would be 200 MB below where it began.
    assert!(drift > -(4 << 20), "the thread's count drifted by {} MB", drift >> 20);
}

/// A `catch` after flush() drops only what was written after the output it
/// sent, and writes where the exception left the HTML (found by the 1.2
/// audit: it cut the output at an offset taken before the flush, leaving a
/// tag open, so escaped text became attributes).
#[test]
fn a_catch_after_flush_writes_where_the_output_was_cut() {
    let src = r#"<?phv
import std.http.{request, response};
function title(): string {
    throw new RuntimeException("no title");
}
$m = request()->query("q") ?? "";
?>
<p>Hi</p>
<?phv try { ?>
<?phv response()->flush(); ?>
<div class="box"><a title="<?= title() ?>">link</a></div>
<?phv } catch (RuntimeException) { ?>
<b><?= $m ?></b>
<?phv } ?>
<p>end</p>
"#;
    let (all, err) = streamed("cut", src, "x tabindex=1 autofocus onfocus=alert(1) mk1");
    assert_eq!(err, None);
    assert!(!all.contains("<div"), "{all}");
    assert!(all.contains("<p>Hi</p>") && all.contains("<b>x tabindex=1 autofocus onfocus=alert(1) mk1</b>"), "{all}");
    assert!(all.trim_end().ends_with("<p>end</p>"), "{all}");
}

/// When flush() sent part of the HTML an exception left unfinished (or a
/// `try` began in), the `catch` cannot write in its place: the page stops,
/// and its response is cut off.
#[test]
fn a_catch_cannot_write_into_html_that_flush_sent() {
    let inside = r#"<?phv
import std.http.{request, response};
function first_id(): int {
    response()->flush();
    throw new Exception("lookup failed");
}
$q = request()->query("q") ?? "";
?>
<p>results</p>
<?phv try { ?><a href="/item?id=<?= first_id() ?>">first</a><?phv } catch (Exception $e) { ?><p title="<?= $q ?>">none</p><?phv } ?>
<p>end</p>
"#;
    let (all, err) = streamed("inside", inside, "x onmouseover=alert(1) mk1");
    assert!(err.as_deref().is_some_and(|e| e.contains("caught after response()->flush()")), "{err:?}");
    assert!(!all.contains("mk1"), "{all}");
    // A `try` that began inside an attribute, with a flush in it.
    let began = r#"<?phv
import std.http.{request, response};
function late(): string {
    throw new Exception("late");
}
$q = request()->query("q") ?? "";
?>
<a title="<?phv try { response()->flush(); echo late(); } catch (Exception $e) { echo $q; } ?>">x</a>
"#;
    let (all, err) = streamed("began", began, "mk1");
    assert!(err.as_deref().is_some_and(|e| e.contains("caught after response()->flush()")), "{err:?}");
    assert!(!all.contains("mk1"), "{all}");
    // Without a flush, the same pages go on as before.
    let (all, err) = streamed("unflushed", &inside.replace("    response()->flush();\n", ""), "mk1");
    assert_eq!(err, None);
    assert!(all.contains("<p title=\"mk1\">none</p>") && !all.contains("href"), "{all}");
    // A caught exception outside such HTML goes on after a flush.
    let text = r#"<?phv
import std.http.response;
?>
<p>a</p>
<?phv try { response()->flush(); throw new Exception("x"); } catch (Exception $e) { echo "caught"; } ?>
<p>b</p>
"#;
    let (all, err) = streamed("text", text, "");
    assert_eq!(err, None);
    assert!(all.contains("<p>a</p>") && all.contains("caught") && all.contains("<p>b</p>"), "{all}");
}

/// A page cannot switch to an escaped type (HTML) after writing output that
/// was not escaped (found by the 1.2 audit: text/plain, a visitor's string,
/// then text/html served it as markup).
#[test]
fn output_written_unescaped_cannot_become_html() {
    let src = r#"<?phv
import std.http.{request, response};
$v = request()->query("q") ?? "";
response()->type("text/plain; charset=utf-8");
echo "You searched for: ", $v, "\n";
response()->type("TYPE");
?>
<p>done</p>
"#;
    for ty in ["text/html; charset=utf-8", "application/xhtml+xml", "image/svg+xml", "application/x-unknown"] {
        let (out, err) =
            page("type", &src.replace("TYPE", ty), "<img src=x onerror=alert(1)>", gone(), Limits::default());
        assert!(err.as_deref().is_some_and(|e| e.contains("before writing any output")), "{ty}: {err:?} {out}");
    }
    // Plain types may follow one another, and HTML may become plain.
    let (out, err) = page("plain", &src.replace("TYPE", "text/csv"), "<b>", gone(), Limits::default());
    assert_eq!(err, None);
    assert!(out.contains("You searched for: <b>"), "{out}");
    let html_first =
        "<?phv\nimport std.http.response;\necho \"<b>\";\nresponse()->type(\"text/plain\");\necho \"ok\";\n";
    let (out, err) = page("html", html_first, "", gone(), Limits::default());
    assert_eq!(err, None);
    assert_eq!(out, "&lt;b&gt;ok");
    // Only whitespace written before is fine.
    let blank = "<?phv\nimport std.http.response;\nresponse()->type(\"text/plain\");\necho \"\\n\";\nresponse()->type(\"text/html\");\necho \"<b>\";\n";
    let (out, err) = page("blank", blank, "", gone(), Limits::default());
    assert_eq!(err, None);
    assert_eq!(out, "\n&lt;b&gt;");
}
