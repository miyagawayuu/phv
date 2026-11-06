//! The admin port (`[admin] listen`): `/health`, `/ready` and `/metrics`, on a
//! listener of its own, so nothing is added to the site's own paths and a
//! firewall can keep the port inside the network.
//!
//! - `/health`: 200 while the server runs, 503 once it is stopping.
//! - `/ready`: 503 too while a store the server uses (sessions, rate limits,
//!   `publish()`, jobs) failed the last time it was used. No probe makes the
//!   server connect anywhere.
//! - `/metrics`: the Prometheus text format; with `[admin] token_env`, only
//!   with `Authorization: Bearer <token>`.

use crate::{body::RespBody, simple, State};
use bytes::Bytes;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// Connections the admin port serves at once.
const MAX_ADMIN_CONNECTIONS: usize = 64;
/// A client must send its request (headers) within this time.
const ADMIN_HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// Serves the admin port until the runtime stops (after the site's
/// connections, so `/health` answers 503 while requests finish).
pub(crate) async fn serve(state: Arc<State>, listener: TcpListener, token: Option<Vec<u8>>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_ADMIN_CONNECTIONS));
    let token = Arc::new(token);
    loop {
        let Ok(permit) = slots.clone().acquire_owned().await else { return };
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (state, token) = (state.clone(), token.clone());
        tokio::spawn(async move {
            let service = service_fn(move |req| answer(state.clone(), token.clone(), req));
            let mut b = http1::Builder::new();
            b.timer(TokioTimer::new())
                .header_read_timeout(ADMIN_HEADER_TIMEOUT)
                .keep_alive(false)
                .max_buf_size(16 * 1024);
            let conn = b.serve_connection(TokioIo::new(stream), service);
            let _ = tokio::time::timeout(Duration::from_secs(30), conn).await;
            drop(permit);
        });
    }
}

async fn answer(
    state: Arc<State>,
    token: Arc<Option<Vec<u8>>>,
    req: Request<Incoming>,
) -> Result<Response<RespBody>, Infallible> {
    let text = |status: StatusCode, body: String| {
        let mut r = simple(status, "text/plain; charset=utf-8", body);
        r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
        r
    };
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Ok(text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n".into()));
    }
    let stopping = state.stopping.load(Ordering::SeqCst);
    Ok(match req.uri().path() {
        "/health" if stopping => text(StatusCode::SERVICE_UNAVAILABLE, "stopping\n".into()),
        "/health" => text(StatusCode::OK, "ok\n".into()),
        "/ready" => {
            let down = state.metrics.health.down();
            if stopping {
                text(StatusCode::SERVICE_UNAVAILABLE, "stopping\n".into())
            } else if !down.is_empty() {
                text(StatusCode::SERVICE_UNAVAILABLE, format!("unavailable: {}\n", down.join(", ")))
            } else {
                text(StatusCode::OK, "ready\n".into())
            }
        }
        "/metrics" => {
            if let Some(t) = token.as_ref() {
                let sent = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or("");
                if !phv_std::constant_time_eq(sent.as_bytes(), t) {
                    let mut r = text(StatusCode::UNAUTHORIZED, "unauthorized\n".into());
                    r.headers_mut().insert("www-authenticate", HeaderValue::from_static("Bearer"));
                    return Ok(r);
                }
            }
            let body = state.metrics_text();
            let mut r = Response::new(RespBody::full(Bytes::from(body)));
            r.headers_mut()
                .insert("content-type", HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"));
            r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
            r
        }
        _ => text(StatusCode::NOT_FOUND, "not found\n".into()),
    })
}
