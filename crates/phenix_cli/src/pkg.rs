//! `phenix install`: fetches the `[dependencies]` of phenix.toml.
//!
//! - `path` packages are used in place.
//! - `git` packages are cloned into `vendor/<name>` and pinned to an exact
//!   commit in `phenix.lock`; later installs check out the locked commit.
//! - Packages are plain Phenix source. Installing never runs package code.

use phenix_driver::load_config;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, ExitCode};

#[derive(Serialize, Deserialize, Default)]
struct Lock {
    #[serde(default, rename = "package")]
    packages: Vec<Locked>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct Locked {
    name: String,
    git: String,
    rev: String,
    commit: String,
}

/// Only plain transports; refuses `ext::` and option-looking URLs.
fn safe_git_url(url: &str) -> bool {
    let ok_scheme = url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.starts_with("file://")
        || (url.contains('@') && url.contains(':') && !url.contains("://"));
    ok_scheme && !url.starts_with('-') && !url.contains("::") && !url.chars().any(|c| c.is_control() || c == ' ')
}

/// A tag, branch or commit name that cannot be mistaken for a git option.
fn safe_rev(rev: &str) -> bool {
    !rev.is_empty()
        && !rev.starts_with('-')
        && rev.len() <= 200
        && rev.chars().all(|c| c.is_ascii_alphanumeric() || "._/-+".contains(c))
}

fn is_commit_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn git(args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    // Hooks, fsmonitor commands and unusual transports never run; repositories
    // are always fresh clones, so no repository-local config is trusted.
    let no_hooks = std::env::temp_dir().join("phenix-empty-hooks");
    let _ = std::fs::create_dir_all(&no_hooks);
    let hooks = format!("core.hooksPath={}", no_hooks.display());
    let mut cmd = Command::new("git");
    cmd.args(["-c", &hooks, "-c", "core.fsmonitor=false", "-c", "advice.detachedHead=false"]);
    cmd.args(["-c", "protocol.allow=never", "-c", "protocol.https.allow=always", "-c", "protocol.ssh.allow=always"]);
    cmd.args(["-c", "protocol.file.allow=always", "-c", "submodule.recurse=false"]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn install(dir: &Path) -> ExitCode {
    let config = match load_config(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if config.dependencies.is_empty() {
        println!("no [dependencies] in phenix.toml");
        return ExitCode::SUCCESS;
    }
    let lock_path = dir.join("phenix.lock");
    let old: Lock = std::fs::read_to_string(&lock_path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default();
    let mut new = Lock::default();
    let mut failed = false;
    for (name, dep) in &config.dependencies {
        if let Some(p) = &dep.path {
            let full = dir.join(p);
            if full.is_dir() {
                println!("  {name}: using {}", full.display());
            } else {
                eprintln!("error: {name}: directory {} does not exist", full.display());
                failed = true;
            }
            continue;
        }
        let url = dep.git.clone().unwrap_or_default();
        if !safe_git_url(&url) {
            eprintln!("error: {name}: refusing git URL \"{url}\" (use https://, ssh:// or file://)");
            failed = true;
            continue;
        }
        let rev = dep.rev.clone().unwrap_or_else(|| "HEAD".into());
        if !safe_rev(&rev) {
            eprintln!("error: {name}: invalid rev \"{rev}\"");
            failed = true;
            continue;
        }
        let locked = old.packages.iter().find(|l| l.name == *name && l.git == url && l.rev == rev).cloned();
        if let Some(l) = &locked {
            if !is_commit_hash(&l.commit) {
                eprintln!("error: {name}: phenix.lock has an invalid commit \"{}\"", l.commit);
                failed = true;
                continue;
            }
        }
        let dest = dir.join("vendor").join(name);
        let tmp = dir.join("vendor").join(format!(".tmp-{name}-{}", std::process::id()));
        let result = (|| -> Result<String, String> {
            std::fs::create_dir_all(dir.join("vendor")).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_dir_all(&tmp);
            git(&["clone", "--quiet", "--no-checkout", "--", &url, &tmp.to_string_lossy()], None)?;
            let target = match &locked {
                Some(l) => l.commit.clone(),
                None if rev == "HEAD" => "origin/HEAD".into(),
                None => rev.clone(),
            };
            git(&["checkout", "--quiet", "--detach", &target, "--"], Some(&tmp)).or_else(|_| {
                git(&["checkout", "--quiet", "--detach", &format!("origin/{target}"), "--"], Some(&tmp))
            })?;
            let commit = git(&["rev-parse", "HEAD"], Some(&tmp))?;
            if let Some(l) = &locked {
                if l.commit != commit {
                    return Err(format!("checked out {commit} but phenix.lock pins {}", l.commit));
                }
            }
            // Replace whatever was in vendor/<name> (it is generated by this command).
            if dest.exists() {
                std::fs::remove_dir_all(&dest).map_err(|e| format!("cannot replace {}: {e}", dest.display()))?;
            }
            std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
            Ok(commit)
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        match result {
            Ok(commit) => {
                let how = if locked.is_some() { "locked" } else { "resolved" };
                println!("  {name}: {how} at {}", &commit[..commit.len().min(12)]);
                new.packages.push(Locked { name: name.clone(), git: url, rev, commit });
            }
            Err(e) => {
                eprintln!("error: {name}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        return ExitCode::FAILURE;
    }
    if !new.packages.is_empty() {
        let text = format!(
            "# Generated by `phenix install`. Commit this file.\n{}",
            toml::to_string(&new).unwrap_or_default()
        );
        if let Err(e) = std::fs::write(&lock_path, text) {
            eprintln!("error: cannot write phenix.lock: {e}");
            return ExitCode::FAILURE;
        }
    }
    println!("{} package(s) ready", config.dependencies.len());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_dangerous_urls() {
        assert!(safe_git_url("https://github.com/a/b.git"));
        assert!(safe_git_url("git@github.com:a/b.git"));
        assert!(!safe_git_url("ext::sh -c touch% /tmp/pwned"));
        assert!(!safe_git_url("--upload-pack=evil"));
        assert!(!safe_git_url("http://insecure.example/x.git"));
        assert!(safe_rev("v1.2.0") && safe_rev("main") && !safe_rev("--recurse-submodules") && !safe_rev("a b"));
        assert!(is_commit_hash(&"a".repeat(40)) && !is_commit_hash("--recurse-submodules"));
    }
}
