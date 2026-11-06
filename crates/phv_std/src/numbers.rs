//! Mathematics, number bases, hex and base64url encodings, UUIDs, and the
//! formatting behind `format()` (whose format string the checker reads).

use super::{float, int, s};
use base64::Engine;
use phv_syntax::fmtspec::{parse_spec, Spec};
use phv_vm::{BuiltinFn, Value, Vm, VmError};
use std::fmt::Write;

fn value_error(vm: &mut Vm, msg: impl Into<String>) -> VmError {
    vm.throw("ValueError", msg)
}

fn radix_ok(vm: &mut Vm, r: i64, what: &str) -> Result<u32, VmError> {
    if (2..=36).contains(&r) {
        Ok(r as u32)
    } else {
        Err(value_error(vm, format!("{what}: the base must be 2 to 36")))
    }
}

fn to_radix(mut n: u64, radix: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(std::char::from_digit((n % radix as u64) as u32, radix).unwrap_or('?'));
        n /= radix as u64;
    }
    out.iter().rev().collect()
}

fn parse_radix(vm: &mut Vm, text: &str, radix: u32, what: &str) -> Result<i64, VmError> {
    let t = text.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t),
    };
    let n = u64::from_str_radix(digits, radix).map_err(|_| {
        // At most 64 characters of the text are quoted (it may be long).
        let shown = match t.char_indices().nth(64) {
            Some((i, _)) => format!("{}...", &t[..i]),
            None => t.to_string(),
        };
        value_error(vm, format!("{what}: `{shown}` is not a base-{radix} number"))
    })?;
    let n = i64::try_from(n)
        .map_err(|_| vm.throw("ArithmeticError", format!("{what}: the number is too large for int")))?;
    Ok(if neg { -n } else { n })
}

/// A random UUID (version 4), or one that starts with the time (version 7, sortable).
fn uuid(v7: bool) -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("operating system random number generator failed");
    if v7 {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    }
    b[6] = (b[6] & 0x0f) | if v7 { 0x70 } else { 0x40 };
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn group(digits: &str) -> String {
    let (sign, digits) = match digits.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", digits),
    };
    let (int_part, rest) = digits.split_once('.').map_or((digits, None), |(a, b)| (a, Some(b)));
    let mut out = String::new();
    for (i, c) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    match rest {
        Some(r) => format!("{sign}{out}.{r}"),
        None => format!("{sign}{out}"),
    }
}

/// Formats one value by a spec (the checker made sure the types fit).
pub fn format_value(v: &Value, spec: &Spec) -> String {
    let mut text = match (v, spec.kind) {
        (Value::Int(i), Some('x')) => format!("{i:x}"),
        (Value::Int(i), Some('X')) => format!("{i:X}"),
        (Value::Int(i), Some('o')) => format!("{i:o}"),
        (Value::Int(i), Some('b')) => format!("{i:b}"),
        (Value::Int(i), Some('e')) => format!("{:e}", *i as f64),
        (Value::Float(f), Some('e')) => match spec.precision {
            Some(p) => format!("{f:.p$e}"),
            None => format!("{f:e}"),
        },
        (Value::Int(i), None) if spec.precision.is_some() => format!("{:.*}", spec.precision.unwrap_or(0), *i as f64),
        (Value::Float(f), None) if spec.precision.is_some() => format!("{f:.*}", spec.precision.unwrap_or(0)),
        _ => v.to_display_string(),
    };
    if spec.grouping && matches!(v, Value::Int(_) | Value::Float(_)) && spec.kind.is_none() {
        text = group(&text);
    }
    let len = text.chars().count();
    if len >= spec.width {
        return text;
    }
    let pad = spec.width - len;
    if spec.zero && spec.align.is_none() {
        let (sign, digits) = match text.strip_prefix('-') {
            Some(d) => ("-", d.to_string()),
            None => ("", text.clone()),
        };
        return format!("{sign}{}{digits}", "0".repeat(pad));
    }
    let fill = |n: usize| spec.fill.to_string().repeat(n);
    let numeric = matches!(v, Value::Int(_) | Value::Float(_));
    match spec.align.unwrap_or(if numeric { '>' } else { '<' }) {
        '>' => format!("{}{text}", fill(pad)),
        '^' => format!("{}{text}{}", fill(pad / 2), fill(pad - pad / 2)),
        _ => format!("{text}{}", fill(pad)),
    }
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "sin" => |_, a| Ok(Value::Float(float(&a[0]).sin())),
        "cos" => |_, a| Ok(Value::Float(float(&a[0]).cos())),
        "tan" => |_, a| Ok(Value::Float(float(&a[0]).tan())),
        "asin" => |_, a| Ok(Value::Float(float(&a[0]).asin())),
        "acos" => |_, a| Ok(Value::Float(float(&a[0]).acos())),
        "atan" => |_, a| Ok(Value::Float(float(&a[0]).atan())),
        "atan2" => |_, a| Ok(Value::Float(float(&a[0]).atan2(float(&a[1])))),
        "exp" => |_, a| Ok(Value::Float(float(&a[0]).exp())),
        "log" => |_, a| Ok(Value::Float(float(&a[0]).ln())),
        "log_base" => |_, a| Ok(Value::Float(float(&a[0]).log(float(&a[1])))),
        "log10" => |_, a| Ok(Value::Float(float(&a[0]).log10())),
        "log2" => |_, a| Ok(Value::Float(float(&a[0]).log2())),
        "pi" => |_, _| Ok(Value::Float(std::f64::consts::PI)),
        "hypot" => |_, a| Ok(Value::Float(float(&a[0]).hypot(float(&a[1])))),
        "clamp_int" => |vm, a| {
            let (x, lo, hi) = (int(&a[0]), int(&a[1]), int(&a[2]));
            if lo > hi {
                return Err(value_error(vm, "clamp(): the minimum is above the maximum"));
            }
            Ok(Value::Int(x.clamp(lo, hi)))
        },
        "clamp_float" => |vm, a| {
            let (x, lo, hi) = (float(&a[0]), float(&a[1]), float(&a[2]));
            if lo > hi || lo.is_nan() || hi.is_nan() {
                return Err(value_error(vm, "clamp(): the minimum is above the maximum"));
            }
            Ok(Value::Float(x.clamp(lo, hi)))
        },
        "random_float" => |_, _| {
            let mut b = [0u8; 8];
            getrandom::fill(&mut b).expect("operating system random number generator failed");
            // 53 random bits: every float in [0, 1) with that spacing is equally likely.
            Ok(Value::Float((u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64))
        },
        "bindec" => |vm, a| parse_radix(vm, s(&a[0]), 2, "bindec()").map(Value::Int),
        "octdec" => |vm, a| parse_radix(vm, s(&a[0]), 8, "octdec()").map(Value::Int),
        "hexdec" => |vm, a| parse_radix(vm, s(&a[0]), 16, "hexdec()").map(Value::Int),
        "decbin" => |_, a| Ok(Value::str(format!("{:b}", int(&a[0])))),
        "decoct" => |_, a| Ok(Value::str(format!("{:o}", int(&a[0])))),
        "dechex" => |_, a| Ok(Value::str(format!("{:x}", int(&a[0])))),
        "base_convert" => |vm, a| {
            let from = radix_ok(vm, int(&a[1]), "base_convert()")?;
            let to = radix_ok(vm, int(&a[2]), "base_convert()")?;
            let n = parse_radix(vm, s(&a[0]), from, "base_convert()")?;
            let digits = to_radix(n.unsigned_abs(), to);
            Ok(Value::str(if n < 0 { format!("-{digits}") } else { digits }))
        },
        "bin2hex" => |vm, a| {
            let t = s(&a[0]);
            vm.reserve(t.len().saturating_mul(2))?;
            let mut out = String::with_capacity(t.len() * 2);
            for b in t.bytes() {
                let _ = write!(out, "{b:02x}");
            }
            Ok(Value::str(out))
        },
        "hex2bin" => |vm, a| {
            let h = s(&a[0]);
            let bytes = (h.len().is_multiple_of(2) && h.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| {
                    (0..h.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0))
                        .collect::<Vec<u8>>()
                })
                .ok_or_else(|| value_error(vm, "hex2bin(): not an even number of hex digits"))?;
            String::from_utf8(bytes)
                .map(Value::str)
                .map_err(|_| value_error(vm, "hex2bin(): the bytes are not UTF-8 text"))
        },
        "base64url_encode" => |_, a| Ok(Value::str(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s(&a[0])))),
        "base64url_decode" => |vm, a| {
            let t = s(&a[0]).trim_end_matches('=');
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(t)
                .map_err(|_| value_error(vm, "base64url_decode(): not base64url"))?;
            String::from_utf8(bytes)
                .map(Value::str)
                .map_err(|_| value_error(vm, "base64url_decode(): the bytes are not UTF-8 text"))
        },
        "uuid_v4" => |_, _| Ok(Value::str(uuid(false))),
        "uuid_v7" => |_, _| Ok(Value::str(uuid(true))),
        // `format()`: one value and its spec text (checked when the program was).
        "__fmt" => |_, a| {
            let spec = parse_spec(s(&a[1])).unwrap_or(Spec { fill: ' ', ..Spec::default() });
            Ok(Value::str(format_value(&a[0], &spec)))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(v: Value, spec: &str) -> String {
        format_value(&v, &parse_spec(spec).unwrap())
    }

    #[test]
    fn specs() {
        assert_eq!(fmt(Value::Float(1.23456), ".2"), "1.23");
        assert_eq!(fmt(Value::Int(42), "5"), "   42");
        assert_eq!(fmt(Value::str("ab"), "5"), "ab   ");
        assert_eq!(fmt(Value::str("ab"), "*^6"), "**ab**");
        assert_eq!(fmt(Value::Int(-7), "04"), "-007");
        assert_eq!(fmt(Value::Int(255), "x"), "ff");
        assert_eq!(fmt(Value::Int(1234567), ","), "1,234,567");
        assert_eq!(fmt(Value::Float(-1234.5), ",.2"), "-1,234.50");
        assert_eq!(fmt(Value::Int(5), "08b"), "00000101");
        assert!(parse_spec("q").is_err() && parse_spec(".").is_err() && parse_spec("99999").is_err());
    }

    #[test]
    fn uuids() {
        let (a, b) = (uuid(false), uuid(true));
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert_eq!(&b[14..15], "7");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }
}
