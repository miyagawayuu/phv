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
use std::path::Path;
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
}

impl Default for Security {
    fn default() -> Self {
        Security { csrf: true, secure_cookies: false, session_ttl: default_ttl(), memory_limit_mb: default_memory() }
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
            dependencies: BTreeMap::new(),
        }
    }
}

fn check_dir(kind: &str, d: &str) -> Result<(), String> {
    let p = Path::new(d);
    let ok = !d.is_empty()
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
    if ok {
        Ok(())
    } else {
        Err(format!("[permissions].{kind}: \"{d}\" must be a directory inside the project (no `..`, not absolute)"))
    }
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
    }
    for (name, dep) in &file.dependencies {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.starts_with("std") {
            return Err(format!("[dependencies]: invalid package name \"{name}\""));
        }
        if dep.path.is_some() == dep.git.is_some() {
            return Err(format!("[dependencies].{name}: give exactly one of `path` or `git`"));
        }
    }
    let permissions = Permissions {
        read: file.permissions.read,
        write: file.permissions.write,
        env: file.permissions.env,
    };
    Ok(Config {
        name: file.project.name,
        host: Arc::new(Host { root: root.to_path_buf(), permissions }),
        csrf: file.security.csrf,
        secure_cookies: file.security.secure_cookies,
        session_ttl: file.security.session_ttl,
        memory_limit_mb: file.security.memory_limit_mb,
        dependencies: file.dependencies,
    })
}
