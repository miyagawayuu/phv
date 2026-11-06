//! `phv install`: fetches the `[dependencies]` of phv.toml, and theirs.
//!
//! - `path` packages are used in place. Only the project's own phv.toml may
//!   name them.
//! - `git` packages, and packages of the registry (`name = "^1.2"`), are
//!   cloned into `vendor/<name>` and pinned to an exact commit in `phv.lock`;
//!   later installs check out the locked commit.
//! - A package's own `[dependencies]` are installed too, into `vendor/`. Each
//!   package is installed once: for a `version` requirement, at the newest
//!   `vX.Y.Z` tag that every package asking for it accepts.
//! - The registry is a git repository of `index/<name>.toml` files
//!   (`git = "<url>"`); `PHV_REGISTRY` names another one.
//! - Packages are plain PHV source. Installing never runs package code.

use phv_driver::config::{check_dependencies, Dependency};
use phv_driver::load_config;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// The registry used unless `PHV_REGISTRY` names another.
const REGISTRY: &str = "https://github.com/miyagawayuu/phv-packages.git";

#[derive(Serialize, Deserialize, Default, PartialEq)]
struct Lock {
    #[serde(default, rename = "package")]
    packages: Vec<Locked>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct Locked {
    name: String,
    git: String,
    /// The `rev` asked for (git packages pinned by rev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
    /// The version installed (packages chosen by `version`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    commit: String,
    /// The packages this one depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dependencies: Vec<String>,
}

/// Only plain transports; refuses `ext::`, option-looking URLs, `file://`
/// URLs naming another machine (`file:////host/share`, which Windows would
/// reach over SMB), and characters that a URL does not need (so that a URL
/// copied into phv.lock or a shell is plain text).
fn safe_git_url(url: &str) -> bool {
    let ok_scheme = url.starts_with("https://")
        || url.starts_with("ssh://")
        || (url.starts_with("file:///") && !url.starts_with("file:////") && !url.contains('\\'))
        || (url.contains('@') && url.contains(':') && !url.contains("://"));
    ok_scheme
        && !url.starts_with('-')
        && !url.contains("::")
        && url.len() <= 500
        && url.chars().all(|c| c.is_ascii_alphanumeric() || "-._~:/@%+".contains(c))
}

/// What the packages of packages and registry entries may name: public
/// repositories over https. Other transports (ssh offers the user's keys,
/// `file://` reads the user's disk) are for the project's own phv.toml.
/// A local registry (`PHV_REGISTRY=file:///...`, for offline mirrors and
/// tests) may also name local repositories.
fn public_git_url(url: &str) -> bool {
    let local_registry = std::env::var("PHV_REGISTRY").is_ok_and(|r| r.starts_with("file:///"));
    (url.starts_with("https://") || (local_registry && url.starts_with("file:///"))) && safe_git_url(url)
}

/// True for a symbolic link, a junction or another reparse point: the
/// installer never writes through one (a repository could commit `vendor`
/// or `phv.lock` as a link to somewhere else).
fn is_link(p: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return false };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Packages one install may bring in.
const MAX_PACKAGES: usize = 500;

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

/// An empty directory only this process created and only this user can write:
/// git's hooks are looked up in it, so none run. A fixed name in a shared
/// temporary directory could be created first, with hooks in it, by another
/// user; this one has a random name, is created new (never reused) and is
/// removed when dropped.
struct NoHooks(PathBuf);

impl NoHooks {
    fn new() -> Result<NoHooks, String> {
        for _ in 0..16 {
            let mut r = [0u8; 16];
            getrandom::fill(&mut r).map_err(|e| format!("cannot make a temporary directory: {e}"))?;
            let name: String = r.iter().map(|b| format!("{b:02x}")).collect();
            let dir = std::env::temp_dir().join(format!("phv-no-hooks-{name}"));
            #[allow(unused_mut)] // the mode is set on Unix only
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&dir) {
                Ok(()) => return Ok(NoHooks(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot make a temporary directory: {e}")),
            }
        }
        Err("cannot make a temporary directory".into())
    }
}

impl Drop for NoHooks {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn git(args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    // Hooks, fsmonitor commands and unusual transports never run; repositories
    // are always fresh clones, so no repository-local config is trusted.
    let no_hooks = NoHooks::new()?;
    let hooks = format!("core.hooksPath={}", no_hooks.0.display());
    let mut cmd = Command::new("git");
    // Git runs in its own empty directory and never looks above it: in the
    // project's directory it would find a repository there (a project laid
    // out as a bare repository, `HEAD`, `config`, `objects/`, `refs/`) and
    // read that repository's config, which can turn on transports that run
    // commands. Paths given to git are absolute (see `abs`).
    cmd.current_dir(&no_hooks.0);
    cmd.env("GIT_CEILING_DIRECTORIES", &no_hooks.0);
    for v in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY"] {
        cmd.env_remove(v);
    }
    cmd.args(["-c", &hooks, "-c", "core.fsmonitor=false", "-c", "advice.detachedHead=false"]);
    cmd.args(["-c", "protocol.allow=never", "-c", "protocol.https.allow=always", "-c", "protocol.ssh.allow=always"]);
    cmd.args(["-c", "protocol.file.allow=always", "-c", "submodule.recurse=false"]);
    // Links in a package are checked out as plain files holding the target's
    // name: a package cannot point at files outside itself.
    cmd.args(["-c", "core.symlinks=false"]);
    // No filter runs on a package's files: git-lfs (on by default in Git for
    // Windows) would read the package's `.lfsconfig` and contact any host it
    // names, over any transport (found by the 1.0.0 audit).
    cmd.args(["-c", "filter.lfs.process=", "-c", "filter.lfs.smudge=", "-c", "filter.lfs.clean="]);
    cmd.args(["-c", "filter.lfs.required=false"]);
    cmd.env("GIT_LFS_SKIP_SMUDGE", "1");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = dir {
        cmd.arg("-C").arg(abs(d));
    }
    cmd.args(args);
    let out = cmd.output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Gives a fresh clone attributes that come before the package's own
/// `.gitattributes`: no filter driver of the user's git configuration (such
/// as git-lfs, which contacts hosts a package names) runs on its files
/// (found by the 1.0.0 audit).
fn no_filters(clone: &Path) -> Result<(), String> {
    let info = clone.join(".git").join("info");
    std::fs::create_dir_all(&info)
        .and_then(|_| std::fs::write(info.join("attributes"), "* -filter -ident\n"))
        .map_err(|e| format!("cannot prepare the checkout: {e}"))
}

/// `p` as an absolute path: git runs in a directory of its own.
fn abs(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// What a project or a package asks for under one name.
#[derive(Clone)]
enum Want {
    Path(PathBuf),
    Rev { git: String, rev: String },
    Version { git: String, req: VersionReq, text: String },
}

/// One request, and who made it (`phv.toml`, or a package's name).
#[derive(Clone)]
struct Request {
    want: Want,
    by: String,
}

/// The package installed under a name.
#[derive(Clone, PartialEq)]
struct Chosen {
    git: Option<String>,
    rev: Option<String>,
    version: Option<Version>,
    commit: Option<String>,
}

struct Installer<'a> {
    root: &'a Path,
    old: Lock,
    locked_only: bool,
    /// The registry, fetched once per install.
    registry: Option<PathBuf>,
    tags: HashMap<String, Vec<(Version, String)>>,
    /// Commits that revs were checked out at during this install, by (git, rev).
    revs: HashMap<(String, String), String>,
}

impl Installer<'_> {
    /// The git URL of a registry package.
    fn registry_url(&mut self, name: &str) -> Result<String, String> {
        if self.registry.is_none() {
            let url = std::env::var("PHV_REGISTRY").unwrap_or_else(|_| REGISTRY.to_string());
            if !safe_git_url(&url) {
                return Err(format!("refusing registry URL \"{url}\""));
            }
            let dir = cache_dir().join(format!("registry-{}", &phv_std::sha256_hex(url.as_bytes())[..16]));
            if dir.join("HEAD").is_file() {
                git(&["fetch", "--quiet", "--force", "origin", "+refs/heads/*:refs/heads/*"], Some(&dir))
                    .map_err(|e| format!("cannot update the package registry: {e}"))?;
            } else {
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(dir.parent().unwrap_or(&dir)).map_err(|e| e.to_string())?;
                git(&["clone", "--quiet", "--bare", "--", &url, &abs(&dir).to_string_lossy()], None)
                    .map_err(|e| format!("cannot fetch the package registry: {e}"))?;
            }
            self.registry = Some(dir);
        }
        let dir = self.registry.clone().unwrap_or_default();
        let entry = git(&["show", &format!("HEAD:index/{name}.toml")], Some(&dir))
            .map_err(|_| format!("there is no package \"{name}\" in the registry"))?;
        #[derive(Deserialize)]
        struct Entry {
            git: String,
        }
        let e: Entry = toml::from_str(&entry).map_err(|_| format!("the registry entry of \"{name}\" is invalid"))?;
        if !public_git_url(&e.git) {
            return Err(format!("the registry entry of \"{name}\" does not give an https:// git URL"));
        }
        Ok(e.git)
    }

    /// The `vX.Y.Z` tags of a repository with their commits, newest first.
    fn versions(&mut self, url: &str) -> Result<Vec<(Version, String)>, String> {
        if let Some(v) = self.tags.get(url) {
            return Ok(v.clone());
        }
        let out = git(&["ls-remote", "--tags", "--", url], None)?;
        let mut tags: BTreeMap<String, String> = BTreeMap::new();
        for line in out.lines() {
            let Some((sha, name)) = line.split_once('\t') else { continue };
            let Some(tag) = name.strip_prefix("refs/tags/") else { continue };
            // Annotated tags: the `^{}` line gives the commit itself.
            match tag.strip_suffix("^{}") {
                Some(t) => {
                    tags.insert(t.to_string(), sha.to_string());
                }
                None => {
                    tags.entry(tag.to_string()).or_insert_with(|| sha.to_string());
                }
            }
        }
        let mut versions: Vec<(Version, String)> = tags
            .into_iter()
            .filter_map(|(t, sha)| Some((Version::parse(t.strip_prefix('v')?).ok()?, sha)))
            .filter(|(_, sha)| is_commit_hash(sha))
            .collect();
        versions.sort_by(|a, b| b.0.cmp(&a.0));
        self.tags.insert(url.to_string(), versions.clone());
        Ok(versions)
    }

    /// Decides what to install under `name` from all the requests for it.
    fn choose(&mut self, name: &str, reqs: &[Request]) -> Result<Chosen, String> {
        let who = |r: &Request| r.by.clone();
        match &reqs[0].want {
            Want::Path(p) => {
                if reqs.iter().any(|r| !matches!(&r.want, Want::Path(q) if q == p)) {
                    let by: Vec<String> = reqs.iter().map(who).collect();
                    return Err(format!("asked for in different ways by {}", by.join(", ")));
                }
                Ok(Chosen { git: None, rev: None, version: None, commit: None })
            }
            Want::Rev { git: url, rev } => {
                if reqs.iter().any(|r| !matches!(&r.want, Want::Rev { git: g, rev: v } if g == url && v == rev)) {
                    let by: Vec<String> = reqs.iter().map(who).collect();
                    return Err(format!("pinned to different sources or revs by {}", by.join(", ")));
                }
                let locked = self
                    .old
                    .packages
                    .iter()
                    .find(|l| l.name == name && l.git == *url && l.rev.as_deref() == Some(rev.as_str()));
                let commit =
                    locked.map(|l| l.commit.clone()).or_else(|| self.revs.get(&(url.clone(), rev.clone())).cloned());
                if self.locked_only && commit.is_none() {
                    return Err("not in phv.lock (run `phv install` without --locked)".into());
                }
                Ok(Chosen { git: Some(url.clone()), rev: Some(rev.clone()), version: None, commit })
            }
            Want::Version { git: url, .. } => {
                let mut reqs_v: Vec<(&VersionReq, &str, String)> = Vec::new();
                for r in reqs {
                    match &r.want {
                        Want::Version { git: g, req, text } if g == url => reqs_v.push((req, text, r.by.clone())),
                        _ => {
                            let by: Vec<String> = reqs.iter().map(who).collect();
                            return Err(format!("asked for from different sources by {}", by.join(", ")));
                        }
                    }
                }
                let fits = |v: &Version| reqs_v.iter().all(|(req, ..)| req.matches(v));
                // The locked version, while it still fits every request.
                let locked = self.old.packages.iter().find(|l| l.name == name && l.git == *url).and_then(|l| {
                    let v = Version::parse(l.version.as_deref()?).ok()?;
                    fits(&v).then(|| (v, l.commit.clone()))
                });
                let picked = match locked {
                    Some(l) => Some(l),
                    None if self.locked_only => {
                        return Err("not in phv.lock at a fitting version (run `phv install` without --locked)".into())
                    }
                    None => self.versions(url)?.into_iter().find(|(v, _)| fits(v)),
                };
                let Some((version, commit)) = picked else {
                    let asked: Vec<String> = reqs_v.iter().map(|(_, text, by)| format!("{text} ({by})")).collect();
                    return Err(format!("no version fits every requirement: {}", asked.join(", ")));
                };
                Ok(Chosen { git: Some(url.clone()), rev: None, version: Some(version), commit: Some(commit) })
            }
        }
    }

    /// What `dep` (written in the phv.toml of `by`) asks for.
    fn want(&mut self, name: &str, dep: &Dependency, by: &str) -> Result<Want, String> {
        if let Some(p) = &dep.path {
            if by != "phv.toml" {
                return Err(format!(
                    "package \"{by}\" names \"{name}\" by `path`; packages can only use git and the registry"
                ));
            }
            let dir = self.root.join(p);
            // Nothing is looked at there before this check: opening a folder on
            // another machine makes Windows contact the host with the user's
            // credentials (found by the 16th assessment; see `Loader::remote`).
            if phv_std::safefs::on_another_machine_below(self.root, &dir) {
                return Err(format!(
                    "the path \"{}\" is on another machine (a network share or drive, or a link to one); \
                     packages are read only from this machine",
                    phv_syntax::diag::printable_text(p)
                ));
            }
            return Ok(Want::Path(dir));
        }
        let url = match &dep.git {
            Some(u) => u.clone(),
            None => self.registry_url(name)?,
        };
        if !safe_git_url(&url) {
            return Err(format!("refusing git URL \"{url}\" (use https://, ssh:// or file:///)"));
        }
        if by != "phv.toml" && !public_git_url(&url) {
            return Err(format!("package \"{by}\" names \"{name}\" by a git URL that is not https://"));
        }
        if let Some(text) = &dep.version {
            let req = VersionReq::parse(text).map_err(|_| format!("invalid version requirement \"{text}\""))?;
            return Ok(Want::Version { git: url, req, text: text.clone() });
        }
        let rev = dep.rev.clone().unwrap_or_else(|| "HEAD".into());
        if !safe_rev(&rev) {
            return Err(format!("invalid rev \"{rev}\""));
        }
        Ok(Want::Rev { git: url, rev })
    }

    /// Clones `c` into `vendor/<name>` and returns the commit checked out.
    fn fetch(&self, name: &str, c: &Chosen) -> Result<String, String> {
        let url = c.git.clone().unwrap_or_default();
        let dest = self.root.join("vendor").join(name);
        let tmp = self.root.join("vendor").join(format!(".tmp-{name}-{}", std::process::id()));
        let result = (|| -> Result<String, String> {
            let vendor = self.root.join("vendor");
            if is_link(&vendor) || is_link(&dest) {
                return Err(
                    "vendor/ (or the package's directory in it) is a link; remove it and run `phv install` again"
                        .into(),
                );
            }
            std::fs::create_dir_all(&vendor).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_dir_all(&tmp);
            git(&["clone", "--quiet", "--no-checkout", "--", &url, &abs(&tmp).to_string_lossy()], None)?;
            no_filters(&tmp)?;
            let target = match (&c.commit, c.rev.as_deref()) {
                (Some(commit), _) => commit.clone(),
                (None, Some("HEAD") | None) => "origin/HEAD".into(),
                (None, Some(rev)) => rev.to_string(),
            };
            git(&["checkout", "--quiet", "--detach", &target, "--"], Some(&tmp)).or_else(|_| {
                git(&["checkout", "--quiet", "--detach", &format!("origin/{target}"), "--"], Some(&tmp))
            })?;
            let commit = git(&["rev-parse", "HEAD"], Some(&tmp))?;
            if let Some(want) = &c.commit {
                if *want != commit {
                    return Err(format!("checked out {commit} but expected {want}"));
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
        result
    }
}

/// Where the registry is kept between installs.
fn cache_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("phv")
}

/// The `[dependencies]` of the package in `dir` (none without a phv.toml).
/// Only a regular file of at most 1 MB is read, and its text is not echoed
/// in errors.
fn package_dependencies(dir: &Path) -> Result<BTreeMap<String, Dependency>, String> {
    let path = dir.join("phv.toml");
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return Ok(BTreeMap::new()) };
    if is_link(&path) || !meta.is_file() || meta.len() > 1 << 20 {
        return Err("its phv.toml is not a regular file of at most 1 MB".into());
    }
    let text = std::fs::read_to_string(&path).map_err(|_| "its phv.toml cannot be read".to_string())?;
    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        dependencies: BTreeMap<String, Dependency>,
    }
    let m: Manifest = toml::from_str(&text).map_err(|_| "its phv.toml is not valid TOML".to_string())?;
    check_dependencies(&m.dependencies)?;
    Ok(m.dependencies)
}

pub fn install(dir: &Path, locked_only: bool) -> ExitCode {
    let config = match load_config(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if config.dependencies.is_empty() {
        println!("no [dependencies] in phv.toml");
        return ExitCode::SUCCESS;
    }
    let lock_path = dir.join("phv.lock");
    if is_link(&lock_path) {
        eprintln!("error: phv.lock is a link; remove it and run `phv install` again");
        return ExitCode::FAILURE;
    }
    // At most 8 MB, like the sources: a committed lock file cannot fill the memory.
    let old: Lock = phv_driver::read_source(&lock_path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default();
    if let Some(bad) = old.packages.iter().find(|l| !is_commit_hash(&l.commit)) {
        eprintln!("error: {}: phv.lock has an invalid commit \"{}\"", bad.name, bad.commit);
        return ExitCode::FAILURE;
    }
    let mut ins = Installer { root: dir, old, locked_only, registry: None, tags: HashMap::new(), revs: HashMap::new() };
    match resolve(&mut ins, &config.dependencies) {
        Ok(new) => {
            if locked_only && new != ins.old {
                eprintln!("error: phv.lock is out of date (run `phv install` without --locked)");
                return ExitCode::FAILURE;
            }
            if !new.packages.is_empty() && new != ins.old {
                let text = format!(
                    "# Generated by `phv install`. Commit this file.\n{}",
                    toml::to_string(&new).unwrap_or_default()
                );
                // A new file with a random name (never a link a repository planted),
                // renamed over the old one.
                if let Err(e) = phv_driver::write_new_file(&lock_path, text.as_bytes()) {
                    eprintln!("error: cannot write phv.lock: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves and installs every package, and returns the new lock.
fn resolve(ins: &mut Installer, root_deps: &BTreeMap<String, Dependency>) -> Result<Lock, String> {
    let mut requests: BTreeMap<String, Vec<Request>> = BTreeMap::new();
    for (name, dep) in root_deps {
        let want = ins.want(name, dep, "phv.toml").map_err(|e| format!("{name}: {e}"))?;
        requests.entry(name.clone()).or_default().push(Request { want, by: "phv.toml".into() });
    }
    let mut chosen: BTreeMap<String, (Chosen, Vec<String>)> = BTreeMap::new();
    // Choosing a package can change what it asks for, so this repeats until
    // nothing changes (a few rounds at most for real dependency graphs).
    for _ in 0..50 {
        let mut changed = false;
        let names: Vec<String> = requests.keys().cloned().collect();
        for name in names {
            let reqs = requests[&name].clone();
            if reqs.is_empty() {
                if chosen.remove(&name).is_some() {
                    changed = true;
                }
                continue;
            }
            let pick = ins.choose(&name, &reqs).map_err(|e| format!("{name}: {e}"))?;
            if chosen.get(&name).is_some_and(|(c, ..)| *c == pick) {
                continue;
            }
            changed = true;
            let (commit, pkg_dir) = match &reqs[0].want {
                Want::Path(p) => {
                    if !p.is_dir() {
                        return Err(format!("{name}: directory {} does not exist", p.display()));
                    }
                    let shown = phv_syntax::diag::printable_text(&format!("  {name}: using {}", p.display()));
                    println!("{shown}");
                    (String::new(), p.clone())
                }
                _ => {
                    let commit = ins.fetch(&name, &pick).map_err(|e| format!("{name}: {e}"))?;
                    if let (Some(git), Some(rev)) = (&pick.git, &pick.rev) {
                        ins.revs.insert((git.clone(), rev.clone()), commit.clone());
                    }
                    let at = match &pick.version {
                        Some(v) => format!("v{v}"),
                        None => commit[..commit.len().min(12)].to_string(),
                    };
                    println!("{}", phv_syntax::diag::printable_text(&format!("  {name}: {at}")));
                    (commit, ins.root.join("vendor").join(&name))
                }
            };
            let deps = package_dependencies(&pkg_dir).map_err(|e| format!("{name}: {e}"))?;
            // What this package asked for before is replaced by what it asks for now.
            for list in requests.values_mut() {
                list.retain(|r| r.by != name);
            }
            if requests.len() + deps.len() > MAX_PACKAGES {
                return Err(format!("more than {MAX_PACKAGES} packages are needed"));
            }
            for (dname, dep) in &deps {
                if dname == &name {
                    return Err(format!("{name}: a package cannot depend on itself"));
                }
                let want = ins.want(dname, dep, &name).map_err(|e| format!("{dname} (for {name}): {e}"))?;
                requests.entry(dname.clone()).or_default().push(Request { want, by: name.clone() });
            }
            let pick = Chosen { commit: (!commit.is_empty()).then_some(commit), ..pick };
            chosen.insert(name.clone(), (pick, deps.keys().cloned().collect()));
        }
        if !changed {
            let mut lock = Lock::default();
            for (name, (c, deps)) in chosen {
                let (Some(git), Some(commit)) = (c.git, c.commit) else { continue };
                lock.packages.push(Locked {
                    name,
                    git,
                    rev: c.rev,
                    version: c.version.map(|v| v.to_string()),
                    commit,
                    dependencies: deps,
                });
            }
            let total = requests.values().filter(|r| !r.is_empty()).count();
            println!("{total} package(s) ready");
            return Ok(lock);
        }
    }
    Err("the dependency requirements do not settle".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Git never uses a repository it finds where `phv install` runs (the
    /// project, which may be laid out as a bare repository with a config that
    /// turns on transports that run commands). Tests run inside PHV's own
    /// repository: git must not see it.
    #[test]
    fn git_ignores_the_repository_it_runs_in() {
        if git(&["--version"], None).is_err() {
            return;
        }
        assert!(git(&["rev-parse", "--git-dir"], None).is_err(), "git found a repository around phv");
    }

    /// Each git command gets its own new directory for hooks (a fixed name in a
    /// shared temporary directory could be created first by another user).
    #[test]
    fn hooks_directories_are_private_and_new() {
        let a = NoHooks::new().unwrap();
        let b = NoHooks::new().unwrap();
        assert_ne!(a.0, b.0);
        assert!(std::fs::read_dir(&a.0).unwrap().next().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&a.0).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let path = a.0.clone();
        drop(a);
        assert!(!path.exists());
    }

    /// A package's `.gitattributes` cannot select a filter (git-lfs, or any
    /// driver of the user's configuration).
    #[test]
    fn packages_cannot_select_filters() {
        if git(&["--version"], None).is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("phv-filters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (src, clone) = (dir.join("src"), dir.join("clone"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        std::fs::write(src.join("a.bin"), "x").unwrap();
        let commit = ["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "x"];
        git(&["init", "-q"], Some(&src)).unwrap();
        git(&["add", "."], Some(&src)).unwrap();
        git(&commit, Some(&src)).unwrap();
        let url = abs(&src).to_string_lossy().into_owned();
        git(&["clone", "--quiet", "--no-checkout", "--", &url, &abs(&clone).to_string_lossy()], None).unwrap();
        no_filters(&clone).unwrap();
        let attr = git(&["check-attr", "filter", "--", "a.bin"], Some(&clone)).unwrap();
        assert!(attr.ends_with("unset"), "{attr}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Found by the 16th assessment: a `path` package that is a link to a
    /// share (or a share by name) was opened (`is_dir`, its phv.toml), which
    /// makes Windows contact the host. Making a link needs Developer Mode or
    /// the privilege: that part is skipped without.
    #[cfg(windows)]
    #[test]
    fn path_packages_on_another_machine_are_refused_before_they_are_opened() {
        let dir = std::env::temp_dir().join(format!("phv-path-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkgs/local")).unwrap();
        let mut ins = Installer {
            root: &dir,
            old: Lock::default(),
            locked_only: false,
            registry: None,
            tags: HashMap::new(),
            revs: HashMap::new(),
        };
        let path = |p: &str| Dependency { path: Some(p.into()), ..Default::default() };
        let start = std::time::Instant::now();
        for p in [r"\\phv-test.invalid\share\pkg", "//phv-test.invalid/share/pkg", r"\\?\UNC\phv-test.invalid\s\pkg"] {
            let e = ins.want("far", &path(p), "phv.toml").err().unwrap_or_default();
            assert!(e.contains("another machine"), "{p}: {e}");
        }
        assert!(matches!(ins.want("near", &path("pkgs/local"), "phv.toml"), Ok(Want::Path(_))));
        if std::os::windows::fs::symlink_dir(r"\\phv-test.invalid\share\pkg", dir.join("pkgs/far")).is_ok() {
            let e = ins.want("far", &path("pkgs/far"), "phv.toml").err().unwrap_or_default();
            assert!(e.contains("another machine"), "{e}");
            // Also behind a link to a folder of this machine that holds it.
            std::os::windows::fs::symlink_dir(dir.join("pkgs"), dir.join("via")).unwrap();
            let e = ins.want("far", &path("via/far"), "phv.toml").err().unwrap_or_default();
            assert!(e.contains("another machine"), "{e}");
            let _ = std::fs::remove_dir(dir.join("via"));
            let _ = std::fs::remove_dir(dir.join("pkgs/far"));
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
