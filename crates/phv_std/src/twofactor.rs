//! Two-factor authentication: TOTP codes (RFC 6238, the six-digit codes of
//! authenticator apps) and QR codes for setting them up.
//!
//! PHV strings are UTF-8, so the binary key and counter that TOTP needs cannot
//! be built in PHV code; these builtins take the secret as Base32 text, the
//! form authenticator apps use.

use crate::{int, s};
use hmac::{Hmac, Mac};
use phv_vm::{BuiltinFn, Value, Vm, VmError};
use sha1::Sha1;

/// Seconds per code, digits per code, and how many steps before and after
/// now a code is still accepted (clocks drift and people type slowly).
const PERIOD: i64 = 30;
const DIGITS: u32 = 6;
const WINDOW: i64 = 1;
/// 160 bits, the key size RFC 4226 recommends for HMAC-SHA1.
const SECRET_BYTES: usize = 20;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Accepts lower case, spaces and `=` padding, as people copy secrets in many forms.
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let v = BASE32.iter().position(|&b| b as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The code for one time step (RFC 4226 dynamic truncation).
fn code_at(key: &[u8], step: i64) -> String {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&(step as u64).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let offset = (h[h.len() - 1] & 0x0f) as usize;
    let n = u32::from_be_bytes([h[offset] & 0x7f, h[offset + 1], h[offset + 2], h[offset + 3]]);
    format!("{:0width$}", n % 10u32.pow(DIGITS), width = DIGITS as usize)
}

fn secret(vm: &Vm, text: &str) -> Result<Vec<u8>, VmError> {
    base32_decode(text)
        .ok_or_else(|| vm.throw("ValueError", "the TOTP secret must be Base32 text (from totp_secret())"))
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The time step of `code` if it is valid now (within the window), compared
/// in constant time. Callers store the step and refuse it the next time, so
/// a code cannot be used twice.
fn verify(key: &[u8], code: &str, time: i64) -> Option<i64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let current = time.div_euclid(PERIOD);
    let mut found = None;
    for step in current - WINDOW..=current + WINDOW {
        if crate::sys::constant_time_eq(code_at(key, step).as_bytes(), code.as_bytes()) && found.is_none() {
            found = Some(step);
        }
    }
    found
}

/// A QR code as an `<svg>` element: black modules on white with the quiet
/// zone, at least 200 px wide. Only the modules come from `text`, never the
/// text itself, so the markup is safe to embed.
fn qr_svg(vm: &Vm, text: &str) -> Result<String, VmError> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(text.as_bytes())
        .map_err(|_| vm.throw("ValueError", "qr_svg(): the text is too long for a QR code"))?;
    let image = code
        .render::<svg::Color>()
        .min_dimensions(200, 200)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    // Drop the XML declaration so the result can sit inside HTML.
    Ok(image.find("<svg").map(|i| image[i..].to_string()).unwrap_or(image))
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "totp_secret" => |_, _| {
            let mut buf = [0u8; SECRET_BYTES];
            getrandom::fill(&mut buf).expect("operating system random number generator failed");
            Ok(Value::str(base32_encode(&buf)))
        },
        "totp_code" => |vm, a| {
            let key = secret(vm, s(&a[0]))?;
            Ok(Value::str(code_at(&key, now().div_euclid(PERIOD))))
        },
        "totp_code_at" => |vm, a| {
            let key = secret(vm, s(&a[0]))?;
            Ok(Value::str(code_at(&key, int(&a[1]).div_euclid(PERIOD))))
        },
        "totp_verify" => |vm, a| {
            let key = secret(vm, s(&a[0]))?;
            Ok(verify(&key, s(&a[1]), now()).map(Value::Int).unwrap_or(Value::Null))
        },
        "qr_svg" => |vm, a| qr_svg(vm, s(&a[0])).map(Value::html),
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B: the SHA-1 key is the ASCII "12345678901234567890".
    const RFC_KEY: &[u8] = b"12345678901234567890";

    #[test]
    fn matches_rfc_6238() {
        // The RFC lists 8-digit codes; the 6-digit code is their last six digits.
        for (time, eight) in [
            (59, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ] {
            assert_eq!(code_at(RFC_KEY, time / PERIOD), eight[2..], "t={time}");
        }
    }

    #[test]
    fn base32_round_trip() {
        assert_eq!(base32_encode(RFC_KEY), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(base32_decode("gezd gnbv-gy3t qojq gezd gnbv gy3t qojq====").unwrap(), RFC_KEY);
        assert_eq!(base32_decode("not base32!"), None);
        assert_eq!(base32_decode(""), None);
        let mut buf = [0u8; SECRET_BYTES];
        getrandom::fill(&mut buf).unwrap();
        assert_eq!(base32_decode(&base32_encode(&buf)).unwrap(), buf);
    }

    #[test]
    fn verify_accepts_one_step_of_drift() {
        let t = 1111111111;
        let step = t / PERIOD;
        assert_eq!(verify(RFC_KEY, "050471", t), Some(step));
        assert_eq!(verify(RFC_KEY, " 050 471 ", t), Some(step), "spaces are ignored");
        assert_eq!(verify(RFC_KEY, &code_at(RFC_KEY, step - 1), t), Some(step - 1));
        assert_eq!(verify(RFC_KEY, &code_at(RFC_KEY, step + 1), t), Some(step + 1));
        assert_eq!(verify(RFC_KEY, &code_at(RFC_KEY, step - 2), t), None);
        assert_eq!(verify(RFC_KEY, "05047", t), None);
        assert_eq!(verify(RFC_KEY, "05047x", t), None);
        assert_eq!(verify(RFC_KEY, "0504711", t), None);
    }
}
