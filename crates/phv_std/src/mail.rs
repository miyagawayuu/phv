//! Sending mail over SMTP (`std.mail`), to a server named in phv.toml:
//!
//! ```toml
//! [mail.default]
//! url_env = "SMTP_URL"     # smtps://user:pass@smtp.example.com (TLS) or smtp://...:587 (STARTTLS)
//! from = "Example <noreply@example.com>"
//! ```
//!
//! A server on another machine is reached over TLS (`smtps://`, or `STARTTLS`
//! on `smtp://`), unless `tls = false`; credentials are never sent without it.
//! Addresses and the subject cannot hold line breaks, so a value from a form
//! cannot add headers or recipients.

use crate::netdb::is_local_host;
use crate::netio::{self, Stream};
use base64::Engine;
use phv_vm::{MailSpec, Value, Vm, VmError};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

type R = Result<Value, VmError>;

/// Recipients of one message, and its size.
const MAX_RECIPIENTS: usize = 50;
const MAX_MESSAGE: usize = 10 << 20;
/// Longest reply line and reply the client reads.
const MAX_LINE: usize = 4096;
const MAX_REPLY_LINES: usize = 100;

fn text(v: &Value) -> String {
    match v {
        Value::Str(s) | Value::Html(s) => s.to_string(),
        other => other.to_display_string(),
    }
}

/// `__mail_send(via, to, subject, text, html, reply_to)`.
pub fn send(vm: &mut Vm, a: &[Value]) -> R {
    let via = text(&a[0]);
    let Some(spec) = vm.host.mail.get(&via).cloned() else {
        return Err(vm.throw("PermissionError", format!("phv.toml has no [mail.{}]", crate::httpclient::brief(&via))));
    };
    // Before anything is copied or encoded: the message is built from several
    // copies of its texts (found by the 21st assessment: the size was checked
    // on the finished message, so a text of 110 MB took 841 MB under a 128 MB
    // limit before it was refused).
    let len = |v: &Value| match v {
        Value::Str(s) | Value::Html(s) => s.len(),
        _ => 0,
    };
    let (subject_len, text_len, html_len) = (len(&a[2]), len(&a[3]), len(&a[4]));
    if !fits(subject_len, text_len, html_len) {
        return Err(vm.throw("ValueError", format!("mail: {TOO_LARGE}")));
    }
    // The addresses too, before any is copied, and never shown whole (found
    // by the re-attack of the 21st assessment: a 50 MB reply-to took 317 MB
    // under a 128 MB limit, and a 40 MB recipient came back as a 41 MB
    // exception message).
    let recipients = match &a[1] {
        Value::List(l) => l.as_slice(),
        _ => &[],
    };
    if recipients.len() > MAX_RECIPIENTS {
        return Err(vm.throw("ValueError", format!("mail: a message needs 1 to {MAX_RECIPIENTS} recipients")));
    }
    if recipients.iter().chain([&a[5]]).any(|v| len(v) > MAX_ADDRESS) {
        return Err(vm.throw("ValueError", format!("mail: {LONG_ADDRESS}")));
    }
    if subject_len > MAX_SUBJECT {
        return Err(vm.throw("ValueError", format!("mail: {LONG_SUBJECT}")));
    }
    // The copies made on the way: the texts, their base64, its lines, the
    // parts, the message, and the message as it is sent.
    vm.reserve((subject_len + text_len + html_len).saturating_mul(8).saturating_add(64 << 10))?;
    let to: Vec<String> = match &a[1] {
        Value::List(l) => l.iter().map(text).collect(),
        _ => Vec::new(),
    };
    let reply_to = text(&a[5]);
    let msg = Message { to: &to, subject: &text(&a[2]), text: &text(&a[3]), html: &text(&a[4]), reply_to: &reply_to };
    let data = match build(&spec.from, &msg) {
        Ok(d) => d,
        Err(e) => return Err(vm.throw("ValueError", format!("mail: {e}"))),
    };
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err(vm.throw("NetworkError", format!("mail: the environment variable {var} is not set")));
    };
    let until = crate::deadline::until(vm, Duration::from_secs(30));
    match deliver(&url, &spec, &to, &data, until) {
        Ok(()) => Ok(Value::Null),
        Err(e) => Err(vm.throw("NetworkError", format!("sending mail failed: {e}"))),
    }
}

pub struct Message<'a> {
    pub to: &'a [String],
    pub subject: &'a str,
    pub text: &'a str,
    pub html: &'a str,
    pub reply_to: &'a str,
}

/// The address in `Name <addr>` or `addr`, if it is a plain address.
pub fn address_of(s: &str) -> Option<&str> {
    let addr = match (s.rfind('<'), s.ends_with('>')) {
        (Some(i), true) => &s[i + 1..s.len() - 1],
        (None, false) => s,
        _ => return None,
    };
    let ok = addr.len() <= 254
        && addr.split('@').count() == 2
        && addr.split('@').all(|p| !p.is_empty())
        && addr.bytes().all(|b| b.is_ascii_graphic() && !b"<>,;:\"()[]\\".contains(&b));
    ok.then_some(addr)
}

fn no_breaks(what: &str, v: &str) -> Result<(), String> {
    if v.contains(['\r', '\n', '\0']) {
        return Err(format!("{what} cannot contain line breaks"));
    }
    Ok(())
}

const TOO_LARGE: &str = "the message is larger than 10 MB";

/// The longest address (RFC 5321: 254 bytes).
const MAX_ADDRESS: usize = 254;
const LONG_ADDRESS: &str = "an address is longer than 254 bytes";

/// The longest subject: a header line's 998 characters (RFC 5322).
const MAX_SUBJECT: usize = 998;
const LONG_SUBJECT: &str = "the subject is longer than 998 bytes";

/// The bytes of `n` bytes of text as base64 on lines of 76 characters.
fn encoded_len(n: usize) -> usize {
    let b64 = n.div_ceil(3).saturating_mul(4);
    b64.saturating_add(b64 / 76 * 2)
}

/// Whether a message with a subject and texts of these lengths can be within
/// `MAX_MESSAGE` once encoded: checked before it is built.
fn fits(subject: usize, text: usize, html: usize) -> bool {
    encoded_len(subject).saturating_add(encoded_len(text)).saturating_add(encoded_len(html)) <= MAX_MESSAGE
}

/// `=?UTF-8?B?...?=` words for text that is not plain ASCII, split so no line grows long.
fn header_text(s: &str) -> String {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return s.to_string();
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for ch in s.chars() {
        if chunk.len() + ch.len_utf8() > 45 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(ch);
    }
    words.push(chunk);
    let b64 = base64::engine::general_purpose::STANDARD;
    words.iter().map(|w| format!("=?UTF-8?B?{}?=", b64.encode(w))).collect::<Vec<_>>().join("\r\n ")
}

fn base64_lines(s: &str) -> String {
    let enc = base64::engine::general_purpose::STANDARD.encode(s);
    let mut out = String::with_capacity(enc.len() + enc.len() / 38);
    for (i, ch) in enc.chars().enumerate() {
        if i > 0 && i % 76 == 0 {
            out.push_str("\r\n");
        }
        out.push(ch);
    }
    out
}

/// The message as sent after `DATA` (headers, then a text part, or text and
/// HTML alternatives), lines ending in CRLF.
pub fn build(from: &str, m: &Message) -> Result<String, String> {
    if !fits(m.subject.len(), m.text.len(), m.html.len()) {
        return Err(TOO_LARGE.into());
    }
    if m.to.is_empty() || m.to.len() > MAX_RECIPIENTS {
        return Err(format!("a message needs 1 to {MAX_RECIPIENTS} recipients"));
    }
    // Before any is looked at or shown: a message names an address, never a text of any length.
    if m.to.iter().any(|t| t.len() > MAX_ADDRESS) || m.reply_to.len() > MAX_ADDRESS {
        return Err(LONG_ADDRESS.into());
    }
    if m.subject.len() > MAX_SUBJECT {
        return Err(LONG_SUBJECT.into());
    }
    for t in m.to {
        no_breaks("an address", t)?;
        if address_of(t).is_none_or(|a| a != t) {
            return Err(format!("`{}` is not an email address", crate::httpclient::brief(t)));
        }
    }
    no_breaks("the subject", m.subject)?;
    if !m.reply_to.is_empty() {
        no_breaks("the reply-to address", m.reply_to)?;
        // A bare address: a display name from a form could list more addresses.
        if address_of(m.reply_to).is_none_or(|a| a != m.reply_to) {
            return Err(format!("`{}` is not an email address", crate::httpclient::brief(m.reply_to)));
        }
    }
    let from_addr = address_of(from).ok_or("[mail] from is not an email address")?;
    let domain = from_addr.rsplit('@').next().unwrap_or("localhost");
    let mut h = String::new();
    h.push_str(&format!("From: {from}\r\nTo: {}\r\n", m.to.join(", ")));
    if !m.reply_to.is_empty() {
        h.push_str(&format!("Reply-To: {}\r\n", m.reply_to));
    }
    h.push_str(&format!("Subject: {}\r\n", header_text(m.subject)));
    h.push_str(&format!("Date: {}\r\n", chrono::Utc::now().to_rfc2822()));
    h.push_str(&format!("Message-ID: <{}@{domain}>\r\nMIME-Version: 1.0\r\n", crate::random_hex(16)));
    let part = |ct: &str, body: &str| {
        format!(
            "Content-Type: {ct}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            base64_lines(body)
        )
    };
    if m.html.is_empty() {
        h.push_str(&part("text/plain", m.text));
    } else {
        let b = format!("phv-{}", crate::random_hex(12));
        h.push_str(&format!("Content-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n"));
        h.push_str(&format!(
            "--{b}\r\n{}--{b}\r\n{}--{b}--\r\n",
            part("text/plain", m.text),
            part("text/html", m.html)
        ));
    }
    if h.len() > MAX_MESSAGE {
        return Err(TOO_LARGE.into());
    }
    Ok(h)
}

/// `smtp://` or `smtps://`, user, password, host and port.
struct Target {
    tls: bool,
    user: Option<(String, String)>,
    host: String,
    port: u16,
}

fn parse_url(url: &str) -> Result<Target, String> {
    let (tls, rest) = match url.split_once("://") {
        Some(("smtps", r)) => (true, r),
        Some(("smtp", r)) => (false, r),
        _ => return Err("the URL must start with smtp:// or smtps://".into()),
    };
    let rest = rest.trim_end_matches('/');
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => {
            let (name, pass) = u.split_once(':').unwrap_or((u, ""));
            let dec = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
            (Some((dec(name), dec(pass))), h)
        }
        None => (None, rest),
    };
    let (host, port) = match hostport.rsplit_once(':').filter(|(h, p)| !h.ends_with(':') && !p.contains(']')) {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "the URL has an invalid port".to_string())?),
        None => (hostport, if tls { 465 } else { 587 }),
    };
    if host.is_empty() {
        return Err("the URL has no host".into());
    }
    Ok(Target { tls, user, host: host.to_string(), port })
}

/// One reply: its code and text (the lines of a multi-line reply joined).
fn reply(s: &mut Stream) -> Result<(u16, String), String> {
    let mut text = String::new();
    for _ in 0..MAX_REPLY_LINES {
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        loop {
            match s.read(&mut b) {
                Ok(0) => return Err("the server closed the connection".into()),
                Ok(_) => {}
                Err(e) => return Err(netio::describe(&e)),
            }
            if b[0] == b'\n' {
                break;
            }
            line.push(b[0]);
            if line.len() > MAX_LINE {
                return Err("the server sent a line that is too long".into());
            }
        }
        let line = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
        let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).ok_or("the server sent an invalid reply")?;
        text.push_str(line.get(4..).unwrap_or(""));
        text.push('\n');
        if line.as_bytes().get(3) != Some(&b'-') {
            return Ok((code, text));
        }
    }
    Err("the server sent too long a reply".into())
}

fn command(s: &mut Stream, line: &str, want: &[u16]) -> Result<String, String> {
    s.write_all(format!("{line}\r\n").as_bytes()).map_err(|e| netio::describe(&e))?;
    expect(s, want, line.split(' ').next().unwrap_or(line))
}

fn expect(s: &mut Stream, want: &[u16], what: &str) -> Result<String, String> {
    let (code, text) = reply(s)?;
    if !want.contains(&code) {
        // The server's words, cut short: they never hold the password.
        let first = text.lines().next().unwrap_or("").chars().take(200).collect::<String>();
        return Err(format!("{what}: the server answered {code} {first}"));
    }
    Ok(text)
}

/// Sends `data` to every address of `to` through the server at `url`.
pub fn deliver(url: &str, spec: &MailSpec, to: &[String], data: &str, until: Instant) -> Result<(), String> {
    let t = parse_url(url)?;
    let local = is_local_host(&t.host);
    // TLS is needed for another machine (unless `tls = false`), and always for a password.
    let need_tls = spec.tls.unwrap_or(!local) || (t.user.is_some() && !local);
    let mut s = netio::dial(&t.host, t.port, t.tls, until)?;
    expect(&mut s, &[220], "greeting")?;
    let mut caps = command(&mut s, "EHLO phv", &[250])?;
    if !t.tls {
        if caps.lines().any(|l| l.trim().eq_ignore_ascii_case("STARTTLS")) {
            command(&mut s, "STARTTLS", &[220])?;
            s = s.start_tls(&t.host)?;
            caps = command(&mut s, "EHLO phv", &[250])?;
        } else if need_tls {
            return Err("the server does not offer STARTTLS; use an smtps:// URL, or set tls = false".into());
        }
    }
    if let Some((user, pass)) = &t.user {
        if !caps.lines().any(|l| l.to_ascii_uppercase().starts_with("AUTH") && l.to_ascii_uppercase().contains("PLAIN"))
        {
            return Err("the server does not accept AUTH PLAIN".into());
        }
        let token = base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"));
        command(&mut s, &format!("AUTH PLAIN {token}"), &[235])
            .map_err(|_| "the server refused the user name or password")?;
    }
    let from = address_of(&spec.from).ok_or("[mail] from is not an email address")?;
    command(&mut s, &format!("MAIL FROM:<{from}>"), &[250])?;
    for addr in to {
        command(&mut s, &format!("RCPT TO:<{addr}>"), &[250, 251])?;
    }
    command(&mut s, "DATA", &[354])?;
    let mut body = String::with_capacity(data.len() + 16);
    for line in data.split("\r\n") {
        // Dot-stuffing: a line starting with `.` would otherwise end the message early.
        if line.starts_with('.') {
            body.push('.');
        }
        body.push_str(line);
        body.push_str("\r\n");
    }
    body.push_str(".\r\n");
    s.write_all(body.as_bytes()).map_err(|e| netio::describe(&e))?;
    expect(&mut s, &[250], "DATA")?;
    let _ = command(&mut s, "QUIT", &[221]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    fn spec(from: &str) -> MailSpec {
        MailSpec { url: None, url_env: None, tls: None, from: from.into() }
    }

    #[test]
    fn messages_are_built_safely() {
        let to = vec!["ann@example.com".to_string()];
        let m = Message { to: &to, subject: "Grüße", text: "hi", html: "", reply_to: "" };
        let data = build("App <app@example.com>", &m).unwrap();
        assert!(data.contains("Subject: =?UTF-8?B?"), "{data}");
        assert!(data.contains("Content-Type: text/plain; charset=utf-8"));
        for bad in ["ann@example.com\r\nBcc: x@evil.example", "ann", "a@b, c@d", "<a@b>"] {
            let to = vec![bad.to_string()];
            assert!(build("app@example.com", &Message { to: &to, ..m }).is_err(), "{bad}");
        }
        assert!(build("app@example.com", &Message { subject: "a\r\nBcc: x@y.z", ..m }).is_err());
        let list = "attacker@evil.example, \"x\" <me@example.com>";
        assert!(build("app@example.com", &Message { reply_to: list, ..m }).is_err());
        let html = build("app@example.com", &Message { html: "<b>x</b>", ..m }).unwrap();
        assert!(html.contains("multipart/alternative") && html.contains("text/html"));
    }

    /// A message past the cap is refused by the lengths of its texts, before
    /// any of it is encoded (21st assessment: the finished message was
    /// measured, after four copies of each text had been made).
    #[test]
    fn a_message_past_the_cap_is_refused_before_it_is_built() {
        let to = vec!["ann@example.com".to_string()];
        let big = "a".repeat(64 << 20);
        let m = Message { to: &to, subject: "s", text: &big, html: "", reply_to: "" };
        let before = phv_vm::alloc::thread_allocated();
        let start = std::time::Instant::now();
        assert_eq!(build("app@example.com", &m).unwrap_err(), TOO_LARGE);
        assert_eq!(build("app@example.com", &Message { text: "t", html: &big, ..m }).unwrap_err(), TOO_LARGE);
        assert_eq!(
            build("app@example.com", &Message { text: "t", subject: &big[..999], ..m }).unwrap_err(),
            LONG_SUBJECT
        );
        assert!(build("app@example.com", &Message { text: "t", subject: &big[..998], ..m }).is_ok());
        assert!(phv_vm::alloc::thread_allocated() - before < 1 << 20, "nothing was built");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        // What fits is what the encoded message can hold.
        assert!(fits(0, 7 << 20, 0) && !fits(0, 8 << 20, 0));
        assert!(fits(100, 3 << 20, 4 << 20) && !fits(100, 4 << 20, 4 << 20));
        let m = Message { text: &big[..7 << 20], ..m };
        assert!(build("app@example.com", &m).is_ok_and(|d| d.len() <= MAX_MESSAGE));
        assert_eq!(encoded_len(57), 78);
        // Addresses are measured too, and a message never shows one whole
        // (re-attack of the 21st assessment).
        let long = format!("{}@example.com", "a".repeat(40 << 20));
        let many = vec![long.clone()];
        let m = Message { to: &many, subject: "s", text: "t", html: "", reply_to: "" };
        let before = phv_vm::alloc::thread_allocated();
        assert_eq!(build("app@example.com", &m).unwrap_err(), LONG_ADDRESS);
        assert_eq!(build("app@example.com", &Message { to: &to, reply_to: &long, ..m }).unwrap_err(), LONG_ADDRESS);
        assert!(phv_vm::alloc::thread_allocated() - before < 1 << 20, "nothing was copied");
        let odd = vec![format!("{} b@example.com", "a".repeat(200))];
        let said = build("app@example.com", &Message { to: &odd, ..m }).unwrap_err();
        assert!(said.ends_with("is not an email address") && said.len() < 120, "{said}");
        let fine = vec![format!("{}@example.com", "a".repeat(242))];
        assert!(build("app@example.com", &Message { to: &fine, ..m }).is_ok());
        assert_eq!(encoded_len(58), 82);
    }

    #[test]
    fn urls() {
        let t = parse_url("smtps://me%40x.com:p%3Ass@mail.example.com").unwrap();
        assert!(t.tls && t.port == 465 && t.host == "mail.example.com");
        assert_eq!(t.user, Some(("me@x.com".into(), "p:ss".into())));
        assert_eq!(parse_url("smtp://127.0.0.1:2525").unwrap().port, 2525);
        assert!(parse_url("http://x").is_err());
    }

    /// A local server without TLS: the dialog, and dot-stuffing.
    #[test]
    fn delivers_to_a_local_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            let mut said = Vec::new();
            w.write_all(b"220 hello\r\n").unwrap();
            let mut data = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                said.push(line.trim_end().to_string());
                let answer: &[u8] = match line.trim_end() {
                    l if l.starts_with("EHLO") => b"250-local\r\n250 SIZE 1000000\r\n",
                    "DATA" => {
                        w.write_all(b"354 go\r\n").unwrap();
                        loop {
                            let mut l = String::new();
                            r.read_line(&mut l).unwrap();
                            if l == ".\r\n" {
                                break;
                            }
                            data.push_str(&l);
                        }
                        b"250 queued\r\n"
                    }
                    "QUIT" => {
                        w.write_all(b"221 bye\r\n").unwrap();
                        break;
                    }
                    _ => b"250 ok\r\n",
                };
                w.write_all(answer).unwrap();
            }
            (said, data)
        });
        let to = vec!["ann@example.com".to_string(), "bob@example.com".to_string()];
        let data = "Subject: x\r\n\r\n.hidden line\r\nend";
        deliver(
            &format!("smtp://127.0.0.1:{port}"),
            &spec("App <app@example.com>"),
            &to,
            data,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        let (said, got) = server.join().unwrap();
        assert!(said.contains(&"MAIL FROM:<app@example.com>".to_string()));
        assert!(said.contains(&"RCPT TO:<bob@example.com>".to_string()));
        assert!(got.contains("..hidden line"), "{got}");
    }
}
