//! `phv.toml`: project settings, security options, permissions and dependencies.
//!
//! ```toml
//! [project]
//! name = "blog"
//!
//! [security]
//! csrf = true             # reject form POSTs without a valid token (default)
//! secure_cookies = false  # mark the session cookie `Secure` (use behind HTTPS)
//! session_ttl = 7200      # seconds of inactivity before a session expires
//! memory_limit_mb = 128   # per request
//!
//! [permissions]
//! read = ["content/"]
//! write = ["data/", "storage/"]
//! env = ["APP_*"]
//!
//! [dependencies]
//! utils = { path = "../utils" }
//! markdown = { git = "https://example.com/markdown.git", rev = "v1.2.0" }
//!
//! [databases.main]
//! url_env = "DATABASE_URL"  # postgres://, mysql://, redis:// or rediss://
//!
//! [websockets.game]
//! url_env = "GAME_WS_URL"   # ws:// or wss://
//!
//! [http.weather]
//! url = "https://api.example.com/v1"
//! headers_env = { Authorization = "WEATHER_AUTH" }
//! ```

use phv_vm::{DatabaseSpec, Host, HttpSpec, Permissions, WebSocketSpec};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    project: Project,
    #[serde(default)]
    security: Security,
    #[serde(default)]
    permissions: PermissionsSection,
    #[serde(default)]
    dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    databases: BTreeMap<String, DatabaseSection>,
    #[serde(default)]
    websockets: BTreeMap<String, WebSocketSection>,
    #[serde(default)]
    http: BTreeMap<String, HttpSection>,
}

/// `[databases.<name>]`: a server scripts reach with `Database::connect("<name>")`
/// or `Redis::connect("<name>")`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseSection {
    /// The URL itself (convenient for local development; it may contain a password).
    url: Option<String>,
    /// The environment variable holding the URL (recommended: keeps credentials out of the project).
    url_env: Option<String>,
    /// `false` allows a plain connection to a remote host; by default only localhost may skip TLS.
    tls: Option<bool>,
    /// Connections kept per server process (default 8).
    pool_size: Option<usize>,
}

const DATABASE_SCHEMES: &[&str] = &["postgres://", "postgresql://", "mysql://", "redis://", "rediss://"];

fn database_spec(name: &str, d: DatabaseSection) -> Result<DatabaseSpec, String> {
    if !valid_server_name(name) {
        return Err(format!("[databases]: invalid name \"{name}\" (letters, digits, `_` and `-`)"));
    }
    let url = server_url("databases", name, d.url, &d.url_env)?;
    if let Some(u) = &url {
        if !DATABASE_SCHEMES.iter().any(|s| u.starts_with(s)) {
            let from = d.url_env.as_deref().map(|v| format!(" (from ${v})")).unwrap_or_default();
            return Err(format!(
                "[databases.{name}]: the URL{from} must start with postgres://, mysql://, redis:// or rediss://"
            ));
        }
    }
    let pool_size = d.pool_size.unwrap_or(8);
    if !(1..=256).contains(&pool_size) {
        return Err(format!("[databases.{name}].pool_size must be between 1 and 256"));
    }
    Ok(DatabaseSpec { url, url_env: d.url_env, tls: d.tls, pool_size })
}

/// `[websockets.<name>]`: a server scripts reach with `WebSocket::connect("<name>")`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSocketSection {
    url: Option<String>,
    url_env: Option<String>,
    /// `false` allows `ws://` to a remote host; by default only localhost may skip TLS.
    tls: Option<bool>,
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The URL of a `[databases]`, `[websockets]` or `[http]` entry: `url`, or the value of
/// `url_env`. An unset variable is reported when a script connects, so
/// `phv check` and the editor work without the production credentials.
fn server_url(
    section: &str,
    name: &str,
    url: Option<String>,
    url_env: &Option<String>,
) -> Result<Option<String>, String> {
    match (url, url_env) {
        (Some(u), None) => Ok(Some(u)),
        (None, Some(var)) => Ok(std::env::var(var).ok().filter(|u| !u.is_empty())),
        _ => Err(format!("[{section}.{name}]: give exactly one of `url` or `url_env`")),
    }
}

fn websocket_spec(name: &str, w: WebSocketSection) -> Result<WebSocketSpec, String> {
    if !valid_server_name(name) {
        return Err(format!("[websockets]: invalid name \"{name}\" (letters, digits, `_` and `-`)"));
    }
    let url = server_url("websockets", name, w.url, &w.url_env)?;
    if let Some(u) = &url {
        if !u.starts_with("ws://") && !u.starts_with("wss://") {
            let from = w.url_env.as_deref().map(|v| format!(" (from ${v})")).unwrap_or_default();
            return Err(format!("[websockets.{name}]: the URL{from} must start with ws:// or wss://"));
        }
    }
    Ok(WebSocketSpec { url, url_env: w.url_env, tls: w.tls })
}

/// `[http.<name>]`: a service scripts reach with `HttpClient::connect("<name>")`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpSection {
    url: Option<String>,
    url_env: Option<String>,
    /// `false` allows `http://` to a remote host; by default only localhost may skip TLS.
    tls: Option<bool>,
    /// Headers sent with every request.
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Headers whose values come from environment variables (API keys).
    #[serde(default)]
    headers_env: BTreeMap<String, String>,
}

fn http_spec(name: &str, h: HttpSection) -> Result<HttpSpec, String> {
    if !valid_server_name(name) {
        return Err(format!("[http]: invalid name \"{name}\" (letters, digits, `_` and `-`)"));
    }
    let url = server_url("http", name, h.url, &h.url_env)?;
    if let Some(u) = &url {
        if let Err(e) = phv_std::check_http_url(u) {
            let from = h.url_env.as_deref().map(|v| format!(" (from ${v})")).unwrap_or_default();
            return Err(format!("[http.{name}]: the URL{from}: {e}"));
        }
    }
    let mut headers = Vec::new();
    for (header, value) in h.headers {
        phv_std::check_header_name(&header).map_err(|e| format!("[http.{name}] headers: {e}"))?;
        if value.contains(['\r', '\n', '\0']) {
            return Err(format!("[http.{name}] headers: the value of {header} contains a line break"));
        }
        headers.push((header, Some(value)));
    }
    let mut headers_env = Vec::new();
    for (header, var) in h.headers_env {
        phv_std::check_header_name(&header).map_err(|e| format!("[http.{name}] headers_env: {e}"))?;
        if headers.iter().any(|(n, _): &(String, Option<String>)| n.eq_ignore_ascii_case(&header)) {
            return Err(format!("[http.{name}]: {header} is in both headers and headers_env"));
        }
        // Like `url_env`, an unset variable is reported when a request is sent.
        let value = std::env::var(&var).ok().filter(|v| !v.is_empty() && !v.contains(['\r', '\n', '\0']));
        headers.push((header.clone(), value));
        headers_env.push((header, var));
    }
    Ok(HttpSpec { url, url_env: h.url_env, tls: h.tls, headers, headers_env })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Project {
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Security {
    #[serde(default = "yes")]
    csrf: bool,
    #[serde(default)]
    secure_cookies: bool,
    #[serde(default = "default_ttl")]
    session_ttl: u64,
    #[serde(default = "default_memory")]
    memory_limit_mb: u64,
    /// Content-Security-Policy: "default" (when missing), "off", or a policy
    /// in which `{nonce}` becomes this response's nonce.
    #[serde(default)]
    csp: Option<String>,
    /// Requests per minute accepted from one client address (0 = no limit).
    #[serde(default)]
    requests_per_minute: u32,
    /// "memory" (default) or a directory inside the project for session files,
    /// so sessions survive restarts and are shared by several server processes.
    #[serde(default)]
    session_store: Option<String>,
}

impl Default for Security {
    fn default() -> Self {
        Security {
            csrf: true,
            secure_cookies: false,
            session_ttl: default_ttl(),
            memory_limit_mb: default_memory(),
            csp: None,
            requests_per_minute: 0,
            session_store: None,
        }
    }
}

/// Policy used when phv.toml does not set `csp`. Inline scripts run only
/// with the response's nonce, which PHV adds to every `<script>` tag
/// written in a template; injected markup (via `raw()`) never has it.
pub const DEFAULT_CSP: &str = "default-src 'self'; script-src 'self' {nonce}; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; font-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'";

#[derive(Clone, Debug, PartialEq)]
pub enum Csp {
    Off,
    /// A policy; `{nonce}` is replaced per response.
    Policy(String),
}

impl Csp {
    /// Whether event handler attributes and `javascript:` URLs run under this
    /// policy: no policy, or `'unsafe-inline'` for scripts without a nonce,
    /// hash or `'strict-dynamic'` (any of which makes browsers ignore it).
    pub fn allows_inline_code(&self) -> bool {
        let Csp::Policy(p) = self else { return true };
        let directive = |name: &str| {
            p.split(';').find_map(|d| {
                let mut words = d.split_whitespace();
                words.next()?.eq_ignore_ascii_case(name).then(|| words.map(str::to_ascii_lowercase).collect::<Vec<_>>())
            })
        };
        let sources =
            directive("script-src-attr").or_else(|| directive("script-src")).or_else(|| directive("default-src"));
        match sources {
            None => true,
            Some(s) => {
                s.iter().any(|x| x == "'unsafe-inline'")
                    && !s.iter().any(|x| {
                        x.starts_with("'nonce-") || x == "{nonce}" || x.starts_with("'sha") || x == "'strict-dynamic'"
                    })
            }
        }
    }

    /// Whether responses need a nonce (the policy mentions `{nonce}`).
    pub fn uses_nonce(&self) -> bool {
        matches!(self, Csp::Policy(p) if p.contains("{nonce}"))
    }

    /// The header value for a response with `nonce` (None: no inline scripts allowed).
    pub fn header(&self, nonce: Option<&str>) -> Option<String> {
        match self {
            Csp::Off => None,
            Csp::Policy(p) => {
                let n = nonce.map(|n| format!("'nonce-{n}'")).unwrap_or_default();
                // `'nonce-{nonce}'` (as written in the docs) and `{nonce}` both work.
                Some(
                    p.replace("'nonce-{nonce}'", "{nonce}")
                        .replace("{nonce}", &n)
                        .replace("  ", " ")
                        .replace(" ;", ";"),
                )
            }
        }
    }
}

fn yes() -> bool {
    true
}

fn default_ttl() -> u64 {
    7200
}

fn default_memory() -> u64 {
    128
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PermissionsSection {
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub path: Option<String>,
    pub git: Option<String>,
    /// Tag, branch or commit for git dependencies.
    pub rev: Option<String>,
}

#[derive(Clone)]
pub struct Config {
    pub name: Option<String>,
    pub host: Arc<Host>,
    pub csrf: bool,
    pub secure_cookies: bool,
    pub session_ttl: u64,
    pub memory_limit_mb: u64,
    pub csp: Csp,
    pub requests_per_minute: u32,
    /// Directory for session files; None keeps sessions in memory.
    pub session_dir: Option<PathBuf>,
    pub dependencies: BTreeMap<String, Dependency>,
}

impl Config {
    pub fn default_for(root: &Path) -> Config {
        Config {
            name: None,
            host: Arc::new(Host { root: root.to_path_buf(), ..Host::default() }),
            csrf: true,
            secure_cookies: false,
            session_ttl: default_ttl(),
            memory_limit_mb: default_memory(),
            csp: Csp::Policy(DEFAULT_CSP.into()),
            requests_per_minute: 0,
            session_dir: None,
            dependencies: BTreeMap::new(),
        }
    }
}

fn check_dir(kind: &str, d: &str) -> Result<(), String> {
    let p = Path::new(d);
    // Each name must also be safe on Windows: `public./` would open `public/`.
    let ok = !d.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| match c {
            std::path::Component::Normal(n) => n.to_str().is_some_and(phv_std::safefs::valid_name),
            std::path::Component::CurDir => true,
            _ => false,
        });
    if ok {
        Ok(())
    } else {
        Err(format!("[permissions].{kind}: \"{d}\" must be a directory inside the project (no `..`, not absolute)"))
    }
}

/// The lowercase names of a project-relative directory, for comparing paths.
fn dir_names(d: &str) -> Vec<String> {
    Path::new(d)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect()
}

/// Whether one directory is inside the other (or they are the same).
fn overlaps(a: &[String], b: &[String]) -> bool {
    a.iter().zip(b).all(|(x, y)| x == y)
}

/// Reads `<root>/phv.toml` (all settings are optional).
pub fn load_config(root: &Path) -> Result<Config, String> {
    let path = root.join("phv.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default_for(root)),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let file: File = toml::from_str(&text).map_err(|e| format!("phv.toml: {e}"))?;
    for d in &file.permissions.read {
        check_dir("read", d)?;
    }
    for d in &file.permissions.write {
        check_dir("write", d)?;
        if !Path::new(d).components().any(|c| matches!(c, std::path::Component::Normal(_))) {
            return Err(format!(
                "[permissions].write: \"{d}\" is the project root; scripts could rewrite their own source"
            ));
        }
        let first = Path::new(d).components().find(|c| !matches!(c, std::path::Component::CurDir));
        if first.is_some_and(|c| c.as_os_str().eq_ignore_ascii_case("public")) {
            return Err(format!(
                "[permissions].write: \"{d}\" is inside public/; writable files must not be servable (use e.g. \"storage/\")"
            ));
        }
        // Places whose files are loaded as code: the bytecode cache and other
        // hidden directories (.git hooks), and package sources.
        let names = dir_names(d);
        let code_dir = names.first().is_some_and(|n| n.starts_with('.') || n == "vendor")
            || file
                .dependencies
                .values()
                .filter_map(|dep| dep.path.as_deref())
                .filter(|p| !Path::new(p).components().any(|c| c == std::path::Component::ParentDir))
                .any(|p| overlaps(&names, &dir_names(p)));
        if code_dir {
            return Err(format!(
                "[permissions].write: \"{d}\" holds code (hidden directories, vendor/ or a package); use e.g. \"storage/\""
            ));
        }
    }
    for (name, dep) in &file.dependencies {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.starts_with("std") {
            return Err(format!("[dependencies]: invalid package name \"{name}\""));
        }
        if dep.path.is_some() == dep.git.is_some() {
            return Err(format!("[dependencies].{name}: give exactly one of `path` or `git`"));
        }
        if let Some(p) = &dep.path {
            // Relative only: an absolute or UNC path (\\\\host\\share) would make
            // opening the project contact another machine.
            let path = Path::new(p);
            let relative = !p.starts_with(['/', '\\'])
                && path.components().all(|c| {
                    matches!(
                        c,
                        std::path::Component::Normal(_)
                            | std::path::Component::CurDir
                            | std::path::Component::ParentDir
                    )
                });
            if !relative {
                return Err(format!("[dependencies].{name}: `path` must be a relative path"));
            }
        }
    }
    let csp = match file.security.csp.as_deref().map(str::trim) {
        None | Some("default") => Csp::Policy(DEFAULT_CSP.into()),
        Some("off") | Some("") => Csp::Off,
        Some(p) if p.contains(['\r', '\n']) => return Err("[security].csp must be on one line".into()),
        Some(p) => Csp::Policy(p.to_string()),
    };
    // Session files hold logins: the directory must belong to the server alone
    // (its sweep deletes old files), not to the project or to scripts.
    let session_dir = match file.security.session_store.as_deref() {
        None | Some("memory") => None,
        Some(d) => {
            check_dir("session_store", d).map_err(|e| e.replace("[permissions].", "[security]."))?;
            let names = dir_names(d);
            if names.is_empty() {
                return Err("[security].session_store: use a directory of its own, not the project root".into());
            }
            if names[0] == "public" {
                return Err(format!("[security].session_store: \"{d}\" is inside public/, where files are served"));
            }
            let perms = file.permissions.read.iter().chain(file.permissions.write.iter());
            if let Some(p) = perms.into_iter().find(|p| overlaps(&names, &dir_names(p))) {
                return Err(format!(
                    "[security].session_store: \"{d}\" overlaps [permissions] \"{p}\"; scripts could read or forge sessions"
                ));
            }
            Some(root.join(d))
        }
    };
    if !(1..=31_536_000).contains(&file.security.session_ttl) {
        return Err("[security].session_ttl must be between 1 and 31536000 seconds".into());
    }
    if !(1..=1_048_576).contains(&file.security.memory_limit_mb) {
        return Err("[security].memory_limit_mb must be between 1 and 1048576".into());
    }
    let permissions =
        Permissions { read: file.permissions.read, write: file.permissions.write, env: file.permissions.env };
    let mut databases = BTreeMap::new();
    for (name, d) in file.databases {
        let spec = database_spec(&name, d)?;
        databases.insert(name, spec);
    }
    let mut websockets = BTreeMap::new();
    for (name, w) in file.websockets {
        let spec = websocket_spec(&name, w)?;
        websockets.insert(name, spec);
    }
    let mut http = BTreeMap::new();
    for (name, h) in file.http {
        let spec = http_spec(&name, h)?;
        http.insert(name, spec);
    }
    Ok(Config {
        name: file.project.name,
        host: Arc::new(Host { root: root.to_path_buf(), permissions, databases, websockets, http }),
        csrf: file.security.csrf,
        secure_cookies: file.security.secure_cookies,
        session_ttl: file.security.session_ttl,
        memory_limit_mb: file.security.memory_limit_mb,
        csp,
        requests_per_minute: file.security.requests_per_minute,
        session_dir,
        dependencies: file.dependencies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(toml: &str) -> Result<Config, String> {
        // A directory per call: tests run in parallel.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("phv-config-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("phv.toml"), toml).unwrap();
        let r = load_config(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn refuses_unsafe_directories() {
        // Windows drops trailing dots and spaces: these name public/ or the root.
        for bad in ["public.", "public ", "...", ". ", ".. ", "data:x", "CON"] {
            assert!(load(&format!("[permissions]\nwrite = [\"{bad}/\"]\n")).is_err(), "{bad}");
        }
        // The session store needs a directory of its own.
        for bad in [".", "./", "public/sessions", "data/", "data/sessions", "public./s"] {
            let toml = format!("[security]\nsession_store = \"{bad}\"\n[permissions]\nwrite = [\"data/\"]\n");
            assert!(load(&toml).is_err(), "{bad}");
        }
        let ok = load("[security]\nsession_store = \"sessions/\"\n[permissions]\nwrite = [\"data/\"]\n").unwrap();
        assert!(ok.session_dir.is_some());
        assert!(load("[security]\nsession_ttl = 99999999999\n").is_err());
        assert!(load("[security]\nmemory_limit_mb = 0\n").is_err());
    }

    #[test]
    fn websocket_servers() {
        let c = load(
            "[websockets.game]
url = \"ws://127.0.0.1:28016/secret\"
",
        )
        .unwrap();
        let spec = &c.host.websockets["game"];
        assert_eq!(spec.url.as_deref(), Some("ws://127.0.0.1:28016/secret"));
        assert!(!format!("{spec:?}").contains("secret"));
        assert!(load(
            "[websockets.game]
url = \"http://example.com/\"
"
        )
        .is_err());
        assert!(load(
            "[websockets.game]
url = \"ws://a/\"
url_env = \"X\"
"
        )
        .is_err());
        assert!(load(
            "[websockets.\"a b\"]
url = \"ws://a/\"
"
        )
        .is_err());
        assert!(load(
            "[websockets.game]
url = \"ws://a/\"
pool_size = 2
"
        )
        .is_err());
    }

    #[test]
    fn http_services() {
        std::env::set_var("PHV_TEST_HTTP_KEY", "Bearer secret-token");
        let c = load(
            "[http.api]
url = \"https://api.example.com/v1\"
headers = { \"User-Agent\" = \"site\" }
headers_env = { Authorization = \"PHV_TEST_HTTP_KEY\", X-Missing = \"PHV_TEST_HTTP_UNSET\" }
",
        )
        .unwrap();
        let spec = &c.host.http["api"];
        assert_eq!(spec.url.as_deref(), Some("https://api.example.com/v1"));
        assert!(spec.headers.contains(&("Authorization".into(), Some("Bearer secret-token".into()))));
        assert!(spec.headers.contains(&("X-Missing".into(), None)));
        let shown = format!("{spec:?}");
        assert!(!shown.contains("secret") && !shown.contains("example.com"), "{shown}");
        for bad in [
            "[http.api]\nurl = \"ftp://a/\"\n",
            "[http.api]\nurl = \"https://user:pw@a/\"\n",
            "[http.api]\nurl = \"https://a/\"\nheaders = { Host = \"b\" }\n",
            "[http.api]\nurl = \"https://a/\"\nheaders = { \"Bad Name\" = \"b\" }\n",
            "[http.api]\nurl = \"https://a/\"\nheaders = { X = \"1\" }\nheaders_env = { x = \"V\" }\n",
            "[http.api]\nurl = \"https://a/\"\ntimeout = 3\n",
            "[http.\"a b\"]\nurl = \"https://a/\"\n",
        ] {
            assert!(load(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn inline_code_follows_the_policy() {
        assert!(!Csp::Policy(DEFAULT_CSP.into()).allows_inline_code());
        assert!(Csp::Off.allows_inline_code());
        assert!(Csp::Policy("script-src 'self' 'unsafe-inline'".into()).allows_inline_code());
        assert!(!Csp::Policy("script-src 'self' 'unsafe-inline' {nonce}".into()).allows_inline_code());
        assert!(!Csp::Policy("default-src 'self'".into()).allows_inline_code());
        assert!(Csp::Policy("img-src 'self'".into()).allows_inline_code());
    }

    #[test]
    fn nonce_placeholder_in_both_forms() {
        let n = Some("abc");
        assert_eq!(Csp::Policy("script-src {nonce}".into()).header(n).unwrap(), "script-src 'nonce-abc'");
        assert_eq!(Csp::Policy("script-src 'nonce-{nonce}'".into()).header(n).unwrap(), "script-src 'nonce-abc'");
    }
}
