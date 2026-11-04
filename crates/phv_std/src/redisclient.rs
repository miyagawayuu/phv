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
                    // Each element takes at least 3 bytes (`:0\r\n` is 4).
                    if len > (self.budget + (self.buf.len() - self.pos)) / 3 {
                        return Err(RedisError::TooLarge);
                    }
                    let mut items = Vec::with_capacity(len.min(1024));
                    for _ in 0..len {
                        match self.reply(depth + 1)? {
                            Ok(r) => items.push(r),
                            Err(m) => return Ok(Err(m)),
                        }
                    }
                    Reply::Array(items)
                }
            }
            _ => return Err(invalid()),
        }))
    }
}

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
}
