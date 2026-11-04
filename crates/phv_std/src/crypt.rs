//! Encryption, signed tokens and JWT (HS256), with `ring`.
//!
//! - `encrypt` / `decrypt`: ChaCha20-Poly1305. Each message gets a random salt
//!   and its own key derived from the caller's key with HKDF-SHA256, and a
//!   random nonce, so a key can encrypt any number of messages. The output is
//!   base64url: version (1) || salt (16) || nonce (12) || ciphertext || tag (16).
//! - `sign_token` / `verify_token`: `base64url(expiry || data).base64url(mac)`,
//!   HMAC-SHA256 with a key derived for signing (so one key can both encrypt and
//!   sign). The expiry is seconds since 1970 (0: never).
//! - `jwt_encode` / `jwt_decode`: HS256 only, so a token cannot choose a weaker
//!   algorithm (`none`) or another kind of key. `exp` and `nbf` are enforced.
//!
//! Keys are secret strings of at least 32 bytes (`random_token()` makes one).

use super::{int, map, s};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use phv_vm::{BuiltinFn, Key, Value, Vm, VmError};
use ring::{aead, hkdf, hmac};
use std::time::{SystemTime, UNIX_EPOCH};

const MIN_KEY: usize = 32;
const VERSION: u8 = 1;
const SALT: usize = 16;
const NONCE: usize = aead::NONCE_LEN;
const TAG: usize = 16;

fn key<'a>(vm: &Vm, v: &'a Value, f: &str) -> Result<&'a [u8], VmError> {
    let k = s(v).as_bytes();
    if k.len() < MIN_KEY {
        return Err(vm.throw(
            "ValueError",
            format!("{f}(): the key must be at least {MIN_KEY} bytes; make one with random_token() and keep it in an environment variable"),
        ));
    }
    Ok(k)
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("operating system random number generator failed");
    b
}

/// A 32-byte key for one purpose (and, for encryption, one message).
fn derive(key: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(key);
    let mut out = [0u8; 32];
    let info = [info];
    let okm = prk.expand(&info, hkdf::HKDF_SHA256).expect("32 bytes is a valid HKDF length");
    okm.fill(&mut out).expect("32 bytes is a valid HKDF length");
    out
}

fn cipher(key: &[u8], salt: &[u8]) -> aead::LessSafeKey {
    let k = derive(key, salt, b"phv encrypt v1");
    aead::LessSafeKey::new(aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &k).expect("32-byte key"))
}

pub fn encrypt(plain: &[u8], key: &[u8]) -> String {
    let salt: [u8; SALT] = random();
    let nonce: [u8; NONCE] = random();
    let mut buf = plain.to_vec();
    cipher(key, &salt)
        .seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::from([VERSION]), &mut buf)
        .expect("message fits ChaCha20-Poly1305");
    let mut out = Vec::with_capacity(1 + SALT + NONCE + buf.len());
    out.push(VERSION);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    URL_SAFE_NO_PAD.encode(out)
}

/// None when the text was not made by `encrypt` with this key, or was changed.
pub fn decrypt(text: &str, key: &[u8]) -> Option<Vec<u8>> {
    let raw = URL_SAFE_NO_PAD.decode(text).ok()?;
    if raw.len() < 1 + SALT + NONCE + TAG || raw[0] != VERSION {
        return None;
    }
    let (salt, rest) = raw[1..].split_at(SALT);
    let (nonce, sealed) = rest.split_at(NONCE);
    let nonce = aead::Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut buf = sealed.to_vec();
    let plain = cipher(key, salt).open_in_place(nonce, aead::Aad::from([VERSION]), &mut buf).ok()?;
    Some(plain.to_vec())
}

fn sign_key(key: &[u8]) -> hmac::Key {
    hmac::Key::new(hmac::HMAC_SHA256, &derive(key, b"", b"phv sign v1"))
}

pub fn sign(data: &[u8], key: &[u8], expires: i64) -> String {
    let mut payload = expires.to_be_bytes().to_vec();
    payload.extend_from_slice(data);
    let mac = hmac::sign(&sign_key(key), &[&[VERSION][..], &payload].concat());
    format!("{}.{}", URL_SAFE_NO_PAD.encode(&payload), URL_SAFE_NO_PAD.encode(mac.as_ref()))
}

/// The signed data, or None if the token is not signed with this key, was
/// changed, or has expired at `now`.
pub fn verify(token: &str, key: &[u8], now: i64) -> Option<Vec<u8>> {
    let (payload, mac) = token.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let mac = URL_SAFE_NO_PAD.decode(mac).ok()?;
    hmac::verify(&sign_key(key), &[&[VERSION][..], &payload].concat(), &mac).ok()?;
    let (expires, data) = payload.split_at_checked(8)?;
    let expires = i64::from_be_bytes(expires.try_into().ok()?);
    if expires != 0 && now >= expires {
        return None;
    }
    Some(data.to_vec())
}

const JWT_HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

fn jwt_mac(key: &[u8], signed: &str) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), signed.as_bytes())
}

fn time_claim(m: &phv_vm::Map, name: &str) -> Result<Option<f64>, ()> {
    match m.get(&Key::Str(name.to_string().into())) {
        None => Ok(None),
        Some(Value::Int(i)) => Ok(Some(*i as f64)),
        Some(Value::Float(f)) => Ok(Some(*f)),
        Some(_) => Err(()),
    }
}

/// The claims of an HS256 token signed with `key`, if it is valid at `now`.
/// With a script's `vm`, its memory and time limits are checked while the
/// JSON is decoded (a token signed by the site may still carry large claims).
fn jwt_decode(vm: Option<&Vm>, token: &str, key: &[u8], now: i64) -> Result<Option<Value>, VmError> {
    let json = |part: &str| -> Result<Option<Value>, VmError> {
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(part) else { return Ok(None) };
        let Ok(text) = std::str::from_utf8(&bytes) else { return Ok(None) };
        Ok(match vm {
            Some(vm) => super::web::json_decode_in(vm, text)?.ok(),
            None => super::web::json_decode(text).ok(),
        })
    };
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Ok(None);
    };
    let Ok(sig) = URL_SAFE_NO_PAD.decode(sig) else { return Ok(None) };
    if hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), format!("{h}.{p}").as_bytes(), &sig).is_err() {
        return Ok(None);
    }
    let Some(header) = json(h)? else { return Ok(None) };
    let Value::Map(header) = &header else { return Ok(None) };
    match header.get(&Key::Str("alg".to_string().into())) {
        Some(Value::Str(alg)) if alg.as_str() == "HS256" => {}
        _ => return Ok(None),
    }
    let Some(claims) = json(p)? else { return Ok(None) };
    let Value::Map(m) = &claims else { return Ok(None) };
    let now = now as f64;
    let (Ok(exp), Ok(nbf)) = (time_claim(m, "exp"), time_claim(m, "nbf")) else { return Ok(None) };
    if exp.is_some_and(|exp| now >= exp) || nbf.is_some_and(|nbf| now < nbf) {
        return Ok(None);
    }
    Ok(Some(claims))
}

fn text(vm: &Vm, bytes: Vec<u8>, f: &str) -> Result<Value, VmError> {
    String::from_utf8(bytes)
        .map(Value::str)
        .map_err(|_| vm.throw("ValueError", format!("{f}(): the data is not UTF-8")))
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "encrypt" => |vm, a| {
            let k = key(vm, &a[1], "encrypt")?;
            Ok(Value::str(encrypt(s(&a[0]).as_bytes(), k)))
        },
        "decrypt" => |vm, a| {
            let k = key(vm, &a[1], "decrypt")?;
            match decrypt(s(&a[0]), k) {
                Some(b) => text(vm, b, "decrypt"),
                None => Ok(Value::Null),
            }
        },
        "sign_token" => |vm, a| {
            let k = key(vm, &a[1], "sign_token")?;
            Ok(Value::str(sign(s(&a[0]).as_bytes(), k, 0)))
        },
        "sign_token_ttl" => |vm, a| {
            let k = key(vm, &a[1], "sign_token")?;
            let ttl = int(&a[2]);
            if ttl <= 0 {
                return Err(vm.throw("ValueError", "sign_token(): the lifetime must be a positive number of seconds"));
            }
            let expires = now()
                .checked_add(ttl)
                .ok_or_else(|| vm.throw("ValueError", "sign_token(): the lifetime is too long"))?;
            Ok(Value::str(sign(s(&a[0]).as_bytes(), k, expires)))
        },
        "verify_token" => |vm, a| {
            let k = key(vm, &a[1], "verify_token")?;
            match verify(s(&a[0]), k, now()) {
                Some(b) => text(vm, b, "verify_token"),
                None => Ok(Value::Null),
            }
        },
        "jwt_encode" => |vm, a| {
            let k = key(vm, &a[1], "jwt_encode")?;
            let mut claims = String::new();
            super::json(vm, &Value::Map(map(&a[0]).clone()), &mut claims, 0)?;
            let signed = format!("{}.{}", URL_SAFE_NO_PAD.encode(JWT_HEADER), URL_SAFE_NO_PAD.encode(claims));
            let mac = jwt_mac(k, &signed);
            Ok(Value::str(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(mac.as_ref()))))
        },
        "jwt_decode" => |vm, a| {
            let k = key(vm, &a[1], "jwt_decode")?;
            Ok(jwt_decode(Some(vm), s(&a[0]), k, now())?.unwrap_or(Value::Null))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn encryption_round_trips_and_detects_changes() {
        let a = encrypt(b"secret", K);
        assert_ne!(a, encrypt(b"secret", K), "a fresh salt and nonce each time");
        assert_eq!(decrypt(&a, K).as_deref(), Some(&b"secret"[..]));
        assert_eq!(decrypt(&a, b"another key that is long enough!!"), None);
        let mut raw = URL_SAFE_NO_PAD.decode(&a).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        assert_eq!(decrypt(&URL_SAFE_NO_PAD.encode(raw), K), None);
        assert_eq!(decrypt("", K), None);
    }

    #[test]
    fn signed_tokens_expire() {
        let t = sign(b"user:7", K, 1000);
        assert_eq!(verify(&t, K, 999).as_deref(), Some(&b"user:7"[..]));
        assert_eq!(verify(&t, K, 1000), None);
        let forever = sign(b"x", K, 0);
        assert!(verify(&forever, K, i64::MAX).is_some());
        // The signing key is not the raw key: a plain HMAC with it does not verify.
        let (p, _) = forever.split_once('.').unwrap();
        let raw_mac = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, K), &URL_SAFE_NO_PAD.decode(p).unwrap());
        assert_eq!(verify(&format!("{p}.{}", URL_SAFE_NO_PAD.encode(raw_mac.as_ref())), K, 0), None);
    }

    #[test]
    fn jwt_rejects_other_algorithms() {
        let claims = URL_SAFE_NO_PAD.encode(r#"{"sub":"7","exp":2000}"#);
        let none = format!("{}.{claims}.", URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#));
        assert!(jwt_decode(None, &none, K, 0).is_ok_and(|c| c.is_none()));
        let h = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS512"}"#);
        let mac = jwt_mac(K, &format!("{h}.{claims}"));
        assert!(jwt_decode(None, &format!("{h}.{claims}.{}", URL_SAFE_NO_PAD.encode(mac.as_ref())), K, 0)
            .is_ok_and(|c| c.is_none()));
        let h = URL_SAFE_NO_PAD.encode(JWT_HEADER);
        let mac = jwt_mac(K, &format!("{h}.{claims}"));
        let good = format!("{h}.{claims}.{}", URL_SAFE_NO_PAD.encode(mac.as_ref()));
        assert!(jwt_decode(None, &good, K, 1999).is_ok_and(|c| c.is_some()));
        assert!(jwt_decode(None, &good, K, 2000).is_ok_and(|c| c.is_none()), "expired");
    }
}
