//! What the server counts, for `/metrics` on the admin port (`[admin]`), in
//! the Prometheus text format. Counters are atomics: counting costs a few
//! nanoseconds a request and takes no lock, except the histogram of a page's
//! route, found in a map under a mutex. Labels never hold text from a request:
//! routes are the names of the site's files (at most `MAX_ROUTES` of them).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What answered a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Page,
    Static,
    Ws,
    Sse,
    /// The server itself: 404 without a page, 429, a CORS preflight, ...
    Other,
}

const KINDS: [(Kind, &str); 5] =
    [(Kind::Page, "page"), (Kind::Static, "static"), (Kind::Ws, "ws"), (Kind::Sse, "sse"), (Kind::Other, "other")];

/// Why a request was turned away before a script ran.
#[derive(Clone, Copy, Debug)]
pub enum Reject {
    /// No worker became free in time.
    Busy,
    /// Too much output was waiting to be sent.
    Output,
    /// The budget of request bodies was used up.
    Body,
    RateLimit,
    TooLarge,
}

const REJECTS: [&str; 5] = ["busy", "output", "body", "rate_limit", "too_large"];

/// How a script failed.
#[derive(Clone, Copy, Debug)]
pub enum Failure {
    Exception,
    Time,
    Memory,
    Compile,
    Other,
}

const FAILURES: [&str; 5] = ["exception", "time", "memory", "compile", "other"];

impl Failure {
    /// The kind of a runtime error, from its message.
    pub fn of(message: &str) -> Failure {
        if message.starts_with("Uncaught ") {
            Failure::Exception
        } else if message.starts_with("maximum execution time") {
            Failure::Time
        } else if message.starts_with("allowed memory size") {
            Failure::Memory
        } else {
            Failure::Other
        }
    }
}

/// A store the server depends on, for `/ready` and `phv_store_errors_total`.
#[derive(Clone, Copy, Debug)]
pub enum StoreKind {
    Sessions,
    RateLimit,
    Publish,
    Jobs,
}

const STORES: [&str; 4] = ["sessions", "rate_limit", "publish", "jobs"];

/// Whether a store answered the last time it was used.
#[derive(Default)]
pub struct Health {
    errors: [AtomicU64; 4],
    /// Milliseconds since the start of the last failure and the last success.
    failed_at: [AtomicU64; 4],
    ok_at: [AtomicU64; 4],
    started: OnceInstant,
}

/// An `Instant` that is set when first read.
#[derive(Default)]
struct OnceInstant(std::sync::OnceLock<Instant>);

impl OnceInstant {
    fn ms(&self) -> u64 {
        // +1: zero means "never".
        self.0.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
    }
}

impl Health {
    pub fn failed(&self, store: StoreKind) {
        let i = store as usize;
        self.errors[i].fetch_add(1, Ordering::Relaxed);
        self.failed_at[i].store(self.started.ms(), Ordering::Relaxed);
    }

    pub fn ok(&self, store: StoreKind) {
        self.ok_at[store as usize].store(self.started.ms(), Ordering::Relaxed);
    }

    /// The stores whose last use failed.
    pub fn down(&self) -> Vec<&'static str> {
        (0..STORES.len())
            .filter(|&i| self.failed_at[i].load(Ordering::Relaxed) > self.ok_at[i].load(Ordering::Relaxed))
            .map(|i| STORES[i])
            .collect()
    }
}

/// Upper bounds of the histogram buckets, in seconds.
const BUCKETS: [f64; 12] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

#[derive(Default)]
pub struct Histogram {
    /// Observations at or below each bound (not cumulative; summed when written).
    buckets: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    sum_us: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        if let Some(i) = BUCKETS.iter().position(|b| secs <= *b) {
            self.buckets[i].fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(d.as_micros().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    fn write(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut total = 0;
        for (i, b) in BUCKETS.iter().enumerate() {
            total += self.buckets[i].load(Ordering::Relaxed);
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {total}");
        }
        let count = self.count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {count}");
        let braces = if labels.is_empty() { String::new() } else { format!("{{{labels}}}") };
        let sum = self.sum_us.load(Ordering::Relaxed) as f64 / 1e6;
        let _ = writeln!(out, "{name}_sum{braces} {sum}");
        let _ = writeln!(out, "{name}_count{braces} {count}");
    }
}

/// Routes with a histogram of their own; the rest share `other`.
pub const MAX_ROUTES: usize = 1000;

#[derive(Default)]
pub struct Metrics {
    requests: [[AtomicU64; 5]; KINDS.len()],
    routes: Mutex<HashMap<String, Arc<Histogram>>>,
    other_route: Arc<Histogram>,
    worker_wait: Histogram,
    rejected: [AtomicU64; REJECTS.len()],
    failures: [AtomicU64; FAILURES.len()],
    pub ws_in: AtomicU64,
    pub ws_out: AtomicU64,
    /// `publish()`: delivered in this process, sent to the others, received
    /// from them, dropped (a queue was full or the store was down), refused
    /// (a message from the store that was not valid), refused for its time
    /// alone (signed with the site's key, but sent too far from this
    /// process's clock or before it subscribed again).
    pub publish_local: AtomicU64,
    pub publish_sent: AtomicU64,
    pub publish_received: AtomicU64,
    pub publish_dropped: AtomicU64,
    pub publish_invalid: AtomicU64,
    pub publish_clock: AtomicU64,
    /// Jobs: added, finished, failed (to be tried again), given up.
    pub jobs_enqueued: AtomicU64,
    pub jobs_done: AtomicU64,
    pub jobs_failed: AtomicU64,
    pub jobs_dead: AtomicU64,
    pub job_duration: Histogram,
    pub health: Arc<Health>,
}

impl Metrics {
    /// Counts a request that was answered with `status`, and how long it took.
    pub fn request(&self, kind: Kind, route: Option<&str>, status: u16, took: Duration) {
        let class = (status / 100).clamp(1, 5) as usize - 1;
        let k = KINDS.iter().position(|(x, _)| *x == kind).unwrap_or(KINDS.len() - 1);
        self.requests[k][class].fetch_add(1, Ordering::Relaxed);
        let label = match (kind, route) {
            (Kind::Page, Some(r)) => r,
            (Kind::Static, _) => "static",
            _ => return,
        };
        let h = {
            let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
            match routes.get(label) {
                Some(h) => h.clone(),
                None if routes.len() < MAX_ROUTES => {
                    let h = Arc::new(Histogram::default());
                    routes.insert(label.to_string(), h.clone());
                    h
                }
                None => self.other_route.clone(),
            }
        };
        h.observe(took);
    }

    pub fn reject(&self, why: Reject) {
        self.rejected[why as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn failure(&self, why: Failure) {
        self.failures[why as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn worker_wait(&self, d: Duration) {
        self.worker_wait.observe(d);
    }

    /// The text of `/metrics`, with the gauges the server reads at the time.
    pub fn render(&self, gauges: &[(&str, &str, f64)]) -> String {
        let mut out = String::with_capacity(8192);
        let head = |out: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        head(&mut out, "phv_build_info", "gauge", "The version of PHV.");
        let _ = writeln!(out, "phv_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));
        for (name, help, value) in gauges {
            head(&mut out, name, "gauge", help);
            let _ = writeln!(out, "{name} {value}");
        }
        head(&mut out, "phv_requests_total", "counter", "Requests answered, by what answered them and status class.");
        for (k, (_, kind)) in KINDS.iter().enumerate() {
            for class in 0..5 {
                let n = self.requests[k][class].load(Ordering::Relaxed);
                let _ = writeln!(out, "phv_requests_total{{kind=\"{kind}\",code=\"{}xx\"}} {n}", class + 1);
            }
        }
        head(
            &mut out,
            "phv_request_duration_seconds",
            "histogram",
            "Time to answer, by page route (a file of public/), static files, and other pages.",
        );
        let mut routes: Vec<(String, Arc<Histogram>)> =
            self.routes.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        routes.sort_by(|a, b| a.0.cmp(&b.0));
        for (route, h) in &routes {
            h.write(&mut out, "phv_request_duration_seconds", &format!("route=\"{}\"", label_value(route)));
        }
        if self.other_route.count.load(Ordering::Relaxed) > 0 {
            self.other_route.write(&mut out, "phv_request_duration_seconds", "route=\"other\"");
        }
        head(&mut out, "phv_worker_wait_seconds", "histogram", "Time a request waited for a worker.");
        self.worker_wait.write(&mut out, "phv_worker_wait_seconds", "");
        head(&mut out, "phv_rejected_total", "counter", "Requests turned away before a script ran, by reason.");
        for (i, r) in REJECTS.iter().enumerate() {
            let _ = writeln!(out, "phv_rejected_total{{reason=\"{r}\"}} {}", self.rejected[i].load(Ordering::Relaxed));
        }
        head(&mut out, "phv_script_failures_total", "counter", "Scripts that failed, by kind of error.");
        for (i, f) in FAILURES.iter().enumerate() {
            let n = self.failures[i].load(Ordering::Relaxed);
            let _ = writeln!(out, "phv_script_failures_total{{kind=\"{f}\"}} {n}");
        }
        head(&mut out, "phv_ws_messages_total", "counter", "WebSocket messages received and sent.");
        let _ = writeln!(out, "phv_ws_messages_total{{dir=\"in\"}} {}", self.ws_in.load(Ordering::Relaxed));
        let _ = writeln!(out, "phv_ws_messages_total{{dir=\"out\"}} {}", self.ws_out.load(Ordering::Relaxed));
        head(&mut out, "phv_publish_total", "counter", "Messages of publish(), by where they went.");
        for (result, n) in [
            ("local", &self.publish_local),
            ("sent", &self.publish_sent),
            ("received", &self.publish_received),
            ("dropped", &self.publish_dropped),
            ("invalid", &self.publish_invalid),
            ("clock", &self.publish_clock),
        ] {
            let _ = writeln!(out, "phv_publish_total{{result=\"{result}\"}} {}", n.load(Ordering::Relaxed));
        }
        head(&mut out, "phv_jobs_total", "counter", "Background jobs, by what became of them.");
        for (result, n) in [
            ("enqueued", &self.jobs_enqueued),
            ("done", &self.jobs_done),
            ("failed", &self.jobs_failed),
            ("dead", &self.jobs_dead),
        ] {
            let _ = writeln!(out, "phv_jobs_total{{result=\"{result}\"}} {}", n.load(Ordering::Relaxed));
        }
        head(&mut out, "phv_job_duration_seconds", "histogram", "Time a job ran.");
        self.job_duration.write(&mut out, "phv_job_duration_seconds", "");
        head(&mut out, "phv_store_errors_total", "counter", "Failures of the stores the server uses.");
        for (i, s) in STORES.iter().enumerate() {
            let n = self.health.errors[i].load(Ordering::Relaxed);
            let _ = writeln!(out, "phv_store_errors_total{{store=\"{s}\"}} {n}");
        }
        out
    }
}

/// A label value: `\`, `"` and line breaks escaped (routes are file names,
/// which can hold them on Unix).
fn label_value(s: &str) -> String {
    // `\r` too, and other controls dropped (found by the 1.1 audit; labels are
    // names of the site's files, so this is for safety only).
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r'))
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// The route of a page: its path under `public/` without `.phv`, and `index`
/// left out (`public/posts/[id].phv` is `/posts/[id]`).
pub fn route_of(public: &std::path::Path, script: &std::path::Path) -> String {
    let rel = script.strip_prefix(public).unwrap_or(script).to_string_lossy().replace('\\', "/");
    let rel = rel.strip_suffix(".phv").unwrap_or(&rel);
    let rel = rel.strip_suffix("index").unwrap_or(rel);
    let rel = rel.strip_suffix('/').unwrap_or(rel);
    format!("/{rel}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A label value cannot end its line (found by the 1.1 audit: `\r` was kept).
    #[test]
    fn label_values_stay_on_their_line() {
        assert_eq!(label_value("a\"b\\c\nd\re\u{1b}f"), "a\\\"b\\\\c\\nd\\re".to_string() + "f");
    }

    #[test]
    fn routes_are_file_names_and_capped() {
        let p = std::path::Path::new("/site/public");
        assert_eq!(route_of(p, &p.join("index.phv")), "/");
        assert_eq!(route_of(p, &p.join("posts/[id].phv")), "/posts/[id]");
        assert_eq!(route_of(p, &p.join("docs/index.phv")), "/docs");
        let m = Metrics::default();
        for i in 0..MAX_ROUTES + 10 {
            m.request(Kind::Page, Some(&format!("/r{i}")), 200, Duration::from_millis(3));
        }
        assert_eq!(m.routes.lock().unwrap().len(), MAX_ROUTES);
        assert_eq!(m.other_route.count.load(Ordering::Relaxed), 10);
        let text = m.render(&[("phv_workers", "Workers.", 8.0)]);
        assert!(text.contains("phv_requests_total{kind=\"page\",code=\"2xx\"} 1010"), "{text}");
        assert!(text.contains("phv_request_duration_seconds_bucket{route=\"other\",le=\"0.005\"} 10"));
        assert!(text.contains("phv_workers 8\n"));
    }

    #[test]
    fn failures_are_told_apart_by_their_message() {
        assert!(matches!(Failure::of("Uncaught ValueError: x"), Failure::Exception));
        assert!(matches!(Failure::of("maximum execution time of 30 seconds exceeded"), Failure::Time));
        assert!(matches!(Failure::of("allowed memory size of 128 MB exhausted"), Failure::Memory));
        assert!(matches!(Failure::of("maximum call stack depth exceeded"), Failure::Other));
    }

    #[test]
    fn a_store_is_down_until_it_answers_again() {
        let h = Health::default();
        assert!(h.down().is_empty());
        h.failed(StoreKind::Publish);
        assert_eq!(h.down(), ["publish"]);
        std::thread::sleep(Duration::from_millis(2));
        h.ok(StoreKind::Publish);
        assert!(h.down().is_empty());
    }
}
