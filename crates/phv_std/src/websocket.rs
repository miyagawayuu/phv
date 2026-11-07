//! WebSocket servers named in phv.toml (`[websockets.<name>]`). Scripts only
//! name a server (`WebSocket::connect("game")`); the URL, which may carry a
//! password, stays in the configuration and is never shown.
//!
//! Every `connect()` opens a new connection, which is closed when the script
//! calls `close()` or ends, so nothing received for one request can reach the
//! next. Only text messages are exchanged; pings are answered while reading.

use crate::deadline::until;
use crate::netio::{check_plain, describe, dial, Stream};
use crate::{float, int, s};
use phv_vm::{Value, Vm, VmError};
use std::io::ErrorKind;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

type R = Result<Value, VmError>;

/// Longest wait for a server to accept a connection and finish the handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for a send when the script has no time limit.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest message accepted from a server.
const MAX_MESSAGE: usize = 16 << 20;
/// Connections one script may hold open at once.
const MAX_OPEN: usize = 16;

/// A script's connection, kept in `vm.resources`.
struct Conn {
    ws: WebSocket<Stream>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // A polite close; the server's reply is not awaited.
        self.ws.get_mut().set_deadline(Instant::now() + Duration::from_secs(1));
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }
}

fn closed(vm: &Vm) -> VmError {
    vm.throw("NetworkError", "the WebSocket connection is closed")
}

/// Runs `f` on the connection behind handle `h`. The connection is dropped
/// (closed) when `f` reports it broken.
fn with_conn(vm: &mut Vm, h: &Value, f: impl FnOnce(&mut Vm, &mut Conn) -> Result<(R, bool), VmError>) -> R {
    let i = int(h) as usize;
    let mut conn = match vm.resources.get_mut(i).and_then(|r| r.take()).map(|b| b.downcast::<Conn>()) {
        Some(Ok(c)) => c,
        Some(Err(other)) => {
            vm.resources[i] = Some(other);
            return Err(closed(vm));
        }
        None => return Err(closed(vm)),
    };
    let (result, broken) = f(vm, &mut conn)?;
    if !broken {
        vm.resources[i] = Some(conn);
    }
    result
}

fn io_message(e: &tungstenite::Error) -> String {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            "the server closed the connection".into()
        }
        tungstenite::Error::Io(e) => describe(e),
        tungstenite::Error::Capacity(_) => format!("the server sent a message larger than {} MB", MAX_MESSAGE >> 20),
        e => e.to_string(),
    }
}

fn open(url: &str, tls_setting: Option<bool>, until: Instant) -> Result<WebSocket<Stream>, String> {
    // The parser's own message could quote the URL, which may hold a password.
    let request = url.into_client_request().map_err(|_| "the URL is not a valid WebSocket URL".to_string())?;
    let uri = request.uri();
    let tls = uri.scheme_str() == Some("wss");
    let host = uri.host().ok_or("the URL has no host")?.to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    check_plain(&host, tls, tls_setting, "wss://")?;
    let stream = dial(&host, port, tls, until)?;
    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    match tungstenite::client::client_with_config(request, stream, Some(config)) {
        Ok((ws, _)) => Ok(ws),
        Err(HandshakeError::Interrupted(_)) => Err("timed out".into()),
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            Err(format!("the server answered HTTP {} instead of accepting the WebSocket", response.status().as_u16()))
        }
        Err(HandshakeError::Failure(e)) => Err(io_message(&e)),
    }
}

/// `__ws_connect(name)`: opens a connection to `[websockets.<name>]`.
pub fn connect(vm: &mut Vm, a: &[Value]) -> R {
    let name = s(&a[0]);
    let open_now = vm.resources.iter().flatten().filter(|r| r.is::<Conn>()).count();
    if open_now >= MAX_OPEN {
        return Err(
            vm.throw("NetworkError", format!("a script can keep at most {MAX_OPEN} WebSocket connections open"))
        );
    }
    let Some(spec) = vm.host.websockets.get(name).cloned() else {
        return Err(
            vm.throw("NetworkError", format!("no WebSocket server named \"{name}\" in [websockets] of phv.toml"))
        );
    };
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err(
            vm.throw("NetworkError", format!("WebSocket server \"{name}\": the environment variable {var} is not set"))
        );
    };
    let result = open(&url, spec.tls, until(vm, CONNECT_TIMEOUT));
    vm.check_limits()?;
    match result {
        Ok(ws) => {
            vm.resources.push(Some(Box::new(Conn { ws })));
            Ok(Value::Int(vm.resources.len() as i64 - 1))
        }
        Err(e) => Err(vm.throw("NetworkError", format!("cannot connect to WebSocket server \"{name}\": {e}"))),
    }
}

/// `__ws_send(handle, text)`: sends one text message.
pub fn send(vm: &mut Vm, a: &[Value]) -> R {
    let text = s(&a[1]).to_string();
    with_conn(vm, &a[0], |vm, c| {
        c.ws.get_mut().set_deadline(until(vm, SEND_TIMEOUT));
        match c.ws.send(Message::text(text)) {
            Ok(()) => Ok((Ok(Value::Null), false)),
            Err(e) => {
                vm.check_limits()?;
                Ok((Err(vm.throw("NetworkError", format!("cannot send: {}", io_message(&e)))), true))
            }
        }
    })
}

/// `__ws_receive(handle, seconds)`: the next text message, or null when none
/// arrives within `seconds`.
pub fn receive(vm: &mut Vm, a: &[Value]) -> R {
    let seconds = float(&a[1]);
    if !(0.0..=86_400.0).contains(&seconds) {
        return Err(vm.throw("ValueError", "the timeout must be between 0 and 86400 seconds"));
    }
    // `seconds` from now, or the script's deadline when that comes first.
    let stop = until(vm, Duration::from_secs_f64(seconds));
    with_conn(vm, &a[0], |vm, c| loop {
        // A zero timeout still picks up a message that has already arrived.
        // Every socket read and write (also the pongs sent while reading) stops at `stop`.
        c.ws.get_mut().set_deadline(stop.max(Instant::now() + Duration::from_millis(1)));
        match c.ws.read() {
            Ok(Message::Text(t)) => return Ok((Ok(Value::str(t.as_str())), false)),
            Ok(Message::Binary(_)) => {
                let e = vm.throw("NetworkError", "the server sent a binary message; only text messages are supported");
                return Ok((Err(e), true));
            }
            Ok(Message::Close(_)) => {
                return Ok((Err(vm.throw("NetworkError", "the server closed the connection")), true))
            }
            // Pings are answered by the library; pongs and raw frames carry nothing for the script.
            // A stream of them still ends at `stop`.
            Ok(_) => {
                if Instant::now() >= stop {
                    vm.check_limits()?;
                    return Ok((Ok(Value::Null), false));
                }
            }
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                vm.check_limits()?;
                if Instant::now() >= stop {
                    return Ok((Ok(Value::Null), false));
                }
            }
            Err(e) => {
                vm.check_limits()?;
                return Ok((Err(vm.throw("NetworkError", format!("cannot receive: {}", io_message(&e)))), true));
            }
        }
    })
}

/// `__ws_close(handle)`.
pub fn close(vm: &mut Vm, a: &[Value]) -> R {
    if let Some(slot) = vm.resources.get_mut(int(&a[0]) as usize) {
        if slot.as_ref().is_some_and(|r| r.is::<Conn>()) {
            *slot = None;
        }
    }
    Ok(Value::Null)
}

pub fn lookup(name: &str) -> Option<phv_vm::BuiltinFn> {
    let f: phv_vm::BuiltinFn = match name {
        "__ws_connect" => connect,
        "__ws_send" => send,
        "__ws_receive" => receive,
        "__ws_close" => close,
        _ => return None,
    };
    Some(f)
}
