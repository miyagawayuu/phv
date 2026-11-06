//! A small Redis client (RESP2) for the commands `std.db` allows, over the
//! connections of `netio`. Every read and write stops at the deadline given for
//! the command, and a reply larger than the byte budget is refused, so neither
//! a huge value nor a slow server can outlast the script's limits.

use crate::netio::{check_plain, describe, dial, Stream};
use std::io::{Read, Write};
use std::time::Instant;

/// Deepest nesting of arrays accepted in a reply (our commands return at most one level).
const MAX_DEPTH: usize = 8;

pub(crate) enum Reply {
    Nil,
    Int(i64),
    Simple(String),
    Bulk(Vec<u8>),
    Array(Vec<Reply>),
}

impl Reply {
    /// The bytes it holds on the heap.
    pub(crate) fn heap_size(&self) -> usize {
        match self {
            Reply::Nil | Reply::Int(_) => 0,
            Reply::Simple(s) => s.capacity(),
            Reply::Bulk(b) => b.capacity(),
            Reply::Array(items) => {
                items.capacity() * std::mem::size_of::<Reply>() + items.iter().map(Reply::heap_size).sum::<usize>()
            }
        }
    }
}

/// A message pushed to a subscribed connection.
pub(crate) enum Push {
    /// The data of a `message`.
    Message(Vec<u8>),
    /// A `message` longer than allowed, thrown away.
    TooLarge,
    /// Another kind (`pong`, `subscribe`, ...).
    Other(Vec<u8>),
}

pub(crate) enum RedisError {
    /// The server answered with an error; the connection is still usable.
    Server(String),
    /// The connection failed or is out of step; it cannot be reused.
    Broken(String),
    /// The reply was larger than the budget (the connection cannot be reused).
    TooLarge,
}

pub(crate) struct RedisConn {
    stream: Stream,
    buf: Vec<u8>,
    /// Bytes of `buf` already parsed.
    pos: usize,
    budget: usize,
}

/// `redis://[user[:password]@]host[:port][/db]` (or `rediss://` for TLS).
struct Target {
    tls: bool,
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
    db: u32,
}

fn parse_url(url: &str) -> Result<Target, String> {
    let invalid = || "the URL is not a valid Redis URL".to_string();
    let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8().map(|c| c.into_owned());
    let (tls, rest) = if let Some(r) = url.strip_prefix("rediss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("redis://") {
        (false, r)
    } else {
        return Err(invalid());
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        None => (None, None),
        Some(u) => {
            let (name, pass) = match u.split_once(':') {
                Some((n, p)) => (n, Some(p)),
                None => (u, None),
            };
            let name = decode(name).map_err(|_| invalid())?;
            let pass = pass.map(decode).transpose().map_err(|_| invalid())?;
            match pass {
                // `redis://:secret@host` and `redis://secret@host`: only a password.
                None => (None, Some(name).filter(|n| !n.is_empty())),
                Some(p) => (Some(name).filter(|n| !n.is_empty()), Some(p)),
            }
        }
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && (!h.contains(':') || h.ends_with(']')) => {
            (h, p.parse::<u16>().map_err(|_| invalid())?)
        }
        _ => (hostport, 6379),
    };
    if host.is_empty() {
        return Err(invalid());
    }
    let db = if path.is_empty() { 0 } else { path.parse::<u32>().map_err(|_| invalid())? };
    Ok(Target { tls, host: host.to_string(), port, user, password, db })
}

impl RedisConn {
    /// Connects, authenticates and selects the database of `url`.
    pub(crate) fn connect(url: &str, tls_setting: Option<bool>, until: Instant) -> Result<RedisConn, String> {
        let t = parse_url(url)?;
        check_plain(&t.host, t.tls, tls_setting, "rediss://")?;
        let stream = dial(&t.host, t.port, t.tls, until)?;
        let mut conn = RedisConn { stream, buf: Vec::new(), pos: 0, budget: 1 << 20 };
        let run = |conn: &mut RedisConn, args: &[&[u8]]| match conn.command(args, until, 1 << 20) {
            Ok(_) => Ok(()),
            Err(RedisError::Server(m)) => Err(m),
            Err(RedisError::Broken(m)) => Err(m),
            Err(RedisError::TooLarge) => Err("the server sent an invalid reply".into()),
        };
        if let Some(pass) = &t.password {
            match &t.user {
                Some(user) => run(&mut conn, &[b"AUTH", user.as_bytes(), pass.as_bytes()])?,
                None => run(&mut conn, &[b"AUTH", pass.as_bytes()])?,
            }
        }
        if t.db != 0 {
            run(&mut conn, &[b"SELECT", t.db.to_string().as_bytes()])?;
        }
        Ok(conn)
    }

    /// Sends one command and reads its reply, before `until` and within `budget` bytes.
    pub(crate) fn command(&mut self, args: &[&[u8]], until: Instant, budget: usize) -> Result<Reply, RedisError> {
        self.stream.set_deadline(until);
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a);
            out.extend_from_slice(b"\r\n");
        }
        let io = |e: std::io::Error| RedisError::Broken(describe(&e));
        self.stream.write_all(&out).map_err(io)?;
        self.stream.flush().map_err(io)?;
        self.budget = budget;
        let reply = self.reply(0);
        // Whatever was not consumed belongs to no command: the connection is out of step.
        if reply.is_ok() && self.pos != self.buf.len() {
            return Err(RedisError::Broken("the server sent an unexpected reply".into()));
        }
        self.buf.clear();
        self.pos = 0;
        match reply? {
            Ok(r) => Ok(r),
            Err(message) => Err(RedisError::Server(message)),
        }
    }

    /// Writes one command without reading its reply (for pipelines and the
    /// commands of a subscribed connection).
    pub(crate) fn send(&mut self, args: &[&[u8]], until: Instant) -> Result<(), RedisError> {
        self.stream.set_deadline(until);
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a);
            out.extend_from_slice(b"\r\n");
        }
        let io = |e: std::io::Error| RedisError::Broken(describe(&e));
        self.stream.write_all(&out).map_err(io)?;
        self.stream.flush().map_err(io)
    }

    /// Reads the next reply, before `until` and within `budget` bytes; bytes
    /// past it are kept for the next one (pipelines, pushed messages).
    pub(crate) fn next_reply(&mut self, until: Instant, budget: usize) -> Result<Reply, RedisError> {
        self.stream.set_deadline(until);
        // What is already buffered counts against the budget too.
        self.budget = budget.saturating_sub(self.buf.len() - self.pos);
        let reply = self.reply(0);
        self.buf.drain(..self.pos);
        self.pos = 0;
        match reply? {
            Ok(r) => Ok(r),
            Err(message) => Err(RedisError::Server(message)),
        }
    }

    /// Reads one message pushed to a subscribed connection (`*N` of bulk
    /// strings), before `until`. Its last element longer than `max` bytes is
    /// read and thrown away as it arrives, never held: the connection stays usable.
    pub(crate) fn next_push(&mut self, until: Instant, max: usize) -> Result<Push, RedisError> {
        let invalid = || RedisError::Broken("the server sent an invalid reply".into());
        self.stream.set_deadline(until);
        // What is held is bounded by the lines (`short_line`), the elements
        // (`max`, 1 KB) and one read ahead: the reads need no budget of their own.
        self.budget = usize::MAX;
        let head = self.short_line()?;
        let n = head.strip_prefix('*').map(Self::length).transpose()?.ok_or_else(invalid)?;
        if !(1..=4).contains(&n) {
            return Err(invalid());
        }
        let mut items: Vec<Vec<u8>> = Vec::new();
        let mut too_large = false;
        for i in 0..n {
            let line = self.short_line()?;
            let len = line.strip_prefix('$').map(Self::length).transpose()?.ok_or_else(invalid)?;
            if len < 0 {
                return Err(invalid());
            }
            let len = len as usize;
            let limit = if i + 1 == n { max } else { 1024 };
            if len > limit {
                if i + 1 != n {
                    return Err(invalid());
                }
                self.skip(len + 2)?;
                too_large = true;
                break;
            }
            while self.buf.len() - self.pos < len + 2 {
                self.fill()?;
            }
            if &self.buf[self.pos + len..self.pos + len + 2] != b"\r\n" {
                return Err(invalid());
            }
            items.push(self.buf[self.pos..self.pos + len].to_vec());
            self.pos += len + 2;
        }
        self.buf.drain(..self.pos);
        self.pos = 0;
        let kind = items.first().cloned().unwrap_or_default();
        Ok(match (kind.as_slice(), too_large) {
            (b"message", true) => Push::TooLarge,
            (b"message", false) if items.len() == 3 => Push::Message(items.pop().unwrap_or_default()),
            _ if too_large => return Err(invalid()),
            _ => Push::Other(kind),
        })
    }

    /// A line of at most 64 bytes (the header of a pushed message, or of one of its elements).
    fn short_line(&mut self) -> Result<String, RedisError> {
        loop {
            if let Some(i) = self.buf[self.pos..].windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[self.pos..self.pos + i]).into_owned();
                self.pos += i + 2;
                return Ok(line);
            }
            if self.buf.len() - self.pos > 64 {
                return Err(RedisError::Broken("the server sent an invalid reply".into()));
            }
            self.fill()?;
        }
    }

    /// Reads `n` bytes and throws them away, a chunk at a time.
    fn skip(&mut self, mut n: usize) -> Result<(), RedisError> {
        loop {
            let have = (self.buf.len() - self.pos).min(n);
            self.pos += have;
            n -= have;
            if n == 0 {
                return Ok(());
            }
            self.buf.clear();
            self.pos = 0;
            let mut chunk = [0u8; 16 << 10];
            let got = loop {
                match self.stream.read(&mut chunk[..n.min(16 << 10)]) {
                    Ok(0) => return Err(RedisError::Broken("the server closed the connection".into())),
                    Ok(k) => break k,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(RedisError::Broken(describe(&e))),
                }
            };
            n -= got;
            if n == 0 {
                return Ok(());
            }
        }
    }

    /// Whether bytes arrive before `until` (`false` when none did: the
    /// connection is still usable). At most `budget` bytes are read.
    pub(crate) fn wait_data(&mut self, until: Instant, budget: usize) -> Result<bool, RedisError> {
        if self.pos < self.buf.len() {
            return Ok(true);
        }
        self.buf.clear();
        self.pos = 0;
        self.stream.set_deadline(until);
        let mut chunk = [0u8; 16 << 10];
        loop {
            match self.stream.read(&mut chunk[..budget.clamp(1, 16 << 10)]) {
                Ok(0) => return Err(RedisError::Broken("the server closed the connection".into())),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Ok(true);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok(false),
                Err(e) => return Err(RedisError::Broken(describe(&e))),
            }
        }
    }

    /// Reads more bytes into the buffer, counting them against the budget.
    fn fill(&mut self) -> Result<(), RedisError> {
        let mut chunk = [0u8; 16 << 10];
        let n = loop {
            match self.stream.read(&mut chunk) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(RedisError::Broken(describe(&e))),
            }
        };
        if n == 0 {
            return Err(RedisError::Broken("the server closed the connection".into()));
        }
        if n > self.budget {
            return Err(RedisError::TooLarge);
        }
        self.budget -= n;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn line(&mut self) -> Result<String, RedisError> {
        loop {
            if let Some(i) = self.buf[self.pos..].windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[self.pos..self.pos + i]).into_owned();
                self.pos += i + 2;
                return Ok(line);
            }
            self.fill()?;
        }
    }

    fn length(line: &str) -> Result<i64, RedisError> {
        let ok =
            !line.is_empty() && line.len() <= 20 && line.trim_start_matches('-').bytes().all(|b| b.is_ascii_digit());
        line.parse::<i64>()
            .ok()
            .filter(|_| ok)
            .ok_or_else(|| RedisError::Broken("the server sent an invalid reply".into()))
    }

    /// One reply: `Ok(Err(message))` for an error reply.
    fn reply(&mut self, depth: usize) -> Result<Result<Reply, String>, RedisError> {
        let invalid = || RedisError::Broken("the server sent an invalid reply".into());
        let line = self.line()?;
        let (kind, rest) = line.split_at(line.len().min(1));
        Ok(Ok(match kind {
            "+" => Reply::Simple(rest.to_string()),
            "-" => {
                // `ERR value is not an integer` → `ResponseError: value is not an integer`.
                let message = match rest.split_once(' ') {
                    Some(("ERR", m)) => format!("ResponseError: {m}"),
                    Some((code, m)) => format!("{code}: {m}"),
                    None => format!("ResponseError: {rest}"),
                };
                return Ok(Err(message));
            }
            ":" => Reply::Int(Self::length(rest)?),
            "$" => {
                let len = Self::length(rest)?;
                if len < 0 {
                    Reply::Nil
                } else {
                    let len = len as usize;
                    if len > self.budget + (self.buf.len() - self.pos) {
                        return Err(RedisError::TooLarge);
                    }
                    while self.buf.len() - self.pos < len + 2 {
                        self.fill()?;
                    }
                    if &self.buf[self.pos + len..self.pos + len + 2] != b"\r\n" {
                        return Err(invalid());
                    }
                    let data = self.buf[self.pos..self.pos + len].to_vec();
                    self.pos += len + 2;
                    Reply::Bulk(data)
                }
            }
            "*" => {
                let len = Self::length(rest)?;
                if len < 0 {
                    Reply::Nil
                } else {
                    if depth >= MAX_DEPTH {
                        return Err(invalid());
                    }
                    let len = len as usize;
                    // Each element takes at least 3 bytes (`:0\r\n` is 4), and in
                    // memory, as a reply and then a value, about `ELEMENT` bytes:
                    // charged to the budget before the list is built (found by
                    // the 1.0 audit: 9 million empty strings took 891 MB).
                    if len > (self.budget + (self.buf.len() - self.pos)) / 3 {
                        return Err(RedisError::TooLarge);
                    }
                    let cost = len.saturating_mul(ELEMENT);
                    if cost > self.budget {
                        return Err(RedisError::TooLarge);
                    }
                    self.budget -= cost;
                    let mut items = Vec::with_capacity(len.min(1024));
                    // An error element (`EXEC`, `EVAL`) is the reply's error,
                    // but the elements after it are read too: left unread,
                    // they would be taken for the next command's reply on a
                    // pooled connection (found by the 1.1.1 audit).
                    let mut error = None;
                    for _ in 0..len {
                        match self.reply(depth + 1)? {
                            Ok(r) if error.is_none() => items.push(r),
                            Ok(_) => {}
                            Err(m) => {
                                error.get_or_insert(m);
                                items = Vec::new();
                            }
                        }
                    }
                    if let Some(m) = error {
                        return Ok(Err(m));
                    }
                    Reply::Array(items)
                }
            }
            _ => return Err(invalid()),
        }))
    }
}

/// What one element of an array reply costs in memory, beyond its bytes.
const ELEMENT: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let t = parse_url("rediss://user:p%40ss@cache.example:6380/2").unwrap();
        assert!(t.tls && t.host == "cache.example" && t.port == 6380 && t.db == 2);
        assert_eq!((t.user.as_deref(), t.password.as_deref()), (Some("user"), Some("p@ss")));
        let t = parse_url("redis://:secret@127.0.0.1").unwrap();
        assert_eq!((t.user, t.password.as_deref(), t.port, t.db), (None, Some("secret"), 6379, 0));
        let t = parse_url("redis://[::1]:7000").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("[::1]", 7000));
        for bad in ["http://a", "redis://", "redis://a:x", "redis://a/x"] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    /// An error inside an array reply is the command's error, and the rest of
    /// the array is read with it: the next command gets its own reply (found by
    /// the 1.1.1 audit: the elements after the error were left for it).
    #[test]
    fn an_error_in_an_array_reply_reads_the_whole_reply() {
        use std::io::{BufRead as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = std::io::BufReader::new(s.try_clone().unwrap());
            let mut s = s;
            // Each command is `*1\r\n$N\r\nNAME\r\n`: three lines.
            let mut command = || {
                let mut line = String::new();
                for _ in 0..3 {
                    line.clear();
                    r.read_line(&mut line).unwrap();
                }
                line.trim().to_string()
            };
            assert_eq!(command(), "EXEC");
            // The elements after the error come a moment later.
            s.write_all(b"*4\r\n:1\r\n-ERR first\r\n").unwrap();
            s.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            s.write_all(b"*2\r\n-WRONGTYPE second\r\n:2\r\n:3\r\n").unwrap();
            assert_eq!(command(), "PING");
            s.write_all(b"+PONG\r\n").unwrap();
            assert_eq!(command(), "PING");
            s.write_all(b"*3\r\n-ERR late\r\n").unwrap();
            s.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            s.write_all(b":1\r\n:2\r\n+PONG\r\n").unwrap();
        });
        let until = || Instant::now() + std::time::Duration::from_secs(5);
        let mut c = RedisConn::connect(&format!("redis://127.0.0.1:{port}"), None, until()).unwrap();
        let budget = 1 << 20;
        assert!(
            matches!(c.command(&[b"EXEC"], until(), budget), Err(RedisError::Server(m)) if m == "ResponseError: first")
        );
        assert!(matches!(c.command(&[b"PING"], until(), budget), Ok(Reply::Simple(m)) if m == "PONG"));
        // Pipelined: each reply is read whole, the next one is the next command's.
        assert!(c.send(&[b"PING"], until()).is_ok());
        assert!(matches!(c.next_reply(until(), budget), Err(RedisError::Server(m)) if m == "ResponseError: late"));
        assert!(matches!(c.next_reply(until(), budget), Ok(Reply::Simple(m)) if m == "PONG"));
        server.join().unwrap();
    }

    /// A subscribed connection reads pushed messages; one longer than allowed
    /// is thrown away as it arrives and the connection goes on; a malformed
    /// one breaks it.
    #[test]
    fn pushed_messages_are_read_or_thrown_away() {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let msg = |data: &[u8]| {
                let mut m = format!("*3\r\n$7\r\nmessage\r\n$2\r\nch\r\n${}\r\n", data.len()).into_bytes();
                m.extend_from_slice(data);
                m.extend_from_slice(b"\r\n");
                m
            };
            s.write_all(&msg(b"hello")).unwrap();
            s.write_all(&msg(&vec![b'x'; 200_000])).unwrap();
            s.write_all(b"*2\r\n$4\r\npong\r\n$0\r\n\r\n").unwrap();
            s.write_all(&msg(b"after")).unwrap();
            s.write_all(b"*3\r\n$7\r\nmessage\r\n$2000\r\n").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(500));
        });
        let until = || Instant::now() + std::time::Duration::from_secs(5);
        let mut c = RedisConn::connect(&format!("redis://127.0.0.1:{port}"), None, until()).unwrap();
        let max = 1000;
        assert!(matches!(c.next_push(until(), max), Ok(Push::Message(m)) if m == b"hello"));
        assert!(matches!(c.next_push(until(), max), Ok(Push::TooLarge)));
        assert!(c.buf.len() <= 64 << 10, "a large message is not held: {}", c.buf.len());
        assert!(matches!(c.next_push(until(), max), Ok(Push::Other(k)) if k == b"pong"));
        assert!(matches!(c.next_push(until(), max), Ok(Push::Message(m)) if m == b"after"));
        // A channel name longer than 1 KB is not a message of ours.
        assert!(matches!(c.next_push(until(), max), Err(RedisError::Broken(_))));
        server.join().unwrap();
    }
}
