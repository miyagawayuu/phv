//! `phenix.toml`: project settings, security options, permissions and dependencies.
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
//! ```

use phenix_vm::{Host, Permissions};
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

/// Policy used when phenix.toml does not set `csp`. Inline scripts run only
/// with the response's nonce, which Phenix adds to every `<script>` tag
/// written in a template; injected markup (via `raw()`) never has it.
pub const DEFAULT_CSP: &str = "default-src 'self'; script-src 'self' {nonce}; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; font-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'";

#[derive(Clone, Debug, PartialEq)]
pub enum Csp {
    Off,
    /// A policy; `{nonce}` is replaced per response.
    Policy(String),
}

impl Csp {
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
            host: Arc::new(Host { root: root.to_path_buf(), permissions: Permissions::default() }),
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
            std::path::Component::Normal(n) => n.to_str().is_some_and(phenix_std::safefs::valid_name),
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

/// Reads `<root>/phenix.toml` (all settings are optional).
pub fn load_config(root: &Path) -> Result<Config, String> {
    let path = root.join("phenix.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default_for(root)),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let file: File = toml::from_str(&text).map_err(|e| format!("phenix.toml: {e}"))?;
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
    Ok(Config {
        name: file.project.name,
        host: Arc::new(Host { root: root.to_path_buf(), permissions }),
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
        let dir = std::env::temp_dir().join(format!("phenix-config-{}-{}", std::process::id(), toml.len()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("phenix.toml"), toml).unwrap();
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
    fn nonce_placeholder_in_both_forms() {
        let n = Some("abc");
        assert_eq!(Csp::Policy("script-src {nonce}".into()).header(n).unwrap(), "script-src 'nonce-abc'");
        assert_eq!(Csp::Policy("script-src 'nonce-{nonce}'".into()).header(n).unwrap(), "script-src 'nonce-abc'");
    }
}
