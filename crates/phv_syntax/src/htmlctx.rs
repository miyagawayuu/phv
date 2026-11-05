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
    /// Only literal text so far; the scheme is not decided yet. After a block
    /// that may or may not have run, each way the text may have begun.
    Undecided(Vec<String>),
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
        /// Where the script's JavaScript has reached (`<script>` only).
        js: Js,
    },
}

/// Where inside a script's JavaScript the text has reached. Values are safe
/// only where an expression goes: inside a string, template, comment or
/// regular expression, a JSON value's quotes or text would change what the
/// code means (`${...}` in a template literal runs).
#[derive(Clone, Copy, Debug, PartialEq)]
enum JsMode {
    Code,
    /// After `/` in code: a comment, a regular expression or a division.
    Slash,
    Str(char),
    StrEscape(char),
    /// After `\` and a carriage return in a string (a line continuation).
    StrEscapeCr(char),
    Template,
    TemplateEscape,
    /// After `$` in a template literal (`${` starts a substitution).
    TemplateDollar,
    LineComment,
    BlockComment,
    /// After `*` in a block comment.
    BlockStar,
    Regex,
    RegexEscape,
    RegexClass,
    RegexClassEscape,
}

/// The most readings of a script followed at once; past it, no value may be
/// inserted until the script ends.
const MAX_READINGS: usize = 8;

/// The JavaScript of a `<script>`, as every way it may have been read so far.
/// Where the text alone does not decide (a `/` after `}` divides after an
/// object literal and starts a regular expression after a block), or a block
/// may or may not have run, each possibility is followed, and a value goes in
/// only where all of them are code.
#[derive(Clone, Debug)]
struct Js {
    readings: Vec<Reading>,
    /// Too many readings: nothing is inserted into this script any more.
    lost: bool,
}

impl Default for Js {
    fn default() -> Self {
        Js { readings: vec![Reading::default()], lost: false }
    }
}

impl PartialEq for Js {
    fn eq(&self, other: &Js) -> bool {
        self.lost == other.lost
            && self.readings.iter().all(|r| other.readings.contains(r))
            && other.readings.iter().all(|r| self.readings.contains(r))
    }
}

impl Js {
    /// True where a value is a whole expression in every reading.
    fn in_code(&self) -> bool {
        !self.lost && self.readings.iter().all(|r| r.in_code())
    }

    fn step(&mut self, c: char) {
        let mut forks = Vec::new();
        for r in &mut self.readings {
            r.step(c);
            if std::mem::take(&mut r.fork) {
                let mut other = r.clone();
                other.regex = !other.regex;
                forks.push(other);
            }
        }
        self.readings.extend(forks);
        self.settle();
    }

    /// A value was inserted (a number, unless every reading is code).
    fn inserted(&mut self) {
        for r in &mut self.readings {
            r.inserted();
        }
        self.settle();
    }

    /// The readings of a block that may or may not have run: both may follow.
    fn merge(&mut self, other: &Js) {
        self.lost |= other.lost;
        self.readings.extend(other.readings.iter().cloned());
        self.settle();
    }

    /// Whether the two have the same open strings, comments, brackets and
    /// parentheses (a block must end where it started).
    fn same_shape(&self, other: &Js) -> bool {
        let shape = |js: &Js| -> Vec<(JsMode, Vec<bool>, Vec<bool>)> {
            let mut v: Vec<_> = js.readings.iter().map(|r| (r.mode, r.braces.clone(), r.parens.clone())).collect();
            v.dedup();
            v
        };
        let (a, b) = (shape(self), shape(other));
        a.iter().all(|x| b.contains(x)) && b.iter().all(|x| a.contains(x))
    }

    fn settle(&mut self) {
        let mut kept: Vec<Reading> = Vec::new();
        for r in self.readings.drain(..) {
            if !kept.contains(&r) {
                kept.push(r);
            }
        }
        if kept.len() > MAX_READINGS {
            self.lost = true;
            kept.truncate(MAX_READINGS);
        }
        self.readings = kept;
    }
}

/// One way of reading a script's JavaScript.
#[derive(Clone, Debug)]
struct Reading {
    mode: JsMode,
    /// For each open `{`: whether it opened a template substitution (`${`).
    braces: Vec<bool>,
    /// For each open `(`: whether it holds the condition of `if`, `while`,
    /// `for` or `with` (a `/` after its `)` starts a regular expression).
    parens: Vec<bool>,
    /// Whether a `/` here would start a regular expression rather than
    /// divide, decided from the token before it.
    regex: bool,
    /// The identifier or number being read, and whether it follows `.` (a
    /// property name such as `.in` or `.return` is not a keyword).
    word: String,
    property: bool,
    /// The keyword just read, if it can begin a condition (`if`, `for`...).
    keyword: &'static str,
    /// The last character of code, and `regex` before it (for `i++ / 2`).
    last: char,
    regex_before_last: bool,
    /// The last three characters of code, as written (for `<!--`).
    recent: [char; 3],
    /// Non-blank characters on this line so far, up to 3 (`-->` at the start
    /// of a line is a comment).
    line: String,
    /// Set by `code` on a `/` right after `}`.
    fork: bool,
}

impl Default for Reading {
    fn default() -> Self {
        Reading {
            mode: JsMode::Code,
            braces: Vec::new(),
            parens: Vec::new(),
            regex: true,
            word: String::new(),
            property: false,
            keyword: "",
            last: ' ',
            regex_before_last: true,
            recent: [' '; 3],
            line: String::new(),
            fork: false,
        }
    }
}

// Two readings are the same if everything that decides how later text is read
// is the same.
impl PartialEq for Reading {
    fn eq(&self, other: &Reading) -> bool {
        // Only these characters change how what follows them is read.
        let last = |r: &Reading| if matches!(r.last, '}' | '+' | '-' | '.') { r.last } else { ' ' };
        // How much of `<!-` the recent characters end with.
        let comment = |r: &Reading| match r.recent {
            [_, '<', '!'] => 2,
            [_, _, '<'] => 1,
            ['<', '!', '-'] => 3,
            _ => 0,
        };
        fn line(r: &Reading) -> &str {
            if "-->".starts_with(r.line.as_str()) {
                r.line.as_str()
            } else {
                "x"
            }
        }
        self.mode == other.mode
            && self.braces == other.braces
            && self.parens == other.parens
            && self.regex == other.regex
            && self.word == other.word
            && self.property == other.property
            && self.keyword == other.keyword
            && last(self) == last(other)
            && (!matches!(self.last, '+' | '-')
                || (self.regex_before_last == other.regex_before_last && self.recent[2] == other.recent[2]))
            && comment(self) == comment(other)
            && line(self) == line(other)
    }
}

/// Keywords after which `/` starts a regular expression.
const REGEX_KEYWORDS: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
    // A line break after these ends the statement.
    "break",
    "continue",
    "debugger",
];

fn line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

impl Reading {
    /// True where a value is a whole expression (not inside a literal or comment).
    fn in_code(&self) -> bool {
        self.mode == JsMode::Code || (self.mode == JsMode::Slash && !self.regex)
    }

    /// A value was inserted. Where an expression goes, a `/` after it divides;
    /// elsewhere only a number may be inserted, read like a digit.
    fn inserted(&mut self) {
        if self.in_code() {
            self.mode = JsMode::Code;
            self.word.clear();
            self.property = false;
            self.keyword = "";
            self.regex = false;
            self.last = '0';
            self.recent = [self.recent[1], self.recent[2], '0'];
            if self.line.len() < 3 {
                self.line.push('0');
            }
        } else {
            self.step('0');
        }
    }

    fn end_word(&mut self) {
        if !self.word.is_empty() {
            let word = std::mem::take(&mut self.word);
            let keyword = !std::mem::take(&mut self.property);
            self.regex = keyword && REGEX_KEYWORDS.contains(&word.as_str());
            // `for await (` holds a condition like `for (`.
            self.keyword = match word.as_str() {
                "if" if keyword => "if",
                "while" if keyword => "while",
                "for" if keyword => "for",
                "with" if keyword => "with",
                "await" if keyword && self.keyword == "for" => "for",
                _ => "",
            };
        }
    }

    fn step(&mut self, c: char) {
        use JsMode::*;
        if line_break(c) && matches!(self.mode, Code | Slash | LineComment | BlockComment | BlockStar) {
            self.line.clear();
        } else if !c.is_whitespace() && self.line.len() < 3 {
            self.line.push(c);
        }
        self.mode = match self.mode {
            Code => return self.code(c),
            Slash => match c {
                // A comment between `if` and `(` keeps the keyword.
                '/' => LineComment,
                '*' => BlockComment,
                _ if self.regex => {
                    self.keyword = "";
                    self.mode = Regex;
                    return self.regex_char(c);
                }
                _ => {
                    self.keyword = "";
                    self.mode = Code;
                    self.regex = true;
                    self.last = '/';
                    return self.code(c);
                }
            },
            Str(q) => match c {
                '\\' => StrEscape(q),
                '\n' | '\r' => Code,
                c if c == q => {
                    self.regex = false;
                    self.last = c;
                    Code
                }
                _ => Str(q),
            },
            StrEscape(q) if c == '\r' => StrEscapeCr(q),
            StrEscape(q) => Str(q),
            StrEscapeCr(q) => match c {
                '\n' => Str(q),
                _ => {
                    self.mode = Str(q);
                    return self.step(c);
                }
            },
            Template => match c {
                '\\' => TemplateEscape,
                '$' => TemplateDollar,
                '`' => {
                    self.regex = false;
                    self.last = c;
                    Code
                }
                _ => Template,
            },
            TemplateEscape => Template,
            TemplateDollar => match c {
                '{' => {
                    self.braces.push(true);
                    self.regex = true;
                    self.last = c;
                    Code
                }
                '\\' => TemplateEscape,
                '$' => TemplateDollar,
                '`' => {
                    self.regex = false;
                    self.last = c;
                    Code
                }
                _ => Template,
            },
            LineComment if line_break(c) => Code,
            LineComment => LineComment,
            BlockComment if c == '*' => BlockStar,
            BlockComment => BlockComment,
            BlockStar => match c {
                '/' => Code,
                '*' => BlockStar,
                _ => BlockComment,
            },
            Regex | RegexEscape | RegexClass | RegexClassEscape => return self.regex_char(c),
        };
    }

    fn regex_char(&mut self, c: char) {
        use JsMode::*;
        self.mode = match (self.mode, c) {
            (Regex, '\\') => RegexEscape,
            (Regex, '[') => RegexClass,
            (Regex, '/') => {
                self.regex = false;
                self.last = c;
                Code
            }
            (RegexClass, '\\') => RegexClassEscape,
            (RegexClass, ']') => Regex,
            // A regular expression cannot span lines.
            (_, c) if line_break(c) => Code,
            (RegexEscape, _) => Regex,
            (RegexClassEscape, _) => RegexClass,
            (m, _) => m,
        };
    }

    fn code(&mut self, c: char) {
        let recent = self.recent;
        self.recent = [recent[1], recent[2], c];
        if c.is_alphanumeric() || c == '_' || c == '$' {
            if self.word.is_empty() {
                // A name right after `.` (or `?.`) is a property, not a keyword;
                // after a digit, `.` is a decimal point and never a keyword either.
                self.property = matches!(self.last, '.' | '#');
            }
            self.word.push(c);
            self.last = c;
            return;
        }
        self.end_word();
        if c.is_whitespace() {
            return;
        }
        // A `/` may start a comment, which keeps the keyword before it.
        let keyword = if c == '/' { self.keyword } else { std::mem::take(&mut self.keyword) };
        let (last, before_last) = (self.last, self.regex_before_last);
        self.last = c;
        self.regex_before_last = self.regex;
        match c {
            // `<!--` anywhere and `-->` at the start of a line begin a comment.
            '-' if recent == ['<', '!', '-'] => self.mode = JsMode::LineComment,
            '>' if last == '-' && self.line == "-->" => self.mode = JsMode::LineComment,
            '"' | '\'' => self.mode = JsMode::Str(c),
            '`' => self.mode = JsMode::Template,
            '/' => {
                self.mode = JsMode::Slash;
                self.fork = last == '}';
            }
            '{' => {
                self.braces.push(false);
                self.regex = true;
            }
            '}' => {
                if self.braces.pop() == Some(true) {
                    self.mode = JsMode::Template;
                } else {
                    // A block; after an object literal `/` divides (see `fork`).
                    self.regex = true;
                }
            }
            '(' => {
                self.parens.push(!keyword.is_empty());
                self.regex = true;
            }
            // After the condition of `if (...)` a statement starts, which may be a
            // regular expression; after other parentheses, `/` divides.
            ')' => self.regex = self.parens.pop().unwrap_or(false),
            ']' => self.regex = false,
            // `i++ / 2` divides: `++` right after an operand.
            '+' | '-' if last == c && recent[2] == c && !before_last => self.regex = false,
            _ => self.regex = true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HtmlTracker {
    state: State,
    /// Text of a tag name being read after `<`.
    pending: Option<String>,
    /// Depth of `<svg>` / `<math>` (foreign content has no RCDATA elements).
    foreign: u32,
    /// Byte offset (in the chunk being fed) of the attribute name or value being
    /// read, if it began in this chunk (else what is found is reported where
    /// it is decided).
    mark: Option<usize>,
    /// Inline JavaScript found in the last `feed` (see `InlineCode`).
    found: Vec<(usize, InlineCode)>,
    /// Names of the attributes of the current tag seen so far.
    attrs: Vec<String>,
    /// A value was inserted into the `content` of the current `<meta>` as text.
    meta_text: bool,
    /// A way through a block reached the current tag without `name`,
    /// `property` or `itemprop` (the attributes of the ways are merged, see
    /// `end_block`): a `<meta>` `content` here may act, not describe.
    maybe_undescribed: bool,
    /// Byte offset (in the chunk being fed) of the character being read.
    here: usize,
    /// The end of the last literal text when it may start a tag (`<`, `</`, or
    /// the start of a raw element's end tag such as `</text`): what follows
    /// decides, so it is read again in front of the next text, and no value
    /// may be inserted here (it would name the tag).
    carry: String,
}

/// JavaScript written directly in template HTML, which a Content-Security-Policy
/// without `'unsafe-inline'` does not run (unlike `<script>`, which gets a nonce).
#[derive(Clone, Debug, PartialEq)]
pub enum InlineCode {
    /// An event handler attribute such as `onclick`.
    Handler(String),
    /// A `javascript:` (or `vbscript:`) URL.
    ScriptUrl,
    /// `http-equiv` after a value inserted into a `<meta>`'s `content` as text
    /// (an error: `http-equiv="refresh"` would redirect to it).
    MetaHttpEquiv,
    /// `<![CDATA[` or `<!--` in a `<script>` or `<style>` in SVG or MathML (an
    /// error: browsers read it as markup, which can hide the end tag).
    ForeignCdata,
}

impl Default for HtmlTracker {
    fn default() -> Self {
        HtmlTracker {
            state: State::Text,
            pending: None,
            foreign: 0,
            mark: None,
            found: Vec::new(),
            attrs: Vec::new(),
            meta_text: false,
            maybe_undescribed: false,
            here: 0,
            carry: String::new(),
        }
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
    &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "plaintext"];

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
        if !self.carry.is_empty() {
            return EchoContext::AfterMarkup;
        }
        if self.pending.is_some() {
            return EchoContext::Tag;
        }
        match &self.state {
            State::Text | State::Comment | State::Bogus => EchoContext::Text,
            State::Raw { tag, js, .. } if tag == "script" => {
                if js.in_code() {
                    EchoContext::Script
                } else {
                    EchoContext::ScriptLiteral
                }
            }
            State::Raw { tag, .. } if tag == "style" => EchoContext::Style,
            State::Raw { .. } => EchoContext::Text,
            State::Tag { .. } | State::AttrName { .. } => EchoContext::Tag,
            State::BeforeValue { .. } | State::Unquoted { .. } => EchoContext::Unquoted,
            State::Quoted { tag, name, .. }
                if tag == "script" && matches!(name.as_str(), "src" | "href" | "xlink:href") =>
            {
                EchoContext::ScriptSrc
            }
            // A `<meta>` that describes the page (name, property, itemprop) may take any text;
            // others, such as `http-equiv="refresh"`, act on their `content`.
            State::Quoted { tag, name, .. }
                if tag == "meta"
                    && name == "content"
                    && (self.attrs.iter().any(|a| a == "http-equiv")
                        || self.maybe_undescribed
                        || !self.described()) =>
            {
                EchoContext::MetaContent
            }
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

    /// Records that a value was inserted. In a URL attribute the start stays
    /// undecided: a checked `SafeUrl` may be empty or end before its scheme
    /// would (`safe_url("")`), so a value after it is checked as a URL too.
    /// In a script, the value was an expression.
    pub fn inserted(&mut self) {
        if self.context() == EchoContext::Attr {
            if let State::Quoted { tag, name, .. } = &self.state {
                self.meta_text |= tag == "meta" && name == "content";
            }
        }
        if let State::Raw { tag, js, .. } = &mut self.state {
            if tag == "script" {
                js.inserted();
            }
        }
    }

    /// True between elements of an HTML document: not inside a tag, comment,
    /// raw-text element or foreign content. A template must end here, so that
    /// joining its output with other `Html` values cannot move them into
    /// another context.
    /// Whether the output is at exactly the same place as at `other`, with
    /// the same attributes of the current tag.
    pub fn same_place(&self, other: &HtmlTracker) -> bool {
        self.state == other.state
            && self.pending == other.pending
            && self.foreign == other.foreign
            && self.carry == other.carry
            && (!self.in_tag()
                || self.attrs == other.attrs
                    && self.meta_text == other.meta_text
                    && self.maybe_undescribed == other.maybe_undescribed)
    }

    /// Inside a start or end tag, where its attributes matter.
    fn in_tag(&self) -> bool {
        matches!(
            self.state,
            State::Tag { .. }
                | State::AttrName { .. }
                | State::BeforeValue { .. }
                | State::Quoted { .. }
                | State::Unquoted { .. }
        )
    }

    /// The current tag has `name`, `property` or `itemprop` (a `<meta>` that describes the page).
    fn described(&self) -> bool {
        self.attrs.iter().any(|a| matches!(a.as_str(), "name" | "property" | "itemprop"))
    }

    pub fn at_top_level(&self) -> bool {
        self.state == State::Text && self.pending.is_none() && self.foreign == 0 && self.carry.is_empty()
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
                    if let (Some(a), Some(b)) = (u1.as_mut(), u2.as_ref()) {
                        if let (UrlState::Undecided(x), UrlState::Undecided(y)) = (&mut *a, b) {
                            // Either way may follow: keep both (a few at most).
                            for p in y {
                                if !x.contains(p) {
                                    x.push(p.clone());
                                }
                            }
                            // At most the prefixes of the three schemes, and `&`.
                        } else if b.rank() > a.rank() {
                            *a = b.clone();
                        }
                    }
                }
                same
            }
            // A script continues after the block in every way it may have been read.
            (State::Raw { tag: t1, escape: e1, js: j1 }, State::Raw { tag: t2, escape: e2, js: j2 }) => {
                let same = t1 == t2 && e1 == e2 && j1.same_shape(j2);
                if same {
                    j1.merge(j2);
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
        let same = same && self.pending == entry.pending && self.foreign == entry.foreign && self.carry == entry.carry;
        if same && self.in_tag() {
            // The attributes of the current tag on either way: a `<meta>` is
            // checked as acting (`http-equiv`, or no `name`) if one way makes
            // it so (found by the 1.0 audit: a block that closed the tag and
            // opened another `<meta name>` hid an `http-equiv` before it).
            let undescribed =
                self.maybe_undescribed || entry.maybe_undescribed || !self.described() || !entry.described();
            for a in &entry.attrs {
                if !self.attrs.contains(a) {
                    self.attrs.push(a.clone());
                }
            }
            self.meta_text |= entry.meta_text;
            self.maybe_undescribed = undescribed;
        }
        same
    }

    fn end_tag(&mut self, tag: String, closing: bool) {
        if std::mem::take(&mut self.meta_text) && tag == "meta" && self.attrs.iter().any(|a| a == "http-equiv") {
            self.found.push((self.here, InlineCode::MetaHttpEquiv));
        }
        if matches!(tag.as_str(), "svg" | "math") {
            self.foreign = if closing { self.foreign.saturating_sub(1) } else { self.foreign + 1 };
        }
        let raw = if self.foreign > 0 { tag == "script" || tag == "style" } else { RAW_TAGS.contains(&tag.as_str()) };
        self.state = if !closing && raw { State::Raw { tag, escape: 0, js: Js::default() } } else { State::Text };
    }

    /// Advances over literal HTML. Returns the byte offsets just after the
    /// name of each opening `<script` tag, where the server's CSP nonce goes.
    pub fn feed(&mut self, text: &str) -> Vec<usize> {
        if self.carry.is_empty() {
            return self.feed_text(text);
        }
        // The end of the last text, read again in front of this one; offsets are
        // reported in this text (the carried characters come before it).
        let carry = std::mem::take(&mut self.carry);
        let shift = carry.len();
        let found = self.found.len();
        let scripts = self.feed_text(&format!("{carry}{text}"));
        for f in &mut self.found[found..] {
            f.0 = f.0.saturating_sub(shift);
        }
        scripts.into_iter().map(|s| s.saturating_sub(shift)).collect()
    }

    fn feed_text(&mut self, text: &str) -> Vec<usize> {
        let chars: Vec<char> = text.chars().collect();
        let offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        let starts = |i: usize, s: &str| -> bool {
            let n = s.chars().count();
            i + n <= chars.len() && chars[i..i + n].iter().collect::<String>().eq_ignore_ascii_case(s)
        };
        let mut scripts = Vec::new();
        let mut i = 0;
        self.mark = None;
        while i < chars.len() {
            let c = chars[i];
            self.here = offsets[i];
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
                self.attrs.clear();
                self.meta_text = false;
                self.maybe_undescribed = false;
                self.state = State::Tag { name: tag, closing };
                continue;
            }
            let state = std::mem::replace(&mut self.state, State::Text);
            let (from, in_script) = (i, matches!(&state, State::Raw { tag, .. } if tag == "script"));
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
                            None => {
                                self.carry = "<".into();
                                State::Text
                            }
                            Some(n) if n.is_ascii_alphabetic() => {
                                self.pending = Some(String::new());
                                State::Text
                            }
                            Some('/') if i + 2 == chars.len() => {
                                self.carry = "</".into();
                                i += 1;
                                State::Text
                            }
                            // `<!` or `<!-` at the end: a value or block could make a comment.
                            Some('!')
                                if chars.len() - i < 4
                                    && "<!--".starts_with(&chars[i..].iter().collect::<String>()) =>
                            {
                                self.carry = chars[i..].iter().collect();
                                i = chars.len() - 1;
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
                State::Raw { tag, escape, js } => {
                    let end = format!("</{tag}");
                    let n = end.len();
                    let rest: String = chars[i..].iter().collect::<String>().to_ascii_lowercase();
                    // In a script, the starts of `<!--`, `<script` and `-->` too: they change
                    // its escape state.
                    let partial = |s: &str| rest.len() < s.len() && s.starts_with(&rest);
                    let script_start = tag == "script"
                        && ((escape == 0 && partial("<!--"))
                            || (escape == 1 && partial("<script"))
                            || (escape > 0 && (partial("-->") || rest == "-")));
                    if script_start || (c == '<' && rest.len() <= n && end.starts_with(&rest)) {
                        // `</tex` + a value or block: it might end the element.
                        self.carry = chars[i..].iter().collect();
                        i = chars.len() - 1;
                        State::Raw { tag, escape, js }
                    } else if self.foreign > 0 && (starts(i, "<![CDATA[") || starts(i, "<!--")) {
                        // In SVG and MathML a script's text is markup: a CDATA section or
                        // comment can hide its end tag from browsers but not from here.
                        self.found.push((offsets[i], InlineCode::ForeignCdata));
                        State::Raw { tag, escape, js }
                    } else if starts(i, &end) && is_delim(chars.get(i + n)) {
                        if tag == "script" && escape == 2 {
                            i += n - 1;
                            State::Raw { tag, escape: 1, js }
                        } else {
                            i += n - 1;
                            State::Tag { name: tag, closing: true }
                        }
                    } else if tag != "script" {
                        State::Raw { tag, escape, js }
                    } else if escape == 0 && starts(i, "<!--") {
                        // `<!-->` and `<!--->` return to script data at once.
                        let mut j = i + 4;
                        while chars.get(j) == Some(&'-') {
                            j += 1;
                        }
                        if chars.get(j) == Some(&'>') {
                            i = j;
                            State::Raw { tag, escape: 0, js }
                        } else {
                            i += 3;
                            State::Raw { tag, escape: 1, js }
                        }
                    } else if escape > 0 && starts(i, "-->") {
                        i += 2;
                        State::Raw { tag, escape: 0, js }
                    } else if escape == 1 && starts(i, "<script") && is_delim(chars.get(i + 7)) {
                        i += 6;
                        State::Raw { tag, escape: 2, js }
                    } else {
                        State::Raw { tag, escape, js }
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
                        self.mark = Some(offsets[i]);
                        State::AttrName { tag: name, closing, name: c.to_ascii_lowercase().to_string() }
                    }
                }
                State::AttrName { tag, closing, mut name } => {
                    if c == '=' {
                        // Event handler names are letters only (`onclick`, not `onclick-free`).
                        let handler =
                            name.len() > 2 && name.starts_with("on") && name.bytes().all(|b| b.is_ascii_lowercase());
                        if handler && !closing {
                            self.found.push((self.mark.unwrap_or(self.here), InlineCode::Handler(name.clone())));
                        }
                        self.attrs.push(name.clone());
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
                            self.attrs.push(name);
                            State::Tag { name: tag, closing }
                        }
                    } else {
                        name.push(c.to_ascii_lowercase());
                        State::AttrName { tag, closing, name }
                    }
                }
                State::BeforeValue { tag, closing, name } => {
                    if c == '"' || c == '\'' {
                        let url = is_url_attr(&tag, &name).then(|| UrlState::Undecided(vec![String::new()]));
                        self.mark = Some(offsets[i]);
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
                        if let Some(UrlState::Undecided(ps)) = &mut url {
                            // The strictest of the ways the text may have begun.
                            let (mut script, mut inline) = (false, false);
                            ps.retain_mut(|p| {
                                // Browsers ignore whitespace and case in a scheme.
                                if !c.is_ascii_whitespace() && !c.is_control() && p.len() < 256 {
                                    p.push(c.to_ascii_lowercase());
                                }
                                match decide_url(p) {
                                    Some(UrlState::Script) => {
                                        // `data:` is fine for images; only script URLs are inline code.
                                        let scheme: String = p
                                            .chars()
                                            .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
                                            .collect::<String>()
                                            .to_ascii_lowercase();
                                        if scheme.starts_with("javascript:") || scheme.starts_with("vbscript:") {
                                            inline = true;
                                        }
                                        script = true;
                                        false
                                    }
                                    Some(_) => false,
                                    // Text that can no longer become a script scheme is a safe start;
                                    // a character reference (`&`) may be anything.
                                    None => {
                                        p.contains('&')
                                            || ["javascript:", "vbscript:", "data:"]
                                                .iter()
                                                .any(|s| s.starts_with(p.as_str()))
                                    }
                                }
                            });
                            for p in ps.iter_mut().filter(|p| p.contains('&')) {
                                *p = "&".into();
                            }
                            ps.sort();
                            ps.dedup();
                            if inline {
                                self.found.push((self.mark.unwrap_or(self.here), InlineCode::ScriptUrl));
                            }
                            if script {
                                url = Some(UrlState::Script);
                            } else if ps.is_empty() {
                                url = Some(UrlState::Safe);
                            }
                        }
                        // A list of URLs: each candidate of `srcset` (after `,`) and each
                        // value of an animation's `values` (after `;`) starts a URL again.
                        let next_url = (name == "srcset" && c == ',')
                            || (name == "values" && c == ';' && ANIMATION_TAGS.contains(&tag.as_str()));
                        if next_url && !matches!(url, Some(UrlState::Script)) {
                            url = Some(UrlState::Undecided(vec![String::new()]));
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
            // The script's text, including what the HTML tokenizer read in one step.
            // (Carried characters are read with the next text, not now.)
            if in_script && self.carry.is_empty() {
                if let State::Raw { js, .. } = &mut self.state {
                    for &c in &chars[from..=i] {
                        js.step(c);
                    }
                }
            }
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
        // (`<!--` also starts a line comment in JavaScript.)
        assert_eq!(ctx("<script><!--<script></script>\nvar e = "), EchoContext::Script);
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
    fn follows_javascript_literals_and_comments() {
        use EchoContext::{Script, ScriptLiteral};
        for (js, want) in [
            ("var a = ", Script),
            ("var a = '", ScriptLiteral),
            ("var a = \"x\\\"", ScriptLiteral),
            ("var a = \"x\\\\\" + ", Script),
            ("var a = `", ScriptLiteral),
            ("var a = `${", Script),
            ("var a = `${ {b: 1}.b }", ScriptLiteral),
            ("var a = `${ `${", Script),
            ("var a = `$", ScriptLiteral),
            ("var a = `x` + ", Script),
            ("// ", ScriptLiteral),
            ("// x\nvar a = ", Script),
            ("/* ", ScriptLiteral),
            ("/* * / */ var a = ", Script),
            ("var r = /", ScriptLiteral),
            ("var r = /[/]'/; var a = ", Script),
            ("var r = /\\/'/; var a = ", Script),
            ("var q = (1) / ", Script),
            ("var q = a[0] / ", Script),
            ("var q = x / ", Script),
            ("return /", ScriptLiteral),
            ("if (a) { } /", ScriptLiteral),
            ("var a = 'it''s' + ", Script),
            // Found by the v0.9.1 audit: each flipped the quotes the checker saw.
            ("var h = i++ / 2; var p = \"/a/\" + h; var m = \"", ScriptLiteral),
            ("var h = i-- / 2; var m = ", Script),
            ("var h = ++i; var r = /\"/; var m = ", Script),
            ("if (s) /\"/.test(s); var m = \"", ScriptLiteral),
            ("while (s) /\"/.test(s); var m = ", Script),
            ("var q = f(s) / \"x\".length; var m = ", Script),
            ("var s = \"a\\\r\nb\"; var m = ", Script),
            ("var s = \"a\\\r\nb", ScriptLiteral),
            ("<!-- \"\nvar m = ", Script),
            ("var a = 1; <!-- `\nvar m = ", Script),
            ("\n --> /*\nvar m = ", Script),
            ("var a = 1 --> 0; var m = ", Script),
            // After `}`, `/` may divide (an object literal) or start a regular
            // expression (a block): a value goes in only where both are code.
            ("if (a) { } /\"/.test(s); var m = ", ScriptLiteral),
            ("var o = {a: 1}\n/\"/.test(s);\nvar m = ", Script),
            // A property named like a keyword divides; `for await (` is a condition.
            ("var r = counts.in / total + \"/\" + n; var m = ", Script),
            ("var r = a?.return / 2; var m = \"", ScriptLiteral),
            ("for await (x of y) /\"/.test(x); var m = \"", ScriptLiteral),
            ("var r = n + +/\"/.source; var m = ", Script),
            // A comment, or a second `}`, after an object literal.
            ("var x = {} /* c */ /\"/.test(s) + \"+1; var m = ", ScriptLiteral),
            ("{}/\"/.test(s); x = {}/\"/.test(s); var m = ", ScriptLiteral),
            // Found by the last review of v0.9.1.
            ("if /* c */ (s) /\"/.test(s); var m = \"", ScriptLiteral),
            ("if // c\n(s) /\"/.test(s); var m = \"", ScriptLiteral),
            ("while (a) break\n/\"/.test(s); var m = \"", ScriptLiteral),
            ("class A { #new = 1; f() { return this.#new / 2 + \"/\"; } } var m = ", Script),
        ] {
            assert_eq!(ctx(&format!("<script>{js}")), want, "{js}");
        }
        // A value inserted where an expression goes: a `/` after it divides.
        let mut t = HtmlTracker::default();
        t.feed("<script>var a = ");
        t.inserted();
        t.feed(" / 2; var b = '");
        assert_eq!(t.context(), ScriptLiteral);
        t.feed("'; var c = ");
        assert_eq!(t.context(), Script);
        // A number inserted inside a literal is read like a digit.
        let mut t = HtmlTracker::default();
        t.feed("<script>var re = /");
        assert_eq!(t.context(), ScriptLiteral);
        t.inserted();
        t.feed("*/g; var s = \"a*/b\"; var m = ");
        assert_eq!(t.context(), Script);
        let mut t = HtmlTracker::default();
        t.feed("<script>var t = `$");
        t.inserted();
        t.feed("{\"`; var m = ");
        assert_eq!(t.context(), Script);
    }

    /// What ends a piece of literal text and could start a tag or comment with
    /// what follows is read again with it; a value right there is refused.
    #[test]
    fn markup_split_by_a_value_is_refused() {
        for (before, after_value) in [
            ("<p>5 <", EchoContext::AfterMarkup),
            ("<p>5 </", EchoContext::AfterMarkup),
            ("<p>5 <!", EchoContext::AfterMarkup),
            ("<p>5 <!-", EchoContext::AfterMarkup),
            ("<textarea>x </text", EchoContext::AfterMarkup),
            ("<script>a <!-", EchoContext::AfterMarkup),
            ("<script>a <", EchoContext::AfterMarkup),
            ("<script>a < ", EchoContext::Script),
            ("<p>5 < ", EchoContext::Text),
        ] {
            let mut t = HtmlTracker::default();
            t.feed(before);
            assert_eq!(t.context(), after_value, "{before}");
        }
        // The carried text is read with the next piece: a tag split across them is seen.
        let mut t = HtmlTracker::default();
        t.feed("<p><");
        t.feed("a href=\"");
        assert_eq!(t.context(), EchoContext::Url);
    }

    /// Found by the 1.0 audit: a block that closes a `<meta http-equiv>` and
    /// opens a `<meta name>` must not make the `content` after it descriptive.
    #[test]
    fn blocks_keep_the_attributes_of_a_meta() {
        let mut t = HtmlTracker::default();
        t.feed("<meta http-equiv=\"refresh\" ");
        let entry = t.clone();
        t.feed("><meta name=\"x\" ");
        assert!(t.end_block(&entry));
        t.feed("content=\"");
        assert_eq!(t.context(), EchoContext::MetaContent);
        // The other way round: a way without `name` makes it acting too.
        let mut t = HtmlTracker::default();
        t.feed("<meta name=\"x\" ");
        let entry = t.clone();
        t.feed("><meta ");
        assert!(t.end_block(&entry));
        t.feed("content=\"");
        assert_eq!(t.context(), EchoContext::MetaContent);
        // A descriptive `<meta>` with a conditional attribute stays descriptive.
        let mut t = HtmlTracker::default();
        t.feed("<meta name=\"x\" ");
        let entry = t.clone();
        t.feed("lang=\"en\" ");
        assert!(t.end_block(&entry));
        t.feed("content=\"");
        assert_eq!(t.context(), EchoContext::Attr);
    }

    /// Each URL of `srcset` and of an animation's `values` is checked.
    #[test]
    fn every_url_of_a_list_is_checked() {
        for (text, ctx) in [
            ("<img srcset=\"/a.png 1x, ", EchoContext::Url),
            ("<img srcset=\"/a.png 1x, /b.png 2x", EchoContext::Attr),
            ("<svg><animate attributeName=\"href\" values=\"/a;", EchoContext::Url),
            ("<svg><animate attributeName=\"href\" values=\"/a;javascript:x", EchoContext::ScriptUrl),
        ] {
            let mut t = HtmlTracker::default();
            t.feed(text);
            assert_eq!(t.context(), ctx, "{text}");
        }
    }

    #[test]
    fn blocks_merge_url_starts_and_script_states() {
        // Either way the block went, the text after it is checked.
        let mut t = HtmlTracker::default();
        t.feed("<a href=\"");
        let entry = t.clone();
        t.feed("x");
        assert!(t.end_block(&entry));
        t.feed("javascript:void(");
        assert_eq!(t.context(), EchoContext::ScriptUrl);
        // A block that leaves `/` dividing where it was a regular expression:
        // both readings follow it.
        let mut t = HtmlTracker::default();
        t.feed("<script>var a = ");
        let entry = t.clone();
        t.feed("b");
        assert!(t.end_block(&entry));
        t.feed("/\"/.test(s); var m = ");
        assert_eq!(t.context(), EchoContext::ScriptLiteral);
        t.feed("\nvar n = ");
        assert_eq!(t.context(), EchoContext::Script);
        // A block may not leave a string or bracket open.
        let mut t = HtmlTracker::default();
        t.feed("<script>var a = ");
        let entry = t.clone();
        t.feed("\"b");
        assert!(!t.end_block(&entry));
        // Found by the v0.9.1 audit: what a block leaves behind decides later text.
        for (before, block, after) in [
            ("<script>s\n", "if", " (s) /\"/.test(s); var m = "),
            ("<script>var a = ", "{}", "/\"/.test(s); var m = "),
            ("<script>/* ", "*/ y /*", " */ /\"/.test(s); var m = "),
        ] {
            let mut t = HtmlTracker::default();
            t.feed(before);
            let entry = t.clone();
            t.feed(block);
            assert!(t.end_block(&entry), "{block}");
            t.feed(after);
            assert_eq!(t.context(), EchoContext::ScriptLiteral, "{block}");
        }
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
