//! Dates, cryptography, encoding, files and environment variables.

use crate::safefs::{self, DirGuard, FsError};
use crate::{int, s};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike, Utc};
use hmac::{Hmac, Mac};
use phv_vm::{BuiltinFn, Key, Value, Vm, VmError};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::ffi::OsString;
use std::fmt::Write;
use std::io::{Read, Write as _};
use std::path::{Component, Path};

// ----- dates ----------------------------------------------------------------

/// PHP `date()` formatting.
fn format_date<Tz: TimeZone>(fmt: &str, t: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let mut out = String::new();
    let mut chars = fmt.chars();
    let wd = t.weekday().num_days_from_monday() as usize;
    let offset_secs = t.offset().fix().local_minus_utc();
    let offset = |colon: bool| {
        let sign = if offset_secs < 0 { '-' } else { '+' };
        let m = offset_secs.abs() / 60;
        if colon {
            format!("{sign}{:02}:{:02}", m / 60, m % 60)
        } else {
            format!("{sign}{:02}{:02}", m / 60, m % 60)
        }
    };
    let leap = NaiveDate::from_ymd_opt(t.year(), 2, 29).is_some();
    let days_in_month = {
        let (y, m) = if t.month() == 12 { (t.year() + 1, 1) } else { (t.year(), t.month() + 1) };
        NaiveDate::from_ymd_opt(y, m, 1).and_then(|d| d.pred_opt()).map(|d| d.day()).unwrap_or(30)
    };
    while let Some(c) = chars.next() {
        let _ = match c {
            'd' => write!(out, "{:02}", t.day()),
            'D' => write!(out, "{}", &DAYS[wd][..3]),
            'j' => write!(out, "{}", t.day()),
            'S' => write!(
                out,
                "{}",
                match (t.day() % 10, t.day() % 100) {
                    (_, 11..=13) => "th",
                    (1, _) => "st",
                    (2, _) => "nd",
                    (3, _) => "rd",
                    _ => "th",
                }
            ),
            'l' => write!(out, "{}", DAYS[wd]),
            'N' => write!(out, "{}", wd + 1),
            'w' => write!(out, "{}", t.weekday().num_days_from_sunday()),
            'z' => write!(out, "{}", t.ordinal0()),
            'W' => write!(out, "{:02}", t.iso_week().week()),
            'F' => write!(out, "{}", MONTHS[t.month0() as usize]),
            'M' => write!(out, "{}", &MONTHS[t.month0() as usize][..3]),
            'm' => write!(out, "{:02}", t.month()),
            'n' => write!(out, "{}", t.month()),
            't' => write!(out, "{days_in_month}"),
            'L' => write!(out, "{}", leap as u8),
            'Y' => write!(out, "{}", t.year()),
            'y' => write!(out, "{:02}", t.year() % 100),
            'a' => write!(out, "{}", if t.hour() < 12 { "am" } else { "pm" }),
            'A' => write!(out, "{}", if t.hour() < 12 { "AM" } else { "PM" }),
            'g' => write!(out, "{}", (t.hour() + 11) % 12 + 1),
            'G' => write!(out, "{}", t.hour()),
            'h' => write!(out, "{:02}", (t.hour() + 11) % 12 + 1),
            'H' => write!(out, "{:02}", t.hour()),
            'i' => write!(out, "{:02}", t.minute()),
            's' => write!(out, "{:02}", t.second()),
            'v' => write!(out, "{:03}", t.timestamp_subsec_millis()),
            'u' => write!(out, "{:06}", t.timestamp_subsec_micros()),
            'P' => write!(out, "{}", offset(true)),
            'O' => write!(out, "{}", offset(false)),
            'Z' => write!(out, "{offset_secs}"),
            'U' => write!(out, "{}", t.timestamp()),
            'c' => write!(out, "{}", t.format("%Y-%m-%dT%H:%M:%S").to_string() + &offset(true)),
            'r' => write!(out, "{}", t.format("%a, %d %b %Y %H:%M:%S ").to_string() + &offset(false)),
            '\\' => match chars.next() {
                Some(n) => write!(out, "{n}"),
                None => Ok(()),
            },
            other => write!(out, "{other}"),
        };
    }
    out
}

fn local_at(ts: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

fn strtotime(text: &str) -> Option<i64> {
    let t = text.trim();
    let now = Local::now();
    let today = || now.date_naive().and_hms_opt(0, 0, 0);
    let naive = match t {
        "now" => return Some(now.timestamp()),
        "today" => today()?,
        "tomorrow" => today()? + chrono::Duration::days(1),
        "yesterday" => today()? - chrono::Duration::days(1),
        _ => {
            if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
                return Some(dt.timestamp());
            }
            let formats = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"];
            match formats.iter().find_map(|f| NaiveDateTime::parse_from_str(t, f).ok()) {
                Some(d) => d,
                None => NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0)?,
            }
        }
    };
    Local.from_local_datetime(&naive).earliest().map(|d| d.timestamp())
}

// ----- crypto -----------------------------------------------------------------

/// SHA-256 of `data` as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(vm: &Vm, algo: &str, data: &[u8]) -> Result<String, VmError> {
    Ok(match algo.to_ascii_lowercase().as_str() {
        "sha256" => hex(&Sha256::digest(data)),
        "sha384" => hex(&Sha384::digest(data)),
        "sha512" => hex(&Sha512::digest(data)),
        other => {
            return Err(vm
                .throw("ValueError", format!("unsupported hash algorithm \"{other}\" (use sha256, sha384 or sha512)")))
        }
    })
}

fn hmac(vm: &Vm, algo: &str, data: &[u8], key: &[u8]) -> Result<String, VmError> {
    macro_rules! run {
        ($t:ty) => {{
            let mut m = <Hmac<$t> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
            m.update(data);
            hex(&m.finalize().into_bytes())
        }};
    }
    Ok(match algo.to_ascii_lowercase().as_str() {
        "sha256" => run!(Sha256),
        "sha384" => run!(Sha384),
        "sha512" => run!(Sha512),
        other => return Err(vm.throw("ValueError", format!("unsupported HMAC algorithm \"{other}\""))),
    })
}

/// Compares in time that depends only on the lengths.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("operating system random number generator failed");
    u64::from_le_bytes(b)
}

fn password_hash(password: &str) -> String {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("operating system random number generator failed");
    let salt = SaltString::encode_b64(&salt).expect("valid salt");
    argon2::Argon2::default().hash_password(password.as_bytes(), &salt).expect("argon2 hashing").to_string()
}

fn password_verify(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(h) = PasswordHash::new(hash) else { return false };
    // The cost comes from the stored hash; refuse values that would let a
    // planted hash use gigabytes of memory or minutes of CPU.
    let param = |name: &str| h.params.get_decimal(name).unwrap_or(0);
    if param("m") > 64 * 1024 || param("t") > 8 || param("p") > 4 {
        return false;
    }
    argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok()
}

// ----- files and environment ------------------------------------------------

fn permission_error(vm: &Vm, msg: String) -> VmError {
    vm.throw("PermissionError", msg)
}

/// Resolves a project-relative path, checking `[permissions]` in phv.toml
/// and refusing `..` and absolute paths. The directory that holds the file is
/// opened without following links (see `safefs`) and returned with the file
/// name; the guard keeps it pinned while the file is used.
pub fn resolve_path(vm: &Vm, rel: &str, write: bool) -> Result<(DirGuard, OsString), VmError> {
    let p = Path::new(rel);
    // `Path` drops a trailing `/`, `/.` or `\`, so `x.phv/` would name `x.phv`.
    let trailing = rel.ends_with(['/', '\\']) || rel.ends_with("/.") || rel.ends_with("\\.");
    let simple = !rel.is_empty()
        && !trailing
        && !rel.contains('\0')
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !simple {
        return Err(permission_error(vm, format!("\"{rel}\" must be a path relative to the project without `..`")));
    }
    let perms = &vm.host.permissions;
    let allowed: Vec<&String> =
        if write { perms.write.iter().collect() } else { perms.read.iter().chain(perms.write.iter()).collect() };
    let kind = if write { "write" } else { "read" };
    let Some(dir) = allowed.iter().find(|d| p.starts_with(Path::new(d.trim_end_matches('/')))) else {
        let msg = if p.components().count() > 1 {
            let first = p.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
            format!("{kind} access to \"{rel}\" is not allowed; add \"{first}/\" to [permissions].{kind} in phv.toml")
        } else {
            format!("{kind} access to \"{rel}\" is not allowed; keep files in a directory listed in [permissions].{kind} in phv.toml (e.g. \"data/\")")
        };
        return Err(permission_error(vm, msg));
    };
    let dir = dir.trim_end_matches('/');
    let inner = p.strip_prefix(dir).unwrap_or(p);
    let base = vm.host.root.join(dir);
    let (guard, name) = safefs::open_parent(&base, inner).map_err(|e| fs_error(vm, rel, dir, e))?;
    // Checked on the final name every write goes to (file_put_contents,
    // unlink, saveTo, Database::open), after the path is normalized.
    if write && is_source_name(&name.to_string_lossy()) {
        return Err(permission_error(vm, format!("scripts cannot write or delete PHV source files (\"{rel}\")")));
    }
    Ok((guard, name))
}

fn is_source_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".phv") || lower.ends_with(".phv.html")
}

/// Turns a `safefs` refusal into the exception a script sees.
pub fn fs_error(vm: &Vm, rel: &str, dir: &str, e: FsError) -> VmError {
    match e {
        FsError::Link => permission_error(vm, format!("\"{rel}\" goes through a link; links are not followed")),
        FsError::HardLink => {
            permission_error(vm, format!("\"{rel}\" has several hard links; only files with one link can be used"))
        }
        FsError::NotFile => vm.throw("RuntimeException", format!("\"{rel}\" is not a regular file")),
        FsError::NoDir if dir.is_empty() => {
            vm.throw("RuntimeException", format!("directory of \"{rel}\" does not exist"))
        }
        FsError::NoDir => {
            vm.throw("RuntimeException", format!("directory of \"{rel}\" does not exist (or \"{dir}\" is missing)"))
        }
        FsError::BadName => permission_error(
            vm,
            format!("\"{rel}\" is not a safe file name (no `:`, trailing dots or spaces, or device names like CON)"),
        ),
        FsError::Shared => permission_error(
            vm,
            format!("\"{dir}/\" is writable by other users; remove that permission (chmod o-w) before using it"),
        ),
        FsError::Io(e) => vm.throw("RuntimeException", format!("cannot access \"{rel}\": {}", e.kind())),
    }
}

fn env_allowed(vm: &Vm, name: &str) -> bool {
    vm.host.permissions.env.iter().any(|pat| match pat.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pat == name,
    })
}

/// The Content-Type `response()->file()` sends when none is given. Types a
/// browser could run as a page (HTML, SVG, XML) are sent as downloads.
fn file_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn io_error(vm: &Vm, what: &str, path: &str, e: std::io::Error) -> VmError {
    vm.throw("RuntimeException", format!("{what} \"{path}\": {}", e.kind()))
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "date" => |_, a| Ok(Value::str(format_date(s(&a[0]), &Local::now()))),
        "date_ts" => |vm, a| match local_at(int(&a[1])) {
            Some(t) => Ok(Value::str(format_date(s(&a[0]), &t))),
            None => Err(vm.throw("ValueError", "timestamp out of range")),
        },
        "gmdate" => |_, a| Ok(Value::str(format_date(s(&a[0]), &Utc::now()))),
        "gmdate_ts" => |vm, a| match Utc.timestamp_opt(int(&a[1]), 0).single() {
            Some(t) => Ok(Value::str(format_date(s(&a[0]), &t))),
            None => Err(vm.throw("ValueError", "timestamp out of range")),
        },
        "mktime" => |vm, a| {
            let n: Vec<i64> = a.iter().map(int).collect();
            // Out-of-range parts are an error, not silently wrapped (`as` would wrap).
            let u = |i: usize| u32::try_from(n[i]).ok();
            let d = i32::try_from(n[5])
                .ok()
                .zip(u(3).zip(u(4)))
                .and_then(|(y, (m, day))| NaiveDate::from_ymd_opt(y, m, day))
                .and_then(|d| d.and_hms_opt(u(0)?, u(1)?, u(2)?))
                .and_then(|d| Local.from_local_datetime(&d).earliest());
            d.map(|d| Value::Int(d.timestamp())).ok_or_else(|| vm.throw("ValueError", "invalid date"))
        },
        "checkdate" => |_, a| {
            let part = |i: usize| u32::try_from(int(&a[i])).ok();
            let ok = i32::try_from(int(&a[2]))
                .ok()
                .zip(part(0).zip(part(1)))
                .is_some_and(|(y, (m, d))| NaiveDate::from_ymd_opt(y, m, d).is_some());
            Ok(Value::Bool(ok))
        },
        "strtotime" => |_, a| Ok(strtotime(s(&a[0])).map(Value::Int).unwrap_or(Value::Null)),
        "random_int" => |vm, a| {
            let (lo, hi) = (int(&a[0]), int(&a[1]));
            if lo > hi {
                return Err(vm.throw("ValueError", "random_int(): min must not be greater than max"));
            }
            let span = (hi as i128 - lo as i128 + 1) as u128;
            // Rejection sampling avoids modulo bias.
            let limit = u64::MAX as u128 - (u64::MAX as u128 + 1) % span;
            loop {
                let r = random_u64() as u128;
                if r <= limit {
                    return Ok(Value::Int((lo as i128 + (r % span) as i128) as i64));
                }
            }
        },
        "random_token" => |_, _| Ok(Value::str(crate::web::random_hex(32))),
        "random_token_n" => |vm, a| {
            let n = int(&a[0]);
            if !(1..=1024).contains(&n) {
                return Err(vm.throw("ValueError", "random_token(): length must be between 1 and 1024 bytes"));
            }
            Ok(Value::str(crate::web::random_hex(n as usize)))
        },
        "hash" => |vm, a| digest(vm, s(&a[0]), s(&a[1]).as_bytes()).map(Value::str),
        "hash_hmac" => |vm, a| hmac(vm, s(&a[0]), s(&a[1]).as_bytes(), s(&a[2]).as_bytes()).map(Value::str),
        "hash_equals" => |_, a| Ok(Value::Bool(constant_time_eq(s(&a[0]).as_bytes(), s(&a[1]).as_bytes()))),
        "password_hash" => |_, a| Ok(Value::str(password_hash(s(&a[0])))),
        "password_verify" => |_, a| Ok(Value::Bool(password_verify(s(&a[0]), s(&a[1])))),
        "base64_encode" => |_, a| {
            use base64::Engine;
            Ok(Value::str(base64::engine::general_purpose::STANDARD.encode(s(&a[0]))))
        },
        "base64_decode" => |_, a| {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD.decode(s(&a[0]).trim()).ok();
            Ok(bytes.and_then(|b| String::from_utf8(b).ok()).map(Value::str).unwrap_or(Value::Null))
        },
        "urlencode" => |_, a| Ok(Value::str(form_encode(s(&a[0])))),
        "rawurlencode" => |_, a| {
            const SET: &percent_encoding::AsciiSet =
                &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
            Ok(Value::str(percent_encoding::utf8_percent_encode(s(&a[0]), SET).to_string()))
        },
        "urldecode" => |_, a| {
            let t = s(&a[0]).replace('+', " ");
            Ok(Value::str(percent_encoding::percent_decode_str(&t).decode_utf8_lossy().into_owned()))
        },
        "http_build_query" => |_, a| {
            let Value::Map(m) = &a[0] else { return Ok(Value::str("")) };
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| {
                    let k = match k {
                        Key::Str(s) => s.to_string(),
                        Key::Int(i) => i.to_string(),
                    };
                    format!("{}={}", form_encode(&k), form_encode(&v.to_display_string()))
                })
                .collect();
            Ok(Value::str(parts.join("&")))
        },
        "file_get_contents" => |vm, a| {
            let rel = s(&a[0]).to_string();
            let (dir, name) = resolve_path(vm, &rel, false)?;
            let mut f = dir.open(&name, false).map_err(|e| fs_error(vm, &rel, "", e))?;
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            vm.reserve(size as usize)?;
            let mut bytes = Vec::with_capacity(size as usize);
            f.read_to_end(&mut bytes).map_err(|e| io_error(vm, "cannot read", &rel, e))?;
            String::from_utf8(bytes)
                .map(Value::str)
                .map_err(|_| vm.throw("ValueError", format!("\"{rel}\" is not UTF-8 text")))
        },
        "__resp_file" => |vm, a| {
            let rel = s(&a[0]).to_string();
            let ct = match s(&a[1]) {
                "" => file_type(&rel).to_string(),
                given => given.to_string(),
            };
            crate::check_header(vm, "Content-Type", &ct)?;
            let (dir, name) = resolve_path(vm, &rel, false)?;
            let mut f = dir.open(&name, false).map_err(|e| fs_error(vm, &rel, "", e))?;
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            vm.reserve(size as usize)?;
            let mut bytes = Vec::with_capacity(size as usize);
            f.read_to_end(&mut bytes).map_err(|e| io_error(vm, "cannot read", &rel, e))?;
            crate::set_type(vm, &ct)?;
            vm.http.body_file = Some(bytes);
            Ok(Value::Null)
        },
        "file_put_contents" => |vm, a| {
            let rel = s(&a[0]).to_string();
            let (dir, name) = resolve_path(vm, &rel, true)?;
            let mut f = dir.open(&name, true).map_err(|e| fs_error(vm, &rel, "", e))?;
            f.write_all(s(&a[1]).as_bytes()).map_err(|e| io_error(vm, "cannot write", &rel, e))?;
            Ok(Value::Null)
        },
        "file_exists" => |vm, a| {
            let rel = s(&a[0]).to_string();
            let (dir, name) = resolve_path(vm, &rel, false)?;
            match dir.exists(&name) {
                Ok(b) => Ok(Value::Bool(b)),
                Err(e) => Err(fs_error(vm, &rel, "", e)),
            }
        },
        "unlink" => |vm, a| {
            let rel = s(&a[0]).to_string();
            let (dir, name) = resolve_path(vm, &rel, true)?;
            dir.remove(&name).map_err(|e| match e {
                FsError::Io(e) => io_error(vm, "cannot delete", &rel, e),
                e => fs_error(vm, &rel, "", e),
            })?;
            Ok(Value::Null)
        },
        "getenv" => |vm, a| {
            let name = s(&a[0]);
            if !env_allowed(vm, name) {
                return Err(permission_error(
                    vm,
                    format!("reading the environment variable \"{name}\" is not allowed; add it to [permissions].env in phv.toml"),
                ));
            }
            Ok(std::env::var(name).map(Value::str).unwrap_or(Value::Null))
        },
        _ => return None,
    };
    Some(f)
}

fn form_encode(t: &str) -> String {
    form_urlencoded_byte_serialize(t)
}

fn form_urlencoded_byte_serialize(t: &str) -> String {
    let mut out = String::new();
    for b in t.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => out.push(b as char),
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_like_php() {
        let t = Utc.with_ymd_and_hms(2026, 3, 7, 14, 5, 9).unwrap();
        assert_eq!(format_date("Y-m-d H:i:s", &t), "2026-03-07 14:05:09");
        assert_eq!(format_date("D, j M y g:ia", &t), "Sat, 7 Mar 26 2:05pm");
        assert_eq!(format_date("l jS \\o\\f F", &t), "Saturday 7th of March");
        assert_eq!(format_date("N t L P U", &t), format!("6 31 0 +00:00 {}", t.timestamp()));
    }

    #[test]
    fn hashes_and_passwords() {
        assert_eq!(hex(&Sha256::digest(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let h = password_hash("secret");
        assert!(h.starts_with("$argon2id$"));
        assert!(password_verify("secret", &h));
        assert!(!password_verify("wrong", &h));
        assert!(!password_verify("secret", "not a hash"));
        assert!(constant_time_eq(b"abc", b"abc") && !constant_time_eq(b"abc", b"abd"));
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(form_encode("a b&c=d/é"), "a+b%26c%3Dd%2F%C3%A9");
    }
}
