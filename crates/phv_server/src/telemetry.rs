//! Traces sent to an OpenTelemetry collector (`[telemetry]`), as OTLP/HTTP
//! with JSON bodies (`POST <endpoint>/v1/traces`).
//!
//! Each request gets a server span, each request a script sends to an HTTP
//! service a client span under it (its `traceparent` names that span), and
//! each run of a background job a span of its own. Spans are queued and sent
//! by one thread, in batches of up to 512 every 5 seconds. The queue holds at
//! most 8,192 spans: past that, spans are dropped (counted in
//! `phv_telemetry_dropped_spans_total`), so a collector that is slow or down
//! never holds the server's memory or its requests. A failed export is
//! dropped too, and reported at most once a minute.

use phv_driver::config::Telemetry;
use phv_vm::{Span, SpanKind, SpanValue};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

const QUEUE: usize = 8192;
const BATCH: usize = 512;
const EVERY: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(10);
const WARN_EVERY: Duration = Duration::from_secs(60);

pub struct Exporter {
    /// None once closed.
    tx: std::sync::Mutex<Option<SyncSender<Vec<Span>>>>,
    /// Gets a message when the thread has sent what it held.
    done: std::sync::Mutex<Receiver<()>>,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    sample: f64,
}

impl Exporter {
    /// Starts the thread that sends spans; None (with a warning) when the
    /// endpoint's environment variable is not set.
    pub fn start(cfg: &Telemetry, dropped: Arc<AtomicU64>) -> Option<Arc<Exporter>> {
        let Some(url) = cfg.url.clone() else {
            let var = cfg.endpoint_env.as_deref().unwrap_or("?");
            eprintln!("warning: [telemetry]: the environment variable {var} is not set; traces are not sent");
            return None;
        };
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        for (name, value) in &cfg.headers {
            match value {
                Some(v) => headers.push((name.clone(), v.clone())),
                None => eprintln!("warning: [telemetry]: the {name} header's environment variable is not set"),
            }
        }
        let (tx, rx) = sync_channel(QUEUE);
        let queued = Arc::new(AtomicUsize::new(0));
        let resource = resource(&cfg.service_name);
        let (tls, q) = (cfg.tls, queued.clone());
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("phv-telemetry".into())
            .spawn(move || {
                send_loop(rx, q, url, tls, headers, resource);
                let _ = done_tx.send(());
            })
            .ok()?;
        let (tx, done) = (std::sync::Mutex::new(Some(tx)), std::sync::Mutex::new(done));
        Some(Arc::new(Exporter { tx, done, queued, dropped, sample: cfg.sample }))
    }

    /// Sends what is queued and stops, waiting at most `wait`.
    pub fn close(&self, wait: Duration) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        let _ = self.done.lock().unwrap_or_else(|e| e.into_inner()).recv_timeout(wait);
    }

    /// Whether a new trace is recorded.
    pub fn sample(&self) -> bool {
        if self.sample >= 1.0 {
            return true;
        }
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).expect("operating system random number generator failed");
        let share = (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64;
        share < self.sample
    }

    /// Queues spans to send, or drops them when the queue is full.
    pub fn send(&self, spans: Vec<Span>) {
        let n = spans.len();
        if n == 0 {
            return;
        }
        let sent = || match &*self.tx.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(tx) => tx.try_send(spans).is_ok(),
            None => false,
        };
        if self.queued.fetch_add(n, Ordering::Relaxed) + n > QUEUE || !sent() {
            self.queued.fetch_sub(n, Ordering::Relaxed);
            self.dropped.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
}

fn resource(service: &str) -> Value {
    json!({
        "attributes": [
            attr("service.name", &SpanValue::Str(service.to_string())),
            attr("telemetry.sdk.name", &SpanValue::Str("phv".into())),
            attr("telemetry.sdk.language", &SpanValue::Str("phv".into())),
            attr("telemetry.sdk.version", &SpanValue::Str(env!("CARGO_PKG_VERSION").into())),
        ]
    })
}

fn attr(key: &str, v: &SpanValue) -> Value {
    match v {
        SpanValue::Str(s) => json!({ "key": key, "value": { "stringValue": s } }),
        // OTLP/JSON writes 64-bit integers as strings.
        SpanValue::Int(i) => json!({ "key": key, "value": { "intValue": i.to_string() } }),
    }
}

fn nanos(t: std::time::SystemTime) -> String {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().to_string()
}

/// A span in OTLP/JSON (ids as hex, as the JSON encoding asks).
pub fn span_json(s: &Span) -> Value {
    let kind = match s.kind {
        SpanKind::Internal => 1,
        SpanKind::Server => 2,
        SpanKind::Client => 3,
        SpanKind::Consumer => 5,
    };
    let mut v = json!({
        "traceId": s.trace_id,
        "spanId": s.span_id,
        "name": s.name,
        "kind": kind,
        "startTimeUnixNano": nanos(s.start),
        "endTimeUnixNano": nanos(s.end),
        "attributes": s.attributes.iter().map(|(k, v)| attr(k, v)).collect::<Vec<_>>(),
        "status": { "code": if s.error { 2 } else { 0 } },
    });
    if let Some(p) = &s.parent_id {
        v["parentSpanId"] = json!(p);
    }
    v
}

pub fn body(resource: &Value, spans: &[Span]) -> Vec<u8> {
    let doc = json!({
        "resourceSpans": [{
            "resource": resource,
            "scopeSpans": [{
                "scope": { "name": "phv", "version": env!("CARGO_PKG_VERSION") },
                "spans": spans.iter().map(span_json).collect::<Vec<_>>(),
            }],
        }],
    });
    serde_json::to_vec(&doc).unwrap_or_default()
}

fn send_loop(
    rx: Receiver<Vec<Span>>,
    queued: Arc<AtomicUsize>,
    url: String,
    tls: Option<bool>,
    headers: Vec<(String, String)>,
    resource: Value,
) {
    let mut batch: Vec<Span> = Vec::new();
    let mut due = Instant::now() + EVERY;
    let mut warned: Option<Instant> = None;
    let mut export = |batch: &mut Vec<Span>| {
        if batch.is_empty() {
            return;
        }
        let spans = std::mem::take(batch);
        queued.fetch_sub(spans.len(), Ordering::Relaxed);
        let result = phv_std::post_telemetry(&url, tls, headers.clone(), body(&resource, &spans), TIMEOUT);
        let problem = match result {
            Ok(status) if (200..300).contains(&status) => None,
            Ok(status) => Some(format!("the collector answered {status}")),
            Err(e) => Some(e),
        };
        if let Some(p) = problem {
            if warned.is_none_or(|w| w.elapsed() >= WARN_EVERY) {
                eprintln!("warning: [telemetry]: {} spans were not sent: {p}", spans.len());
                warned = Some(Instant::now());
            }
        }
    };
    loop {
        let wait = due.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(spans) => {
                batch.extend(spans);
                while batch.len() >= BATCH {
                    let rest = batch.split_off(BATCH);
                    export(&mut batch);
                    batch = rest;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                export(&mut batch);
                return;
            }
        }
        if Instant::now() >= due {
            export(&mut batch);
            due = Instant::now() + EVERY;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_otlp_json() {
        let t = std::time::SystemTime::now();
        let s = Span {
            trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
            span_id: "b7ad6b7169203331".into(),
            parent_id: None,
            name: "GET /posts/[id]".into(),
            kind: SpanKind::Server,
            start: t,
            end: t,
            attributes: vec![("http.response.status_code", SpanValue::Int(200))],
            error: false,
        };
        let v: Value = serde_json::from_slice(&body(&resource("blog"), &[s])).unwrap();
        let span = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["kind"], 2);
        assert_eq!(span["traceId"], "0af7651916cd43dd8448eb211c80319c");
        assert!(span.get("parentSpanId").is_none());
        assert_eq!(span["attributes"][0]["value"]["intValue"], "200");
        assert_eq!(v["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"], "blog");
    }
}
