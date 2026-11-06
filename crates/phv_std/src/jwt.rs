//! JSON Web Tokens signed with RS256, ES256, EdDSA (Ed25519) or HS256, and
//! key sets (JWKS), for `std.jwt` and the sign-in of `std.oauth`.
//!
//! A key's type decides its algorithm (an RSA key is RS256, a P-256 key
//! ES256, an Ed25519 key EdDSA, a secret HS256), and a token is verified only
//! with keys of the algorithm its header names: a public key can never be
//! used as an HMAC secret, and `none` is never accepted. Header parameters
//! that point at other keys (`jku`, `x5u`, `jwk`, `x5c`) are ignored, and a
//! token with `crit` is refused.
//!
//! Keys live in `vm.resources` (the scripts hold their index), so a private
//! key never appears in a script's values, `dump()` or an error page.
//! Key sets fetched from an HTTP service are kept for the whole process (see
//! `Cache`) and fetched again when they are old, or when a token names a key
//! id the set does not have.

use crate::httpclient::{self, Response};
use crate::{int, list, map, s};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use phv_vm::{Key, Map, Value, Vm, VmError};
use ring::hmac;
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, KeyPair, RsaKeyPair, RsaPublicKeyComponents};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type R = Result<Value, VmError>;

/// Largest token read; larger ones are invalid.
const MAX_TOKEN: usize = 256 << 10;
/// Largest decoded header of a token.
const MAX_HEADER: usize = 8 << 10;
/// Largest key set document, and keys read from one.
const MAX_JWKS: usize = 512 << 10;
const MAX_SET_KEYS: usize = 64;
/// Keys tried for one token (each costs a signature check).
const MAX_TRIES: usize = 8;
/// Smallest HS256 secret.
const MIN_SECRET: usize = 32;
/// Largest leeway for `exp`, `nbf` and `iat`, in seconds.
pub(crate) const MAX_LEEWAY: i64 = 300;
/// A key id: at most this many bytes.
const MAX_KID: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Alg {
    Rs256,
    Es256,
    EdDsa,
    Hs256,
}

impl Alg {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Alg::Rs256 => "RS256",
            Alg::Es256 => "ES256",
            Alg::EdDsa => "EdDSA",
            Alg::Hs256 => "HS256",
        }
    }

    fn from_name(n: &str) -> Option<Alg> {
        Some(match n {
            "RS256" => Alg::Rs256,
            "ES256" => Alg::Es256,
            "EdDSA" => Alg::EdDsa,
            "HS256" => Alg::Hs256,
            _ => return None,
        })
    }
}

enum Public {
    Rsa {
        n: Vec<u8>,
        e: Vec<u8>,
    },
    /// An uncompressed P-256 point (`04 || x || y`).
    Ec(Vec<u8>),
    Ed(Vec<u8>),
    Secret(hmac::Key),
}

enum Private {
    Rsa(RsaKeyPair),
    Ec(EcdsaKeyPair),
    Ed(Ed25519KeyPair),
}

pub(crate) struct JwtKey {
    pub(crate) alg: Alg,
    pub(crate) kid: Option<String>,
    public: Public,
    private: Option<Private>,
}

impl JwtKey {
    fn can_sign(&self) -> bool {
        self.private.is_some() || matches!(self.public, Public::Secret(_))
    }

    fn verify_sig(&self, msg: &[u8], sig: &[u8]) -> bool {
        match &self.public {
            Public::Rsa { n, e } => {
                RsaPublicKeyComponents { n, e }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg, sig).is_ok()
            }
            Public::Ec(p) => {
                signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, p).verify(msg, sig).is_ok()
            }
            Public::Ed(p) => signature::UnparsedPublicKey::new(&signature::ED25519, p).verify(msg, sig).is_ok(),
            Public::Secret(k) => hmac::verify(k, msg, sig).is_ok(),
        }
    }

    fn sign(&self, msg: &[u8]) -> Option<Vec<u8>> {
        let rng = SystemRandom::new();
        match (&self.private, &self.public) {
            (Some(Private::Rsa(kp)), _) => {
                let mut sig = vec![0; kp.public().modulus_len()];
                kp.sign(&signature::RSA_PKCS1_SHA256, &rng, msg, &mut sig).ok()?;
                Some(sig)
            }
            (Some(Private::Ec(kp)), _) => kp.sign(&rng, msg).ok().map(|s| s.as_ref().to_vec()),
            (Some(Private::Ed(kp)), _) => Some(kp.sign(msg).as_ref().to_vec()),
            (None, Public::Secret(k)) => Some(hmac::sign(k, msg).as_ref().to_vec()),
            _ => None,
        }
    }

    /// The public key as a JWK (JSON), or None for a secret.
    fn public_jwk(&self) -> Option<String> {
        let b = |v: &[u8]| URL_SAFE_NO_PAD.encode(v);
        let mut out = match &self.public {
            Public::Rsa { n, e } => format!(r#"{{"kty":"RSA","n":"{}","e":"{}""#, b(n), b(e)),
            Public::Ec(p) => {
                format!(r#"{{"kty":"EC","crv":"P-256","x":"{}","y":"{}""#, b(&p[1..33]), b(&p[33..65]))
            }
            Public::Ed(p) => format!(r#"{{"kty":"OKP","crv":"Ed25519","x":"{}""#, b(p)),
            Public::Secret(_) => return None,
        };
        out.push_str(&format!(r#","alg":"{}","use":"sig""#, self.alg.name()));
        if let Some(kid) = &self.kid {
            out.push_str(",\"kid\":");
            crate::json_string(kid, &mut out);
        }
        out.push('}');
        Some(out)
    }
}

// --- DER and PEM ---------------------------------------------------------

const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

/// One DER element: its tag, its contents and what follows it.
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 3 || rest.len() < n {
            return None;
        }
        let len = rest[..n].iter().fold(0usize, |a, &x| (a << 8) | x as usize);
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// A DER INTEGER as unsigned big-endian bytes without leading zeros.
fn positive(b: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let (tag, v, rest) = tlv(b)?;
    if tag != 0x02 || v.is_empty() || v[0] & 0x80 != 0 {
        return None;
    }
    let start = v.iter().position(|&x| x != 0).unwrap_or(v.len());
    Some((v[start..].to_vec(), rest))
}

/// `RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }`.
fn rsa_public(der: &[u8]) -> Option<Public> {
    let (tag, seq, rest) = tlv(der)?;
    if tag != 0x30 || !rest.is_empty() {
        return None;
    }
    let (n, seq) = positive(seq)?;
    let (e, seq) = positive(seq)?;
    if !seq.is_empty() {
        return None;
    }
    rsa_checked(n, e)
}

/// RSA public keys of 2048 to 8192 bits (as `RSA_PKCS1_2048_8192_SHA256`
/// verifies), with an exponent of at most 33 bits. A modulus of zero (no
/// bytes once leading zeros are dropped) is refused like any other short one.
fn rsa_checked(n: Vec<u8>, e: Vec<u8>) -> Option<Public> {
    let n: Vec<u8> = n.into_iter().skip_while(|&x| x == 0).collect();
    let e: Vec<u8> = e.into_iter().skip_while(|&x| x == 0).collect();
    // The first byte is not zero, so it has at most 7 leading zero bits.
    let bits = match n.first() {
        Some(b) => n.len().saturating_mul(8).saturating_sub(b.leading_zeros() as usize),
        None => 0,
    };
    if !(2048..=8192).contains(&bits) || e.is_empty() || e.len() > 5 {
        return None;
    }
    Some(Public::Rsa { n, e })
}

/// `SubjectPublicKeyInfo` of an RSA, P-256 or Ed25519 key.
fn spki(der: &[u8]) -> Option<(Alg, Public)> {
    let (tag, seq, rest) = tlv(der)?;
    if tag != 0x30 || !rest.is_empty() {
        return None;
    }
    let (tag, algid, rest) = tlv(seq)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, bits, rest) = tlv(rest)?;
    if tag != 0x03 || !rest.is_empty() {
        return None;
    }
    let (&unused, key) = bits.split_first()?;
    if unused != 0 {
        return None;
    }
    let (tag, oid, params) = tlv(algid)?;
    if tag != 0x06 {
        return None;
    }
    match oid {
        OID_RSA => Some((Alg::Rs256, rsa_public(key)?)),
        OID_EC => {
            let (tag, curve, rest) = tlv(params)?;
            if tag != 0x06 || curve != OID_P256 || !rest.is_empty() || key.len() != 65 || key[0] != 4 {
                return None;
            }
            Some((Alg::Es256, Public::Ec(key.to_vec())))
        }
        OID_ED25519 if params.is_empty() && key.len() == 32 => Some((Alg::EdDsa, Public::Ed(key.to_vec()))),
        _ => None,
    }
}

/// `ECPrivateKey` (SEC 1, `BEGIN EC PRIVATE KEY`) of a P-256 key: its
/// private scalar and its public point.
fn sec1(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (tag, seq, rest) = tlv(der)?;
    if tag != 0x30 || !rest.is_empty() {
        return None;
    }
    let (tag, version, mut seq) = tlv(seq)?;
    if tag != 0x02 || version != [1] {
        return None;
    }
    let (tag, scalar, rest) = tlv(seq)?;
    if tag != 0x04 {
        return None;
    }
    seq = rest;
    let mut point = None;
    while !seq.is_empty() {
        let (tag, v, rest) = tlv(seq)?;
        match tag {
            0xa0 => {
                let (t, curve, r) = tlv(v)?;
                if t != 0x06 || curve != OID_P256 || !r.is_empty() {
                    return None;
                }
            }
            0xa1 => {
                let (t, bits, r) = tlv(v)?;
                let (&unused, p) = bits.split_first()?;
                if t != 0x03 || unused != 0 || !r.is_empty() {
                    return None;
                }
                point = Some(p.to_vec());
            }
            _ => return None,
        }
        seq = rest;
    }
    Some((scalar.to_vec(), point?))
}

/// The first PEM block of `text`: its label and its bytes.
fn pem(text: &str) -> Result<(String, Vec<u8>), String> {
    let start = text.find("-----BEGIN ").ok_or("the key is not in PEM form (-----BEGIN ...-----)")?;
    let rest = &text[start + 11..];
    let end = rest.find("-----").ok_or("the PEM header is not closed")?;
    let label = rest[..end].to_string();
    let body = &rest[end + 5..];
    let close = format!("-----END {label}-----");
    let stop = body.find(&close).ok_or_else(|| format!("no {close} line"))?;
    let b64: String = body[..stop].chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let der = STANDARD.decode(b64).map_err(|_| "the PEM block is not valid base64".to_string())?;
    Ok((label, der))
}

const KEY_KINDS: &str = "RSA (2048 to 4096 bits to sign, up to 8192 to verify), P-256 or Ed25519";

/// A key from PEM text: a private key (PKCS#8, PKCS#1 RSA or SEC 1 P-256),
/// which signs and verifies, or a public key (SPKI or PKCS#1 RSA).
pub(crate) fn from_pem(text: &str, kid: Option<String>) -> Result<JwtKey, String> {
    let (label, der) = pem(text)?;
    let rng = SystemRandom::new();
    let private = |p: Private| {
        let (alg, public) = match &p {
            Private::Rsa(kp) => {
                let c = RsaPublicKeyComponents::<Vec<u8>>::from(kp.public());
                (Alg::Rs256, Public::Rsa { n: c.n, e: c.e })
            }
            Private::Ec(kp) => (Alg::Es256, Public::Ec(kp.public_key().as_ref().to_vec())),
            Private::Ed(kp) => (Alg::EdDsa, Public::Ed(kp.public_key().as_ref().to_vec())),
        };
        JwtKey { alg, kid: kid.clone(), public, private: Some(p) }
    };
    let refused = || format!("the key is not an {KEY_KINDS} key");
    match label.as_str() {
        "PRIVATE KEY" => {
            if let Ok(kp) = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der) {
                return Ok(private(Private::Ed(kp)));
            }
            if let Ok(kp) = EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng) {
                return Ok(private(Private::Ec(kp)));
            }
            RsaKeyPair::from_pkcs8(&der).map(|kp| private(Private::Rsa(kp))).map_err(|_| refused())
        }
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der).map(|kp| private(Private::Rsa(kp))).map_err(|_| refused()),
        "EC PRIVATE KEY" => {
            let (scalar, point) = sec1(&der).ok_or_else(refused)?;
            EcdsaKeyPair::from_private_key_and_public_key(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &scalar,
                &point,
                &rng,
            )
            .map(|kp| private(Private::Ec(kp)))
            .map_err(|_| refused())
        }
        "PUBLIC KEY" => {
            let (alg, public) = spki(&der).ok_or_else(refused)?;
            Ok(JwtKey { alg, kid, public, private: None })
        }
        "RSA PUBLIC KEY" => {
            let public = rsa_public(&der).ok_or_else(refused)?;
            Ok(JwtKey { alg: Alg::Rs256, kid, public, private: None })
        }
        "ENCRYPTED PRIVATE KEY" => Err("the key is encrypted; decrypt it first (openssl pkcs8 -topk8 -nocrypt)".into()),
        _ => Err(format!("a PEM block of \"{}\" is not a key", label.escape_debug())),
    }
}

/// An HS256 key.
pub(crate) fn from_secret(secret: &[u8], kid: Option<String>) -> Result<JwtKey, String> {
    if secret.len() < MIN_SECRET {
        return Err(format!(
            "the secret must be at least {MIN_SECRET} bytes; make one with random_token() and keep it in an environment variable"
        ));
    }
    Ok(JwtKey {
        alg: Alg::Hs256,
        kid,
        public: Public::Secret(hmac::Key::new(hmac::HMAC_SHA256, secret)),
        private: None,
    })
}

fn key_str<'a>(m: &'a Map, k: &str) -> Option<&'a str> {
    match m.get(&Key::Str(Rc::new(k.to_string()))) {
        Some(Value::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn has(m: &Map, k: &str) -> bool {
    m.contains_key(&Key::Str(Rc::new(k.to_string())))
}

fn b64(m: &Map, k: &str) -> Result<Vec<u8>, String> {
    let v = key_str(m, k).ok_or_else(|| format!("the JWK has no \"{k}\""))?;
    URL_SAFE_NO_PAD.decode(v).map_err(|_| format!("the JWK's \"{k}\" is not base64url"))
}

/// The public key of a JWK (private members are not read). Keys meant for
/// encryption (`use` other than `sig`), and symmetric keys (`oct`), are refused.
pub(crate) fn from_jwk(m: &Map) -> Result<JwtKey, String> {
    let kty = key_str(m, "kty").ok_or("the JWK has no \"kty\"")?;
    if let Some(u) = m.get(&Key::Str(Rc::new("use".into()))) {
        if !matches!(u, Value::Str(s) if s.as_str() == "sig") {
            return Err("the JWK is not for signatures (\"use\" is not \"sig\")".into());
        }
    }
    if let Some(ops) = m.get(&Key::Str(Rc::new("key_ops".into()))) {
        let ok =
            matches!(ops, Value::List(l) if l.iter().any(|o| matches!(o, Value::Str(s) if s.as_str() == "verify")));
        if !ok {
            return Err("the JWK's \"key_ops\" does not allow \"verify\"".into());
        }
    }
    let kid = match m.get(&Key::Str(Rc::new("kid".into()))) {
        None => None,
        Some(Value::Str(s)) if !s.is_empty() && s.len() <= MAX_KID => Some(s.to_string()),
        Some(_) => return Err(format!("the JWK's \"kid\" must be a string of 1 to {MAX_KID} bytes")),
    };
    let (alg, public) = match kty {
        "RSA" => (Alg::Rs256, rsa_checked(b64(m, "n")?, b64(m, "e")?).ok_or("the RSA key is not 2048 to 8192 bits")?),
        "EC" => {
            if key_str(m, "crv") != Some("P-256") {
                return Err("only EC keys on the P-256 curve are supported".into());
            }
            let (x, y) = (b64(m, "x")?, b64(m, "y")?);
            if x.len() != 32 || y.len() != 32 {
                return Err("the P-256 key's x and y must be 32 bytes".into());
            }
            (Alg::Es256, Public::Ec([&[4u8][..], &x, &y].concat()))
        }
        "OKP" => {
            if key_str(m, "crv") != Some("Ed25519") {
                return Err("only OKP keys on Ed25519 are supported".into());
            }
            let x = b64(m, "x")?;
            if x.len() != 32 {
                return Err("the Ed25519 key's x must be 32 bytes".into());
            }
            (Alg::EdDsa, Public::Ed(x))
        }
        "oct" => return Err("symmetric (oct) keys are not read from JWKs; use JwtKey::secret()".into()),
        other => return Err(format!("unsupported key type \"{}\"", other.escape_debug())),
    };
    if let Some(a) = m.get(&Key::Str(Rc::new("alg".into()))) {
        if !matches!(a, Value::Str(s) if s.as_str() == alg.name()) {
            return Err(format!("the JWK's \"alg\" is not {} (the only algorithm of its key type)", alg.name()));
        }
    }
    Ok(JwtKey { alg, kid, public, private: None })
}

/// The usable keys of a JWKS document; keys of other kinds are skipped. With
/// a script's `vm`, the document is decoded within its memory and time limits
/// (the outer error); the inner error says why the document is not a key set.
pub(crate) fn from_jwks(vm: Option<&Vm>, text: &str) -> Result<Result<Vec<Arc<JwtKey>>, String>, VmError> {
    if text.len() > MAX_JWKS {
        return Ok(Err(format!("the key set is larger than {} KB", MAX_JWKS >> 10)));
    }
    let doc = match vm {
        Some(vm) => crate::web::json_decode_in(vm, text)?,
        None => crate::web::json_decode(text),
    };
    Ok(jwks_keys(doc))
}

fn jwks_keys(doc: Result<Value, String>) -> Result<Vec<Arc<JwtKey>>, String> {
    let doc = doc.map_err(|e| format!("the key set is not JSON: {e}"))?;
    let Value::Map(doc) = &doc else { return Err("the key set is not a JSON object".into()) };
    let Some(Value::List(keys)) = doc.get(&Key::Str(Rc::new("keys".into()))) else {
        return Err("the key set has no \"keys\" list".into());
    };
    let mut out = Vec::new();
    for k in keys.iter() {
        if let Value::Map(m) = k {
            if let Ok(key) = from_jwk(m) {
                out.push(Arc::new(key));
                if out.len() == MAX_SET_KEYS {
                    break;
                }
            }
        }
    }
    if out.is_empty() {
        return Err("the key set holds no RS256, ES256 or EdDSA signing key".into());
    }
    Ok(out)
}

// --- Tokens --------------------------------------------------------------

/// What a token must satisfy besides its signature.
#[derive(Default)]
pub(crate) struct Expect<'a> {
    pub issuer: Option<&'a str>,
    /// With an audience, `aud` must be it or a list holding it; without one, a
    /// token that has `aud` is refused (it is meant for someone named).
    pub audience: Option<&'a str>,
    pub leeway: i64,
}

pub(crate) fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

fn claim<'a>(m: &'a Map, k: &str) -> Option<&'a Value> {
    m.get(&Key::Str(Rc::new(k.to_string())))
}

fn number(v: Option<&Value>) -> Result<Option<f64>, ()> {
    match v {
        None => Ok(None),
        Some(Value::Int(i)) => Ok(Some(*i as f64)),
        Some(Value::Float(f)) if f.is_finite() => Ok(Some(*f)),
        Some(_) => Err(()),
    }
}

/// Whether the claims are valid at `now` for `expect`.
pub(crate) fn claims_ok(m: &Map, expect: &Expect, now: f64) -> bool {
    let leeway = expect.leeway as f64;
    let (Ok(exp), Ok(nbf), Ok(iat)) = (number(claim(m, "exp")), number(claim(m, "nbf")), number(claim(m, "iat")))
    else {
        return false;
    };
    if exp.is_some_and(|exp| now >= exp + leeway) || nbf.is_some_and(|nbf| now + leeway < nbf) {
        return false;
    }
    // Issued in the future: a clock far off, or a token made to last.
    if iat.is_some_and(|iat| iat > now + leeway.max(60.0)) {
        return false;
    }
    if let Some(iss) = expect.issuer {
        if !matches!(claim(m, "iss"), Some(Value::Str(s)) if s.as_str() == iss) {
            return false;
        }
    }
    match (expect.audience, claim(m, "aud")) {
        (_, None) => expect.audience.is_none(),
        (None, Some(_)) => false,
        (Some(a), Some(Value::Str(s))) => s.as_str() == a,
        (Some(a), Some(Value::List(l))) => l.iter().any(|x| matches!(x, Value::Str(s) if s.as_str() == a)),
        (Some(_), Some(_)) => false,
    }
}

/// The parts of a token whose header and signature are well formed, and
/// the algorithm and key id its header names.
struct Parsed<'t> {
    signed: &'t str,
    payload: &'t str,
    sig: Vec<u8>,
    alg: Alg,
    kid: Option<String>,
}

/// JSON of a token's part, decoded within the script's limits when there is one.
fn json_part(vm: Option<&Vm>, text: &str) -> Result<Option<Value>, VmError> {
    Ok(match vm {
        Some(vm) => crate::web::json_decode_in(vm, text)?.ok(),
        None => crate::web::json_decode(text).ok(),
    })
}

fn parse<'t>(vm: Option<&Vm>, token: &'t str) -> Result<Option<Parsed<'t>>, VmError> {
    if token.len() > MAX_TOKEN {
        return Ok(None);
    }
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Ok(None);
    };
    if h.len() > MAX_HEADER * 4 / 3 + 4 {
        return Ok(None);
    }
    let (Ok(header), Ok(sig)) = (URL_SAFE_NO_PAD.decode(h), URL_SAFE_NO_PAD.decode(sig)) else { return Ok(None) };
    let Ok(header) = std::str::from_utf8(&header) else { return Ok(None) };
    let Some(header) = json_part(vm, header)? else { return Ok(None) };
    let Value::Map(header) = &header else { return Ok(None) };
    // Extensions this implementation does not understand must not be ignored (RFC 7515 4.1.11).
    if has(header, "crit") {
        return Ok(None);
    }
    let Some(alg) = key_str(header, "alg").and_then(Alg::from_name) else { return Ok(None) };
    let kid = match claim(header, "kid") {
        None => None,
        Some(Value::Str(s)) => Some(s.to_string()),
        Some(_) => return Ok(None),
    };
    Ok(Some(Parsed { signed: &token[..h.len() + 1 + p.len()], payload: p, sig, alg, kid }))
}

/// Which of `keys` signed the token, if any; `Err(())` when the token names
/// a key id none of them has (a key set may then be fetched again).
fn find_signer(keys: &[Arc<JwtKey>], t: &Parsed) -> Result<bool, ()> {
    let candidates: Vec<&Arc<JwtKey>> =
        keys.iter().filter(|k| k.alg == t.alg && (t.kid.is_none() || k.kid.is_none() || k.kid == t.kid)).collect();
    if candidates.is_empty() && t.kid.is_some() {
        return Err(());
    }
    // A key with the token's id first.
    let mut ordered: Vec<&Arc<JwtKey>> = candidates.iter().copied().filter(|k| k.kid.is_some()).collect();
    ordered.extend(candidates.iter().copied().filter(|k| k.kid.is_none()));
    Ok(ordered.iter().take(MAX_TRIES).any(|k| k.verify_sig(t.signed.as_bytes(), &t.sig)))
}

/// The claims of a token signed by one of `keys` and valid for `expect`.
fn decode_claims(vm: Option<&Vm>, t: &Parsed, expect: &Expect) -> Result<Option<Value>, VmError> {
    let Ok(payload) = URL_SAFE_NO_PAD.decode(t.payload) else { return Ok(None) };
    let Ok(payload) = std::str::from_utf8(&payload) else { return Ok(None) };
    let Some(claims) = json_part(vm, payload)? else { return Ok(None) };
    let Value::Map(m) = &claims else { return Ok(None) };
    if !claims_ok(m, expect, now()) {
        return Ok(None);
    }
    Ok(Some(claims))
}

/// A token signed with `key`: the header names the algorithm and key id.
pub(crate) fn sign(vm: &Vm, key: &JwtKey, claims: &Value) -> Result<Option<String>, VmError> {
    let mut header = format!(r#"{{"alg":"{}","typ":"JWT""#, key.alg.name());
    if let Some(kid) = &key.kid {
        header.push_str(",\"kid\":");
        crate::json_string(kid, &mut header);
    }
    header.push('}');
    let mut body = String::new();
    crate::json(vm, claims, &mut body, 0)?;
    let signed = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(body));
    let Some(sig) = key.sign(signed.as_bytes()) else { return Ok(None) };
    Ok(Some(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(sig))))
}

// --- Key sets fetched over HTTP -------------------------------------------

/// Where a key set comes from.
#[derive(Clone)]
pub(crate) enum Source {
    /// A path of an `[http.<name>]` service.
    Service { name: String, path: String },
    /// A URL a provider named (`jwks_uri` of OpenID Connect discovery).
    Url { url: String, tls: Option<bool> },
}

struct Cached {
    keys: Arc<[Arc<JwtKey>]>,
    fetched: Instant,
    max_age: Duration,
    /// When it was last fetched or tried, successfully or not.
    tried: Instant,
    /// When a token naming a key id the set lacked last made it fetch again.
    forced: Option<Instant>,
}

/// Key sets by `cache_key` (the URL, and a service's name and headers), the
/// most recently used last. Each set is fetched again
/// once it is older than its `Cache-Control: max-age` (1 minute to a day; an
/// hour without one), or when a token names a key id it lacks (at most every
/// 30 seconds, so tokens with made-up ids cannot make the server fetch it
/// over and over). A set that cannot be fetched again is used for up to a
/// day after it was last fetched, and tried again every 30 seconds (so a
/// provider that is down does not make every request wait for it).
static CACHE: LazyLock<Mutex<Vec<(String, Cached)>>> = LazyLock::new(|| Mutex::new(Vec::new()));
const CACHE_SETS: usize = 32;
const REFETCH_EVERY: Duration = Duration::from_secs(30);
const STALE_FOR: Duration = Duration::from_secs(86_400);

fn max_age(resp: &Response) -> Duration {
    let secs = resp
        .header("cache-control")
        .and_then(|cc| {
            cc.split(',').find_map(|d| d.trim().strip_prefix("max-age=").and_then(|v| v.trim().parse::<u64>().ok()))
        })
        .unwrap_or(3600);
    Duration::from_secs(secs.clamp(60, 86_400))
}

/// Recent failures to fetch a document that could not be used at all (a key
/// set or a discovery document with no copy kept), by cache key: while one
/// is recent, the next request fails at once with the same error instead of
/// waiting for the provider again, so a provider that is down does not hold
/// every request until its timeout. A document is tried again at most every
/// `RETRY_FAILED`.
pub(crate) struct Failures(Mutex<Vec<(String, Instant, String)>>);
pub(crate) const RETRY_FAILED: Duration = Duration::from_secs(10);
const FAILURES_KEPT: usize = 64;

impl Failures {
    pub(crate) const fn new() -> Failures {
        Failures(Mutex::new(Vec::new()))
    }

    /// The error of a failure of `key` less than `RETRY_FAILED` ago.
    pub(crate) fn recent(&self, key: &str) -> Option<String> {
        let list = self.0.lock().unwrap_or_else(|e| e.into_inner());
        list.iter().find(|(k, at, _)| k == key && at.elapsed() < RETRY_FAILED).map(|(_, _, e)| e.clone())
    }

    pub(crate) fn note(&self, key: &str, error: &str) {
        let mut list = self.0.lock().unwrap_or_else(|e| e.into_inner());
        list.retain(|(k, at, _)| k != key && at.elapsed() < RETRY_FAILED);
        if list.len() >= FAILURES_KEPT {
            list.remove(0);
        }
        list.push((key.to_string(), Instant::now(), error.to_string()));
    }

    pub(crate) fn clear(&self, key: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).retain(|(k, _, _)| k != key);
    }
}

static FAILED: Failures = Failures::new();

/// What a key set is kept under. A service's sets are told apart by the
/// service's name and configured headers as well as the URL: two services
/// may share a URL and differ only in a header (a tenant, a credential) that
/// makes the server answer with another set.
fn cache_key(vm: &mut Vm, source: &Source) -> Result<String, VmError> {
    Ok(match source {
        Source::Service { name, path } => {
            // A service's URL may come from the environment: it is known once the request is built.
            let url = httpclient::service_url(vm, name, path)?;
            let mut headers = String::new();
            for (h, v) in vm.host.http.get(name).map(|spec| spec.headers.as_slice()).unwrap_or_default() {
                let v = v.as_deref().unwrap_or("");
                headers.push_str(&format!("{}:{h}{}:{v}", h.len(), v.len()));
            }
            // Credentials are not kept in the key itself.
            format!("service {} {} {url}", crate::sha256_hex(name.as_bytes()), crate::sha256_hex(headers.as_bytes()))
        }
        Source::Url { url, .. } => format!("url {url}"),
    })
}

/// The keys of `source`: kept ones while they are fresh (unless `refresh`),
/// otherwise fetched. `refresh` asks again only if the last try is 30
/// seconds old. Errors are NetworkError messages.
pub(crate) fn remote_keys(vm: &mut Vm, source: &Source, refresh: bool) -> Result<Arc<[Arc<JwtKey>]>, VmError> {
    let url = cache_key(vm, source)?;
    if let Some(keys) = kept(&url, refresh) {
        return Ok(keys);
    }
    let what = match source {
        Source::Service { name, .. } => format!("HTTP service \"{name}\""),
        Source::Url { .. } => "the provider".to_string(),
    };
    if let Some(e) = FAILED.recent(&url) {
        let secs = RETRY_FAILED.as_secs();
        let msg = format!("{what}: cannot read the key set: {e} (it is tried again at most every {secs} seconds)");
        return Err(vm.throw("NetworkError", msg));
    }
    let forced = mark_tried(&url, refresh);
    let result = match source {
        Source::Service { name, path } => httpclient::get_service(vm, name, path, MAX_JWKS)?,
        Source::Url { url, tls } => {
            let accept = vec![("Accept".to_string(), "application/json".to_string())];
            httpclient::send_url(vm, "GET", url, *tls, accept, Vec::new(), MAX_JWKS)
        }
    };
    vm.check_limits()?;
    // Decoded within the script's limits before anything is kept.
    let parsed = match result {
        Ok(resp) if resp.status() != 200 => Err(format!("the key set answered {}", resp.status())),
        Ok(resp) => match std::str::from_utf8(resp.body()) {
            Ok(text) => from_jwks(Some(vm), text)?.map(|keys| (keys, max_age(&resp))),
            Err(_) => Err("the key set is not UTF-8".to_string()),
        },
        Err(e) => Err(e),
    };
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let i = cache.iter().position(|(u, _)| *u == url);
    match parsed {
        Ok((keys, age)) => {
            let keys: Arc<[Arc<JwtKey>]> = keys.into();
            let now = Instant::now();
            let entry = Cached { keys: keys.clone(), fetched: now, max_age: age, tried: now, forced };
            if let Some(i) = i {
                cache.remove(i);
            }
            if cache.len() >= CACHE_SETS {
                cache.remove(0);
            }
            FAILED.clear(&url);
            cache.push((url, entry));
            Ok(keys)
        }
        Err(e) => match i.map(|i| &cache[i].1) {
            Some(c) if c.fetched.elapsed() < STALE_FOR => Ok(c.keys.clone()),
            _ => {
                drop(cache);
                FAILED.note(&url, &e);
                Err(vm.throw("NetworkError", format!("{what}: cannot read the key set: {e}")))
            }
        },
    }
}

/// Kept keys for `url` that can be used without fetching.
fn kept(url: &str, refresh: bool) -> Option<Arc<[Arc<JwtKey>]>> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let i = cache.iter().position(|(u, _)| u == url)?;
    let entry = cache.remove(i);
    let c = &entry.1;
    let usable = if refresh {
        c.forced.is_some_and(|f| f.elapsed() < REFETCH_EVERY)
    } else {
        c.fetched.elapsed() < c.max_age || (c.tried.elapsed() < REFETCH_EVERY && c.fetched.elapsed() < STALE_FOR)
    };
    let keys = c.keys.clone();
    cache.push(entry);
    usable.then_some(keys)
}

/// Notes that the set is being fetched (`forced`: for an unknown key id),
/// and returns when it was last forced, which the new entry keeps.
fn mark_tried(url: &str, forced: bool) -> Option<Instant> {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    match cache.iter_mut().find(|(u, _)| u == url) {
        Some((_, c)) => {
            c.tried = now;
            if forced {
                c.forced = Some(now);
            }
            c.forced
        }
        None => forced.then_some(now),
    }
}

/// Verifies a token with the keys of a handle, fetching a remote set again
/// when the token names a key id it lacks.
pub(crate) fn verify_with(
    vm: &mut Vm,
    keys: &[Arc<JwtKey>],
    remote: Option<&Source>,
    token: &str,
    expect: &Expect,
) -> Result<Option<Value>, VmError> {
    let Some(t) = parse(Some(vm), token)? else { return Ok(None) };
    let signed = match find_signer(keys, &t) {
        Ok(found) => found,
        Err(()) => match remote {
            Some(source) => {
                let fresh = remote_keys(vm, source, true)?;
                find_signer(&fresh, &t).unwrap_or(false)
            }
            None => false,
        },
    };
    if !signed {
        return Ok(None);
    }
    decode_claims(Some(vm), &t, expect)
}

// --- Builtins --------------------------------------------------------------

/// A handle's keys, kept in `vm.resources`.
struct Keys {
    keys: Vec<Arc<JwtKey>>,
    remote: Option<Source>,
}

fn add(vm: &mut Vm, keys: Keys) -> R {
    vm.resources.push(Some(Box::new(keys)));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

fn keys_of(vm: &Vm, h: &Value) -> Result<(Vec<Arc<JwtKey>>, Option<Source>), VmError> {
    let k = usize::try_from(int(h))
        .ok()
        .and_then(|i| vm.resources.get(i))
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<Keys>())
        .ok_or_else(|| vm.throw("ValueError", "not a key"))?;
    Ok((k.keys.clone(), k.remote.clone()))
}

fn kid_arg(vm: &Vm, v: &Value) -> Result<Option<String>, VmError> {
    match v {
        Value::Null => Ok(None),
        v if !s(v).is_empty() && s(v).len() <= MAX_KID => Ok(Some(s(v).to_string())),
        _ => Err(vm.throw("ValueError", format!("a key id must be 1 to {MAX_KID} bytes"))),
    }
}

fn opt_str(v: &Value) -> Option<&str> {
    match v {
        Value::Null => None,
        v => Some(s(v)),
    }
}

pub(crate) fn lookup(name: &str) -> Option<phv_vm::BuiltinFn> {
    let f: phv_vm::BuiltinFn = match name {
        // __jwt_pem(pem, kid): a key handle.
        "__jwt_pem" => |vm, a| {
            let kid = kid_arg(vm, &a[1])?;
            match from_pem(s(&a[0]), kid) {
                Ok(k) => add(vm, Keys { keys: vec![Arc::new(k)], remote: None }),
                Err(e) => Err(vm.throw("ValueError", format!("JwtKey::fromPem(): {e}"))),
            }
        },
        "__jwt_jwk" => |vm, a| {
            let parsed = match crate::web::json_decode_in(vm, s(&a[0]))? {
                Ok(v) => match &v {
                    Value::Map(m) => from_jwk(m),
                    _ => Err("the JWK is not a JSON object".into()),
                },
                Err(e) => Err(format!("the JWK is not JSON: {e}")),
            };
            match parsed {
                Ok(k) => add(vm, Keys { keys: vec![Arc::new(k)], remote: None }),
                Err(e) => Err(vm.throw("ValueError", format!("JwtKey::fromJwk(): {e}"))),
            }
        },
        "__jwt_secret" => |vm, a| {
            let kid = kid_arg(vm, &a[1])?;
            match from_secret(s(&a[0]).as_bytes(), kid) {
                Ok(k) => add(vm, Keys { keys: vec![Arc::new(k)], remote: None }),
                Err(e) => Err(vm.throw("ValueError", format!("JwtKey::secret(): {e}"))),
            }
        },
        // __jwt_info(h): [alg, kid ("" for none), "1" when it can sign].
        "__jwt_info" => |vm, a| {
            let (keys, _) = keys_of(vm, &a[0])?;
            let k = keys.first().ok_or_else(|| vm.throw("ValueError", "not a key"))?;
            Ok(Value::list(vec![
                Value::str(k.alg.name()),
                Value::str(k.kid.as_deref().unwrap_or("")),
                Value::str(if k.can_sign() { "1" } else { "" }),
            ]))
        },
        "__jwt_sign" => |vm, a| {
            let (keys, _) = keys_of(vm, &a[0])?;
            let k = keys.first().ok_or_else(|| vm.throw("ValueError", "not a key"))?;
            let claims = Value::Map(map(&a[1]).clone());
            match sign(vm, k, &claims)? {
                Some(t) => Ok(Value::str(t)),
                None => Err(vm.throw("ValueError", "JwtKey::sign(): a public key cannot sign; load the private key")),
            }
        },
        // __jwt_verify(h, token, issuer, audience, leeway): the claims, or null.
        "__jwt_verify" => |vm, a| {
            let leeway = int(&a[4]);
            if !(0..=MAX_LEEWAY).contains(&leeway) {
                return Err(vm.throw("ValueError", format!("verify(): the leeway must be 0 to {MAX_LEEWAY} seconds")));
            }
            let (keys, remote) = keys_of(vm, &a[0])?;
            let token = s(&a[1]).to_string();
            let (issuer, audience) = (opt_str(&a[2]).map(str::to_string), opt_str(&a[3]).map(str::to_string));
            let expect = Expect { issuer: issuer.as_deref(), audience: audience.as_deref(), leeway };
            // A remote set uses the newest keys kept for the process.
            let keys = match &remote {
                Some(source) => remote_keys(vm, source, false)?.to_vec(),
                None => keys,
            };
            Ok(verify_with(vm, &keys, remote.as_ref(), &token, &expect)?.unwrap_or(Value::Null))
        },
        // __jwt_public(h): the JWK of a key, or the JWKS of a set (public keys only).
        "__jwt_public" => |vm, a| {
            let (keys, remote) = keys_of(vm, &a[0])?;
            let keys = match &remote {
                Some(source) => remote_keys(vm, source, false)?.to_vec(),
                None => keys,
            };
            let jwks: Vec<String> = keys.iter().filter_map(|k| k.public_jwk()).collect();
            if int(&a[1]) == 1 {
                return match jwks.into_iter().next() {
                    Some(j) => Ok(Value::str(j)),
                    None => Err(vm.throw("ValueError", "publicJwk(): a secret key has no public form")),
                };
            }
            Ok(Value::str(format!("{{\"keys\":[{}]}}", jwks.join(","))))
        },
        "__jwt_set" => |vm, a| {
            let mut keys = Vec::new();
            for h in list(&a[0]).iter() {
                keys.extend(keys_of(vm, h)?.0);
            }
            add(vm, Keys { keys, remote: None })
        },
        "__jwt_set_json" => |vm, a| match from_jwks(Some(vm), s(&a[0]))? {
            Ok(keys) => add(vm, Keys { keys, remote: None }),
            Err(e) => Err(vm.throw("ValueError", format!("KeySet::fromJson(): {e}"))),
        },
        // __jwt_set_fetch(service, path): fetched now unless kept and fresh.
        "__jwt_set_fetch" => |vm, a| {
            let source = Source::Service { name: s(&a[0]).to_string(), path: s(&a[1]).to_string() };
            let keys = remote_keys(vm, &source, false)?.to_vec();
            add(vm, Keys { keys, remote: Some(source) })
        },
        _ => return None,
    };
    Some(f)
}

/// The claims of a map, for tests.
#[cfg(test)]
fn claims_map(pairs: &[(&str, Value)]) -> Map {
    let mut m = indexmap::IndexMap::new();
    for (k, v) in pairs {
        m.insert(Key::Str(Rc::new(k.to_string())), v.clone());
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;

    fn pem_of(label: &str, der: &[u8]) -> String {
        format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", STANDARD.encode(der))
    }

    fn ec_key() -> JwtKey {
        let doc =
            EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).unwrap();
        from_pem(&pem_of("PRIVATE KEY", doc.as_ref()), Some("ec1".into())).unwrap()
    }

    fn ed_key() -> JwtKey {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        from_pem(&pem_of("PRIVATE KEY", doc.as_ref()), None).unwrap()
    }

    fn token(key: &JwtKey, header: &str, claims: &str) -> String {
        let signed = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(claims));
        let sig = key.sign(signed.as_bytes()).unwrap();
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(sig))
    }

    fn check(keys: &[Arc<JwtKey>], token: &str) -> bool {
        let Ok(Some(t)) = parse(None, token) else { return false };
        find_signer(keys, &t).unwrap_or(false)
    }

    #[test]
    fn keys_sign_and_verify_with_their_own_algorithm() {
        for key in [ec_key(), ed_key(), from_secret(&[7; 32], None).unwrap()] {
            let alg = key.alg.name();
            let t = token(&key, &format!(r#"{{"alg":"{alg}"}}"#), r#"{"sub":"7"}"#);
            let key = Arc::new(key);
            assert!(check(std::slice::from_ref(&key), &t), "{alg}");
            // The same token naming another algorithm is refused.
            let (_, rest) = t.split_once('.').unwrap();
            for other in ["none", "HS256", "RS256", "ES256", "EdDSA", "ES384"] {
                if other != alg {
                    let h = URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"{other}"}}"#));
                    assert!(!check(std::slice::from_ref(&key), &format!("{h}.{rest}")), "{alg} as {other}");
                }
            }
        }
    }

    #[test]
    fn a_public_key_is_never_an_hmac_secret() {
        let ec = Arc::new(ec_key());
        let jwk = ec.public_jwk().unwrap();
        // A token "signed" with HS256 using the public key's text as the secret.
        let forged = from_secret(jwk.as_bytes(), None).unwrap();
        let t = token(&forged, r#"{"alg":"HS256","kid":"ec1"}"#, r#"{"sub":"admin"}"#);
        assert!(!check(&[ec], &t));
    }

    #[test]
    fn crit_and_bad_headers_are_refused() {
        let k = ec_key();
        let t = token(&k, r#"{"alg":"ES256","crit":["exp"]}"#, "{}");
        assert!(!check(&[Arc::new(k)], &t));
        let k = Arc::new(ec_key());
        assert!(!check(std::slice::from_ref(&k), "a.b"));
        assert!(!check(std::slice::from_ref(&k), "a.b.c.d"));
        assert!(!check(&[k], &format!("{}.e30.", URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#))));
    }

    #[test]
    fn key_ids_choose_keys() {
        let a = ec_key();
        let t = token(&a, r#"{"alg":"ES256","kid":"ec1"}"#, "{}");
        let b = Arc::new(ec_key());
        let a = Arc::new(a);
        assert!(check(&[b.clone(), a.clone()], &t));
        // A token naming an id the set lacks asks for the set again.
        let t2 = token(&a, r#"{"alg":"ES256","kid":"other"}"#, "{}");
        let Ok(Some(parsed)) = parse(None, &t2) else { panic!("not parsed") };
        assert!(find_signer(&[a.clone(), b], &parsed).is_err());
    }

    #[test]
    fn jwks_round_trips_public_keys() {
        let (ec, ed) = (ec_key(), ed_key());
        let doc = format!(
            r#"{{"keys":[{},{},{{"kty":"oct","k":"AAAA"}},{{"kty":"EC","crv":"P-384","x":"","y":""}}]}}"#,
            ec.public_jwk().unwrap(),
            ed.public_jwk().unwrap()
        );
        let keys = from_jwks(None, &doc).unwrap_or_else(|_| panic!("no limits")).unwrap();
        assert_eq!(keys.len(), 2, "oct and P-384 are skipped");
        assert_eq!(keys[0].kid.as_deref(), Some("ec1"));
        assert!(!keys[0].can_sign());
        let t = token(&ec, r#"{"alg":"ES256","kid":"ec1"}"#, "{}");
        assert!(check(&keys, &t));
        let t = token(&ed, r#"{"alg":"EdDSA"}"#, "{}");
        assert!(check(&keys, &t));
    }

    #[test]
    fn public_pem_keys_verify() {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let kp = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        let mut spki = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
        spki.extend_from_slice(kp.public_key().as_ref());
        let public = from_pem(&pem_of("PUBLIC KEY", &spki), None).unwrap();
        assert_eq!(public.alg, Alg::EdDsa);
        assert!(!public.can_sign());
        let private = from_pem(&pem_of("PRIVATE KEY", doc.as_ref()), None).unwrap();
        let t = token(&private, r#"{"alg":"EdDSA"}"#, "{}");
        assert!(check(&[Arc::new(public)], &t));
    }

    #[test]
    fn claims_are_checked() {
        let e = |iss: Option<&'static str>, aud: Option<&'static str>| Expect { issuer: iss, audience: aud, leeway: 0 };
        let m = claims_map(&[("exp", Value::Int(100)), ("nbf", Value::Int(50))]);
        assert!(claims_ok(&m, &e(None, None), 60.0));
        assert!(!claims_ok(&m, &e(None, None), 100.0), "expired");
        assert!(!claims_ok(&m, &e(None, None), 40.0), "not yet");
        assert!(claims_ok(&m, &Expect { leeway: 10, ..e(None, None) }, 105.0));
        let m = claims_map(&[("exp", Value::str("100"))]);
        assert!(!claims_ok(&m, &e(None, None), 0.0), "exp must be a number");
        let m = claims_map(&[
            ("iss", Value::str("https://a")),
            ("aud", Value::list(vec![Value::str("x"), Value::str("y")])),
        ]);
        assert!(claims_ok(&m, &e(Some("https://a"), Some("y")), 0.0));
        assert!(!claims_ok(&m, &e(Some("https://b"), Some("y")), 0.0));
        assert!(!claims_ok(&m, &e(Some("https://a"), Some("z")), 0.0));
        assert!(!claims_ok(&m, &e(Some("https://a"), None), 0.0), "aud present but not expected");
        let m = claims_map(&[]);
        assert!(!claims_ok(&m, &e(None, Some("x")), 0.0), "aud expected but absent");
        let m = claims_map(&[("iat", Value::Int(10_000))]);
        assert!(!claims_ok(&m, &e(None, None), 0.0), "issued in the future");
    }

    #[test]
    fn der_lengths_are_bounded() {
        assert!(tlv(&[0x30, 0x84, 0, 0, 0, 1, 0]).is_none());
        assert!(tlv(&[0x30, 0x82, 0xff, 0xff]).is_none());
        assert!(spki(&[]).is_none());
        assert!(from_pem("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----", None).is_err());
        assert!(from_secret(b"short", None).is_err());
    }

    /// An RSA key whose modulus is zero (found by the 16th assessment: the bit
    /// count underflowed, an uncatchable internal error) is refused like any
    /// other short key, and a key set skips it.
    #[test]
    fn rsa_keys_with_a_zero_modulus_are_refused() {
        assert!(rsa_checked(vec![], vec![1, 0, 1]).is_none());
        assert!(rsa_checked(vec![0, 0, 0], vec![1, 0, 1]).is_none());
        assert!(rsa_checked(vec![0x80; 256], vec![]).is_none(), "no exponent");
        assert!(rsa_checked(vec![0x80; 256], vec![1, 0, 1]).is_some(), "2048 bits");
        assert!(rsa_checked(vec![0x7f; 256], vec![1, 0, 1]).is_none(), "2047 bits");
        // RSAPublicKey { modulus 0, publicExponent 65537 }, in PEM.
        let pem = "-----BEGIN RSA PUBLIC KEY-----\nMAgCAQACAwEAAQ==\n-----END RSA PUBLIC KEY-----\n";
        assert!(from_pem(pem, None).is_err());
        for n in ["", "AA", "AAAA"] {
            let jwk = format!(r#"{{"kty":"RSA","n":"{n}","e":"AQAB"}}"#);
            let Ok(v) = crate::web::json_decode(&jwk) else { panic!("not JSON") };
            let Value::Map(m) = &v else { panic!("not a map") };
            assert!(from_jwk(m).is_err(), "n = {n:?}");
        }
        let ec = ec_key();
        let doc = format!(r#"{{"keys":[{},{{"kty":"RSA","n":"AA","e":"AQAB"}}]}}"#, ec.public_jwk().unwrap());
        let keys = from_jwks(None, &doc).unwrap_or_else(|_| panic!("no limits")).unwrap();
        assert_eq!(keys.len(), 1, "the zero key is skipped");
    }
}
