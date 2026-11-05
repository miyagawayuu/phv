//! Pages with parameters in their path: `public/posts/[id].phv` answers
//! `/posts/42` with the parameter `id` = `42`, `public/users/[id]/edit.phv`
//! answers `/users/7/edit`, and `public/docs/[...path].phv` takes the rest of
//! the path (`/docs/a/b` gives `path` = `a/b`).
//!
//! A file whose exact name matches the path always wins. Otherwise the most
//! specific route does: segment by segment from the left, a literal name
//! before a `[param]` before a `[...rest]`. Two routes that would match the
//! same paths are an error.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Lit(String),
    Param(String),
    Rest(String),
}

impl Seg {
    fn rank(&self) -> u8 {
        match self {
            Seg::Lit(_) => 0,
            Seg::Param(_) => 1,
            Seg::Rest(_) => 2,
        }
    }
}

#[derive(Debug)]
struct Route {
    segs: Vec<Seg>,
    /// The page, as a `/`-separated path under `public/`.
    file: String,
}

/// The pages of a site that have parameters, most specific first, and the
/// WebSocket scripts (`*.ws.phv`) that have them.
#[derive(Debug, Default)]
pub struct Table {
    routes: Vec<Route>,
    ws: Vec<Route>,
}

/// A WebSocket script: `chat.ws.phv`.
pub fn is_ws(file: &str) -> bool {
    file.to_ascii_lowercase().ends_with(".ws.phv")
}

/// Directories deep and pages many that a site may have (a scan of `public/`
/// stops with an error past them instead of walking an endless tree).
const MAX_DEPTH: usize = 32;
const MAX_PAGES: usize = 20_000;

impl Table {
    /// Builds the table from the pages of a site (paths under `public/`);
    /// only those with a `[` in their path are routes with parameters.
    pub fn from_pages<'a>(pages: impl Iterator<Item = &'a str>) -> Result<Table, String> {
        let mut routes = Vec::new();
        let mut ws = Vec::new();
        let mut shapes: HashMap<(bool, Vec<String>), String> = HashMap::new();
        for file in pages {
            if !file.contains('[') {
                continue;
            }
            let segs = parse(file)?;
            // Routes with the same shape match the same paths.
            let shape: Vec<String> = segs
                .iter()
                .map(|s| match s {
                    Seg::Lit(l) => format!("={l}"),
                    Seg::Param(_) => "*".into(),
                    Seg::Rest(_) => "**".into(),
                })
                .collect();
            if let Some(other) = shapes.insert((is_ws(file), shape), file.to_string()) {
                return Err(format!("public/{other} and public/{file} would answer the same paths"));
            }
            let route = Route { segs, file: file.to_string() };
            if is_ws(file) {
                ws.push(route);
            } else {
                routes.push(route);
            }
        }
        for list in [&mut routes, &mut ws] {
            list.sort_by(|a, b| {
                let ka: Vec<u8> = a.segs.iter().map(Seg::rank).collect();
                let kb: Vec<u8> = b.segs.iter().map(Seg::rank).collect();
                // At the first difference the more specific kind wins; then the longer route.
                ka.iter().zip(&kb).find(|(x, y)| x != y).map_or(kb.len().cmp(&ka.len()), |(x, y)| x.cmp(y))
            });
        }
        Ok(Table { routes, ws })
    }

    /// Scans `public/` for pages with parameters. Hidden (`.x`) and private
    /// (`_x`) files and directories, templates and links are left out.
    pub fn scan(public: &Path) -> Result<Table, String> {
        let mut pages = Vec::new();
        walk(public, String::new(), 0, &mut pages)?;
        Table::from_pages(pages.iter().map(String::as_str))
    }

    /// The page for a path (its segments, decoded) and its parameters.
    pub fn find(&self, segments: &[&str]) -> Option<(&str, Vec<(String, String)>)> {
        find_in(&self.routes, segments)
    }

    /// The WebSocket script for a path, and its parameters.
    pub fn find_ws(&self, segments: &[&str]) -> Option<(&str, Vec<(String, String)>)> {
        find_in(&self.ws, segments)
    }
}

fn find_in<'t>(routes: &'t [Route], segments: &[&str]) -> Option<(&'t str, Vec<(String, String)>)> {
    'routes: for r in routes {
        let mut params = Vec::new();
        for (i, seg) in r.segs.iter().enumerate() {
            match seg {
                Seg::Rest(name) => {
                    if i >= segments.len() {
                        continue 'routes;
                    }
                    params.push((name.clone(), segments[i..].join("/")));
                    return Some((&r.file, params));
                }
                _ if i >= segments.len() => continue 'routes,
                Seg::Lit(l) if l != segments[i] => continue 'routes,
                Seg::Lit(_) => {}
                Seg::Param(name) => params.push((name.clone(), segments[i].to_string())),
            }
        }
        if r.segs.len() == segments.len() {
            return Some((&r.file, params));
        }
    }
    None
}

/// `posts/[id].phv` → `posts`, `[id]`; `index.phv` names its directory
/// (`rooms/[id].ws.phv` the same way).
fn parse(file: &str) -> Result<Vec<Seg>, String> {
    let stem = if is_ws(file) { &file[..file.len() - 7] } else { file.strip_suffix(".phv").unwrap_or(file) };
    let mut parts: Vec<&str> = stem.split('/').collect();
    if parts.last() == Some(&"index") {
        parts.pop();
    }
    let mut segs = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let bad = || format!("public/{file}: `{p}` is not a route segment; write `[name]` or `[...name]`");
        let seg = if let Some(inner) = p.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            let (rest, name) = match inner.strip_prefix("...") {
                Some(n) => (true, n),
                None => (false, inner),
            };
            let ident = !name.is_empty()
                && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ident {
                return Err(bad());
            }
            if names.contains(&name) {
                return Err(format!("public/{file}: the parameter `{name}` appears twice"));
            }
            names.push(name);
            if rest && i + 1 != parts.len() {
                return Err(format!("public/{file}: `[...{name}]` must be the last part of the path"));
            }
            if rest {
                Seg::Rest(name.into())
            } else {
                Seg::Param(name.into())
            }
        } else if p.contains(['[', ']']) {
            return Err(bad());
        } else {
            Seg::Lit(p.to_string())
        };
        segs.push(seg);
    }
    Ok(segs)
}

fn walk(dir: &Path, prefix: String, depth: usize, out: &mut Vec<String>) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("public/{prefix} is nested more than {MAX_DEPTH} directories deep"));
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), rel, depth + 1, out)?;
        } else if ft.is_file() {
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".phv") && rel.contains('[') {
                if out.len() >= MAX_PAGES {
                    return Err(format!("public/ has more than {MAX_PAGES} pages with parameters"));
                }
                out.push(rel);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pages: &[&str]) -> Table {
        Table::from_pages(pages.iter().copied()).unwrap()
    }

    #[test]
    fn the_most_specific_route_wins() {
        let t = table(&[
            "posts/[id].phv",
            "posts/[id]/edit.phv",
            "[...path].phv",
            "users/[name]/index.phv",
            "docs/[...page].phv",
            "posts/new/[draft].phv",
        ]);
        let find = |p: &str| {
            let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
            t.find(&segs).map(|(f, ps)| (f.to_string(), ps))
        };
        assert_eq!(find("/posts/42"), Some(("posts/[id].phv".into(), vec![("id".into(), "42".into())])));
        assert_eq!(find("/posts/42/edit").unwrap().0, "posts/[id]/edit.phv");
        assert_eq!(find("/posts/new/x").unwrap().0, "posts/new/[draft].phv");
        assert_eq!(find("/users/ann"), Some(("users/[name]/index.phv".into(), vec![("name".into(), "ann".into())])));
        assert_eq!(find("/docs/a/b"), Some(("docs/[...page].phv".into(), vec![("page".into(), "a/b".into())])));
        assert_eq!(find("/anything/else").unwrap().0, "[...path].phv");
        assert_eq!(find("/"), None);
    }

    #[test]
    fn bad_routes_are_errors() {
        for (pages, msg) in [
            (vec!["a/[id].phv", "a/[slug].phv"], "would answer the same paths"),
            (vec!["a/[id]/index.phv", "a/[x].phv"], "would answer the same paths"),
            (vec!["[...a]/b.phv"], "must be the last part"),
            (vec!["[a-b].phv"], "is not a route segment"),
            (vec!["x[id].phv"], "is not a route segment"),
            (vec!["[id]/[id].phv"], "appears twice"),
        ] {
            let e = Table::from_pages(pages.iter().copied()).unwrap_err();
            assert!(e.contains(msg), "{pages:?}: {e}");
        }
    }
}
