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
//! rate_limit_store = "cache"  # count rate limits in this Redis database (default: in the process)
//! secret_env = "PHV_SECRET"   # the server's secret (32+ bytes), shared by its processes (CSRF tokens)
//! store_namespace = "blog"    # the site's name in shared stores (default: [project] name)
//!
//! [server]
//! workers = 16            # scripts running at once (default: 2 per CPU, at least 8)
//! streams_per_address = 4 # pages streaming with flush() at once from one address (default: workers / 4, at least 2; 0: no limit)
//! static_max_age = 3600   # Cache-Control max-age of static files (default: revalidate each time)
//! compress = true         # gzip text responses for clients that accept it (default)
//! access_log = "logs/access.log"  # also write the access log to this file
//! access_log_format = "json"      # "text" (default) or "json", one request per line
//! shutdown_timeout = 30   # seconds to finish running requests on Ctrl-C / SIGTERM
//! tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"  # serve HTTPS (and HTTP/2)
//! tls_key = "/etc/letsencrypt/live/example.com/privkey.pem"
//! h2c = false             # also accept HTTP/2 without TLS (behind a proxy that speaks it)
//!
//! [cors]
//! origins = ["https://app.example.com"]  # or ["*"]
//! methods = ["GET", "POST"]
//! headers = ["Content-Type"]
//! credentials = false     # allow cookies (not with "*")
//! max_age = 600           # seconds browsers may cache a preflight answer
//!
//! [permissions]
//! read = ["content/"]
//! write = ["data/", "storage/"]
//! env = ["APP_*"]
//!
//! [dependencies]
//! slugify = "^1.2"                                          # from the package registry
//! utils = { path = "../utils" }
//! markdown = { git = "https://example.com/markdown.git", rev = "v1.2.0" }
//! charts = { git = "https://example.com/charts.git", version = "~0.3" }
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
//!
//! [oauth.google]
//! issuer = "https://accounts.google.com"   # OpenID Connect (or authorize_url and token_url)
//! client_id_env = "GOOGLE_CLIENT_ID"
//! client_secret_env = "GOOGLE_CLIENT_SECRET"
//! redirect_uri = "https://example.com/auth/google"
//! scopes = ["openid", "email"]
//!
//! [telemetry]
//! endpoint = "http://localhost:4318"   # an OpenTelemetry collector (OTLP/HTTP)
//! ```

use phv_vm::{DatabaseSpec, Host, HttpSpec, MailSpec, OAuthSpec, Permissions, WebSocketSpec};
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
    server: ServerSection,
    #[serde(default)]
    cors: Option<CorsSection>,
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
    #[serde(default)]
    mail: BTreeMap<String, MailSection>,
    #[serde(default)]
    oauth: BTreeMap<String, OAuthSection>,
    #[serde(default)]
    fmt: FmtSection,
    #[serde(default)]
    admin: Option<AdminSection>,
    #[serde(default)]
    jobs: Option<JobsSection>,
    #[serde(default)]
    schedule: Vec<ScheduleSection>,
    #[serde(default)]
    telemetry: Option<TelemetrySection>,
    #[serde(default)]
    i18n: Option<I18nSection>,
}

/// `[i18n]`: the locales of `std.i18n`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct I18nSection {
    default: String,
    locales: Vec<String>,
    /// The directory of the catalogs (default "lang").
    #[serde(default)]
    dir: Option<String>,
}

/// `[i18n]`, checked: the locales, the default one among them, and the
/// directory of their catalogs (`<dir>/<locale>.toml`), relative to the project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I18n {
    pub default: String,
    pub locales: Vec<String>,
    pub dir: String,
}

/// `[i18n]`, checked.
fn i18n_config(s: I18nSection) -> Result<I18n, String> {
    use phv_std::i18n::{valid_locale, MAX_LOCALES};
    if s.locales.is_empty() || s.locales.len() > MAX_LOCALES {
        return Err(format!("[i18n].locales: list 1 to {MAX_LOCALES} locales"));
    }
    for (i, l) in s.locales.iter().enumerate() {
        // A locale names its catalog's file: `con` would name a device on Windows.
        if !valid_locale(l) || !phv_std::safefs::valid_name(&format!("{l}.toml")) {
            return Err(format!(
                "[i18n].locales: \"{l}\" is not a locale like \"en\", \"pt-BR\" or \"zh-Hant\" \
                 (a language in lowercase, then a script or region)"
            ));
        }
        if s.locales[..i].iter().any(|o| o.eq_ignore_ascii_case(l)) {
            return Err(format!("[i18n].locales: \"{l}\" is listed twice"));
        }
    }
    if !s.locales.contains(&s.default) {
        return Err(format!("[i18n].default: \"{}\" must be one of [i18n].locales", s.default));
    }
    let dir = s.dir.unwrap_or_else(|| "lang".into());
    let p = Path::new(&dir);
    let ok = !dir.contains('~')
        && !p.is_absolute()
        && p.components().all(|c| match c {
            std::path::Component::Normal(n) => n.to_str().is_some_and(phv_std::safefs::valid_name),
            std::path::Component::CurDir => true,
            _ => false,
        })
        && p.components().any(|c| matches!(c, std::path::Component::Normal(_)));
    if !ok {
        return Err(format!(
            "[i18n].dir: \"{dir}\" must be a directory inside the project (no `..`, not absolute, no `~`)"
        ));
    }
    Ok(I18n { default: s.default, locales: s.locales, dir })
}

/// `[telemetry]`: traces sent to an OpenTelemetry collector.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetrySection {
    endpoint: Option<String>,
    endpoint_env: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    headers_env: BTreeMap<String, String>,
    service_name: Option<String>,
    sample: Option<f64>,
    tls: Option<bool>,
}

/// `[telemetry]`, checked.
#[derive(Clone)]
pub struct Telemetry {
    /// Where spans are sent (`<endpoint>/v1/traces`); None when `endpoint_env`
    /// names a variable that is not set (traces are then not sent).
    pub url: Option<String>,
    pub endpoint_env: Option<String>,
    /// Headers sent with each export (from `headers_env`: None when unset).
    pub headers: Vec<(String, Option<String>)>,
    pub service_name: String,
    /// The share of new traces recorded, 0 to 1.
    pub sample: f64,
    pub tls: Option<bool>,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("endpoint_env", &self.endpoint_env)
            .field("service_name", &self.service_name)
            .field("sample", &self.sample)
            .finish_non_exhaustive()
    }
}

fn telemetry(t: TelemetrySection, project: Option<&str>) -> Result<Telemetry, String> {
    let endpoint = value_or_env("telemetry", "endpoint", t.endpoint, &t.endpoint_env)?;
    let url = match endpoint {
        Some(e) => {
            let from = t.endpoint_env.as_deref().map(|v| format!(" (from ${v})")).unwrap_or_default();
            phv_std::check_oauth_url(&e, t.tls).map_err(|err| format!("[telemetry] endpoint{from}: {err}"))?;
            if e.contains('?') || e.contains('#') {
                return Err(format!("[telemetry] endpoint{from}: no query or fragment"));
            }
            let e = e.trim_end_matches('/');
            Some(if e.ends_with("/v1/traces") { e.to_string() } else { format!("{e}/v1/traces") })
        }
        None => None,
    };
    let mut headers = Vec::new();
    for (h, v) in t.headers {
        phv_std::check_header_name(&h).map_err(|e| format!("[telemetry] headers: {e}"))?;
        phv_std::check_header_value(&h, &v).map_err(|e| format!("[telemetry] headers: {e}"))?;
        headers.push((h, Some(v)));
    }
    for (h, var) in t.headers_env {
        phv_std::check_header_name(&h).map_err(|e| format!("[telemetry] headers_env: {e}"))?;
        if headers.iter().any(|(n, _): &(String, Option<String>)| n.eq_ignore_ascii_case(&h)) {
            return Err(format!("[telemetry]: {h} is in both headers and headers_env"));
        }
        let value = std::env::var(&var).ok().filter(|v| !v.is_empty() && phv_std::check_header_value(&h, v).is_ok());
        headers.push((h, value));
    }
    let service_name = t.service_name.or_else(|| project.map(str::to_string)).unwrap_or_else(|| "phv".into());
    if service_name.is_empty() || service_name.len() > 128 || service_name.chars().any(|c| c.is_control()) {
        return Err("[telemetry] service_name: 1 to 128 characters, no control characters".into());
    }
    let sample = t.sample.unwrap_or(1.0);
    if !(0.0..=1.0).contains(&sample) {
        return Err("[telemetry] sample: a share from 0 to 1".into());
    }
    Ok(Telemetry { url, endpoint_env: t.endpoint_env, headers, service_name, sample, tls: t.tls })
}

/// `[jobs]`: background jobs, run by `phv serve` and `phv jobs run`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobsSection {
    /// "memory" (default) or "db:<name>" (PostgreSQL, MySQL or Redis).
    #[serde(default)]
    store: Option<String>,
    /// The directory of the job scripts (default "jobs").
    #[serde(default)]
    dir: Option<String>,
    /// Jobs one process runs at once (default 2).
    #[serde(default)]
    workers: Option<usize>,
    /// Seconds a job may run (default 300).
    #[serde(default)]
    time_limit: Option<u64>,
    /// Runs before a failing job is given up (default 5).
    #[serde(default)]
    max_attempts: Option<u32>,
    /// Seconds before a failed job runs again, doubled after each run (default 30).
    #[serde(default)]
    retry_delay: Option<u64>,
    /// `phv serve` runs jobs too (default true).
    #[serde(default)]
    run: Option<bool>,
}

/// `[[schedule]]`: a job added at the times of a cron expression.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleSection {
    job: String,
    cron: String,
    #[serde(default)]
    timezone: Option<String>,
}

/// `[jobs]`, checked.
#[derive(Clone, Debug)]
pub struct Jobs {
    /// A database of `[databases]`; None keeps jobs in memory.
    pub store: Option<String>,
    /// The directory of the job scripts, relative to the project.
    pub dir: String,
    pub workers: usize,
    pub time_limit: u64,
    pub max_attempts: u32,
    /// Seconds before the first retry (doubled each time, at most an hour).
    pub retry_delay: u64,
    pub run: bool,
}

/// `[[schedule]]`, checked.
#[derive(Clone, Debug)]
pub struct Schedule {
    pub job: String,
    pub cron: phv_std::cron::Cron,
}

/// Whether `name` can name a job: lowercase letters, digits and `_`, at most 64.
pub fn job_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The jobs of a project: the `.phv` files directly in `dir` with a name a
/// job can have (sorted). None when the directory does not exist.
pub fn scan_jobs(root: &Path, dir: &str) -> Option<Vec<String>> {
    // A link or junction is not followed: the jobs are the project's own
    // (found by the 1.1 audit). Nor are more than `MAX_JOB_ENTRIES` entries read.
    if jobs_dir_is_link(root, dir) {
        return Some(Vec::new());
    }
    let entries = std::fs::read_dir(root.join(dir)).ok()?;
    let mut names: Vec<String> = entries
        .take(MAX_JOB_ENTRIES)
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".phv")).map(str::to_string))
        .filter(|n| job_name(n))
        .collect();
    names.sort();
    Some(names)
}

/// The entries of `[jobs] dir` read for job names.
const MAX_JOB_ENTRIES: usize = 10_000;

/// Whether `[jobs] dir` is a link or junction (whose jobs are not run).
pub fn jobs_dir_is_link(root: &Path, dir: &str) -> bool {
    // Every directory on the way, not only the last (found by the 1.1 audit:
    // `work` of `work/jobs` could be a junction).
    let mut at = root.to_path_buf();
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        at.push(part);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink() || reparse_point(&m)) {
            return true;
        }
    }
    false
}

#[cfg(windows)]
fn reparse_point(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

/// `[admin]`: the admin port, with `/health`, `/ready` and `/metrics`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminSection {
    /// The address it listens on (`127.0.0.1:9100`).
    listen: String,
    /// An environment variable holding the token `/metrics` asks for
    /// (`Authorization: Bearer ...`); needed unless `listen` is a loopback address.
    #[serde(default)]
    token_env: Option<String>,
}

/// `[admin]`, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admin {
    pub listen: std::net::SocketAddr,
    pub token_env: Option<String>,
}

/// `[mail.<name>]`: a mail server scripts send through with `std.mail`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailSection {
    url: Option<String>,
    url_env: Option<String>,
    /// `false` allows a server on another machine without TLS.
    tls: Option<bool>,
    from: String,
}

fn mail_spec(name: &str, m: MailSection) -> Result<MailSpec, String> {
    if !valid_server_name(name) {
        return Err(format!("[mail]: invalid name \"{name}\" (letters, digits, `_` and `-`)"));
    }
    let url = server_url("mail", name, m.url, &m.url_env)?;
    if let Some(u) = &url {
        if !u.starts_with("smtp://") && !u.starts_with("smtps://") {
            let from = m.url_env.as_deref().map(|v| format!(" (from ${v})")).unwrap_or_default();
            return Err(format!("[mail.{name}]: the URL{from} must start with smtp:// or smtps://"));
        }
    }
    if m.from.contains(['\r', '\n']) || phv_std::mail_address(&m.from).is_none() {
        return Err(format!("[mail.{name}].from must be an address, or `Name <address>`"));
    }
    Ok(MailSpec { url, url_env: m.url_env, tls: m.tls, from: m.from })
}

/// `[fmt]`: settings for `phv fmt`.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FmtSection {
    /// Lines longer than this many characters are wrapped (default 100).
    #[serde(default = "default_fmt_width")]
    pub width: usize,
}

impl Default for FmtSection {
    fn default() -> Self {
        FmtSection { width: default_fmt_width() }
    }
}

fn default_fmt_width() -> usize {
    phv_syntax::format::DEFAULT_WIDTH
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

/// `[oauth.<name>]`: a sign-in provider of std.oauth.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthSection {
    issuer: Option<String>,
    authorize_url: Option<String>,
    token_url: Option<String>,
    client_id: Option<String>,
    client_id_env: Option<String>,
    client_secret_env: Option<String>,
    redirect_uri: Option<String>,
    redirect_uri_env: Option<String>,
    scopes: Option<Vec<String>>,
    /// "basic" or "post".
    client_auth: Option<String>,
    tls: Option<bool>,
}

/// One of `value` or the value of the variable `env` (None when it is not set).
fn value_or_env(
    section: &str,
    key: &str,
    value: Option<String>,
    env: &Option<String>,
) -> Result<Option<String>, String> {
    match (value, env) {
        (Some(v), None) => Ok(Some(v)),
        (None, Some(var)) => Ok(std::env::var(var).ok().filter(|v| !v.is_empty())),
        _ => Err(format!("[{section}]: give exactly one of `{key}` or `{key}_env`")),
    }
}

fn oauth_spec(name: &str, o: OAuthSection) -> Result<OAuthSpec, String> {
    if !valid_server_name(name) {
        return Err(format!("[oauth]: invalid name \"{name}\" (letters, digits, `_` and `-`)"));
    }
    let section = format!("oauth.{name}");
    let url = |key: &str, u: &str| -> Result<(), String> {
        phv_std::check_oauth_url(u, o.tls).map_err(|e| format!("[{section}] {key}: {e}"))
    };
    match (&o.issuer, &o.authorize_url, &o.token_url) {
        (Some(i), None, None) => {
            url("issuer", i)?;
            if i.contains('?') || i.contains('#') {
                return Err(format!("[{section}] issuer: an issuer has no query or fragment"));
            }
        }
        (None, Some(a), Some(t)) => {
            url("authorize_url", a)?;
            url("token_url", t)?;
        }
        _ => {
            return Err(format!(
                "[{section}]: give `issuer` (OpenID Connect), or `authorize_url` and `token_url` (OAuth 2.0), not both"
            ))
        }
    }
    let client_id = value_or_env(&section, "client_id", o.client_id, &o.client_id_env)?;
    let client_secret = o.client_secret_env.as_ref().and_then(|v| std::env::var(v).ok()).filter(|v| !v.is_empty());
    let redirect_uri = value_or_env(&section, "redirect_uri", o.redirect_uri, &o.redirect_uri_env)?;
    if let Some(r) = &redirect_uri {
        let from = o.redirect_uri_env.as_deref().map(|v| format!(" (from ${v})")).unwrap_or_default();
        phv_std::check_oauth_url(r, None).map_err(|e| format!("[{section}] redirect_uri{from}: {e}"))?;
        if r.contains('#') {
            return Err(format!("[{section}] redirect_uri{from}: a redirect URI has no fragment"));
        }
    }
    let oidc = o.issuer.is_some();
    let scopes = o.scopes.unwrap_or_else(|| if oidc { vec!["openid".to_string()] } else { Vec::new() });
    for s in &scopes {
        let ok =
            !s.is_empty() && s.bytes().all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b));
        if !ok {
            return Err(format!("[{section}] scopes: invalid scope \"{}\"", s.escape_debug()));
        }
    }
    if oidc && !scopes.iter().any(|s| s == "openid") {
        return Err(format!("[{section}] scopes: OpenID Connect needs the \"openid\" scope"));
    }
    let secret_in_body = match o.client_auth.as_deref() {
        None => None,
        Some("basic") => Some(false),
        Some("post") => Some(true),
        Some(other) => {
            return Err(format!("[{section}] client_auth: \"{}\" is not \"basic\" or \"post\"", other.escape_debug()))
        }
    };
    if secret_in_body.is_some() && o.client_secret_env.is_none() {
        return Err(format!("[{section}] client_auth: there is no client_secret_env to send"));
    }
    Ok(OAuthSpec {
        issuer: o.issuer,
        authorize_url: o.authorize_url,
        token_url: o.token_url,
        client_id,
        client_id_env: o.client_id_env,
        client_secret,
        client_secret_env: o.client_secret_env,
        redirect_uri,
        redirect_uri_env: o.redirect_uri_env,
        scopes,
        secret_in_body,
        tls: o.tls,
    })
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
        phv_std::check_header_value(&header, &value).map_err(|e| format!("[http.{name}] headers: {e}"))?;
        headers.push((header, Some(value)));
    }
    let mut headers_env = Vec::new();
    for (header, var) in h.headers_env {
        phv_std::check_header_name(&header).map_err(|e| format!("[http.{name}] headers_env: {e}"))?;
        if headers.iter().any(|(n, _): &(String, Option<String>)| n.eq_ignore_ascii_case(&header)) {
            return Err(format!("[http.{name}]: {header} is in both headers and headers_env"));
        }
        // Like `url_env`, an unset variable is reported when a request is sent.
        let value =
            std::env::var(&var).ok().filter(|v| !v.is_empty() && phv_std::check_header_value(&header, v).is_ok());
        headers.push((header.clone(), value));
        headers_env.push((header, var));
    }
    Ok(HttpSpec { url, url_env: h.url_env, tls: h.tls, headers, headers_env })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Project {
    name: Option<String>,
    /// The versions of PHV the project works with, e.g. `"^1.2"`.
    phv: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ServerSection {
    /// Scripts that `phv serve` runs at once; further requests wait for one.
    workers: Option<usize>,
    /// Pages streaming with `flush()` at once from one client address (0: no limit).
    streams_per_address: Option<usize>,
    /// `Cache-Control: public, max-age=N` for static files (default: `no-cache`).
    static_max_age: Option<u64>,
    /// Compress text responses with gzip (default: true).
    compress: Option<bool>,
    /// A file inside the project the access log is appended to.
    access_log: Option<String>,
    /// "text" (default) or "json".
    access_log_format: Option<String>,
    /// Seconds running requests get to finish when the server is stopped.
    shutdown_timeout: Option<u64>,
    /// The certificate chain (PEM) for HTTPS; relative to the project.
    tls_cert: Option<String>,
    /// The private key (PEM) of `tls_cert`.
    tls_key: Option<String>,
    /// Accept HTTP/2 without TLS ("prior knowledge"), for a proxy in front.
    h2c: Option<bool>,
    /// Give each request an id (`X-Request-Id`, the access log, `log_info()`).
    request_ids: Option<bool>,
    /// Take the request id and `traceparent` a proxy in front sends.
    trust_request_id: Option<bool>,
    /// A Redis database in `[databases]` that carries `publish()` to the
    /// other server processes.
    publish_store: Option<String>,
}

/// `[cors]`: which other sites may call this one from a browser.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CorsSection {
    #[serde(default)]
    origins: Vec<String>,
    methods: Option<Vec<String>>,
    #[serde(default)]
    headers: Vec<String>,
    #[serde(default)]
    credentials: bool,
    max_age: Option<u64>,
}

/// Cross-origin requests the server allows (`[cors]`).
#[derive(Clone, Debug)]
pub struct Cors {
    /// Allowed origins (`https://app.example.com`), lowercase; empty with `any`.
    pub origins: Vec<String>,
    /// `origins = ["*"]`: any origin (never with credentials).
    pub any: bool,
    pub methods: Vec<String>,
    pub headers: Vec<String>,
    pub credentials: bool,
    pub max_age: u64,
}

impl Cors {
    /// Whether a request's `Origin` may read the response.
    pub fn allows(&self, origin: &str) -> bool {
        self.any || self.origins.iter().any(|o| o.eq_ignore_ascii_case(origin))
    }
}

/// The format of the access log file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogFormat {
    Text,
    Json,
}

/// Two workers per CPU, at least 8.
fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(8, |n| n.get() * 2).max(8)
}

/// `[server] streams_per_address` when it is not set: a quarter of the
/// workers, at least 2, so that clients at one address that read a streamed
/// page slowly hold a few workers, never all of them (found by the 1.2 audit).
fn default_streams(workers: usize) -> usize {
    (workers / 4).max(2)
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
    /// A Redis database in `[databases]` to count rate limits in, so that
    /// several server processes share them.
    #[serde(default)]
    rate_limit_store: Option<String>,
    /// An environment variable holding the server's secret (at least 32
    /// bytes), shared by every server process of the site: CSRF tokens made by
    /// one are accepted by the others and after a restart.
    #[serde(default)]
    secret_env: Option<String>,
    /// The previous secret, still accepted while the secret is replaced.
    #[serde(default)]
    secret_old_env: Option<String>,
    /// The site's name in shared stores (default: `[project] name`): sites
    /// sharing a database keep their sessions, rate limits and jobs apart.
    #[serde(default)]
    store_namespace: Option<String>,
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
            rate_limit_store: None,
            secret_env: None,
            secret_old_env: None,
            store_namespace: None,
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
    /// The policy of a page's response. Under the default policy a page that
    /// made no nonce (it has no `<script>` of its own) runs no script at all.
    pub fn page_header(&self, nonce: Option<&str>) -> Option<String> {
        match (self, nonce) {
            (Csp::Policy(p), None) if p == DEFAULT_CSP => {
                Some(p.replace("script-src 'self' {nonce}", "script-src 'none'"))
            }
            _ => self.header(nonce),
        }
    }

    pub fn header(&self, nonce: Option<&str>) -> Option<String> {
        match self {
            Csp::Off => None,
            Csp::Policy(p) => {
                let n = nonce.map(|n| format!("'nonce-{n}'")).unwrap_or_default();
                // A page under the default policy runs only scripts with its nonce (and
                // what they load), not any script of the site: an injected `<script src>`
                // could otherwise load a page of the site that echoes a request value.
                // Static files have no nonce and keep `'self'`.
                if nonce.is_some() && p == DEFAULT_CSP {
                    return Some(p.replace("script-src 'self' {nonce}", &format!("script-src {n} 'strict-dynamic'")));
                }
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

/// A package in `[dependencies]`: a directory (`path`), a git repository at
/// a `rev` or at the newest tag matching `version`, or a package of the
/// registry (only `version`, written as a plain string: `slugify = "^1.2"`).
#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(from = "DependencySpec")]
pub struct Dependency {
    pub path: Option<String>,
    pub git: Option<String>,
    /// Tag, branch or commit for git dependencies.
    pub rev: Option<String>,
    /// A semver requirement (`^1.2`, `~0.3.1`, `>=1, <2`) matched against `vX.Y.Z` tags.
    pub version: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DependencySpec {
    Version(String),
    Table(DependencyTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DependencyTable {
    path: Option<String>,
    git: Option<String>,
    rev: Option<String>,
    version: Option<String>,
}

impl From<DependencySpec> for Dependency {
    fn from(d: DependencySpec) -> Dependency {
        match d {
            DependencySpec::Version(v) => Dependency { version: Some(v), ..Dependency::default() },
            DependencySpec::Table(t) => Dependency { path: t.path, git: t.git, rev: t.rev, version: t.version },
        }
    }
}

/// Whether `v` can be a semver requirement (checked fully by `phv install`).
pub fn plausible_version_req(v: &str) -> bool {
    !v.trim().is_empty() && v.len() <= 100 && v.chars().all(|c| c.is_ascii_alphanumeric() || " .^~*<>=,-+".contains(c))
}

/// Checks the `[dependencies]` of a project's or a package's phv.toml.
pub fn check_dependencies(deps: &BTreeMap<String, Dependency>) -> Result<(), String> {
    for (name, dep) in deps {
        if !valid_package_name(name) {
            return Err(format!("[dependencies]: invalid package name \"{name}\""));
        }
        let sources = [dep.path.is_some(), dep.git.is_some(), dep.path.is_none() && dep.git.is_none()];
        if sources.iter().filter(|s| **s).count() != 1
            || (dep.git.is_none() && dep.path.is_none() && dep.version.is_none())
        {
            return Err(format!("[dependencies].{name}: give a version (\"^1.2\"), or one of `path` or `git`"));
        }
        if dep.path.is_some() && (dep.rev.is_some() || dep.version.is_some()) {
            return Err(format!("[dependencies].{name}: a `path` package takes no `rev` or `version`"));
        }
        if dep.rev.is_some() && dep.version.is_some() {
            return Err(format!("[dependencies].{name}: give `rev` or `version`, not both"));
        }
        if dep.version.as_deref().is_some_and(|v| !plausible_version_req(v)) {
            return Err(format!("[dependencies].{name}: invalid version requirement"));
        }
    }
    Ok(())
}

/// Package names: lowercase letters, digits and `_`, not `std` (which is the
/// standard library). Lowercase only: on Windows and macOS `Greet` and `greet`
/// would be the same directory in vendor/.
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with("std")
}

#[derive(Clone)]
pub struct Config {
    pub name: Option<String>,
    pub host: Arc<Host>,
    pub csrf: bool,
    pub secure_cookies: bool,
    pub session_ttl: u64,
    pub memory_limit_mb: u64,
    /// Scripts that `phv serve` runs at once.
    pub workers: usize,
    /// Pages streaming with `flush()` at once from one client address; 0 is no limit.
    pub streams_per_address: usize,
    pub csp: Csp,
    pub requests_per_minute: u32,
    /// Directory for session files; None keeps sessions in memory.
    pub session_dir: Option<PathBuf>,
    /// The database of `[databases]` sessions are kept in (`session_store = "db:<name>"`).
    pub session_db: Option<String>,
    pub dependencies: BTreeMap<String, Dependency>,
    pub fmt: FmtSection,
    pub static_max_age: Option<u64>,
    pub compress: bool,
    /// The access log file (inside the project) and its format.
    pub access_log: Option<(PathBuf, LogFormat)>,
    pub shutdown_timeout: u64,
    pub cors: Option<Cors>,
    /// HTTPS: the certificate chain and the private key (PEM files).
    pub tls: Option<Tls>,
    /// HTTP/2 without TLS is accepted too.
    pub h2c: bool,
    /// `[security] secret_env` and `secret_old_env`: the environment variables
    /// holding the server's secret and the previous one.
    pub secret_env: Option<String>,
    pub secret_old_env: Option<String>,
    /// Whether `[security] store_namespace` is set (rather than taken from the name).
    pub store_namespace_set: bool,
    /// `[admin]`: the admin port.
    pub admin: Option<Admin>,
    /// `[server] request_ids` and `trust_request_id`.
    pub request_ids: bool,
    pub trust_request_id: bool,
    /// `[server] publish_store`: the Redis database of `[databases]` that
    /// carries `publish()` to the other server processes.
    pub publish_store: Option<String>,
    /// `[jobs]` and `[[schedule]]`.
    pub jobs: Option<Jobs>,
    pub schedules: Vec<Schedule>,
    /// `[telemetry]`: traces sent to an OpenTelemetry collector.
    pub telemetry: Option<Telemetry>,
    /// `[i18n]`: the locales of `std.i18n`.
    pub i18n: Option<I18n>,
}

/// `[server] tls_cert` / `tls_key`, resolved against the project root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
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
            workers: default_workers(),
            streams_per_address: default_streams(default_workers()),
            csp: Csp::Policy(DEFAULT_CSP.into()),
            requests_per_minute: 0,
            session_dir: None,
            session_db: None,
            dependencies: BTreeMap::new(),
            fmt: FmtSection::default(),
            static_max_age: None,
            compress: true,
            access_log: None,
            shutdown_timeout: 30,
            cors: None,
            tls: None,
            h2c: false,
            secret_env: None,
            secret_old_env: None,
            store_namespace_set: false,
            admin: None,
            request_ids: false,
            trust_request_id: false,
            publish_store: None,
            jobs: None,
            schedules: Vec::new(),
            telemetry: None,
            i18n: None,
        }
    }
}

/// `[cors]`, checked.
fn cors_config(c: CorsSection) -> Result<Cors, String> {
    let any = c.origins.iter().any(|o| o == "*");
    if any && c.origins.len() > 1 {
        return Err("[cors].origins: \"*\" cannot be listed with other origins".into());
    }
    if any && c.credentials {
        return Err("[cors]: credentials = true needs a list of origins, not \"*\" (browsers refuse both)".into());
    }
    for o in &c.origins {
        let rest = o.strip_prefix("https://").or_else(|| o.strip_prefix("http://"));
        let ok = o == "*"
            || rest.is_some_and(|h| {
                !h.is_empty()
                    && h.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '[' | ']'))
            });
        if !ok {
            return Err(format!(
                "[cors].origins: \"{o}\" must be \"*\" or an origin like \"https://app.example.com\" (no path)"
            ));
        }
    }
    let token = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b));
    let methods: Vec<String> =
        c.methods.unwrap_or_else(|| vec!["GET".into(), "POST".into()]).iter().map(|m| m.to_ascii_uppercase()).collect();
    if let Some(m) = methods.iter().find(|m| !token(m)) {
        return Err(format!("[cors].methods: \"{m}\" is not an HTTP method"));
    }
    if let Some(h) = c.headers.iter().find(|h| !token(h)) {
        return Err(format!("[cors].headers: \"{h}\" is not a header name"));
    }
    let max_age = c.max_age.unwrap_or(600);
    if max_age > 86_400 {
        return Err("[cors].max_age must be at most 86400 seconds".into());
    }
    Ok(Cors {
        origins: if any { Vec::new() } else { c.origins.iter().map(|o| o.to_ascii_lowercase()).collect() },
        any,
        methods,
        headers: c.headers,
        credentials: c.credentials,
        max_age,
    })
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
    if !ok {
        return Err(format!(
            "[permissions].{kind}: \"{d}\" must be a directory inside the project (no `..`, not absolute)"
        ));
    }
    // A Windows short name (`GIT~1`) names another directory than it spells:
    // the checks on these names (no `.git`, no `.phv-cache`, not `public/`)
    // compare the spelling (found by the 1.0 audit).
    if d.contains('~') {
        return Err(format!("[permissions].{kind}: \"{d}\" has a `~`; write the directory's full name"));
    }
    Ok(())
}

/// A Windows network share or device path (`\\host\share`, `//host/share`,
/// `\\?\`, `\\.\`): opening one makes Windows contact the host with the
/// user's credentials.
fn network_path(p: &str) -> bool {
    let mut c = p.chars();
    matches!((c.next(), c.next()), (Some('\\' | '/'), Some('\\' | '/')))
}

/// `[project] phv`: the project needs a version of PHV that `req` (a semver
/// requirement such as `"^1.2"`) accepts.
fn check_phv_version(req: &str, running: &str) -> Result<(), String> {
    let r = semver::VersionReq::parse(req)
        .map_err(|e| format!("[project].phv: \"{req}\" is not a version requirement such as \"^1.0\" ({e})"))?;
    let v = semver::Version::parse(running).map_err(|e| e.to_string())?;
    if !r.matches(&v) {
        return Err(format!("this project needs PHV {req} ([project].phv in phv.toml); this is PHV {v}"));
    }
    Ok(())
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
    let text = match crate::read_source(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default_for(root)),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse_config(root, &text)
}

/// `load_config` for the loader, which reads `phv.toml` again at each load:
/// while the file is unchanged its text is not read again (see
/// `read_known`). It is still parsed and checked each time: the checks also
/// look at the project's folders (`[jobs] dir`, the TLS files).
pub(crate) fn load_config_known(root: &Path) -> Result<Config, String> {
    let path = root.join("phv.toml");
    let text = match crate::read_known(&path) {
        Ok(s) => s.text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default_for(root)),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse_config(root, &text)
}

/// The configuration in `text` (a phv.toml) for the project at `root`.
pub fn parse_config(root: &Path, text: &str) -> Result<Config, String> {
    parse_config_with_jobs(root, text, None)
}

/// `parse_config`, with the names of the jobs given (a built site's, from its
/// bundle) instead of read from `[jobs] dir` (found by the 1.1 audit: a built
/// site checked its schedules against the folder it was started in).
pub fn parse_config_with_jobs(root: &Path, text: &str, jobs_given: Option<Vec<String>>) -> Result<Config, String> {
    let file: File = toml::from_str(text).map_err(|e| format!("phv.toml: {e}"))?;
    if let Some(req) = &file.project.phv {
        check_phv_version(req, env!("CARGO_PKG_VERSION"))?;
    }
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
    check_dependencies(&file.dependencies)?;
    for (name, dep) in &file.dependencies {
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
    let mut session_db = None;
    let session_dir = match file.security.session_store.as_deref() {
        None | Some("memory") => None,
        Some(d) if d.starts_with("db:") => {
            let name = &d[3..];
            if !file.databases.contains_key(name) {
                return Err(format!("[security].session_store: no database named \"{name}\" in [databases]"));
            }
            session_db = Some(name.to_string());
            None
        }
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
    let rate_limit_store = file.security.rate_limit_store.clone();
    if let Some(name) = &rate_limit_store {
        let redis = |u: &str| u.starts_with("redis://") || u.starts_with("rediss://");
        match file.databases.get(name) {
            None => return Err(format!("[security].rate_limit_store: no database named \"{name}\" in [databases]")),
            Some(d) if d.url.as_deref().is_some_and(|u| !redis(u)) => {
                return Err(format!("[security].rate_limit_store: database \"{name}\" is not a Redis server"))
            }
            Some(_) => {}
        }
    }
    for (key, var) in [("secret_env", &file.security.secret_env), ("secret_old_env", &file.security.secret_old_env)] {
        if let Some(v) = var {
            if !env_name(v) {
                return Err(format!("[security].{key} must be the name of an environment variable (A-Z, 0-9, _)"));
            }
        }
    }
    if file.security.secret_old_env.is_some() && file.security.secret_env.is_none() {
        return Err("[security].secret_old_env needs secret_env".into());
    }
    let admin = match file.admin {
        None => None,
        Some(a) => {
            let listen: std::net::SocketAddr = a.listen.parse().map_err(|_| {
                format!("[admin].listen must be an address and port, such as \"127.0.0.1:9100\" (not \"{}\")", a.listen)
            })?;
            if let Some(v) = &a.token_env {
                if !env_name(v) {
                    return Err("[admin].token_env must be the name of an environment variable (A-Z, 0-9, _)".into());
                }
            } else if !listen.ip().is_loopback() {
                return Err(format!(
                    "[admin].listen = \"{listen}\" can be reached from other machines: give token_env too, so that \
                     /metrics needs a token"
                ));
            }
            Some(Admin { listen, token_env: a.token_env })
        }
    };
    let publish_store = file.server.publish_store.clone();
    if let Some(name) = &publish_store {
        let redis = |u: &str| u.starts_with("redis://") || u.starts_with("rediss://");
        match file.databases.get(name) {
            None => return Err(format!("[server].publish_store: no database named \"{name}\" in [databases]")),
            Some(d) if d.url.as_deref().is_some_and(|u| !redis(u)) => {
                return Err(format!("[server].publish_store: database \"{name}\" is not a Redis server"))
            }
            Some(_) => {}
        }
        if file.security.secret_env.is_none() {
            return Err(
                "[server].publish_store needs [security].secret_env: the messages between server processes are signed with it"
                    .into(),
            );
        }
    }
    let jobs = match &file.jobs {
        None => None,
        Some(j) => {
            let store = match j.store.as_deref() {
                None | Some("memory") => None,
                Some(s) => {
                    let Some(name) = s.strip_prefix("db:") else {
                        return Err(
                            "[jobs].store must be \"memory\" or \"db:<name>\" (a database of [databases])".into()
                        );
                    };
                    if !file.databases.contains_key(name) {
                        return Err(format!("[jobs].store: no database named \"{name}\" in [databases]"));
                    }
                    Some(name.to_string())
                }
            };
            let dir = j.dir.clone().unwrap_or_else(|| "jobs".into());
            let segment_ok = |s: &str| {
                !s.is_empty()
                    && !s.starts_with('.')
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            };
            let trimmed = dir.trim_end_matches('/');
            // In any case: on Windows `Public` is `public` (found by the 1.1 audit).
            let first = trimmed.split('/').next().unwrap_or("");
            if !trimmed.split('/').all(segment_ok) || first.eq_ignore_ascii_case("public") {
                return Err(format!(
                    "[jobs].dir: \"{dir}\" must be a directory of the project outside public/ (letters, digits, _ and -)"
                ));
            }
            let workers = j.workers.unwrap_or(2);
            if !(1..=64).contains(&workers) {
                return Err("[jobs].workers must be between 1 and 64".into());
            }
            let time_limit = j.time_limit.unwrap_or(300);
            if !(1..=3600).contains(&time_limit) {
                return Err("[jobs].time_limit must be between 1 and 3600 seconds".into());
            }
            let max_attempts = j.max_attempts.unwrap_or(5);
            if !(1..=100).contains(&max_attempts) {
                return Err("[jobs].max_attempts must be between 1 and 100".into());
            }
            let retry_delay = j.retry_delay.unwrap_or(30);
            if !(1..=3600).contains(&retry_delay) {
                return Err("[jobs].retry_delay must be between 1 and 3600 seconds".into());
            }
            Some(Jobs {
                store,
                dir: trimmed.to_string(),
                workers,
                time_limit,
                max_attempts,
                retry_delay,
                run: j.run.unwrap_or(true),
            })
        }
    };
    // Counted before anything is read for them (found by the 1.1 audit: the
    // jobs folder was read for each entry, then the count was checked).
    if file.schedule.len() > 100 {
        return Err("at most 100 [[schedule]] entries".into());
    }
    if let Some(j) = &jobs {
        if jobs_given.is_none() && jobs_dir_is_link(root, &j.dir) {
            return Err(format!("[jobs] dir: {} is a link; the jobs must be in the project", j.dir));
        }
    }
    // The job names, read once.
    let job_names = match &jobs {
        Some(j) => jobs_given.or_else(|| scan_jobs(root, &j.dir)),
        None => None,
    };
    let mut schedules = Vec::new();
    for s in &file.schedule {
        let Some(j) = &jobs else { return Err("[[schedule]] needs a [jobs] section".into()) };
        if !job_name(&s.job) {
            return Err(format!("[[schedule]]: \"{}\" is not the name of a job (a-z, 0-9, _)", s.job));
        }
        if let Some(names) = &job_names {
            if !names.contains(&s.job) {
                return Err(format!("[[schedule]]: there is no job {}/{}.phv", j.dir, s.job));
            }
        }
        let cron = phv_std::cron::Cron::parse(&s.cron, s.timezone.as_deref())
            .map_err(|e| format!("[[schedule]] of {}: {e}", s.job))?;
        schedules.push(Schedule { job: s.job.clone(), cron });
    }
    if file.server.workers.is_some_and(|w| !(1..=1024).contains(&w)) {
        return Err("[server].workers must be between 1 and 1024".into());
    }
    let workers = file.server.workers.unwrap_or_else(default_workers);
    if file.server.static_max_age.is_some_and(|a| a > 31_536_000) {
        return Err("[server].static_max_age must be at most 31536000 seconds (a year)".into());
    }
    let shutdown_timeout = file.server.shutdown_timeout.unwrap_or(30);
    if shutdown_timeout > 3600 {
        return Err("[server].shutdown_timeout must be at most 3600 seconds".into());
    }
    let format = match file.server.access_log_format.as_deref() {
        None | Some("text") => LogFormat::Text,
        Some("json") => LogFormat::Json,
        Some(other) => return Err(format!("[server].access_log_format: \"{other}\" is not \"text\" or \"json\"")),
    };
    let access_log = match &file.server.access_log {
        None => None,
        Some(f) => {
            check_dir("access_log", f).map_err(|_| {
                format!("[server].access_log: \"{f}\" must be a file inside the project (no `..`, not absolute)")
            })?;
            let names = dir_names(f);
            if names.first().is_some_and(|n| n == "public") {
                return Err(format!("[server].access_log: \"{f}\" is inside public/"));
            }
            Some((root.join(f), format))
        }
    };
    let tls = match (&file.server.tls_cert, &file.server.tls_key) {
        (None, None) => None,
        (Some(cert), Some(key)) if !cert.trim().is_empty() && !key.trim().is_empty() => {
            for (what, p) in [("tls_cert", cert), ("tls_key", key)] {
                if network_path(p) {
                    return Err(format!(
                        "[server].{what}: \"{p}\" is a network or device path; give a file on this machine"
                    ));
                }
                // A network drive, or a link to a share (found by the 1.1.1 review).
                if phv_std::safefs::on_another_machine(&root.join(p)) {
                    return Err(format!(
                        "[server].{what}: \"{p}\" is on another machine (a network drive or share, or a link to \
                         one); give a file on this machine"
                    ));
                }
            }
            Some(Tls { cert: root.join(cert), key: root.join(key) })
        }
        (Some(_), Some(_)) => return Err("[server].tls_cert and tls_key must not be empty".into()),
        _ => return Err("[server]: give both tls_cert and tls_key, or neither".into()),
    };
    let cors = file.cors.map(cors_config).transpose()?;
    let mut permissions = Permissions {
        read: file.permissions.read,
        write: file.permissions.write,
        env: file.permissions.env,
        hidden_env: Vec::new(),
    };
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
    let mut mail = BTreeMap::new();
    for (name, m) in file.mail {
        let spec = mail_spec(&name, m)?;
        mail.insert(name, spec);
    }
    let mut http = BTreeMap::new();
    for (name, h) in file.http {
        let spec = http_spec(&name, h)?;
        http.insert(name, spec);
    }
    let mut oauth = BTreeMap::new();
    for (name, o) in file.oauth {
        let spec = oauth_spec(&name, o)?;
        oauth.insert(name, spec);
    }
    // The site's namespace in shared stores: `[security] store_namespace`, or
    // the project's name (found by the 1.1 audit: sites sharing a database
    // shared sessions, rate limits and jobs). Hashed: it names tables and keys.
    if let Some(n) = &file.security.store_namespace {
        if n.is_empty() || n.len() > 64 || !n.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
            return Err("[security].store_namespace: 1 to 64 letters, digits, `_`, `-` and `.`".into());
        }
    }
    let store_namespace =
        file.security.store_namespace.as_deref().or(file.project.name.as_deref()).map(phv_std::store_namespace);
    // What the server reads for itself, no script reads.
    let hidden = &mut permissions.hidden_env;
    hidden.extend(file.security.secret_env.iter().chain(&file.security.secret_old_env).cloned());
    hidden.extend(admin.iter().filter_map(|a| a.token_env.clone()));
    hidden.extend(databases.values().filter_map(|d| d.url_env.clone()));
    hidden.extend(websockets.values().filter_map(|w| w.url_env.clone()));
    hidden.extend(mail.values().filter_map(|m| m.url_env.clone()));
    for h in http.values() {
        hidden.extend(h.url_env.iter().cloned());
        hidden.extend(h.headers_env.iter().map(|(_, var)| var.clone()));
    }
    hidden.extend(oauth.values().filter_map(|o| o.client_secret_env.clone()));
    let i18n = file.i18n.map(i18n_config).transpose()?;
    let telemetry = match file.telemetry {
        Some(t) => {
            hidden.extend(t.endpoint_env.iter().cloned());
            hidden.extend(t.headers_env.values().cloned());
            Some(telemetry(t, file.project.name.as_deref())?)
        }
        None => None,
    };
    Ok(Config {
        name: file.project.name,
        host: Arc::new(Host {
            root: root.to_path_buf(),
            permissions,
            databases,
            websockets,
            http,
            rate_limit_store,
            site: None,
            mail,
            cli: false,
            no_stdin: false,
            args: Vec::new(),
            store_namespace,
            oauth,
            // Set by `phv serve` once it knows its address.
            listen: None,
            jobs: jobs.as_ref().map(|j| phv_vm::JobsSpec {
                store: j.store.clone(),
                names: job_names.clone(),
                memory_key: root.to_string_lossy().into_owned(),
            }),
        }),
        csrf: file.security.csrf,
        // Served over HTTPS: cookies are `Secure` and HSTS is sent.
        secure_cookies: file.security.secure_cookies || tls.is_some(),
        session_ttl: file.security.session_ttl,
        memory_limit_mb: file.security.memory_limit_mb,
        workers,
        streams_per_address: file.server.streams_per_address.unwrap_or_else(|| default_streams(workers)),
        csp,
        requests_per_minute: file.security.requests_per_minute,
        session_dir,
        session_db,
        dependencies: file.dependencies,
        fmt: file.fmt,
        static_max_age: file.server.static_max_age,
        compress: file.server.compress.unwrap_or(true),
        access_log,
        shutdown_timeout,
        cors,
        tls,
        h2c: file.server.h2c.unwrap_or(false),
        secret_env: file.security.secret_env,
        secret_old_env: file.security.secret_old_env,
        store_namespace_set: file.security.store_namespace.is_some(),
        admin,
        request_ids: file.server.request_ids.unwrap_or(false),
        trust_request_id: file.server.trust_request_id.unwrap_or(false),
        publish_store,
        jobs,
        schedules,
        telemetry,
        i18n,
    })
}

/// A name an environment variable can have: letters, digits and `_`, not
/// starting with a digit.
fn env_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !s.as_bytes()[0].is_ascii_digit()
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

    /// Found by the 1.2 audit: slow readers of a streamed page held every worker.
    #[test]
    fn streams_per_address_defaults_to_a_quarter_of_the_workers() {
        assert_eq!(load("[server]\nworkers = 16\n").unwrap().streams_per_address, 4);
        assert_eq!(load("[server]\nworkers = 2\n").unwrap().streams_per_address, 2);
        assert_eq!(load("[server]\nworkers = 16\nstreams_per_address = 0\n").unwrap().streams_per_address, 0);
        assert_eq!(load("[server]\nstreams_per_address = 1\n").unwrap().streams_per_address, 1);
    }

    /// Found by the 1.0 audit.
    #[test]
    fn paths_that_leave_the_machine_or_hide_a_name_are_refused() {
        for p in ["//host/share/key.pem", "\\\\host\\share\\key.pem", "\\\\?\\C:\\key.pem"] {
            let toml = format!("[server]\ntls_cert = \"certs/c.pem\"\ntls_key = '{p}'\n");
            let e = load(&toml).err().unwrap_or_default();
            assert!(e.contains("network or device path"), "{p}: {e}");
        }
        for d in ["GIT~1/hooks/", "data~1/"] {
            let e = load(&format!("[permissions]\nwrite = [\"{d}\"]\n")).err().unwrap_or_default();
            assert!(e.contains("has a `~`"), "{d}: {e}");
        }
    }

    /// A link to a share passed the check of the name's first characters
    /// (found by the 1.1.1 review). Making a link needs Developer Mode or the
    /// privilege: without, only the local paths are tried.
    #[cfg(windows)]
    #[test]
    fn tls_files_behind_a_link_to_a_share_are_refused() {
        let dir = std::env::temp_dir().join(format!("phv-config-tlslink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        let toml = "[server]\ntls_cert = \"certs/c.pem\"\ntls_key = \"certs/k.pem\"\n";
        std::fs::write(dir.join("phv.toml"), toml).unwrap();
        std::fs::write(dir.join("certs/c.pem"), "x").unwrap();
        std::fs::write(dir.join("certs/k.pem"), "x").unwrap();
        // Files on this machine, by a relative or an absolute name, are fine.
        assert!(load_config(&dir).is_ok());
        let abs = dir.join("certs/k.pem").canonicalize().unwrap().display().to_string();
        let toml_abs =
            format!("[server]\ntls_cert = \"certs/c.pem\"\ntls_key = '{}'\n", abs.trim_start_matches(r"\\?\"));
        std::fs::write(dir.join("phv.toml"), toml_abs).unwrap();
        assert!(load_config(&dir).is_ok());
        std::fs::write(dir.join("phv.toml"), toml).unwrap();
        std::fs::remove_file(dir.join("certs/k.pem")).unwrap();
        if std::os::windows::fs::symlink_file(r"\\phv-test.invalid\share\k.pem", dir.join("certs/k.pem")).is_ok() {
            let e = load_config(&dir).err().unwrap_or_default();
            assert!(e.contains("is on another machine"), "{e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the server reads for itself is hidden from scripts, whatever
    /// `[permissions] env` says (found by the 1.1 audit).
    #[test]
    fn server_variables_are_hidden_from_scripts() {
        let c = load(concat!(
            "[permissions]\nenv = [\"APP_*\"]\n",
            "[security]\nsecret_env = \"APP_SECRET\"\nsecret_old_env = \"APP_OLD\"\n",
            "[admin]\nlisten = \"127.0.0.1:9100\"\ntoken_env = \"APP_TOKEN\"\n",
            "[databases.main]\nurl_env = \"APP_DB\"\n",
            "[http.api]\nurl_env = \"APP_API\"\nheaders_env = { Authorization = \"APP_AUTH\" }\n",
        ))
        .unwrap();
        let hidden = &c.host.permissions.hidden_env;
        for v in ["APP_SECRET", "APP_OLD", "APP_TOKEN", "APP_DB", "APP_API", "APP_AUTH"] {
            assert!(hidden.iter().any(|h| h == v), "{v}: {hidden:?}");
        }
    }

    /// A site's namespace in shared stores comes from `[security]
    /// store_namespace` or its name (found by the 1.1 audit).
    #[test]
    fn sites_get_a_namespace_from_their_name() {
        let ns = |toml: &str| load(toml).unwrap().host.store_namespace.clone();
        assert_eq!(ns(""), None);
        let blog = ns("[project]\nname = \"blog\"\n").unwrap();
        assert_eq!(blog.len(), 32);
        assert!(blog.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(ns("[project]\nname = \"shop\"\n"), Some(blog.clone()));
        let staging = ns("[project]\nname = \"blog\"\n[security]\nstore_namespace = \"blog-staging\"\n");
        assert!(staging.is_some() && staging != Some(blog));
        for bad in ["", "a b", "x/y", &"n".repeat(65)] {
            let e = load(&format!("[security]\nstore_namespace = \"{bad}\"\n")).err().unwrap_or_default();
            assert!(e.contains("store_namespace"), "{bad:?}: {e}");
        }
    }

    /// The count of `[[schedule]]` entries is checked before any is looked at,
    /// and the jobs folder is read once (found by the 1.1 audit: 200,000
    /// entries and 2,000 job files took 159 seconds of `phv check`).
    #[test]
    fn many_schedules_are_refused_at_once() {
        let dir = std::env::temp_dir().join(format!("phv-config-sched-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("jobs")).unwrap();
        for i in 0..1000 {
            std::fs::write(dir.join("jobs").join(format!("j{i}.phv")), "<?phv\n").unwrap();
        }
        let mut toml = String::from("[jobs]\n");
        for _ in 0..100_000 {
            toml.push_str("[[schedule]]\njob = \"j1\"\ncron = \"* * * * *\"\n");
        }
        let start = std::time::Instant::now();
        let e = parse_config(&dir, &toml).err().unwrap_or_default();
        assert!(e.contains("at most 100"), "{e}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
        // A built site checks against its bundle's jobs, not the folder it runs in.
        let one = "[jobs]\n[[schedule]]\njob = \"cleanup\"\ncron = \"0 * * * *\"\n";
        assert!(parse_config(&dir, one).is_err());
        let c = parse_config_with_jobs(&dir, one, Some(vec!["cleanup".into()])).unwrap();
        assert_eq!(c.host.jobs.as_ref().unwrap().names.as_deref(), Some(&["cleanup".to_string()][..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A jobs folder that is a link is refused (found by the 1.1 audit).
    #[test]
    fn a_linked_jobs_folder_is_refused() {
        let dir = std::env::temp_dir().join(format!("phv-config-jlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::write(dir.join("outside").join("x.phv"), "<?phv\n").unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(dir.join("outside"), dir.join("jobs")).is_ok();
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.join("jobs"))
            .arg(dir.join("outside"))
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            let e = parse_config(&dir, "[jobs]\n").err().unwrap_or_default();
            assert!(e.contains("is a link"), "{e}");
            assert_eq!(scan_jobs(&dir, "jobs"), Some(Vec::new()));
            // A link on the way counts too.
            std::fs::create_dir_all(dir.join("outside").join("inner")).unwrap();
            let e = parse_config(&dir, "[jobs]\ndir = \"jobs/inner\"\n").err().unwrap_or_default();
            assert!(e.contains("is a link"), "{e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_server_secret_is_named_by_an_environment_variable() {
        let c = load("[security]\nsecret_env = \"APP_SECRET\"\nsecret_old_env = \"APP_SECRET_OLD\"\n").unwrap();
        assert_eq!(c.secret_env.as_deref(), Some("APP_SECRET"));
        assert_eq!(c.secret_old_env.as_deref(), Some("APP_SECRET_OLD"));
        assert!(load("").unwrap().secret_env.is_none());
        for bad in ["", "1ABC", "A-B", "A B", "$X"] {
            let e = load(&format!("[security]\nsecret_env = \"{bad}\"\n")).err().unwrap_or_default();
            assert!(e.contains("must be the name of an environment variable"), "{bad}: {e}");
        }
        let e = load("[security]\nsecret_old_env = \"OLD\"\n").err().unwrap_or_default();
        assert!(e.contains("needs secret_env"), "{e}");
    }

    #[test]
    fn the_admin_port_and_request_ids() {
        let c = load("[admin]\nlisten = \"127.0.0.1:9100\"\n[server]\nrequest_ids = true\n").unwrap();
        assert_eq!(c.admin.as_ref().unwrap().listen, "127.0.0.1:9100".parse().unwrap());
        assert!(c.request_ids && !c.trust_request_id);
        assert!(load("").unwrap().admin.is_none());
        let e = load("[admin]\nlisten = \"0.0.0.0:9100\"\n").err().unwrap_or_default();
        assert!(e.contains("give token_env too"), "{e}");
        assert!(load("[admin]\nlisten = \"0.0.0.0:9100\"\ntoken_env = \"ADMIN_TOKEN\"\n").is_ok());
        let e = load("[admin]\nlisten = \"localhost\"\n").err().unwrap_or_default();
        assert!(e.contains("must be an address and port"), "{e}");
        let e = load("[admin]\nlisten = \"127.0.0.1:1\"\ntoken_env = \"a b\"\n").err().unwrap_or_default();
        assert!(e.contains("token_env must be the name"), "{e}");
    }

    #[test]
    fn publish_store_is_a_redis_database_and_needs_the_secret() {
        let redis =
            "[databases.cache]\nurl = \"redis://localhost\"\n[databases.pg]\nurl = \"postgres://localhost/x\"\n";
        let ok = format!("{redis}[security]\nsecret_env = \"S\"\n[server]\npublish_store = \"cache\"\n");
        assert_eq!(load(&ok).unwrap().publish_store.as_deref(), Some("cache"));
        let e = load(&format!("{redis}[server]\npublish_store = \"cache\"\n")).err().unwrap_or_default();
        assert!(e.contains("needs [security].secret_env"), "{e}");
        let e = load(&format!("{redis}[security]\nsecret_env = \"S\"\n[server]\npublish_store = \"pg\"\n"))
            .err()
            .unwrap_or_default();
        assert!(e.contains("is not a Redis server"), "{e}");
        let e = load(&format!("{redis}[security]\nsecret_env = \"S\"\n[server]\npublish_store = \"x\"\n"))
            .err()
            .unwrap_or_default();
        assert!(e.contains("no database named"), "{e}");
    }

    #[test]
    fn the_project_may_require_a_phv_version() {
        assert!(check_phv_version("^1.2", "1.4.0").is_ok());
        assert!(check_phv_version(">=1.0, <2", "1.0.0").is_ok());
        let e = check_phv_version("^1.2", "1.1.3").unwrap_err();
        assert_eq!(e, "this project needs PHV ^1.2 ([project].phv in phv.toml); this is PHV 1.1.3");
        assert!(check_phv_version("^1.2", "2.0.0").is_err());
        assert!(check_phv_version("one", "1.0.0").unwrap_err().contains("is not a version requirement"));
        // The running version always accepts its own major and minor.
        let me = env!("CARGO_PKG_VERSION");
        let v = semver::Version::parse(me).unwrap();
        assert!(load(&format!("[project]\nphv = \"^{}.{}\"\n", v.major, v.minor)).is_ok());
    }

    #[test]
    fn server_and_cors_settings() {
        for bad in [
            "[cors]
origins = [\"*\"]
credentials = true
",
            "[cors]
origins = [\"*\", \"https://a.example\"]
",
            "[cors]
origins = [\"https://a.example/path\"]
",
            "[cors]
origins = [\"a.example\"]
",
            "[cors]
methods = [\"GET POST\"]
",
            "[server]
access_log = \"public/log.txt\"
",
            "[server]
access_log = \"../log.txt\"
",
            "[server]
access_log_format = \"xml\"
",
            "[server]
shutdown_timeout = 99999
",
        ] {
            assert!(load(bad).is_err(), "{bad}");
        }
        let c = load(
            "[cors]
origins = [\"https://App.example\"]
[server]
access_log = \"logs/a.log\"
",
        )
        .unwrap();
        let cors = c.cors.unwrap();
        assert!(cors.allows("https://app.example") && !cors.allows("https://other.example"));
        assert_eq!(cors.methods, ["GET", "POST"]);
        assert!(c.access_log.is_some() && c.compress);
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
    fn oauth_providers() {
        std::env::set_var("PHV_TEST_OAUTH_SECRET_1", "very-secret");
        let c = load(
            "[oauth.google]
issuer = \"https://accounts.google.com\"
client_id = \"123.apps\"
client_secret_env = \"PHV_TEST_OAUTH_SECRET_1\"
redirect_uri = \"https://example.com/auth/google\"

[oauth.github]
authorize_url = \"https://github.com/login/oauth/authorize\"
token_url = \"https://github.com/login/oauth/access_token\"
client_id_env = \"PHV_TEST_OAUTH_UNSET\"
client_secret_env = \"PHV_TEST_OAUTH_SECRET_1\"
client_auth = \"post\"
redirect_uri = \"http://localhost:8080/auth/github\"
scopes = [\"read:user\"]
",
        )
        .unwrap();
        let g = &c.host.oauth["google"];
        assert_eq!(g.scopes, ["openid"]);
        assert_eq!(g.client_secret.as_deref(), Some("very-secret"));
        assert_eq!(g.secret_in_body, None);
        let shown = format!("{g:?}");
        assert!(!shown.contains("very-secret"), "{shown}");
        let gh = &c.host.oauth["github"];
        assert_eq!(gh.client_id, None, "an unset variable is reported when used");
        assert_eq!(gh.secret_in_body, Some(true));
        assert!(c.host.permissions.hidden_env.contains(&"PHV_TEST_OAUTH_SECRET_1".to_string()));
        let ok = "client_id = \"a\"\nredirect_uri = \"https://e.com/cb\"\n";
        for bad in [
            format!("[oauth.x]\n{ok}"),
            format!("[oauth.x]\nissuer = \"http://id.example\"\n{ok}"),
            format!("[oauth.x]\nissuer = \"https://id.example?x=1\"\n{ok}"),
            format!("[oauth.x]\nissuer = \"https://id.example\"\nauthorize_url = \"https://id.example/a\"\n{ok}"),
            format!("[oauth.x]\nauthorize_url = \"https://id.example/a\"\n{ok}"),
            format!("[oauth.x]\nissuer = \"https://id.example\"\nscopes = [\"email\"]\n{ok}"),
            format!("[oauth.x]\nissuer = \"https://id.example\"\nscopes = [\"a b\"]\n{ok}"),
            format!("[oauth.x]\nissuer = \"https://id.example\"\nclient_auth = \"post\"\n{ok}"),
            format!("[oauth.x]\nissuer = \"https://id.example\"\nclient_secret = \"inline\"\n{ok}"),
            "[oauth.x]\nissuer = \"https://id.example\"\nclient_id = \"a\"\nredirect_uri = \"http://e.com/cb\"\n"
                .into(),
            "[oauth.x]\nissuer = \"https://id.example\"\nclient_id = \"a\"\nredirect_uri = \"https://e.com/cb#x\"\n"
                .into(),
            "[oauth.x]\nissuer = \"https://id.example\"\nredirect_uri = \"https://e.com/cb\"\n".into(),
        ] {
            assert!(load(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn telemetry() {
        std::env::set_var("PHV_TEST_OTEL_KEY", "api-key-1");
        let c = load(
            "[project]\nname = \"shop\"\n\n[telemetry]\nendpoint = \"https://otel.example.com/\"\n\
             headers_env = { \"X-Key\" = \"PHV_TEST_OTEL_KEY\" }\nsample = 0.25\n",
        )
        .unwrap();
        let t = c.telemetry.as_ref().unwrap();
        assert_eq!(t.url.as_deref(), Some("https://otel.example.com/v1/traces"));
        assert_eq!(t.service_name, "shop");
        assert_eq!(t.sample, 0.25);
        assert_eq!(t.headers, [("X-Key".to_string(), Some("api-key-1".to_string()))]);
        assert!(!format!("{t:?}").contains("api-key-1"));
        assert!(c.host.permissions.hidden_env.contains(&"PHV_TEST_OTEL_KEY".to_string()));
        let c = load("[telemetry]\nendpoint = \"http://localhost:4318/v1/traces\"\n").unwrap();
        let t = c.telemetry.as_ref().unwrap();
        assert_eq!(t.url.as_deref(), Some("http://localhost:4318/v1/traces"));
        assert_eq!(t.service_name, "phv");
        for bad in [
            "[telemetry]\n",
            "[telemetry]\nendpoint = \"http://otel.example.com\"\n",
            "[telemetry]\nendpoint = \"https://otel.example.com?x=1\"\n",
            "[telemetry]\nendpoint = \"https://otel.example.com\"\nsample = 1.5\n",
            "[telemetry]\nendpoint = \"https://otel.example.com\"\nheaders = { Host = \"x\" }\n",
            "[telemetry]\nendpoint = \"https://otel.example.com\"\nservice_name = \"\"\n",
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
