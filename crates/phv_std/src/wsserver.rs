//! The server side of WebSockets (`std.ws`): a script `public/*.ws.phv` runs
//! once for each event of a connection (opened, a message, closed). What it
//! asks for (messages to send, topics to join, values to keep) is collected
//! in `vm.http.ws` and carried out by the server after the script ends.

use crate::{int, s};
use phv_vm::{BuiltinFn, Value, Vm, VmError, WsCtx, WsEvent};

/// Bytes of keys and values a connection may keep (`$ws->set()`), each
/// entry counting `ENTRY` more for what keeping it costs.
pub const MAX_STATE: usize = 64 * 1024;
const ENTRY: usize = 64;
/// Values one connection may keep.
pub const MAX_KEYS: usize = 256;
/// The longest message a script may send or publish.
pub const MAX_TEXT: usize = 1 << 20;
/// Topics one connection may have joined.
pub const MAX_TOPICS: usize = 64;
/// The longest topic name.
pub const MAX_TOPIC: usize = 200;
/// Messages one script run may send and publish, in all.
pub const MAX_MESSAGES: usize = 1000;

type R = Result<Value, VmError>;

fn ws(vm: &mut Vm) -> Result<&mut WsCtx, VmError> {
    if vm.http.ws.is_none() {
        return Err(vm.throw("LogicException", "socket() can only be used in a WebSocket script (public/*.ws.phv)"));
    }
    Ok(vm.http.ws.as_mut().expect("checked above"))
}

fn state_size(w: &WsCtx) -> usize {
    w.state.iter().map(|(k, v)| k.len() + v.len() + ENTRY).sum()
}

fn text_arg(vm: &mut Vm, what: &str, v: &Value) -> Result<String, VmError> {
    let t = s(v);
    if t.len() > MAX_TEXT {
        return Err(vm.throw("ValueError", format!("{what}: a message is at most 1 MB")));
    }
    Ok(t.to_string())
}

fn topic_arg(vm: &mut Vm, what: &str, v: &Value) -> Result<String, VmError> {
    let t = s(v);
    if t.is_empty() || t.len() > MAX_TOPIC || t.chars().any(char::is_control) {
        return Err(
            vm.throw("ValueError", format!("{what}: a topic is 1 to {MAX_TOPIC} bytes without control characters"))
        );
    }
    Ok(t.to_string())
}

/// Counts one more message of this run (sends and publishes together).
fn count_message(vm: &mut Vm, what: &str) -> Result<(), VmError> {
    let sent = vm.http.publishes.len() + vm.http.ws.as_ref().map_or(0, |w| w.sends.len());
    if sent >= MAX_MESSAGES {
        return Err(vm.throw("ValueError", format!("{what}: a script may send at most {MAX_MESSAGES} messages")));
    }
    Ok(())
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__sock_event" => |vm, _| -> R {
            let e = ws(vm)?.event;
            Ok(Value::Int(match e {
                WsEvent::Open => 0,
                WsEvent::Message => 1,
                WsEvent::Close => 2,
            }))
        },
        "__sock_id" => |vm, _| -> R { Ok(Value::str(ws(vm)?.id.clone())) },
        "__sock_message" => |vm, _| -> R { Ok(Value::str(ws(vm)?.message.clone())) },
        "__sock_get" => |vm, a| -> R {
            let k = s(&a[0]).to_string();
            Ok(ws(vm)?.state.get(&k).map(|v| Value::str(v.as_str())).unwrap_or(Value::Null))
        },
        "__sock_set" => |vm, a| -> R {
            let (k, v) = (s(&a[0]).to_string(), s(&a[1]).to_string());
            let w = ws(vm)?;
            let old = w.state.get(&k).map_or(0, |o| k.len() + o.len() + ENTRY);
            if old == 0 && w.state.len() >= MAX_KEYS {
                return Err(vm.throw("ValueError", format!("set(): a connection keeps at most {MAX_KEYS} values")));
            }
            if state_size(w) - old + k.len() + v.len() + ENTRY > MAX_STATE {
                return Err(vm.throw("ValueError", "set(): a connection keeps at most 64 KB of values"));
            }
            ws(vm)?.state.insert(k, v);
            Ok(Value::Null)
        },
        "__sock_remove" => |vm, a| -> R {
            let k = s(&a[0]).to_string();
            ws(vm)?.state.shift_remove(&k);
            Ok(Value::Null)
        },
        "__sock_send" => |vm, a| -> R {
            ws(vm)?;
            let t = text_arg(vm, "send()", &a[0])?;
            count_message(vm, "send()")?;
            ws(vm)?.sends.push(t);
            Ok(Value::Null)
        },
        "__sock_join" => |vm, a| -> R {
            ws(vm)?;
            let t = topic_arg(vm, "join()", &a[0])?;
            let w = ws(vm)?;
            w.leaves.retain(|x| *x != t);
            if !w.joins.contains(&t) {
                if w.joins.len() >= MAX_TOPICS {
                    return Err(vm.throw("ValueError", format!("join(): at most {MAX_TOPICS} topics at once")));
                }
                w.joins.push(t);
            }
            Ok(Value::Null)
        },
        "__sock_leave" => |vm, a| -> R {
            ws(vm)?;
            let t = topic_arg(vm, "leave()", &a[0])?;
            let w = ws(vm)?;
            w.joins.retain(|x| *x != t);
            if !w.leaves.contains(&t) {
                w.leaves.push(t);
            }
            Ok(Value::Null)
        },
        "__sock_close" => |vm, a| -> R {
            let code = int(&a[0]);
            // 1000 (normal) or a code for applications (RFC 6455, 7.4.2).
            if code != 1000 && !(3000..=4999).contains(&code) {
                return Err(vm.throw("ValueError", "close(): the code is 1000 or 3000-4999"));
            }
            let reason = s(&a[1]);
            if reason.len() > 123 {
                return Err(vm.throw("ValueError", "close(): the reason is at most 123 bytes"));
            }
            let reason = reason.to_string();
            ws(vm)?.close = Some((code as u16, reason));
            Ok(Value::Null)
        },
        "__sock_publish" => |vm, a| -> R {
            if !vm.http.can_publish {
                return Err(vm.throw("LogicException", "publish() can only be used in pages served by `phv serve`"));
            }
            let topic = topic_arg(vm, "publish()", &a[0])?;
            let t = text_arg(vm, "publish()", &a[1])?;
            count_message(vm, "publish()")?;
            vm.http.publishes.push((topic, t));
            Ok(Value::Null)
        },
        _ => return None,
    };
    Some(f)
}
