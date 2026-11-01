//! Tracks where in an HTML document a `<?= ?>` appears, so the type checker
//! can refuse insertions that HTML escaping does not make safe (unquoted
//! attributes, URLs, `<script>`, `<style>`, event handler attributes).

use crate::ast::EchoContext;

#[derive(Clone, Debug, PartialEq)]
enum State {
    Text,
    Comment,
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
        empty: bool,
    },
    Unquoted {
        tag: String,
        closing: bool,
        name: String,
    },
    /// Raw text of `<script>` or `<style>` until its end tag.
    Raw(String),
}

#[derive(Clone, Debug)]
pub struct HtmlTracker {
    state: State,
    /// Text of a tag name being read after `<`.
    pending: Option<String>,
}

impl Default for HtmlTracker {
    fn default() -> Self {
        HtmlTracker { state: State::Text, pending: None }
    }
}

const URL_ATTRS: &[&str] =
    &["href", "src", "action", "formaction", "xlink:href", "poster", "data", "cite", "background", "ping", "srcset"];

impl HtmlTracker {
    /// The context a value inserted at the current position lands in.
    pub fn context(&self) -> EchoContext {
        match &self.state {
            State::Text | State::Comment => EchoContext::Text,
            State::Raw(t) if t == "script" => EchoContext::Script,
            State::Raw(_) => EchoContext::Style,
            State::Tag { .. } | State::AttrName { .. } => EchoContext::Tag,
            State::BeforeValue { .. } | State::Unquoted { .. } => EchoContext::Unquoted,
            State::Quoted { name, empty, .. } => {
                if name.starts_with("on") {
                    EchoContext::Handler
                } else if name == "style" {
                    EchoContext::Style
                } else if *empty && URL_ATTRS.contains(&name.as_str()) {
                    EchoContext::Url
                } else {
                    EchoContext::Attr
                }
            }
        }
    }

    /// Records that a value was inserted (an attribute value is no longer empty).
    pub fn inserted(&mut self) {
        if let State::Quoted { empty, .. } = &mut self.state {
            *empty = false;
        }
    }

    fn end_tag(&mut self, tag: String, closing: bool) {
        self.state = if !closing && (tag == "script" || tag == "style") { State::Raw(tag) } else { State::Text };
    }

    /// Advances over literal HTML. Returns the byte offsets just after the
    /// name of each opening `<script` tag, where the server's CSP nonce goes.
    pub fn feed(&mut self, text: &str) -> Vec<usize> {
        let chars: Vec<char> = text.chars().collect();
        let offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
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
                    if c == '<' {
                        let rest: String = chars[i + 1..].iter().take(3).collect();
                        if rest.starts_with("!--") {
                            i += 3;
                            State::Comment
                        } else if chars.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == '/') {
                            self.pending = Some(String::new());
                            if chars[i + 1] == '/' {
                                self.pending = Some("/".into());
                                i += 1;
                            }
                            State::Text
                        } else {
                            State::Text
                        }
                    } else {
                        State::Text
                    }
                }
                State::Comment => {
                    if c == '-' && chars.get(i + 1) == Some(&'-') && chars.get(i + 2) == Some(&'>') {
                        i += 2;
                        State::Text
                    } else {
                        State::Comment
                    }
                }
                State::Raw(tag) => {
                    let end = format!("</{tag}");
                    let ahead: String = chars[i..].iter().take(end.len()).collect::<String>().to_ascii_lowercase();
                    if ahead == end {
                        i += end.len() - 1;
                        State::Tag { name: tag, closing: true }
                    } else {
                        State::Raw(tag)
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
                        State::AttrName { tag: name, closing, name: c.to_ascii_lowercase().to_string() }
                    }
                }
                State::AttrName { tag, closing, mut name } => {
                    if c == '=' {
                        State::BeforeValue { tag, closing, name }
                    } else if c == '>' {
                        self.end_tag(tag, closing);
                        i += 1;
                        continue;
                    } else if c.is_whitespace() {
                        // `name =value` is rare; treat a space as the end of a valueless attribute.
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
                        State::Quoted { tag, closing, name, quote: c, empty: true }
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
                State::Quoted { tag, closing, name, quote, empty } => {
                    if c == quote {
                        State::Tag { name: tag, closing }
                    } else {
                        State::Quoted { tag, closing, name, quote, empty: empty && c.is_whitespace() }
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
    fn finds_script_tags() {
        let mut t = HtmlTracker::default();
        let html = "<p>é</p><SCRIPT src=a.js></script><!-- <script> --><p title=\"<script\"></p><script>";
        let at = t.feed(html);
        assert_eq!(at.len(), 2);
        assert_eq!(&html[..at[0]], "<p>é</p><SCRIPT");
        assert_eq!(&html[at[1]..], ">");
    }
}
