//! A small load generator for `phv serve`, used by `bench/serve.ps1` and
//! `bench/serve.sh`: N keep-alive connections, each sending M requests for one
//! path, then the requests per second and the p50 / p99 latency.
//!
//! ```text
//! cargo run --release -p phv_server --example load -- \
//!     --url http://127.0.0.1:8080 --path / --connections 16 --requests 500 [--warmup 50] \
//!     [--cookie PHVSESSID=...] [--json]
//! ```
//!
//! Every connection first sends `--warmup` requests that are not counted, then
//! all connections start the measured requests together. A response other than
//! 200 is counted as an error; a connection the server closes is reopened.
//! `--cookie` sends that `Cookie` header with every request (a page with a
//! session, for example).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

struct Args {
    url: String,
    path: String,
    connections: usize,
    requests: usize,
    warmup: usize,
    cookie: Option<String>,
    json: bool,
}

const USAGE: &str = "usage: load --url http://HOST:PORT [--path /] [--connections 16] [--requests 500] [--warmup 50] \
                     [--cookie NAME=VALUE] [--json]";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        url: String::new(),
        path: "/".into(),
        connections: 16,
        requests: 500,
        warmup: 50,
        cookie: None,
        json: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--url" => args.url = value("--url")?,
            "--path" => args.path = value("--path")?,
            "--connections" => {
                args.connections = value("--connections")?.parse().map_err(|e| format!("--connections: {e}"))?
            }
            "--requests" => args.requests = value("--requests")?.parse().map_err(|e| format!("--requests: {e}"))?,
            "--warmup" => args.warmup = value("--warmup")?.parse().map_err(|e| format!("--warmup: {e}"))?,
            "--cookie" => {
                let c = value("--cookie")?;
                if c.is_empty() || c.bytes().any(|b| b < 0x20 || b == 0x7f) {
                    return Err("--cookie must be a header value without control characters".into());
                }
                args.cookie = Some(c);
            }
            "--json" => args.json = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if args.url.is_empty() {
        return Err("--url is required".into());
    }
    if args.connections == 0 || args.requests == 0 {
        return Err("--connections and --requests must be at least 1".into());
    }
    Ok(args)
}

/// `http://host:port` (a path after the port is ignored) -> `host:port`.
fn host_port(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("--url must start with http://")?;
    let rest = rest.split('/').next().unwrap_or("");
    if rest.is_empty() {
        return Err("--url has no host".into());
    }
    Ok(if rest.contains(':') { rest.to_string() } else { format!("{rest}:80") })
}

struct Conn {
    reader: BufReader<TcpStream>,
}

impl Conn {
    fn open(addr: &str) -> Result<Conn, String> {
        let s = TcpStream::connect(addr).map_err(|e| format!("cannot connect to {addr}: {e}"))?;
        s.set_nodelay(true).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e| e.to_string())?;
        Ok(Conn { reader: BufReader::new(s) })
    }

    /// Sends one request and reads the whole response; returns the status and
    /// whether the server will close the connection.
    fn request(&mut self, req: &[u8], body: &mut Vec<u8>) -> Result<(u16, bool), String> {
        self.reader.get_mut().write_all(req).map_err(|e| format!("write: {e}"))?;
        let mut line = String::new();
        self.reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
        if line.is_empty() {
            return Err("the server closed the connection".into());
        }
        let status: u16 =
            line.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| format!("bad status line {line:?}"))?;
        let http10 = line.starts_with("HTTP/1.0");
        let mut length: Option<usize> = None;
        let mut chunked = false;
        let mut close = http10;
        loop {
            line.clear();
            self.reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let Some((name, value)) = l.split_once(':') else { continue };
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.to_ascii_lowercase().contains("chunked");
            } else if name.eq_ignore_ascii_case("connection") {
                close = value.eq_ignore_ascii_case("close");
            }
        }
        body.clear();
        if chunked {
            loop {
                line.clear();
                self.reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
                let size = line.trim_end().split(';').next().unwrap_or("");
                let size = usize::from_str_radix(size, 16).map_err(|_| format!("bad chunk size {line:?}"))?;
                if size == 0 {
                    // Trailers, then the empty line.
                    loop {
                        line.clear();
                        self.reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
                        if line.trim_end().is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let at = body.len();
                body.resize(at + size + 2, 0);
                self.reader.read_exact(&mut body[at..]).map_err(|e| format!("read: {e}"))?;
                body.truncate(at + size);
            }
        } else if let Some(n) = length {
            body.resize(n, 0);
            self.reader.read_exact(body).map_err(|e| format!("read: {e}"))?;
        } else if !close {
            return Err("response without Content-Length or chunked encoding".into());
        } else {
            self.reader.read_to_end(body).map_err(|e| format!("read: {e}"))?;
        }
        Ok((status, close))
    }
}

struct Run {
    start: Instant,
    end: Instant,
    latencies: Vec<Duration>,
    errors: usize,
}

fn worker(addr: &str, req: &[u8], warmup: usize, n: usize, barrier: &Barrier) -> Result<Run, String> {
    let mut conn = Conn::open(addr)?;
    let mut body = Vec::new();
    let send = |conn: &mut Conn, body: &mut Vec<u8>| -> Result<u16, String> {
        let (status, close) = conn.request(req, body)?;
        if close {
            *conn = Conn::open(addr)?;
        }
        Ok(status)
    };
    for _ in 0..warmup {
        send(&mut conn, &mut body)?;
    }
    barrier.wait();
    let start = Instant::now();
    let mut latencies = Vec::with_capacity(n);
    let mut errors = 0;
    for _ in 0..n {
        let t = Instant::now();
        let status = send(&mut conn, &mut body)?;
        latencies.push(t.elapsed());
        if status != 200 {
            errors += 1;
        }
    }
    Ok(Run { start, end: Instant::now(), latencies, errors })
}

/// Nearest-rank percentile of sorted durations, in milliseconds.
fn percentile(sorted: &[Duration], q: f64) -> f64 {
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1].as_secs_f64() * 1000.0
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let addr = match host_port(&args.url) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let cookie = args.cookie.as_ref().map_or(String::new(), |c| format!("Cookie: {c}\r\n"));
    let req = Arc::new(
        format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: phv-bench-load\r\nAccept: text/html,application/json\r\n{cookie}\r\n",
            args.path, addr
        )
        .into_bytes(),
    );
    let barrier = Arc::new(Barrier::new(args.connections));
    let handles: Vec<_> = (0..args.connections)
        .map(|_| {
            let (addr, req, barrier) = (addr.clone(), req.clone(), barrier.clone());
            let (warmup, n) = (args.warmup, args.requests);
            std::thread::spawn(move || worker(&addr, &req, warmup, n, &barrier))
        })
        .collect();
    let mut runs = Vec::new();
    for h in handles {
        match h.join() {
            Ok(Ok(r)) => runs.push(r),
            Ok(Err(e)) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            Err(_) => {
                eprintln!("error: a connection thread panicked");
                std::process::exit(1);
            }
        }
    }
    let start = runs.iter().map(|r| r.start).min().unwrap();
    let end = runs.iter().map(|r| r.end).max().unwrap();
    let seconds = (end - start).as_secs_f64();
    let errors: usize = runs.iter().map(|r| r.errors).sum();
    let mut latencies: Vec<Duration> = runs.into_iter().flat_map(|r| r.latencies).collect();
    latencies.sort();
    let total = latencies.len();
    let per_second = total as f64 / seconds;
    let (p50, p99) = (percentile(&latencies, 0.5), percentile(&latencies, 0.99));
    if args.json {
        let out = serde_json::json!({
            "url": args.url,
            "path": args.path,
            "connections": args.connections,
            "requests": total,
            "seconds": (seconds * 1000.0).round() / 1000.0,
            "requests_per_s": (per_second * 10.0).round() / 10.0,
            "p50_ms": (p50 * 1000.0).round() / 1000.0,
            "p99_ms": (p99 * 1000.0).round() / 1000.0,
            "errors": errors,
        });
        println!("{out}");
    } else {
        println!(
            "{}: {} requests on {} connections in {seconds:.3} s: {per_second:.1} req/s, p50 {p50:.3} ms, p99 {p99:.3} ms, {errors} errors",
            args.path, total, args.connections
        );
    }
    if errors > 0 {
        std::process::exit(1);
    }
}
