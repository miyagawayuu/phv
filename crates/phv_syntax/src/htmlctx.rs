//! Tracks where in an HTML document a `<?= ?>` appears, so the type checker
//! can refuse insertions that HTML escaping does not make safe (unquoted
//! attributes, URLs, `<script>`, `<style>`, event handler attributes).
//!
//! The tracker follows the parts of the WHATWG tokenizer that decide where
//! text ends up: raw-text elements and their end tags, the escaped states of
//! `<script>`, comments (including `<!-->` and `--!>`), bogus comments, and
//! foreign content (`<svg>`, `<math>`). When it cannot be sure, it reports
//! the stricter context.

use crate::ast::EchoContext;

/// What is known about the start of a URL attribute's value.
#[derive(Clone, Debug, PartialEq)]
enum UrlState {
    /// Only this literal text so far; the scheme is not decided yet.
    Undecided(String),
    /// The scheme is decided (`https:`, a relative path, or an inserted safe URL).
    Safe,
    /// A `javascript:`, `vbscript:` or `data:` URL written in the template.
    Script,
}

impl UrlState {
    fn rank(&self) -> u8 {
        match self {
            UrlState::Safe => 0,
            UrlState::Undecided(_) => 1,
            UrlState::Script => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Text,
    Comment,
    /// `<!x ...>`, `<?x ...>`, `</ ...>`: ignored up to the next `>`.
    Bogus,
    /// Inside `<tag ...>` between attributes.
    Tag {
        name: String,
        closing: bool,
    },
    AttrName {
        tag: String,
        closing: bool,
        name: String,
    },
    /// After `name=`, before the value.
    BeforeValue {
        tag: String,
        closing: bool,
        name: String,
    },
    Quoted {
        tag: String,
        closing: bool,
        name: String,
        quote: char,
        /// Set for attributes whose value is a URL.
        url: Option<UrlState>,
    },
    Unquoted {
        tag: String,
        closing: bool,
        name: String,
    },
    /// Text of an element that ends only at its end tag. For `<script>`,
    /// `escape` is 1 after `<!--` and 2 in the "double escaped" state, where
    /// `</script>` does not end the script.
    Raw {
        tag: String,
        escape: u8,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct HtmlTracker {
    state: State,
    /// Text of a tag name being read after `<`.
    pending: Option<String>,
    /// Depth of `<svg>` / `<math>` (foreign content has no RCDATA elements).
    foreign: u32,
    /// Byte offset (in the chunk being fed) of the attribute name or value being read.
    mark: usize,
    /// Inline JavaScript found in the last `feed` (see `InlineCode`).
    found: Vec<(usize, InlineCode)>,
}

/// JavaScript written directly in template HTML, which a Content-Security-Policy
/// without `'unsafe-inline'` does not run (unlike `<script>`, which gets a nonce).
#[derive(Clone, Debug, PartialEq)]
pub enum InlineCode {
    /// An event handler attribute such as `onclick`.
    Handler(String),
    /// A `javascript:` (or `vbscript:`) URL.
    ScriptUrl,
}

impl Default for HtmlTracker {
    fn default() -> Self {
        HtmlTracker { state: State::Text, pending: None, foreign: 0, mark: 0, found: Vec::new() }
    }
}

const URL_ATTRS: &[&str] = &[
    "href",
    "src",
    "action",
    "formaction",
    "xlink:href",
    "poster",
    "data",
    "cite",
    "background",
    "ping",
    "srcset",
    "manifest",
    "codebase",
    "longdesc",
    "lowsrc",
    "dynsrc",
];

/// SVG animation elements can set `href` to their `to`/`from`/`values`.
const ANIMATION_TAGS: &[&str] = &["animate", "set", "animatemotion", "animatetransform"];

fn is_url_attr(tag: &str, name: &str) -> bool {
    URL_ATTRS.contains(&name)
        || (ANIMATION_TAGS.contains(&tag) && matches!(name, "to" | "from" | "values" | "by" | "attributename"))
}

/// Elements whose content is not parsed as markup, in HTML (not foreign) content.
const RAW_TAGS: &[&str] =
    &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript", "plaintext"];

/// Decides a URL from the literal text at its start. Browsers strip leading
/// spaces and controls and decode character references, so `&` leaves it
/// undecided (`&#106;avascript:` is `javascript:`).
fn decide_url(prefix: &str) -> Option<UrlState> {
    let mut scheme = String::new();
    for c in prefix.chars() {
        if c.is_ascii_whitespace() || c.is_control() {
            continue;
        }
        match c {
            '/' | '?' | '#' => return Some(UrlState::Safe),
            ':' => {
                let s = scheme.to_ascii_lowercase();
                return Some(if matches!(s.as_str(), "javascript" | "vbscript" | "data") {
                    UrlState::Script
                } else {
                    UrlState::Safe
                });
            }
            '&' => return None,
            c => scheme.push(c),
        }
    }
    None
}

fn is_delim(c: Option<&char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_whitespace() || *c == '/' || *c == '>')
}

impl HtmlTracker {
    /// The context a value inserted at the current position lands in.
    pub fn context(&self) -> EchoContext {
        if self.pending.is_some() {
            return EchoContext::Tag;
        }
        match &self.state {
            State::Text | State::Comment | State::Bogus => EchoContext::Text,
            State::Raw { tag, .. } if tag == "script" => EchoContext::Script,
            State::Raw { tag, .. } if tag == "style" => EchoContext::Style,
            State::Raw { .. } => EchoContext::Text,
            State::Tag { .. } | State::AttrName { .. } => EchoContext::Tag,
            State::BeforeValue { .. } | State::Unquoted { .. } => EchoContext::Unquoted,
            State::Quoted { name, url, .. } => {
                if name.starts_with("on") {
                    EchoContext::Handler
                } else if name == "style" {
                    EchoContext::Style
                } else if name == "srcdoc" {
                    EchoContext::Srcdoc
                } else {
                    match url {
                        Some(UrlState::Undecided(_)) => EchoContext::Url,
                        Some(UrlState::Script) => EchoContext::ScriptUrl,
                        _ => EchoContext::Attr,
                    }
                }
            }
        }
    }

    /// Inline JavaScript seen by the last `feed`, as (byte offset in that text, what).
    pub fn take_inline_code(&mut self) -> Vec<(usize, InlineCode)> {
        std::mem::take(&mut self.found)
    }

    /// Records that a value was inserted. In a URL attribute the inserted
    /// value (a checked `SafeUrl` or a number) decides the scheme.
    pub fn inserted(&mut self) {
        if let State::Quoted { url: Some(u @ UrlState::Undecided(_)), .. } = &mut self.state {
            *u = UrlState::Safe;
        }
    }

    /// After a block that may run zero or more times: true if it ended in the
    /// same place it started. The state is then merged with `entry`, keeping
    /// whichever URL start is less decided.
    pub fn end_block(&mut self, entry: &HtmlTracker) -> bool {
        // An attribute name or unquoted value in progress is "between attributes".
        fn norm(s: &State) -> State {
            match s {
                State::AttrName { tag, closing, .. } | State::Unquoted { tag, closing, .. } => {
                    State::Tag { name: tag.clone(), closing: *closing }
                }
                s => s.clone(),
            }
        }
        let same = match (&mut self.state, &entry.state) {
            (
                State::Quoted { tag: t1, closing: c1, name: n1, quote: q1, url: u1 },
                State::Quoted { tag: t2, closing: c2, name: n2, quote: q2, url: u2 },
            ) => {
                let same = t1 == t2 && c1 == c2 && n1 == n2 && q1 == q2 && u1.is_some() == u2.is_some();
                if same {
                    if let (Some(a), Some(b)) = (u1.as_ref(), u2.as_ref()) {
                        if b.rank() > a.rank() {
                            *u1 = Some(b.clone());
                        }
                    }
                }
                same
            }
            (a, b) => {
                let same = norm(a) == norm(b);
                if same {
                    *a = b.clone();
                }
                same
            }
        };
        same && self.pending == entry.pending && self.foreign == entry.foreign
    }

    fn end_tag(&mut self, tag: String, closing: bool) {
        if matches!(tag.as_str(), "svg" | "math") {
            self.foreign = if closing { self.foreign.saturating_sub(1) } else { self.foreign + 1 };
        }
        let raw = if self.foreign > 0 { tag == "script" || tag == "style" } else { RAW_TAGS.contains(&tag.as_str()) };
        self.state = if !closing && raw { State::Raw { tag, escape: 0 } } else { State::Text };
    }

    /// Advances over literal HTML. Returns the byte offsets just after the
    /// name of each opening `<script` tag, where the server's CSP nonce goes.
    pub fn feed(&mut self, text: &str) -> Vec<usize> {
        let chars: Vec<char> = text.chars().collect();
        let offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        let starts = |i: usize, s: &str| -> bool {
            let n = s.chars().count();
            i + n <= chars.len() && chars[i..i + n].iter().collect::<String>().eq_ignore_ascii_case(s)
        };
        let mut scripts = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if let Some(name) = &mut self.pending {
                if c.is_ascii_alphanumeric() || c == '-' || c == ':' {
                    name.push(c.to_ascii_lowercase());
                    i += 1;
                    continue;
                }
                let raw = std::mem::take(name);
                self.pending = None;
                let (closing, tag) = match raw.strip_prefix('/') {
                    Some(t) => (true, t.to_string()),
                    None => (false, raw),
                };
                if !closing && tag == "script" {
                    scripts.push(offsets[i]);
                }
                self.state = State::Tag { name: tag, closing };
                continue;
            }
            let state = std::mem::replace(&mut self.state, State::Text);
            self.state = match state {
                State::Text => {
                    if c != '<' {
                        State::Text
                    } else if starts(i, "<!--") {
                        // `<!-->` and `<!--->` are complete (empty) comments.
                        if chars.get(i + 4) == Some(&'>') {
                            i += 4;
                            State::Text
                        } else if starts(i + 4, "->") {
                            i += 5;
                            State::Text
                        } else {
                            i += 3;
                            State::Comment
                        }
                    } else {
                        match chars.get(i + 1) {
                            Some(n) if n.is_ascii_alphabetic() => {
                                self.pending = Some(String::new());
                                State::Text
                            }
                            Some('/') => match chars.get(i + 2) {
                                Some(n) if n.is_ascii_alphabetic() => {
                                    self.pending = Some("/".into());
                                    i += 1;
                                    State::Text
                                }
                                Some('>') => {
                                    i += 2;
                                    State::Text
                                }
                                _ => {
                                    i += 1;
                                    State::Bogus
                                }
                            },
                            Some('!') | Some('?') => State::Bogus,
                            _ => State::Text,
                        }
                    }
                }
                State::Comment => {
                    if starts(i, "-->") {
                        i += 2;
                        State::Text
                    } else if starts(i, "--!>") {
                        i += 3;
                        State::Text
                    } else {
                        State::Comment
                    }
                }
                State::Bogus => {
                    if c == '>' {
                        State::Text
                    } else {
                        State::Bogus
                    }
                }
                State::Raw { tag, escape } => {
                    let end = format!("</{tag}");
                    let n = end.len();
                    if starts(i, &end) && is_delim(chars.get(i + n)) {
                        if tag == "script" && escape == 2 {
                            i += n - 1;
                            State::Raw { tag, escape: 1 }
                        } else {
                            i += n - 1;
                            State::Tag { name: tag, closing: true }
                        }
                    } else if tag != "script" {
                        State::Raw { tag, escape }
                    } else if escape == 0 && starts(i, "<!--") {
                        i += 3;
                        State::Raw { tag, escape: 1 }
                    } else if escape > 0 && starts(i, "-->") {
                        i += 2;
                        State::Raw { tag, escape: 0 }
                    } else if escape == 1 && starts(i, "<script") && is_delim(chars.get(i + 7)) {
                        i += 6;
                        State::Raw { tag, escape: 2 }
                    } else {
                        State::Raw { tag, escape }
                    }
                }
                State::Tag { name, closing } => {
                    if c == '>' {
                        self.end_tag(name, closing);
                        i += 1;
                        continue;
                    } else if c.is_whitespace() || c == '/' {
                        State::Tag { name, closing }
                    } else {
                        self.mark = offsets[i];
                        State::AttrName { tag: name, closing, name: c.to_ascii_lowercase().to_string() }
                    }
                }
                State::AttrName { tag, closing, mut name } => {
                    if c == '=' {
                        // Event handler names are letters only (`onclick`, not `onclick-free`).
                        let handler =
                            name.len() > 2 && name.starts_with("on") && name.bytes().all(|b| b.is_ascii_lowercase());
                        if handler && !closing {
                            self.found.push((self.mark, InlineCode::Handler(name.clone())));
                        }
                        State::BeforeValue { tag, closing, name }
                    } else if c == '>' {
                        self.end_tag(tag, closing);
                        i += 1;
                        continue;
                    } else if c.is_whitespace() || c == '/' {
                        // `name =value` is rare; a space ends a valueless attribute
                        // unless `=` follows.
                        let next = chars[i..].iter().find(|c| !c.is_whitespace());
                        if next == Some(&'=') {
                            State::AttrName { tag, closing, name }
                        } else {
                            State::Tag { name: tag, closing }
                        }
                    } else {
                        name.push(c.to_ascii_lowercase());
                        State::AttrName { tag, closing, name }
                    }
                }
                State::BeforeValue { tag, closing, name } => {
                    if c == '"' || c == '\'' {
                        let url = is_url_attr(&tag, &name).then(|| UrlState::Undecided(String::new()));
                        self.mark = offsets[i];
                        State::Quoted { tag, closing, name, quote: c, url }
                    } else if c.is_whitespace() {
                        State::BeforeValue { tag, closing, name }
                    } else if c == '>' {
                        self.end_tag(tag, closing);
                        i += 1;
                        continue;
                    } else {
                        State::Unquoted { tag, closing, name }
                    }
                }
                State::Quoted { tag, closing, name, quote, mut url } => {
                    if c == quote {
                        State::Tag { name: tag, closing }
                    } else {
                        if let Some(UrlState::Undecided(p)) = &mut url {
                            if p.len() < 256 {
                                p.push(c);
                            }
                            if let Some(d) = decide_url(p) {
                                // `data:` is fine for images; only script URLs are inline code.
                                let scheme: String =
                                    p.chars().filter(|c| !c.is_ascii_whitespace() && !c.is_control()).collect();
                                let scheme = scheme.to_ascii_lowercase();
                                if d == UrlState::Script
                                    && (scheme.starts_with("javascript:") || scheme.starts_with("vbscript:"))
                                {
                                    self.found.push((self.mark, InlineCode::ScriptUrl));
                                }
                                url = Some(d);
                            }
                        }
                        State::Quoted { tag, closing, name, quote, url }
                    }
                }
                State::Unquoted { tag, closing, name } => {
                    if c.is_whitespace() {
                        State::Tag { name: tag, closing }
                    } else if c == '>' {
                        self.end_tag(tag, closing);
                        i += 1;
                        continue;
                    } else {
                        State::Unquoted { tag, closing, name }
                    }
                }
            };
            i += 1;
        }
        scripts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(html: &str) -> EchoContext {
        let mut t = HtmlTracker::default();
        t.feed(html);
        t.context()
    }

    #[test]
    fn finds_contexts() {
        assert_eq!(ctx("<p>Hello "), EchoContext::Text);
        assert_eq!(ctx("<a title=\"x "), EchoContext::Attr);
        assert_eq!(ctx("<a href=\""), EchoContext::Url);
        assert_eq!(ctx("<a href=\"/post?id="), EchoContext::Attr);
        assert_eq!(ctx("<a href="), EchoContext::Unquoted);
        assert_eq!(ctx("<img src=x alt="), EchoContext::Unquoted);
        assert_eq!(ctx("<div "), EchoContext::Tag);
        assert_eq!(ctx("<button onclick=\"go("), EchoContext::Handler);
        assert_eq!(ctx("<p style=\"color:"), EchoContext::Style);
        assert_eq!(ctx("<script>var x = "), EchoContext::Script);
        assert_eq!(ctx("<script>var x = 1;</script><p>"), EchoContext::Text);
        assert_eq!(ctx("<style>p { color: "), EchoContext::Style);
        assert_eq!(ctx("<!-- <script> --><p>"), EchoContext::Text);
        assert_eq!(ctx("<input value='a' disabled>"), EchoContext::Text);
        assert_eq!(ctx("<p>1 < 2 and "), EchoContext::Text);
    }

    #[test]
    fn follows_the_browser_tokenizer() {
        // An end tag needs a delimiter after its name.
        assert_eq!(ctx("<script>var a = \"</scripts>\"; var b = "), EchoContext::Script);
        // Comments that browsers close early.
        assert_eq!(ctx("<!--><script>var c = "), EchoContext::Script);
        assert_eq!(ctx("<!---><script>var c = "), EchoContext::Script);
        assert_eq!(ctx("<!-- --!><script>var d = "), EchoContext::Script);
        // `<!--<script>` inside a script: `</script>` does not end it.
        assert_eq!(ctx("<script><!--<script></script>var e = "), EchoContext::Script);
        assert_eq!(ctx("<script><!--<script></script>--></script><p>"), EchoContext::Text);
        // Raw-text and RCDATA elements hide markup.
        assert_eq!(ctx("<textarea><a title=\"</textarea><script>var f = "), EchoContext::Script);
        assert_eq!(ctx("<title><p title=\"</title><p>"), EchoContext::Text);
        // In SVG, <title> is parsed as markup.
        assert_eq!(ctx("<svg><title><script>var g = "), EchoContext::Script);
        assert_eq!(ctx("<svg></svg><title><script>"), EchoContext::Text);
        // Bogus comments end at the first `>`.
        assert_eq!(ctx("<!DOCTYPE html><p>"), EchoContext::Text);
        assert_eq!(ctx("<? x ><script>var h = "), EchoContext::Script);
        // srcdoc is a document of its own.
        assert_eq!(ctx("<iframe srcdoc=\""), EchoContext::Srcdoc);
    }

    #[test]
    fn decides_url_starts_from_literal_text() {
        assert_eq!(ctx("<a href=\" "), EchoContext::Url);
        assert_eq!(ctx("<a href=\"&#x20;"), EchoContext::Url);
        assert_eq!(ctx("<a href=\"j"), EchoContext::Url);
        assert_eq!(ctx("<a href=\"&#106;"), EchoContext::Url);
        assert_eq!(ctx("<a href=\"https://example.com/?q="), EchoContext::Attr);
        assert_eq!(ctx("<a href=\"mailto:"), EchoContext::Attr);
        assert_eq!(ctx("<a href=\"#"), EchoContext::Attr);
        assert_eq!(ctx("<a href=\" JavaScript:go("), EchoContext::ScriptUrl);
        assert_eq!(ctx("<a href=\"data:text/html,"), EchoContext::ScriptUrl);
        assert_eq!(ctx("<svg><animate attributeName=\"href\" values=\""), EchoContext::Url);
        let mut t = HtmlTracker::default();
        t.feed("<a href=\"");
        t.inserted();
        t.feed("?page=");
        assert_eq!(t.context(), EchoContext::Attr);
    }

    #[test]
    fn finds_inline_code() {
        let mut t = HtmlTracker::default();
        let html = "<button onclick=\"go()\" class=x>a</button><a href=\" javascript:void(0)\">b</a><img src=\"data:image/png,x\"><p online><p on-x=\"1\">";
        t.feed(html);
        let found = t.take_inline_code();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].1, InlineCode::Handler("onclick".into()));
        assert!(html[found[0].0..].starts_with("onclick"));
        assert_eq!(found[1].1, InlineCode::ScriptUrl);
        assert!(html[found[1].0..].starts_with("\" javascript"));
        assert!(t.take_inline_code().is_empty());
    }

    #[test]
    fn checks_blocks() {
        let mut t = HtmlTracker::default();
        t.feed("<a href=\"");
        let entry = t.clone();
        t.feed("/x");
        assert!(t.end_block(&entry));
        // The block may not have run: the URL start is still undecided.
        assert_eq!(t.context(), EchoContext::Url);
        let mut t = HtmlTracker::default();
        let entry = t.clone();
        t.feed("<script>var x = ");
        assert!(!t.end_block(&entry));
        // `<option <?phv if (...): ?>selected<?phv endif; ?>>` is fine.
        let mut t = HtmlTracker::default();
        t.feed("<option value=\"1\" ");
        let entry = t.clone();
        t.feed("selected");
        assert!(t.end_block(&entry));
        t.feed(">");
        assert_eq!(t.context(), EchoContext::Text);
    }

    #[test]
    fn finds_script_tags() {
        let mut t = HtmlTracker::default();
        let html = "<p>é</p><SCRIPT src=a.js></script><!-- <script> --><p title=\"<script\"></p><script>";
        let at = t.feed(html);
        assert_eq!(at.len(), 2);
        assert_eq!(&html[..at[0]], "<p>é</p><SCRIPT");
        assert_eq!(&html[at[1]..], ">");
        // `<script` inside a textarea is text, not a tag.
        let mut t = HtmlTracker::default();
        assert!(t.feed("<textarea><script></textarea>").is_empty());
    }
}
