//! CSV, URLs, logging, and what a command-line script reads: its arguments
//! and standard input.

use super::{list, map, s};
use phv_vm::{BuiltinFn, Key, Value, Vm, VmError};
use std::rc::Rc;

/// Fields and rows a CSV text may have (a crafted file cannot exhaust memory first).
const MAX_CSV_CELLS: usize = 1_000_000;

/// RFC 4180: fields separated by commas, rows by `\n` or `\r\n`, quoted
/// fields may contain commas, line breaks and `""` for a quote.
pub fn csv_parse(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut cells = 0usize;
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut at_start = true;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if at_start => {
                quoted = true;
                at_start = false;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                at_start = true;
                cells += 1;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                at_start = true;
                cells += 1;
            }
            _ => {
                field.push(c);
                at_start = false;
            }
        }
        if cells > MAX_CSV_CELLS {
            return Err(format!("more than {MAX_CSV_CELLS} fields"));
        }
    }
    if quoted {
        return Err("a quoted field is not closed".into());
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

pub fn csv_encode(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|f| {
                if f.contains([',', '"', '\n', '\r']) || f.starts_with(' ') || f.ends_with(' ') {
                    format!("\"{}\"", f.replace('"', "\"\""))
                } else {
                    f.clone()
                }
            })
            .collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out
}

/// A log line's value: quoted when it has spaces or quotes; line breaks and
/// control characters escaped, so a value cannot forge another line.
fn log_value(v: &str) -> String {
    let escaped: String = v
        .chars()
        .map(|c| match c {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            c if c.is_control() => format!("\\u{{{:x}}}", c as u32),
            c => c.to_string(),
        })
        .collect();
    if escaped.is_empty() || escaped.contains([' ', '"', '=']) {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn log(level: &str, a: &[Value]) {
    let time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut line = format!("{time} {level} {}", log_value(s(&a[0])));
    if let Some(ctx) = a.get(1) {
        for (k, v) in map(ctx).iter() {
            let key = match k {
                Key::Str(k) => k.to_string(),
                Key::Int(i) => i.to_string(),
            };
            line.push_str(&format!(" {}={}", log_value(&key), log_value(&v.to_display_string())));
        }
    }
    eprintln!("{line}");
}

fn cli_only(vm: &mut Vm, what: &str) -> Result<(), VmError> {
    if vm.host.cli {
        Ok(())
    } else {
        Err(vm.throw("RuntimeException", format!("{what} is only available to scripts run with `phv run`")))
    }
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "csv_parse" => |vm, a| {
            let rows = csv_parse(s(&a[0])).map_err(|e| vm.throw("ValueError", format!("csv_parse(): {e}")))?;
            Ok(Value::list(rows.into_iter().map(|r| Value::list(r.into_iter().map(Value::str).collect())).collect()))
        },
        "csv_encode" => |_, a| {
            let rows: Vec<Vec<String>> =
                list(&a[0]).iter().map(|r| list(r).iter().map(|f| s(f).to_string()).collect()).collect();
            Ok(Value::str(csv_encode(&rows)))
        },
        "parse_url" => |vm, a| {
            let u = url::Url::parse(s(&a[0])).map_err(|e| vm.throw("ValueError", format!("parse_url(): {e}")))?;
            let mut m = indexmap::IndexMap::new();
            let mut put = |k: &str, v: String| {
                m.insert(Key::Str(Rc::new(k.to_string())), Value::str(v));
            };
            put("scheme", u.scheme().to_string());
            if let Some(h) = u.host_str() {
                put("host", h.to_string());
            }
            if let Some(p) = u.port() {
                put("port", p.to_string());
            }
            if !u.username().is_empty() {
                put("user", u.username().to_string());
            }
            if let Some(p) = u.password() {
                put("pass", p.to_string());
            }
            put("path", u.path().to_string());
            if let Some(q) = u.query() {
                put("query", q.to_string());
            }
            if let Some(f) = u.fragment() {
                put("fragment", f.to_string());
            }
            Ok(Value::map(m))
        },
        "parse_query" => |_, a| {
            let mut m = indexmap::IndexMap::new();
            for (k, v) in url::form_urlencoded::parse(s(&a[0]).trim_start_matches('?').as_bytes()) {
                m.insert(Key::Str(Rc::new(k.into_owned())), Value::str(v.into_owned()));
            }
            Ok(Value::map(m))
        },
        "log_info" => |_, a| {
            log("INFO", a);
            Ok(Value::Null)
        },
        "log_warn" => |_, a| {
            log("WARN", a);
            Ok(Value::Null)
        },
        "log_error" => |_, a| {
            log("ERROR", a);
            Ok(Value::Null)
        },
        "argv" => |vm, _| {
            cli_only(vm, "argv()")?;
            Ok(Value::list(vm.host.args.iter().map(|a| Value::str(a.as_str())).collect()))
        },
        "stdin_read_line" => |vm, _| {
            cli_only(vm, "stdin_read_line()")?;
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) => Ok(Value::Null),
                Ok(_) => Ok(Value::str(line.trim_end_matches(['\n', '\r']).to_string())),
                Err(e) => Err(vm.throw("RuntimeException", format!("stdin_read_line(): {e}"))),
            }
        },
        "stdin_read_all" => |vm, _| {
            cli_only(vm, "stdin_read_all()")?;
            let mut text = String::new();
            use std::io::Read;
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| vm.throw("ValueError", format!("stdin_read_all(): {e}")))?;
            Ok(Value::str(text))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_round_trip() {
        let text = "name,note\r\nAnn,\"says \"\"hi\"\", then, leaves\"\nBob,\"two\nlines\"\n,\n";
        let rows = csv_parse(text).unwrap();
        assert_eq!(rows[1], ["Ann", "says \"hi\", then, leaves"]);
        assert_eq!(rows[2], ["Bob", "two\nlines"]);
        assert_eq!(rows[3], ["", ""]);
        assert_eq!(csv_parse(&csv_encode(&rows)).unwrap(), rows);
        assert!(csv_parse("\"open").is_err());
        assert_eq!(log_value("a\nb c"), "\"a\\nb c\"");
    }
}
