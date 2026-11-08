//! Tracks where in an HTML document a `<?= ?>` appears, so the type checker
//! can refuse insertions that HTML escaping does not make safe (unquoted
//! attributes, URLs, `<script>`, `<style>`, event handler attributes).
//!
//! The tracker follows the parts of the WHATWG tokenizer that decide where
//! text ends up: raw-text elements and their end tags, the escaped states of
//! `<script>`, comments (including `<!-->` and `--!>`), bogus comments, CDATA
//! sections and foreign content (`<svg>`, `<math>`). When it cannot be sure,
//! it reports the stricter context.
//!
//! # The tables
//!
//! The rules are tables; the code around them only applies them. A reader
//! who wants to know what the tracker does reads the tables in this order:
//!
//! 1. [`ELEMENTS`]: one row per element name that matters, with flags (raw
//!    text, void, breakout, integration point, `<select>`, `<noscript>`...).
//!    An element not in the table has no flags.
//! 2. [`URL_ATTRS`], [`ANIMATION_URL_ATTRS`], [`SCRIPT_SRC_ATTRS`],
//!    [`DESCRIBING_ATTRS`], [`URL_LISTS`] and [`SCRIPT_SCHEMES`]: the
//!    attributes whose values are URLs or otherwise special, and the schemes
//!    that run code. `decide_url` (code) reads a URL's start by them.
//! 3. [`RULES`]: the character rules, one table per context ([`Ctx`]). A row
//!    says what the text at the current character must be ([`On`]), what
//!    must hold around it ([`When`]), what to do ([`Then`]) and how many
//!    characters that reads ([`Read`]). The first matching row of the
//!    current context wins; the last row of every context matches anything.
//!    `feed_text` is the interpreter.
//! 4. [`START_RULES`] and [`END_RULES`]: what a start tag and an end tag do
//!    to the elements open inside `<svg>` or `<math>` ([`HtmlTracker::open`]),
//!    by what the innermost open element is ([`Top`]) and what the tag is
//!    ([`TagIs`]). `open_start` and `open_end` are the interpreters.
//! 5. [`COMMENT_ENDS_AFTER_VALUE`] and [`COMMENT_END_STARTS`]: the text that
//!    ends a comment after a value (`comment_end_after`).
//!
//! What a tag does when its `>` is read (`close_tag`), what context a
//! position reports (`context`), and what a block, loop or jump must keep
//! (`end_block`, `end_loop`, `same_place`) are code, each a short list of
//! steps that name the table they use. The JavaScript reader (`Js`,
//! `Reading`) stays code: it follows several readings at once and merges
//! them, which a row per character does not express.
//!
//! A comment on a row names the WHATWG rule it follows and, where there is
//! one, the assessment that found the case (`docs/security-audit.md`).

use crate::ast::EchoContext;

// ---------------------------------------------------------------------------
// 1. Elements
// ---------------------------------------------------------------------------

/// What an element is for the tracker: bits of `Element::flags`.
type Flags = u32;
/// Its text is read raw in HTML content, up to its end tag (the WHATWG
/// RAWTEXT, RCDATA, script data and PLAINTEXT states).
const RAW: Flags = 1 << 0;
/// Its text is raw inside SVG and MathML too (the SVG `<script>` and
/// `<style>`; browsers read their text as markup, see `ForeignCdata`).
const RAW_IN_FOREIGN: Flags = 1 << 1;
/// A browser may read its text as raw text where this tracker reads markup,
/// so its end tag in a comment or an attribute value is watched (`RAW`, and
/// `<noscript>` with scripting on; found by the 1.0.0 and 1.1.1 audits).
const HIDES_END: Flags = 1 << 2;
/// Its start tag inside `<select>` is ignored by older parsers, which then
/// read its text as markup (R19-26).
const IGNORED_IN_SELECT: Flags = 1 << 3;
/// Has no end tag (`<plaintext>`): its text is reported as `Text`.
const NO_END_TAG: Flags = 1 << 4;
/// Never open after its start tag (the WHATWG void elements, and the obsolete
/// ones browsers treat alike).
const VOID: Flags = 1 << 5;
/// A start tag that ends SVG or MathML content for browsers where it is not
/// in an integration point (the WHATWG "in foreign content" list; R19-8).
/// `<font>` only with `color`, `face` or `size`; it is counted always.
const BREAKOUT: Flags = 1 << 6;
/// `<svg>`: opens SVG content.
const SVG_ROOT: Flags = 1 << 7;
/// `<math>`: opens MathML content.
const MATH_ROOT: Flags = 1 << 8;
/// An SVG element whose content is HTML (an HTML integration point).
const SVG_HTML_POINT: Flags = 1 << 9;
/// A MathML element whose content is HTML but for `<mglyph>` and
/// `<malignmark>` (a text integration point).
const MATH_TEXT_POINT: Flags = 1 << 10;
/// MathML `<annotation-xml>`: HTML or MathML by its `encoding`, which is not
/// followed (R19-8).
const ANNOTATION_XML: Flags = 1 << 11;
/// Stays MathML inside a text integration point.
const MGLYPH: Flags = 1 << 12;
/// An SVG animation element, whose `to`, `from`, `values`, `by` and
/// `attributeName` can set `href`.
const ANIMATION: Flags = 1 << 13;
/// `<select>`: see `IGNORED_IN_SELECT`.
const SELECT: Flags = 1 << 14;
/// `<noscript>`: raw text for browsers with scripting on.
const NOSCRIPT: Flags = 1 << 15;
/// `<meta>`: an undescribed one acts on its `content` (found by the 1.0 audit).
const META: Flags = 1 << 16;
/// `<script>`: its text is JavaScript, its URL attributes get the nonce.
const SCRIPT: Flags = 1 << 17;
/// `<style>`: its text is CSS.
const STYLE: Flags = 1 << 18;
/// `<svg>` or `<math>`.
const FOREIGN_ROOT: Flags = SVG_ROOT | MATH_ROOT;

struct Element {
    name: &'static str,
    flags: Flags,
}

/// The elements the tracker knows, by their lowercased name. The raw-text
/// elements first, then the elements with a part in foreign content, then
/// the void and breakout lists of the WHATWG specification.
const ELEMENTS: &[Element] = &[
    // Raw text (WHATWG 13.2.5: script data, RAWTEXT, RCDATA, PLAINTEXT).
    Element { name: "script", flags: RAW | RAW_IN_FOREIGN | HIDES_END | SCRIPT },
    Element { name: "style", flags: RAW | RAW_IN_FOREIGN | HIDES_END | IGNORED_IN_SELECT | STYLE },
    Element { name: "textarea", flags: RAW | HIDES_END },
    // `<title>` is RCDATA in HTML and an HTML integration point in SVG, where it is parsed as markup.
    Element { name: "title", flags: RAW | HIDES_END | IGNORED_IN_SELECT | SVG_HTML_POINT },
    Element { name: "xmp", flags: RAW | HIDES_END | IGNORED_IN_SELECT },
    Element { name: "iframe", flags: RAW | HIDES_END | IGNORED_IN_SELECT },
    Element { name: "noembed", flags: RAW | HIDES_END | IGNORED_IN_SELECT },
    Element { name: "noframes", flags: RAW | HIDES_END | IGNORED_IN_SELECT },
    Element { name: "plaintext", flags: RAW | HIDES_END | IGNORED_IN_SELECT | NO_END_TAG },
    // Raw text when scripting is on (found by the 1.0.0 and 1.1.1 audits).
    Element { name: "noscript", flags: HIDES_END | NOSCRIPT },
    Element { name: "select", flags: SELECT },
    // Foreign content (WHATWG 13.2.6.5 "in foreign content", 13.2.6.4 integration points).
    Element { name: "svg", flags: SVG_ROOT },
    Element { name: "math", flags: MATH_ROOT },
    Element { name: "foreignobject", flags: SVG_HTML_POINT },
    Element { name: "desc", flags: SVG_HTML_POINT },
    Element { name: "mi", flags: MATH_TEXT_POINT },
    Element { name: "mo", flags: MATH_TEXT_POINT },
    Element { name: "mn", flags: MATH_TEXT_POINT },
    Element { name: "ms", flags: MATH_TEXT_POINT },
    Element { name: "mtext", flags: MATH_TEXT_POINT },
    Element { name: "annotation-xml", flags: ANNOTATION_XML },
    Element { name: "mglyph", flags: MGLYPH },
    Element { name: "malignmark", flags: MGLYPH },
    // SVG animation elements can set `href` to their `to`/`from`/`values`.
    Element { name: "animate", flags: ANIMATION },
    Element { name: "set", flags: ANIMATION },
    Element { name: "animatemotion", flags: ANIMATION },
    Element { name: "animatetransform", flags: ANIMATION },
    // Void elements (WHATWG 13.1.2, plus the obsolete ones browsers treat alike).
    Element { name: "area", flags: VOID },
    Element { name: "base", flags: VOID },
    Element { name: "basefont", flags: VOID },
    Element { name: "bgsound", flags: VOID },
    Element { name: "br", flags: VOID | BREAKOUT },
    Element { name: "col", flags: VOID },
    Element { name: "embed", flags: VOID | BREAKOUT },
    Element { name: "frame", flags: VOID },
    Element { name: "hr", flags: VOID | BREAKOUT },
    Element { name: "image", flags: VOID },
    Element { name: "img", flags: VOID | BREAKOUT },
    Element { name: "input", flags: VOID },
    Element { name: "keygen", flags: VOID },
    Element { name: "link", flags: VOID },
    Element { name: "meta", flags: VOID | BREAKOUT | META },
    Element { name: "param", flags: VOID },
    Element { name: "source", flags: VOID },
    Element { name: "track", flags: VOID },
    Element { name: "wbr", flags: VOID },
    // Breakout start tags (WHATWG 13.2.6.5; R19-8): the rest of the list.
    Element { name: "b", flags: BREAKOUT },
    Element { name: "big", flags: BREAKOUT },
    Element { name: "blockquote", flags: BREAKOUT },
    Element { name: "body", flags: BREAKOUT },
    Element { name: "center", flags: BREAKOUT },
    Element { name: "code", flags: BREAKOUT },
    Element { name: "dd", flags: BREAKOUT },
    Element { name: "div", flags: BREAKOUT },
    Element { name: "dl", flags: BREAKOUT },
    Element { name: "dt", flags: BREAKOUT },
    Element { name: "em", flags: BREAKOUT },
    Element { name: "font", flags: BREAKOUT },
    Element { name: "h1", flags: BREAKOUT },
    Element { name: "h2", flags: BREAKOUT },
    Element { name: "h3", flags: BREAKOUT },
    Element { name: "h4", flags: BREAKOUT },
    Element { name: "h5", flags: BREAKOUT },
    Element { name: "h6", flags: BREAKOUT },
    Element { name: "head", flags: BREAKOUT },
    Element { name: "i", flags: BREAKOUT },
    Element { name: "li", flags: BREAKOUT },
    Element { name: "listing", flags: BREAKOUT },
    Element { name: "menu", flags: BREAKOUT },
    Element { name: "nobr", flags: BREAKOUT },
    Element { name: "ol", flags: BREAKOUT },
    Element { name: "p", flags: BREAKOUT },
    Element { name: "pre", flags: BREAKOUT },
    Element { name: "ruby", flags: BREAKOUT },
    Element { name: "s", flags: BREAKOUT },
    Element { name: "small", flags: BREAKOUT },
    Element { name: "span", flags: BREAKOUT },
    Element { name: "strike", flags: BREAKOUT },
    Element { name: "strong", flags: BREAKOUT },
    Element { name: "sub", flags: BREAKOUT },
    Element { name: "sup", flags: BREAKOUT },
    Element { name: "table", flags: BREAKOUT },
    Element { name: "tt", flags: BREAKOUT },
    Element { name: "u", flags: BREAKOUT },
    Element { name: "ul", flags: BREAKOUT },
    Element { name: "var", flags: BREAKOUT },
];

/// The flags of an element (none for one not in the table).
fn flags(name: &str) -> Flags {
    ELEMENTS.iter().find(|e| e.name == name).map_or(0, |e| e.flags)
}

/// Whether an element has every flag of `mask`.
fn has(name: &str, mask: Flags) -> bool {
    flags(name) & mask == mask
}

/// The raw-text element `tag` names, for `EchoContext::RawText` (not
/// `<plaintext>`, which has no end tag).
fn raw_text_name(tag: &str) -> Option<&'static str> {
    ELEMENTS.iter().find(|e| e.name == tag && e.flags & RAW != 0 && e.flags & NO_END_TAG == 0).map(|e| e.name)
}

/// Whether `name` (lowercase letters) begins the name of an element with the flag.
fn starts_element_with(name: &str, flag: Flags) -> bool {
    ELEMENTS.iter().any(|e| e.flags & flag != 0 && e.name.starts_with(name))
}

/// The most elements followed inside SVG or MathML (see `HtmlTracker::open`).
const MAX_OPEN: usize = 4096;

// ---------------------------------------------------------------------------
// 2. Attributes and URLs
// ---------------------------------------------------------------------------

/// Attributes whose value is a URL (the WHATWG ones and the legacy ones
/// browsers still follow).
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

/// On an `ANIMATION` element, attributes that name or set `href`.
const ANIMATION_URL_ATTRS: &[&str] = &["to", "from", "values", "by", "attributename"];

/// On `<script>`, the script's URL (`src`; `href` in SVG): the tag gets the
/// CSP nonce, so a URL inserted there would run as a trusted script.
const SCRIPT_SRC_ATTRS: &[&str] = &["src", "href", "xlink:href"];

/// A `<meta>` with one of these describes the page; its `content` is text.
/// Without one (`http-equiv="refresh"`), `content` acts.
const DESCRIBING_ATTRS: &[&str] = &["name", "property", "itemprop"];

/// Attributes that hold a list of URLs: after the separator, a URL starts
/// again (each candidate of `srcset`; each value of an animation's `values`).
/// The element must have the flags (0: any element).
const URL_LISTS: &[(&str, char, Flags)] = &[("srcset", ',', 0), ("values", ';', ANIMATION)];

/// Schemes that run code or hold a document: a URL that starts with one is
/// `ScriptUrl`. Only the first two are inline code for the CSP warning
/// (`data:` is fine for images).
const SCRIPT_SCHEMES: &[&str] = &["javascript:", "vbscript:", "data:"];

/// How much of a URL's start is kept undecided (a prefix of a scheme, or `&`).
const MAX_URL_PREFIX: usize = 256;

fn is_url_attr(tag: &str, name: &str) -> bool {
    URL_ATTRS.contains(&name) || (has(tag, ANIMATION) && ANIMATION_URL_ATTRS.contains(&name))
}

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
                let s = format!("{}:", scheme.to_ascii_lowercase());
                return Some(if SCRIPT_SCHEMES.contains(&s.as_str()) { UrlState::Script } else { UrlState::Safe });
            }
            '&' => return None,
            c => scheme.push(c),
        }
    }
    None
}

/// Whether `p` (lowercased, without spaces) may still become a script scheme.
fn may_become_script_scheme(p: &str) -> bool {
    SCRIPT_SCHEMES.iter().any(|s| s.starts_with(p))
}

// ---------------------------------------------------------------------------
// 3. Contexts and the character rules
// ---------------------------------------------------------------------------

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
        /// The element's flags (`ELEMENTS`), looked up once.
        flags: Flags,
        escape: u8,
        /// Where the script's JavaScript has reached (`<script>` only).
        js: Js,
    },
    /// `<![CDATA[` inside SVG or MathML, up to `]]>`. Where a browser's current
    /// element is HTML it is a bogus comment that ends at the first `>`, so
    /// that must be the `>` of `]]>` (`brackets`: how many `]` just came).
    Cdata {
        brackets: u8,
    },
}

/// A context: a `State` without its data, and the tag name being read
/// (`HtmlTracker::pending`). The order is that of `RULES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctx {
    Text,
    /// After `<` or `</` and a letter: the tag name.
    TagName,
    Tag,
    AttrName,
    BeforeValue,
    Quoted,
    Unquoted,
    Comment,
    Bogus,
    Cdata,
    Raw,
}

impl State {
    fn ctx(&self) -> Ctx {
        match self {
            State::Text => Ctx::Text,
            State::Comment => Ctx::Comment,
            State::Bogus => Ctx::Bogus,
            State::Tag { .. } => Ctx::Tag,
            State::AttrName { .. } => Ctx::AttrName,
            State::BeforeValue { .. } => Ctx::BeforeValue,
            State::Quoted { .. } => Ctx::Quoted,
            State::Unquoted { .. } => Ctx::Unquoted,
            State::Raw { .. } => Ctx::Raw,
            State::Cdata { .. } => Ctx::Cdata,
        }
    }

    /// The tag and whether it is an end tag, inside one.
    fn take_tag(self) -> (String, bool) {
        match self {
            State::Tag { name, closing } => (name, closing),
            State::AttrName { tag, closing, .. }
            | State::BeforeValue { tag, closing, .. }
            | State::Quoted { tag, closing, .. }
            | State::Unquoted { tag, closing, .. } => (tag, closing),
            _ => (String::new(), false),
        }
    }
}

/// A class of characters, as the HTML tokenizer reads them.
#[derive(Clone, Copy, Debug)]
enum Class {
    /// Tab, LF, FF, CR and space only (found by the 1.1 audit: U+3000 or
    /// U+00A0 between attributes was taken for a separator, while browsers
    /// read it as part of a name or value).
    Space,
    /// `Space`, or `/`.
    SpaceOrSlash,
    /// A character of a tag name: anything but `Space`, `/` and `>` (found by
    /// the 1.1 audit: `<div\u{3000}title="` ended the name here, while
    /// browsers read on).
    NameChar,
    /// An ASCII letter.
    Alpha,
    /// `"` or `'`.
    Quote,
    /// What may start markup after `<` in a foreign raw-text element: a
    /// letter, `!`, `/` or `?`.
    MarkupStart,
}

impl Class {
    fn has(self, c: char) -> bool {
        match self {
            Class::Space => html_space(c),
            Class::SpaceOrSlash => html_space(c) || c == '/',
            Class::NameChar => !html_space(c) && c != '/' && c != '>',
            Class::Alpha => c.is_ascii_alphabetic(),
            Class::Quote => c == '"' || c == '\'',
            Class::MarkupStart => c.is_ascii_alphabetic() || matches!(c, '!' | '/' | '?'),
        }
    }
}

/// What the text at the current character must be for a rule to apply.
#[derive(Clone, Copy, Debug)]
enum On {
    Any,
    Is(char),
    Not(char),
    In(Class),
    /// This text, in any ASCII case.
    Lit(&'static str),
    /// This text, exactly.
    Exact(&'static str),
    /// The quote that opened the current attribute value.
    TheQuote,
    /// The raw-text element's end tag, `</name` and a delimiter (space, `/`
    /// or `>`; `</scripts>` is not one).
    EndTag,
    /// The rest of the text is shorter than this and begins it (any ASCII
    /// case): what follows (a value, a block) could complete it, so the rest
    /// is read again in front of the next text.
    RestStarts(&'static str),
    /// The same, exactly.
    RestStartsExact(&'static str),
    /// The rest of the text is the raw-text element's `</name` or a start of it.
    RestStartsEndTag,
}

/// What must hold around the match for a rule to apply.
#[derive(Clone, Copy, Debug)]
enum When {
    /// The match ends the text.
    AtEnd,
    /// The character after the match is in the class.
    Next(Class),
    /// After the match: HTML whitespace, `/` or `>`.
    DelimAfter,
    /// After this character: HTML whitespace, then `=` (`name =value`).
    SpacesThenEq,
    /// After the match: `-`s, then `>` (`<!-->`, `<!--->` in a script).
    DashesThenGt,
    InForeign,
    /// The raw-text element is `<script>`.
    Script,
    NotScript,
    /// The script's escape state is this.
    Escape(u8),
    /// The script's escape state is 1 or 2.
    Escaped,
    /// This many `]` just came in a CDATA section.
    Brackets(u8),
}

/// What a rule does.
#[derive(Clone, Copy, Debug)]
enum Then {
    Stay,
    Text,
    /// Open a comment (its start is remembered for the end of the text).
    Comment,
    Bogus,
    Cdata,
    /// Start reading a tag name (`""`) or an end tag's name (`"/"`).
    TagName(&'static str),
    /// Keep the rest of the text for the next one (`carry`) and stop.
    Carry,
    /// Add the character to the tag or attribute name, lowercased.
    NamePush,
    /// The tag name is complete: into the tag (its attributes).
    NameEnd,
    /// `>`: the tag is complete (`close_tag`).
    Close(SelfClosing),
    /// Start an attribute name with this character.
    AttrName,
    /// `=`: the attribute has a value.
    AttrValue,
    /// The attribute has no value: back between attributes.
    AttrDone,
    /// A quoted value begins (a URL state for a URL attribute).
    Quoted,
    Unquoted,
    /// Back between attributes after a value.
    ToTag,
    /// A character of a quoted value: the URL's start follows it (`url_char`).
    UrlChar,
    /// The script's escape state becomes this.
    Escape(u8),
    /// The raw-text element's end tag: into it.
    CloseRaw,
    /// Markup in a foreign raw-text element (`ForeignCdata`).
    ForeignMarkup,
    /// A `]` in a CDATA section.
    Bracket,
    /// Another character in a CDATA section.
    Brackets0,
    /// A `>` in a CDATA section that is not the end of `]]>` (`CdataEnd`).
    CdataEnd,
}

/// Whether `<tag ... />` is self-closing when its `>` is read.
#[derive(Clone, Copy, Debug)]
enum SelfClosing {
    /// Yes if the character before `>` is `/` (between attributes).
    IfSlashBefore,
    /// Never (`>` ends an unquoted value or follows `=`).
    Never,
}

/// How many characters a rule reads.
#[derive(Clone, Copy, Debug)]
enum Read {
    One,
    /// None: the same character is read again in the new context.
    Zero,
    /// The matched text.
    Match,
    /// The run of HTML whitespace here (at least one character; found by the
    /// 1.0.0 audit: looking ahead at each space was quadratic).
    Spaces,
    /// Through the `>` of `DashesThenGt`.
    ThroughGt,
    /// The rest of the text.
    Rest,
}

/// A row of the character rules.
struct Rule {
    on: On,
    when: &'static [When],
    then: Then,
    read: Read,
}

use Class::*;
use Read::{Match, One, Rest, Zero};

/// The character rules, one table per context, in the order of `Ctx`. The
/// first matching row wins; the last row of each table matches anything.
const RULES: &[(Ctx, &[Rule])] = &[
    // Text (WHATWG data state and tag open state).
    (
        Ctx::Text,
        &[
            Rule { on: On::Not('<'), when: &[], then: Then::Stay, read: One },
            // `<!-->` and `<!--->` are complete (empty) comments.
            Rule { on: On::Lit("<!-->"), when: &[], then: Then::Text, read: Match },
            Rule { on: On::Lit("<!--->"), when: &[], then: Then::Text, read: Match },
            Rule { on: On::Lit("<!--"), when: &[], then: Then::Comment, read: Match },
            // `<` at the end: what follows decides what it starts.
            Rule { on: On::Is('<'), when: &[When::AtEnd], then: Then::Carry, read: Rest },
            Rule { on: On::Is('<'), when: &[When::Next(Alpha)], then: Then::TagName(""), read: One },
            Rule { on: On::Lit("</"), when: &[When::AtEnd], then: Then::Carry, read: Rest },
            // `<!` or `<!-` at the end: a value or block could make a comment.
            Rule { on: On::RestStartsExact("<!--"), when: &[], then: Then::Carry, read: Rest },
            // In SVG and MathML, `<![CDATA[` (in this case only) starts a CDATA section
            // (R19-9), so its start is read again with what follows too.
            Rule { on: On::RestStartsExact("<![CDATA["), when: &[When::InForeign], then: Then::Carry, read: Rest },
            Rule { on: On::Exact("<![CDATA["), when: &[When::InForeign], then: Then::Cdata, read: Match },
            Rule { on: On::Lit("</"), when: &[When::Next(Alpha)], then: Then::TagName("/"), read: Match },
            // `</>` is nothing; `</` and another character is a bogus comment.
            Rule { on: On::Lit("</>"), when: &[], then: Then::Text, read: Match },
            Rule { on: On::Lit("</"), when: &[], then: Then::Bogus, read: Match },
            Rule { on: On::Lit("<!"), when: &[], then: Then::Bogus, read: One },
            Rule { on: On::Lit("<?"), when: &[], then: Then::Bogus, read: One },
            Rule { on: On::Any, when: &[], then: Then::Stay, read: One },
        ],
    ),
    // A tag name goes on as browsers read it, up to HTML whitespace, `/` or `>`.
    (
        Ctx::TagName,
        &[
            Rule { on: On::In(NameChar), when: &[], then: Then::NamePush, read: One },
            Rule { on: On::Any, when: &[], then: Then::NameEnd, read: Zero },
        ],
    ),
    // Between attributes (WHATWG before attribute name state).
    (
        Ctx::Tag,
        &[
            Rule { on: On::Is('>'), when: &[], then: Then::Close(SelfClosing::IfSlashBefore), read: One },
            Rule { on: On::In(SpaceOrSlash), when: &[], then: Then::Stay, read: One },
            Rule { on: On::Any, when: &[], then: Then::AttrName, read: One },
        ],
    ),
    // An attribute name (WHATWG attribute name and after attribute name states).
    (
        Ctx::AttrName,
        &[
            Rule { on: On::Is('='), when: &[], then: Then::AttrValue, read: One },
            Rule { on: On::Is('>'), when: &[], then: Then::Close(SelfClosing::IfSlashBefore), read: One },
            // `name =value` is rare; a space ends a valueless attribute unless `=` follows.
            Rule { on: On::In(Space), when: &[When::SpacesThenEq], then: Then::Stay, read: Read::Spaces },
            Rule { on: On::In(SpaceOrSlash), when: &[], then: Then::AttrDone, read: Read::Spaces },
            Rule { on: On::Any, when: &[], then: Then::NamePush, read: One },
        ],
    ),
    // After `name=` (WHATWG before attribute value state).
    (
        Ctx::BeforeValue,
        &[
            Rule { on: On::In(Quote), when: &[], then: Then::Quoted, read: One },
            Rule { on: On::In(Space), when: &[], then: Then::Stay, read: One },
            Rule { on: On::Is('>'), when: &[], then: Then::Close(SelfClosing::Never), read: One },
            Rule { on: On::Any, when: &[], then: Then::Unquoted, read: One },
        ],
    ),
    // A quoted value ends only at its quote.
    (
        Ctx::Quoted,
        &[
            Rule { on: On::TheQuote, when: &[], then: Then::ToTag, read: One },
            Rule { on: On::Any, when: &[], then: Then::UrlChar, read: One },
        ],
    ),
    // An unquoted value ends at HTML whitespace or `>`.
    (
        Ctx::Unquoted,
        &[
            Rule { on: On::In(Space), when: &[], then: Then::ToTag, read: One },
            Rule { on: On::Is('>'), when: &[], then: Then::Close(SelfClosing::Never), read: One },
            Rule { on: On::Any, when: &[], then: Then::Stay, read: One },
        ],
    ),
    // A comment ends at `-->` or `--!>` (WHATWG comment end and comment end bang states).
    (
        Ctx::Comment,
        &[
            Rule { on: On::Lit("-->"), when: &[], then: Then::Text, read: Match },
            Rule { on: On::Lit("--!>"), when: &[], then: Then::Text, read: Match },
            Rule { on: On::Any, when: &[], then: Then::Stay, read: One },
        ],
    ),
    // A bogus comment ends at the first `>`.
    (
        Ctx::Bogus,
        &[
            Rule { on: On::Is('>'), when: &[], then: Then::Text, read: One },
            Rule { on: On::Any, when: &[], then: Then::Stay, read: One },
        ],
    ),
    // A CDATA section ends at `]]>`; a `>` elsewhere is refused (R19-9).
    (
        Ctx::Cdata,
        &[
            Rule { on: On::Is(']'), when: &[], then: Then::Bracket, read: One },
            Rule { on: On::Is('>'), when: &[When::Brackets(2)], then: Then::Text, read: One },
            Rule { on: On::Is('>'), when: &[], then: Then::CdataEnd, read: One },
            Rule { on: On::Any, when: &[], then: Then::Brackets0, read: One },
        ],
    ),
    // Raw text (WHATWG RAWTEXT, RCDATA and script data states). In a script,
    // the starts of `<!--`, `<script` and `-->` at the end of the text are
    // carried too: they change its escape state.
    (
        Ctx::Raw,
        &[
            Rule { on: On::RestStarts("<!--"), when: &[When::Script, When::Escape(0)], then: Then::Carry, read: Rest },
            Rule {
                on: On::RestStarts("<script"),
                when: &[When::Script, When::Escape(1)],
                then: Then::Carry,
                read: Rest,
            },
            Rule { on: On::RestStarts("-->"), when: &[When::Script, When::Escaped], then: Then::Carry, read: Rest },
            // `</tex` + a value or block: it might end the element.
            Rule { on: On::RestStartsEndTag, when: &[], then: Then::Carry, read: Rest },
            // In the double escaped state, `</script>` returns to the escaped state.
            Rule { on: On::EndTag, when: &[When::Script, When::Escape(2)], then: Then::Escape(1), read: Match },
            Rule { on: On::EndTag, when: &[], then: Then::CloseRaw, read: Match },
            // In SVG and MathML a script's text is markup: a tag, comment, CDATA
            // section or bogus comment can hide its end tag from browsers but not from
            // here (found by the 1.0 audit for CDATA and comments, by the 1.0.0 one for
            // tags and bogus comments).
            Rule {
                on: On::Is('<'),
                when: &[When::InForeign, When::Next(MarkupStart)],
                then: Then::ForeignMarkup,
                read: One,
            },
            Rule { on: On::Any, when: &[When::NotScript], then: Then::Stay, read: One },
            // Script data escape states (WHATWG 13.2.5.18 to 13.2.5.29): `<!-->` and
            // `<!--->` return to script data at once.
            Rule {
                on: On::Lit("<!--"),
                when: &[When::Escape(0), When::DashesThenGt],
                then: Then::Stay,
                read: Read::ThroughGt,
            },
            Rule { on: On::Lit("<!--"), when: &[When::Escape(0)], then: Then::Escape(1), read: Match },
            Rule { on: On::Lit("-->"), when: &[When::Escaped], then: Then::Escape(0), read: Match },
            Rule {
                on: On::Lit("<script"),
                when: &[When::Escape(1), When::DelimAfter],
                then: Then::Escape(2),
                read: Match,
            },
            Rule { on: On::Any, when: &[], then: Then::Stay, read: One },
        ],
    ),
];

/// The rules of a context.
fn rules(ctx: Ctx) -> &'static [Rule] {
    let (c, rules) = RULES[ctx as usize];
    debug_assert_eq!(c, ctx);
    rules
}

// ---------------------------------------------------------------------------
// 4. The open elements of foreign content
// ---------------------------------------------------------------------------

/// The namespace of an element open inside SVG or MathML.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ns {
    Html,
    Svg,
    Math,
}

/// An element open inside the outermost `<svg>` or `<math>` (see `HtmlTracker::open`).
#[derive(Clone, Debug, PartialEq)]
struct Open {
    name: String,
    ns: Ns,
}

/// What the innermost open element is for the tag inside it (WHATWG 13.2.6.4
/// and 13.2.6.5: whether the tag is read as HTML or as foreign content).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Top {
    /// No element open: HTML content outside `<svg>` and `<math>`.
    None,
    /// An HTML element, opened in an integration point.
    Html,
    /// An SVG HTML integration point (`<foreignObject>`, `<desc>`, `<title>`).
    HtmlPoint,
    /// A MathML text integration point (`<mi>`, `<mo>`, `<mn>`, `<ms>`, `<mtext>`).
    TextPoint,
    /// MathML `<annotation-xml>`.
    AnnotationXml,
    /// Any other SVG or MathML element.
    Foreign,
}

impl Open {
    fn top(&self) -> Top {
        match self.ns {
            Ns::Html => Top::Html,
            Ns::Svg if has(&self.name, SVG_HTML_POINT) => Top::HtmlPoint,
            Ns::Math if has(&self.name, MATH_TEXT_POINT) => Top::TextPoint,
            Ns::Math if has(&self.name, ANNOTATION_XML) => Top::AnnotationXml,
            _ => Top::Foreign,
        }
    }
}

/// The tops whose content browsers read as HTML.
const HTML_CONTENT: &[Top] = &[Top::Html, Top::HtmlPoint, Top::TextPoint, Top::AnnotationXml];
/// The tops inside SVG or MathML (an open element that is not HTML).
const FOREIGN_TOPS: &[Top] = &[Top::HtmlPoint, Top::TextPoint, Top::AnnotationXml, Top::Foreign];

/// A class of tag names in `START_RULES` and `END_RULES`.
#[derive(Clone, Copy, Debug)]
enum TagIs {
    Any,
    /// Has every flag of the mask.
    Has(Flags),
    /// Has a flag of the mask.
    AnyOf(Flags),
    /// Lacks every flag of the mask.
    Lacks(Flags),
    /// One of these names.
    Named(&'static [&'static str]),
    /// The name of the innermost open element.
    TopName,
    /// A foreign element of this name is open above the innermost HTML element.
    ForeignOpenAbove,
    /// An HTML element of this name is open.
    HtmlOpen,
}

/// What a row does to the open elements.
#[derive(Clone, Copy, Debug)]
enum Stack {
    Nothing,
    /// Report the error, then go on to the next matching row.
    Continue,
    /// Open `<svg>` or `<math>` outside foreign content.
    PushRoot,
    PushSvg,
    PushMath,
    PushHtml,
    /// Open the element in the namespace of the innermost open element.
    PushSameNs,
    /// Close the innermost open element.
    Pop,
    /// Close the innermost foreign element of this name, wherever it is.
    CloseForeignNamed,
    /// Close the innermost foreign element of this name above the innermost
    /// HTML element.
    CloseForeignAbove,
    /// Close the innermost HTML element of this name.
    CloseHtmlNamed,
}

/// What a row reports (`InlineCode::ForeignStructure`; the messages are in
/// `report`), or nothing.
#[derive(Clone, Copy, Debug)]
enum Wrong {
    None,
    /// An element inside `<annotation-xml>` (R19-8).
    AnnotationXml,
    /// A breakout start tag directly in foreign content (R19-8).
    Breakout,
    /// `</svg>` or `</math>` that browsers ignore (R19-8).
    IgnoredEnd,
    /// `</p>` or `</br>` directly in foreign content.
    BreakoutEnd,
    /// An end tag inside foreign content that an HTML element around it is open for.
    EndAround,
}

/// Whether `<tag ... />` was self-closing (start rules only).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Closed {
    Any,
    Yes,
    No,
}

/// A row of `START_RULES` or `END_RULES`: what the innermost open element
/// is, what the tag is, whether it is self-closing, what is reported, and
/// what is done.
struct OpenRule {
    top: &'static [Top],
    tag: TagIs,
    self_closing: Closed,
    wrong: Wrong,
    then: Stack,
}

impl OpenRule {
    const fn new(top: &'static [Top], tag: TagIs, self_closing: Closed, wrong: Wrong, then: Stack) -> OpenRule {
        OpenRule { top, tag, self_closing, wrong, then }
    }
}

/// What a start tag does to the open elements (`open_start`). Before the
/// table: when `MAX_OPEN` elements are open, the tag is ignored (reported once).
/// Columns: top, tag, self-closing, reported, done.
const START_RULES: &[OpenRule] = &[
    // Outside foreign content, `<svg>` and `<math>` open it; a self-closed one
    // (`<svg/>`) opens nothing (found by the 1.0.0 audit).
    OpenRule::new(&[Top::None], TagIs::AnyOf(FOREIGN_ROOT), Closed::No, Wrong::None, Stack::PushRoot),
    OpenRule::new(&[Top::None], TagIs::Any, Closed::Any, Wrong::None, Stack::Nothing),
    // In a text integration point, `<mglyph>` and `<malignmark>` stay MathML.
    OpenRule::new(&[Top::TextPoint], TagIs::Has(MGLYPH), Closed::Yes, Wrong::None, Stack::Nothing),
    OpenRule::new(&[Top::TextPoint], TagIs::Has(MGLYPH), Closed::No, Wrong::None, Stack::PushSameNs),
    // `<annotation-xml>` holds HTML or MathML depending on its `encoding`, which is
    // not followed here; `<svg>` is SVG either way (R19-8). Read on as HTML.
    OpenRule::new(&[Top::AnnotationXml], TagIs::Lacks(SVG_ROOT), Closed::Any, Wrong::AnnotationXml, Stack::Continue),
    // HTML content: `<svg>` and `<math>` nest (not when self-closed); browsers
    // ignore `/>` on HTML elements, so only void elements stay closed.
    OpenRule::new(HTML_CONTENT, TagIs::AnyOf(FOREIGN_ROOT), Closed::Yes, Wrong::None, Stack::Nothing),
    OpenRule::new(HTML_CONTENT, TagIs::Has(SVG_ROOT), Closed::Any, Wrong::None, Stack::PushSvg),
    OpenRule::new(HTML_CONTENT, TagIs::Has(MATH_ROOT), Closed::Any, Wrong::None, Stack::PushMath),
    OpenRule::new(HTML_CONTENT, TagIs::Has(VOID), Closed::Any, Wrong::None, Stack::Nothing),
    OpenRule::new(HTML_CONTENT, TagIs::Any, Closed::Any, Wrong::None, Stack::PushHtml),
    // Foreign content: a breakout tag ends it for browsers (R19-8); it is refused,
    // and read on as if it were in a `<foreignObject>`.
    OpenRule::new(&[Top::Foreign], TagIs::Has(BREAKOUT | VOID), Closed::Any, Wrong::Breakout, Stack::Nothing),
    OpenRule::new(&[Top::Foreign], TagIs::Has(BREAKOUT), Closed::Any, Wrong::Breakout, Stack::PushHtml),
    // Other elements open in the namespace of their parent (`<path/>` opens nothing).
    OpenRule::new(&[Top::Foreign], TagIs::Any, Closed::Yes, Wrong::None, Stack::Nothing),
    OpenRule::new(&[Top::Foreign], TagIs::Any, Closed::No, Wrong::None, Stack::PushSameNs),
];

/// What an end tag does to the open elements (`open_end`). Columns: top,
/// tag, self-closing (any), reported, done.
const END_RULES: &[OpenRule] = &[
    OpenRule::new(&[Top::None], TagIs::Any, Closed::Any, Wrong::None, Stack::Nothing),
    // Browsers close an HTML element at its end tag; other end tags here (that
    // browsers may or may not act on) leave it open, so a later `</svg>` is
    // refused and read as if it closed (R19-8).
    OpenRule::new(&[Top::Html], TagIs::TopName, Closed::Any, Wrong::None, Stack::Pop),
    OpenRule::new(&[Top::Html], TagIs::AnyOf(FOREIGN_ROOT), Closed::Any, Wrong::IgnoredEnd, Stack::CloseForeignNamed),
    OpenRule::new(&[Top::Html], TagIs::Any, Closed::Any, Wrong::None, Stack::Nothing),
    // `</p>` and `</br>` directly in foreign content end it for browsers.
    OpenRule::new(
        &[Top::AnnotationXml, Top::Foreign],
        TagIs::Named(&["p", "br"]),
        Closed::Any,
        Wrong::BreakoutEnd,
        Stack::Nothing,
    ),
    // Browsers close the innermost SVG or MathML element of that name above the
    // current HTML element (`</math>` in `<svg>` closes nothing; found by the
    // 1.0.0 audit).
    OpenRule::new(FOREIGN_TOPS, TagIs::ForeignOpenAbove, Closed::Any, Wrong::None, Stack::CloseForeignAbove),
    // An HTML element of that name around it: browsers may close it, with the
    // SVG or MathML inside it; refused, and read as if it closed.
    OpenRule::new(FOREIGN_TOPS, TagIs::HtmlOpen, Closed::Any, Wrong::EndAround, Stack::CloseHtmlNamed),
    OpenRule::new(FOREIGN_TOPS, TagIs::Any, Closed::Any, Wrong::None, Stack::Nothing),
];

// ---------------------------------------------------------------------------
// 5. Comments after a value
// ---------------------------------------------------------------------------

/// Text right after a value in a comment that would end it if the value ended
/// with `--`, `--!` or `-`; and the starts of these (`""` too) that later
/// text may complete.
const COMMENT_ENDS_AFTER_VALUE: [&str; 4] = [">", "!>", "->", "-!>"];
const COMMENT_END_STARTS: [&str; 4] = ["", "-", "!", "-!"];

/// Reads `text` after each start in `starts` (bits of `COMMENT_END_STARTS`):
/// whether it completes one of `COMMENT_ENDS_AFTER_VALUE`, and the starts
/// that remain after it (for a short text).
fn comment_end_after(starts: u8, text: &str) -> (bool, u8) {
    let head: String = text.chars().take(4).collect();
    let (mut ends, mut left) = (false, 0);
    for (k, start) in COMMENT_END_STARTS.iter().enumerate() {
        if starts & (1 << k) == 0 {
            continue;
        }
        let s = format!("{start}{head}");
        if COMMENT_ENDS_AFTER_VALUE.iter().any(|e| s.starts_with(e)) {
            ends = true;
        } else if let Some(j) = COMMENT_END_STARTS.iter().position(|p| *p == s) {
            left |= 1 << j;
        }
    }
    (ends, left)
}

/// Where a loop's body begins (see `HtmlTracker::begin_loop`).
#[derive(Clone, Copy, Debug)]
pub struct LoopHead {
    head: u8,
    ends: bool,
}

// ---------------------------------------------------------------------------
// 6. The JavaScript of a `<script>` (code, not a table)
// ---------------------------------------------------------------------------

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
        // One reading that did not fork stays one: nothing to merge.
        if !forks.is_empty() || self.readings.len() > 1 {
            self.readings.extend(forks);
            self.settle();
        }
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

// ---------------------------------------------------------------------------
// 7. The tracker
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct HtmlTracker {
    state: State,
    /// Text of a tag name being read after `<`.
    pending: Option<String>,
    /// The elements open inside the outermost `<svg>` or `<math>` (itself
    /// included; empty outside): foreign content, which has no RCDATA
    /// elements. They are followed as browsers build them, so that an end tag
    /// closes what it closes for a browser: `</svg>` is ignored while an HTML
    /// element opened inside a `<foreignObject>` is open (found by the 1.2
    /// audit), and a self-closed element (`<svg/>`) opens nothing. What this
    /// does not follow (an HTML element right inside SVG, which ends it) is
    /// refused.
    open: Vec<Open>,
    /// `MAX_OPEN` was reached (reported once).
    too_deep: bool,
    /// A `<select>` is open (see `IGNORED_IN_SELECT`).
    select: bool,
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
    /// The last characters of the last literal text (see `hidden_end_tag_start`).
    tail: String,
    /// Depth of `<noscript>` (outside foreign content). Browsers with scripting
    /// on read its content as raw text, up to the first `</noscript`.
    noscript: u32,
    /// A value was inserted into a comment and the literal text after it is
    /// at most the start of `COMMENT_ENDS_AFTER_VALUE` (bits of
    /// `COMMENT_END_STARTS`; 0 for none): the value may end with `-`, `--` or
    /// `--!`, so the text that follows may not complete `-->` or `--!>` with
    /// it (see `feed`).
    after_comment_value: u8,
    /// Nothing was written since the start of the body of the loop being
    /// read but (at most) the start of `COMMENT_ENDS_AFTER_VALUE` (bits of
    /// `COMMENT_END_STARTS`): the body may follow its own end.
    loop_head: u8,
    /// Text at the start of that loop's body completed one of
    /// `COMMENT_ENDS_AFTER_VALUE` (see `end_loop`).
    head_ends_comment: bool,
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
    /// Markup (a tag, comment or CDATA section) in a `<script>` or `<style>` in
    /// SVG or MathML (an error: browsers read it as markup, which can hide the
    /// end tag).
    ForeignCdata,
    /// The end tag of a raw-text element (`</textarea`, `</noscript`...) in a
    /// comment or an attribute value (an error: where a browser reads that
    /// element's text differently, it ends there and what follows is markup).
    HiddenEndTag,
    /// Text after a value in a comment that starts with `>`, `!>`, `->` or
    /// `-!>` (an error: a value ending with `--`, `--!` or `-` would end the
    /// comment there, where this tracker reads on).
    CommentEnd,
    /// Elements in SVG or MathML that browsers do not close (or open) as
    /// they are written, such as `</svg>` while a `<div>` opened inside its
    /// `<foreignObject>` is still open (an error: what follows would be read
    /// as SVG where this tracker reads HTML). The message says which.
    ForeignStructure(String),
    /// `>` inside a CDATA section in SVG or MathML that is not the end of
    /// `]]>` (an error: where it is a bogus comment, it ends there).
    CdataEnd,
    /// A raw-text element such as `<title>` or `<style>` inside `<select>`
    /// (an error: older browsers ignore it there and read its text as markup).
    RawTextInSelect(String),
}

impl Default for HtmlTracker {
    fn default() -> Self {
        HtmlTracker {
            state: State::Text,
            pending: None,
            open: Vec::new(),
            too_deep: false,
            select: false,
            mark: None,
            found: Vec::new(),
            attrs: Vec::new(),
            meta_text: false,
            maybe_undescribed: false,
            here: 0,
            carry: String::new(),
            tail: String::new(),
            noscript: 0,
            after_comment_value: 0,
            loop_head: 0,
            head_ends_comment: false,
        }
    }
}

/// Whether `tail` (literal text) ends where a value could complete the end
/// tag of an element with `flag` (`HIDES_END`, or `NOSCRIPT` alone): after
/// `<`, or `</` and the start of its name.
fn hidden_end_tag_start_of(tail: &str, flag: Flags) -> bool {
    if tail.ends_with('<') {
        return true;
    }
    let lower = tail.to_ascii_lowercase();
    let Some(at) = lower.rfind("</") else { return false };
    let name = &lower[at + 2..];
    name.bytes().all(|b| b.is_ascii_alphabetic()) && starts_element_with(name, flag)
}

/// Whether `tail` ends where a value could complete a hidden end tag.
fn hidden_end_tag_start(tail: &str) -> bool {
    hidden_end_tag_start_of(tail, HIDES_END)
}

/// `hidden_end_tag_start` for `</noscript` alone.
fn hidden_noscript_start(tail: &str) -> bool {
    hidden_end_tag_start_of(tail, NOSCRIPT)
}

fn is_delim(c: Option<&char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_whitespace() || *c == '/' || *c == '>')
}

/// Whitespace as the HTML tokenizer reads it: tab, LF, FF, CR and space only.
fn html_space(c: char) -> bool {
    c.is_ascii_whitespace()
}

/// Whether `s` is at `i` in `chars`, in any ASCII case (compared in place).
fn starts_ci(chars: &[char], i: usize, s: &str) -> bool {
    let mut k = i;
    for want in s.chars() {
        match chars.get(k) {
            Some(c) if c.eq_ignore_ascii_case(&want) => k += 1,
            _ => return false,
        }
    }
    true
}

/// Whether the rest of `chars` from `i` is shorter than `s` and begins it
/// (any ASCII case, or exactly).
fn rest_starts(chars: &[char], i: usize, s: &str, exact: bool) -> bool {
    let left = chars.len() - i;
    left < s.len()
        && chars[i..].iter().zip(s.chars()).all(|(a, b)| if exact { *a == b } else { a.eq_ignore_ascii_case(&b) })
}

impl HtmlTracker {
    /// Where an end tag hidden from this tracker is watched (see `hides`), the
    /// literal text ends where code could complete one: after `<`, or `</`
    /// and the start of a raw-text element's name (inside `<noscript>`,
    /// `<svg>` or `<math>`) or of `noscript` (anywhere).
    pub fn watched_end_tag_start(&self) -> bool {
        let in_comment_or_value = self.in_comment_or_value();
        let in_text = matches!(self.state, State::Text);
        let watched = (in_comment_or_value && self.out_of_step_possible()) || (self.noscript > 0 && !in_text);
        (watched && hidden_end_tag_start(&self.tail)) || (!in_text && hidden_noscript_start(&self.tail))
    }

    /// The context a value inserted at the current position lands in.
    pub fn context(&self) -> EchoContext {
        if !self.carry.is_empty() {
            return EchoContext::AfterMarkup;
        }
        if self.pending.is_some() {
            return EchoContext::Tag;
        }
        if self.watched_end_tag_start() {
            return EchoContext::AfterMarkup;
        }
        // Where a browser may read raw text here, inserted `Html` may not end
        // it (see `out_of_step_possible`; found by the 1.2 audit).
        let watched = self.out_of_step_possible();
        match &self.state {
            // In a comment, inserted `Html` may not end it (found by the 1.1.1 audit).
            State::Comment => EchoContext::Comment { bogus: false, watched },
            // A CDATA section may be a bogus comment for a browser (see `State::Cdata`).
            State::Bogus | State::Cdata { .. } => EchoContext::Comment { bogus: true, watched },
            // Inside SVG or MathML, browsers read inserted markup in foreign
            // content, where this tracker read it (in its own template or
            // function) as HTML (found by the 1.1.1 audit).
            State::Text if self.in_foreign() => EchoContext::ForeignText,
            // Inside `<select>`, older browsers ignore raw-text elements that
            // inserted `Html` may hold (R19-26; found by the 1.2.1 audit).
            State::Text if self.select => EchoContext::SelectText,
            State::Text => EchoContext::Text,
            State::Raw { flags, js, .. } if flags & SCRIPT != 0 => {
                if js.in_code() {
                    EchoContext::Script
                } else {
                    EchoContext::ScriptLiteral
                }
            }
            State::Raw { flags, .. } if flags & STYLE != 0 => EchoContext::Style,
            // The text of `<textarea>`, `<title>`...: inserted markup ends it at
            // the element's end tag (`plaintext` has none).
            State::Raw { tag, .. } => match raw_text_name(tag) {
                Some(t) => EchoContext::RawText(t),
                None => EchoContext::Text,
            },
            State::Tag { .. } | State::AttrName { .. } => EchoContext::Tag,
            State::BeforeValue { .. } | State::Unquoted { .. } => EchoContext::Unquoted,
            State::Quoted { tag, name, url, .. } => self.quoted_context(tag, name, url.as_ref()),
        }
    }

    /// The context of a quoted attribute value, by the attribute tables.
    fn quoted_context(&self, tag: &str, name: &str, url: Option<&UrlState>) -> EchoContext {
        if has(tag, SCRIPT) && SCRIPT_SRC_ATTRS.contains(&name) {
            return EchoContext::ScriptSrc;
        }
        // A `<meta>` that describes the page (name, property, itemprop) may take any text;
        // others, such as `http-equiv="refresh"`, act on their `content`.
        if has(tag, META)
            && name == "content"
            && (self.attrs.iter().any(|a| a == "http-equiv") || self.maybe_undescribed || !self.described())
        {
            return EchoContext::MetaContent;
        }
        if name.starts_with("on") {
            EchoContext::Handler
        } else if name == "style" {
            EchoContext::Style
        } else if name == "srcdoc" {
            EchoContext::Srcdoc
        } else {
            match url {
                // After a literal `&`, a value would continue a character
                // reference (`&` + `#x6a;avascript:`; found by the 1.0.0 audit).
                Some(UrlState::Undecided(ps)) if ps.iter().any(|p| p == "&") => EchoContext::UrlReference,
                Some(UrlState::Undecided(_)) => EchoContext::Url,
                Some(UrlState::Script) => EchoContext::ScriptUrl,
                _ => EchoContext::Attr,
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
                self.meta_text |= has(tag, META) && name == "content";
            }
        }
        if let State::Raw { flags, js, .. } = &mut self.state {
            if *flags & SCRIPT != 0 {
                js.inserted();
            }
        }
        // A value is the first thing after `]` it follows.
        if let State::Cdata { brackets } = &mut self.state {
            *brackets = 0;
        }
        self.after_comment_value = if self.state == State::Comment { 1 } else { 0 };
        self.loop_head = 0;
    }

    /// The start of a loop's body: it may follow the end of the body (or a
    /// `continue`), so text at its start that would end a comment after a
    /// value is remembered (see `end_loop`; found by the 1.2 audit).
    pub fn begin_loop(&mut self) -> LoopHead {
        let outer = LoopHead { head: self.loop_head, ends: self.head_ends_comment };
        self.loop_head = if self.state == State::Comment { 1 | self.loop_head } else { 0 };
        self.head_ends_comment = false;
        outer
    }

    /// The end of a loop's body (after `end_block`): true if the body starts
    /// with what would end a comment after a value (`>`, `!>`, `->`, `-!>`)
    /// and may follow a value, at its end or at a `continue` (`continued`:
    /// `after_comment_value` there). `broke` is that at a `break`: what
    /// follows the loop may follow it.
    pub fn end_loop(&mut self, outer: LoopHead, continued: u8, broke: u8) -> bool {
        let repeats_after_value = self.head_ends_comment && (self.after_comment_value | continued) != 0;
        // A loop at the start of another one is at the start of that one too.
        let inner = self.head_ends_comment;
        self.head_ends_comment = outer.ends || (outer.head != 0 && inner);
        self.loop_head = if outer.head != 0 { outer.head | self.loop_head } else { 0 };
        self.after_comment_value |= broke;
        repeats_after_value
    }

    /// Where the HTML may follow a value written anywhere before (`finally`,
    /// which runs after a `break` or `continue` in the `try` block).
    pub fn after_any_value(&mut self) {
        if self.state == State::Comment {
            self.after_comment_value |= 1;
        }
    }

    /// `after_comment_value` (for a `break` or `continue` here).
    pub fn after_value(&self) -> u8 {
        self.after_comment_value
    }

    /// Whether the output is at exactly the same place as at `other`, with
    /// the same attributes of the current tag.
    pub fn same_place(&self, other: &HtmlTracker) -> bool {
        self.state == other.state
            && self.pending == other.pending
            && self.open == other.open
            && self.select == other.select
            && self.noscript == other.noscript
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
        self.attrs.iter().any(|a| DESCRIBING_ATTRS.contains(&a.as_str()))
    }

    /// True between elements of an HTML document: not inside a tag, comment,
    /// raw-text element, foreign content or `<select>`. A template must end
    /// here, so that joining its output with other `Html` values cannot move
    /// them into another context (inside a `<select>`, older browsers read a
    /// `<title>` or `<style>` that follows as markup; found by the 1.2.1
    /// audit).
    pub fn at_top_level(&self) -> bool {
        self.between_elements() && !self.select
    }

    /// `at_top_level` but for an open `<select>`.
    fn between_elements(&self) -> bool {
        self.state == State::Text && self.pending.is_none() && !self.in_foreign() && self.carry.is_empty()
    }

    /// Only a `<select>` keeps the output from the top level (for the message).
    pub fn only_select_open(&self) -> bool {
        self.select && self.between_elements()
    }

    /// Inside SVG or MathML (see `open`).
    fn in_foreign(&self) -> bool {
        !self.open.is_empty()
    }

    /// In a comment (or CDATA section) or an attribute value.
    fn in_comment_or_value(&self) -> bool {
        matches!(
            self.state,
            State::Comment | State::Bogus | State::Cdata { .. } | State::Quoted { .. } | State::Unquoted { .. }
        )
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
            (State::Raw { tag: t1, escape: e1, js: j1, .. }, State::Raw { tag: t2, escape: e2, js: j2, .. }) => {
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
        // Where a hidden end tag is watched, a block may not begin or end inside
        // one (`</` + `if {no} else {xy}` + `script>`: the ways are read one
        // after the other, not as the page is written; found by the 1.0.0 audit).
        let split = (self.out_of_step_possible()
            && (hidden_end_tag_start(&entry.tail) || hidden_end_tag_start(&self.tail)))
            || (entry.state != State::Text && hidden_noscript_start(&entry.tail))
            || (self.state != State::Text && hidden_noscript_start(&self.tail));
        let same = same
            && !split
            && self.pending == entry.pending
            && self.open == entry.open
            && self.noscript == entry.noscript
            && self.carry == entry.carry;
        // A `<select>` may be open after either way. A value may or may not be
        // the last thing written before what follows, and the block may have
        // written nothing at the start of a loop.
        self.select |= entry.select;
        self.after_comment_value |= entry.after_comment_value;
        self.loop_head |= entry.loop_head;
        self.head_ends_comment |= entry.head_ends_comment;
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

    /// Where a browser may read raw text that this tracker reads as markup:
    /// inside `<noscript>` (scripting on), and inside SVG or MathML (where
    /// breakout elements and integration points such as `<foreignObject>`
    /// return to HTML, whose raw-text elements this tracker does not follow
    /// there). Found by the 1.0.0 audit.
    fn out_of_step_possible(&self) -> bool {
        self.noscript > 0 || self.in_foreign()
    }

    /// Whether the end tag `name` here could end an element for a browser
    /// while this tracker reads on: a raw-text element's in a comment or an
    /// attribute value where the two may disagree; and `</noscript` anywhere
    /// but in text (where this tracker ends a `<noscript>` too).
    ///
    /// The last holds outside a `<noscript>` too: any template or function
    /// may write inside one (`<noscript><?= render(...) ?>`, a layout's body,
    /// a call), while it is checked on its own (found by the 1.1.1 audit: a
    /// partial with `<!--</<?= $v ?>` passed). Inside `<svg>` or `<math>`,
    /// what a template or function writes is checked as it is written (see
    /// `EchoContext::ForeignText`).
    fn hides(&self, in_comment_or_value: bool, in_text: bool, name: &str) -> bool {
        (in_comment_or_value && self.out_of_step_possible() && has(name, HIDES_END))
            || (!in_text && has(name, NOSCRIPT))
    }

    /// The `>` of a tag: what the tag does, in order.
    fn close_tag(&mut self, tag: String, closing: bool, self_closing: bool) {
        // 1. A `<meta>` whose `content` took a value as text may not have `http-equiv` (found by the 1.0 audit).
        if std::mem::take(&mut self.meta_text) && has(&tag, META) && self.attrs.iter().any(|a| a == "http-equiv") {
            self.found.push((self.here, InlineCode::MetaHttpEquiv));
        }
        // 2. The open elements of foreign content (`START_RULES`, `END_RULES`).
        //    A self-closing start tag read as foreign content (not in an
        //    integration point) opens nothing (WHATWG 13.2.6.5): `<svg><script/>`
        //    opens no script text either (found by the 1.2.1 audit).
        let foreign_self_closed = !closing
            && self_closing
            && matches!(self.open.last().map_or(Top::None, Open::top), Top::Foreign | Top::AnnotationXml);
        if closing {
            self.open_end(&tag);
        } else {
            self.open_start(&tag, self_closing);
        }
        // 3. `<noscript>` depth, outside foreign content. Browsers ignore `/>` on
        //    HTML elements: `<noscript/>` opens one.
        if !self.in_foreign() && has(&tag, NOSCRIPT) {
            if closing {
                self.noscript = self.noscript.saturating_sub(1);
            } else {
                self.noscript += 1;
            }
        }
        // 4. `<select>`, and the raw-text elements it hides from older parsers (R19-26).
        if has(&tag, SELECT) {
            self.select = !closing;
        } else if self.select && !closing && has(&tag, IGNORED_IN_SELECT) {
            self.found.push((self.here, InlineCode::RawTextInSelect(tag.clone())));
        }
        // 5. A raw-text element's start tag opens its text (`RAW` in HTML content,
        //    `RAW_IN_FOREIGN` inside SVG or MathML, unless self-closed there);
        //    anything else returns to text.
        let flags = flags(&tag);
        let raw = flags & if self.in_foreign() { RAW_IN_FOREIGN } else { RAW } != 0 && !foreign_self_closed;
        self.state =
            if !closing && raw { State::Raw { tag, flags, escape: 0, js: Js::default() } } else { State::Text };
    }

    fn report(&mut self, wrong: Wrong, tag: &str, top: Option<&Open>) {
        let msg = match wrong {
            Wrong::None => return,
            Wrong::AnnotationXml => format!(
                "the element `<{tag}>` inside MathML `<annotation-xml>`, which browsers read as HTML or as \
                 MathML depending on its `encoding`"
            ),
            Wrong::Breakout => format!(
                "the HTML element `<{tag}>` directly inside `<svg>` or `<math>`, where browsers end the SVG or \
                 MathML before it"
            ),
            Wrong::IgnoredEnd => format!(
                "`</{tag}>` while the HTML element `<{}>` opened inside SVG or MathML is still open, where \
                 browsers ignore it",
                top.map_or("", |t| t.name.as_str())
            ),
            Wrong::BreakoutEnd => {
                format!(
                    "`</{tag}>` directly inside `<svg>` or `<math>`, where browsers end the SVG or MathML before it"
                )
            }
            Wrong::EndAround => format!(
                "`</{tag}>` inside SVG or MathML that an HTML element `<{tag}>` around it is open for, where \
                 browsers may close that element and the SVG or MathML inside it"
            ),
        };
        self.found.push((self.here, InlineCode::ForeignStructure(msg)));
    }

    /// Whether `tag` is in the class, for the open elements as they are.
    fn tag_is(&self, class: TagIs, tag: &str) -> bool {
        match class {
            TagIs::Any => true,
            TagIs::Has(mask) => has(tag, mask),
            TagIs::AnyOf(mask) => flags(tag) & mask != 0,
            TagIs::Lacks(mask) => flags(tag) & mask == 0,
            TagIs::Named(names) => names.contains(&tag),
            TagIs::TopName => self.open.last().is_some_and(|t| t.name == tag),
            TagIs::ForeignOpenAbove => self.open.iter().rev().take_while(|n| n.ns != Ns::Html).any(|n| n.name == tag),
            TagIs::HtmlOpen => self.open.iter().any(|n| n.ns == Ns::Html && n.name == tag),
        }
    }

    /// A start tag, in `open`: the first row of `START_RULES` that applies.
    fn open_start(&mut self, tag: &str, self_closing: bool) {
        let top = self.open.last().cloned();
        // Blocks copy and compare the open elements: their number is bounded
        // (far above what SVG uses; unclosed elements such as `<path>` without
        // `/>` nest).
        if top.is_some() && self.open.len() >= MAX_OPEN {
            if !std::mem::replace(&mut self.too_deep, true) {
                self.found.push((
                    self.here,
                    InlineCode::ForeignStructure(format!(
                        "more than {MAX_OPEN} elements open inside `<svg>` or `<math>` (close them, e.g. `<path ... />`)"
                    )),
                ));
            }
            return;
        }
        let kind = top.as_ref().map_or(Top::None, Open::top);
        for rule in START_RULES {
            if !rule.top.contains(&kind)
                || !self.tag_is(rule.tag, tag)
                || (rule.self_closing == Closed::Yes && !self_closing)
                || (rule.self_closing == Closed::No && self_closing)
            {
                continue;
            }
            self.report(rule.wrong, tag, top.as_ref());
            let ns = match rule.then {
                Stack::Continue => continue,
                Stack::Nothing => return,
                Stack::PushRoot => {
                    if has(tag, SVG_ROOT) {
                        Ns::Svg
                    } else {
                        Ns::Math
                    }
                }
                Stack::PushSvg => Ns::Svg,
                Stack::PushMath => Ns::Math,
                Stack::PushHtml => Ns::Html,
                Stack::PushSameNs => top.as_ref().map_or(Ns::Html, |t| t.ns),
                Stack::Pop | Stack::CloseForeignNamed | Stack::CloseForeignAbove | Stack::CloseHtmlNamed => return,
            };
            self.open.push(Open { name: tag.to_string(), ns });
            return;
        }
    }

    /// An end tag, in `open`: the first row of `END_RULES` that applies.
    fn open_end(&mut self, tag: &str) {
        let top = self.open.last().cloned();
        let kind = top.as_ref().map_or(Top::None, Open::top);
        for rule in END_RULES {
            if !rule.top.contains(&kind) || !self.tag_is(rule.tag, tag) {
                continue;
            }
            self.report(rule.wrong, tag, top.as_ref());
            match rule.then {
                Stack::Continue => continue,
                Stack::Pop => {
                    self.open.pop();
                }
                Stack::CloseForeignNamed => {
                    if let Some(at) = self.open.iter().rposition(|n| n.ns != Ns::Html && n.name == tag) {
                        self.open.truncate(at);
                    }
                }
                Stack::CloseForeignAbove => {
                    let above = self.open.iter().rev().take_while(|n| n.ns != Ns::Html).count();
                    if let Some(k) = self.open[self.open.len() - above..].iter().rposition(|n| n.name == tag) {
                        self.open.truncate(self.open.len() - above + k);
                    }
                }
                Stack::CloseHtmlNamed => {
                    if let Some(at) = self.open.iter().rposition(|n| n.ns == Ns::Html && n.name == tag) {
                        self.open.truncate(at);
                    }
                }
                Stack::Nothing
                | Stack::PushRoot
                | Stack::PushSvg
                | Stack::PushMath
                | Stack::PushHtml
                | Stack::PushSameNs => {}
            }
            return;
        }
    }

    /// An end tag split between the last literal text and this one (by a
    /// statement or an empty block between them: `<!--</` + `noscript>`) is
    /// checked as one (found by the 1.0.0 audit).
    fn check_across_pieces(&mut self, text: &str) {
        if self.tail.is_empty() {
            return;
        }
        let in_comment_or_value = self.in_comment_or_value();
        let in_text = matches!(self.state, State::Text);
        let head: String = text.chars().take(16).collect();
        let joined = format!("{}{head}", self.tail).to_ascii_lowercase();
        let split = self.tail.len();
        for (at, _) in joined.match_indices("</") {
            if at >= split {
                break;
            }
            let rest = &joined[at + 2..];
            let name: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            let after = rest[name.len()..].chars().next();
            // Only one that reaches into this text: one within the last was checked then.
            let crosses = at + 2 + name.len() >= split;
            if crosses && is_delim(after.as_ref()) && self.hides(in_comment_or_value, in_text, &name) {
                self.found.push((0, InlineCode::HiddenEndTag));
            }
        }
    }

    /// Advances over literal HTML. Returns the byte offsets just after the
    /// name of each opening `<script` tag, where the server's CSP nonce goes.
    pub fn feed(&mut self, text: &str) -> Vec<usize> {
        // After a value in a comment, text that a value ending with `-`, `--` or
        // `--!` would make `-->` or `--!>` (found by the 1.1.1 audit: `<!-- `,
        // a value `--`, then `>` ended the comment for browsers, not here).
        // Also across pieces: `-` or `!`, a statement, then `>` (found by the 1.2
        // audit).
        let comment = self.state == State::Comment;
        let (ends, after) = comment_end_after(if comment { self.after_comment_value } else { 0 }, text);
        self.after_comment_value = after;
        if ends {
            self.found.push((0, InlineCode::CommentEnd));
        }
        // The same at the start of a loop's body, which may follow a value at
        // its end (see `end_loop`; found by the 1.2 audit).
        let (head_ends, head) = comment_end_after(if comment { self.loop_head } else { 0 }, text);
        self.loop_head = head;
        self.head_ends_comment |= head_ends && !ends;
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

    /// The current context (`RULES` is indexed by it).
    fn ctx(&self) -> Ctx {
        if self.pending.is_some() {
            Ctx::TagName
        } else {
            self.state.ctx()
        }
    }

    /// Whether `on` matches at `i`: the number of characters it covers.
    fn matched(&self, on: On, chars: &[char], i: usize) -> Option<usize> {
        let c = chars[i];
        match on {
            On::Any => Some(1),
            On::Is(want) => (c == want).then_some(1),
            On::Not(want) => (c != want).then_some(1),
            On::In(class) => class.has(c).then_some(1),
            On::Lit(s) => starts_ci(chars, i, s).then_some(s.len()),
            On::Exact(s) => chars[i..].iter().copied().take(s.len()).eq(s.chars()).then_some(s.len()),
            On::TheQuote => match &self.state {
                State::Quoted { quote, .. } if *quote == c => Some(1),
                _ => None,
            },
            On::EndTag => {
                let State::Raw { tag, .. } = &self.state else { return None };
                let n = 2 + tag.len();
                (c == '<'
                    && chars.get(i + 1) == Some(&'/')
                    && starts_ci(chars, i + 2, tag)
                    && is_delim(chars.get(i + n)))
                .then_some(n)
            }
            On::RestStarts(s) => rest_starts(chars, i, s, false).then_some(chars.len() - i),
            On::RestStartsExact(s) => rest_starts(chars, i, s, true).then_some(chars.len() - i),
            On::RestStartsEndTag => {
                let State::Raw { tag, .. } = &self.state else { return None };
                let left = chars.len() - i;
                (c == '<'
                    && left <= 2 + tag.len()
                    && chars[i..].iter().zip("</".chars().chain(tag.chars())).all(|(a, b)| a.eq_ignore_ascii_case(&b)))
                .then_some(left)
            }
        }
    }

    /// Whether `when` holds for a match of `len` characters at `i`.
    fn holds(&self, when: When, chars: &[char], i: usize, len: usize) -> bool {
        let (escape, script) = match &self.state {
            State::Raw { flags, escape, .. } => (*escape, *flags & SCRIPT != 0),
            _ => (0, false),
        };
        match when {
            When::AtEnd => i + len == chars.len(),
            When::Next(class) => chars.get(i + len).is_some_and(|c| class.has(*c)),
            When::DelimAfter => is_delim(chars.get(i + len)),
            When::SpacesThenEq => chars.get(spaces_end(chars, i)) == Some(&'='),
            When::DashesThenGt => chars.get(dashes_end(chars, i + len)) == Some(&'>'),
            When::InForeign => self.in_foreign(),
            When::Script => script,
            When::NotScript => !script,
            When::Escape(n) => escape == n,
            When::Escaped => escape > 0,
            When::Brackets(n) => matches!(self.state, State::Cdata { brackets } if brackets == n),
        }
    }

    /// Does what a rule says at `i`.
    fn apply(
        &mut self,
        then: Then,
        i: usize,
        chars: &[char],
        offset: usize,
        scripts: &mut Vec<usize>,
        comment_open: &mut Option<usize>,
    ) {
        let c = chars[i];
        match then {
            Then::Stay => {}
            Then::Text => self.state = State::Text,
            Then::Comment => {
                *comment_open = Some(i);
                self.state = State::Comment;
            }
            Then::Bogus => self.state = State::Bogus,
            Then::Cdata => self.state = State::Cdata { brackets: 0 },
            Then::TagName(prefix) => self.pending = Some(prefix.to_string()),
            Then::Carry => self.carry = chars[i..].iter().collect(),
            Then::NamePush => {
                if let Some(name) = &mut self.pending {
                    name.push(c.to_ascii_lowercase());
                } else if let State::AttrName { name, .. } = &mut self.state {
                    name.push(c.to_ascii_lowercase());
                }
            }
            Then::NameEnd => {
                let raw = self.pending.take().unwrap_or_default();
                let (closing, tag) = match raw.strip_prefix('/') {
                    Some(t) => (true, t.to_string()),
                    None => (false, raw),
                };
                if !closing && has(&tag, SCRIPT) {
                    scripts.push(offset);
                }
                self.attrs.clear();
                self.meta_text = false;
                self.maybe_undescribed = false;
                self.state = State::Tag { name: tag, closing };
            }
            Then::Close(sc) => {
                let self_closing = match sc {
                    SelfClosing::IfSlashBefore => i > 0 && chars[i - 1] == '/',
                    SelfClosing::Never => false,
                };
                let (tag, closing) = std::mem::replace(&mut self.state, State::Text).take_tag();
                self.close_tag(tag, closing, self_closing);
            }
            Then::AttrName => {
                self.mark = Some(offset);
                let (tag, closing) = std::mem::replace(&mut self.state, State::Text).take_tag();
                self.state = State::AttrName { tag, closing, name: c.to_ascii_lowercase().to_string() };
            }
            Then::AttrValue => {
                if let State::AttrName { tag, closing, name } = std::mem::replace(&mut self.state, State::Text) {
                    // Event handler names are letters only (`onclick`, not `onclick-free`).
                    let handler =
                        name.len() > 2 && name.starts_with("on") && name.bytes().all(|b| b.is_ascii_lowercase());
                    if handler && !closing {
                        self.found.push((self.mark.unwrap_or(self.here), InlineCode::Handler(name.clone())));
                    }
                    self.attrs.push(name.clone());
                    self.state = State::BeforeValue { tag, closing, name };
                }
            }
            Then::AttrDone => {
                if let State::AttrName { tag, closing, name } = std::mem::replace(&mut self.state, State::Text) {
                    self.attrs.push(name);
                    self.state = State::Tag { name: tag, closing };
                }
            }
            Then::ToTag => {
                let (tag, closing) = std::mem::replace(&mut self.state, State::Text).take_tag();
                self.state = State::Tag { name: tag, closing };
            }
            Then::Quoted => {
                if let State::BeforeValue { tag, closing, name } = std::mem::replace(&mut self.state, State::Text) {
                    let url = is_url_attr(&tag, &name).then(|| UrlState::Undecided(vec![String::new()]));
                    self.mark = Some(offset);
                    self.state = State::Quoted { tag, closing, name, quote: c, url };
                }
            }
            Then::Unquoted => {
                if let State::BeforeValue { tag, closing, name } = std::mem::replace(&mut self.state, State::Text) {
                    self.state = State::Unquoted { tag, closing, name };
                }
            }
            Then::UrlChar => self.url_char(c),
            Then::Escape(n) => {
                if let State::Raw { escape, .. } = &mut self.state {
                    *escape = n;
                }
            }
            Then::CloseRaw => {
                if let State::Raw { tag, .. } = std::mem::replace(&mut self.state, State::Text) {
                    self.state = State::Tag { name: tag, closing: true };
                }
            }
            Then::ForeignMarkup => self.found.push((offset, InlineCode::ForeignCdata)),
            Then::Bracket => {
                if let State::Cdata { brackets } = &mut self.state {
                    *brackets = (*brackets + 1).min(2);
                }
            }
            Then::Brackets0 => self.state = State::Cdata { brackets: 0 },
            Then::CdataEnd => {
                self.found.push((offset, InlineCode::CdataEnd));
                self.state = State::Text;
            }
        }
    }

    /// A character of a quoted attribute value: what it does to the URL's
    /// start (the strictest of the ways the text may have begun).
    fn url_char(&mut self, c: char) {
        let at = self.mark.unwrap_or(self.here);
        let mut inline = false;
        if let State::Quoted { tag, name, url, .. } = &mut self.state {
            if let Some(UrlState::Undecided(ps)) = url {
                let mut script = false;
                ps.retain_mut(|p| {
                    // Browsers ignore whitespace and case in a scheme.
                    if !c.is_ascii_whitespace() && !c.is_control() && p.len() < MAX_URL_PREFIX {
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
                        None => p.contains('&') || may_become_script_scheme(p),
                    }
                });
                for p in ps.iter_mut().filter(|p| p.contains('&')) {
                    *p = "&".into();
                }
                ps.sort();
                ps.dedup();
                if script {
                    *url = Some(UrlState::Script);
                } else if ps.is_empty() {
                    *url = Some(UrlState::Safe);
                }
            }
            // A list of URLs (`URL_LISTS`): after the separator, a URL starts again.
            let next_url = URL_LISTS.iter().any(|(attr, sep, needs)| name == attr && c == *sep && has(tag, *needs));
            if next_url && !matches!(url, Some(UrlState::Script)) {
                *url = Some(UrlState::Undecided(vec![String::new()]));
            }
        }
        if inline {
            self.found.push((at, InlineCode::ScriptUrl));
        }
    }

    /// The interpreter of `RULES` over one piece of literal text.
    fn feed_text(&mut self, text: &str) -> Vec<usize> {
        self.check_across_pieces(text);
        let chars: Vec<char> = text.chars().collect();
        let offsets: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        let mut scripts = Vec::new();
        let mut i = 0;
        // Where the last comment opened in this text began (its `<!--`).
        let mut comment_open = None;
        self.mark = None;
        while i < chars.len() {
            self.here = offsets[i];
            let ctx = self.ctx();
            // An end tag hidden from this tracker (`hides`), in any context but a tag name.
            if ctx != Ctx::TagName && chars[i] == '<' && chars.get(i + 1) == Some(&'/') {
                let name: String = chars[i + 2..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .map(|c| c.to_ascii_lowercase())
                    .collect();
                let in_text = matches!(self.state, State::Text);
                if is_delim(chars.get(i + 2 + name.len())) && self.hides(self.in_comment_or_value(), in_text, &name) {
                    self.found.push((offsets[i], InlineCode::HiddenEndTag));
                }
            }
            let in_script = matches!(&self.state, State::Raw { flags, .. } if flags & SCRIPT != 0);
            let mut read = 1;
            for rule in rules(ctx) {
                let Some(len) = self.matched(rule.on, &chars, i) else { continue };
                if !rule.when.iter().all(|w| self.holds(*w, &chars, i, len)) {
                    continue;
                }
                read = match rule.read {
                    One => 1,
                    Zero => 0,
                    Match => len,
                    Read::Spaces => spaces_end(&chars, i).max(i + 1) - i,
                    Read::ThroughGt => dashes_end(&chars, i + len) + 1 - i,
                    Rest => chars.len() - i,
                };
                self.apply(rule.then, i, &chars, offsets[i], &mut scripts, &mut comment_open);
                break;
            }
            // The script's text, including what the HTML tokenizer read in one step.
            // (Carried characters are read with the next text, not now.)
            if in_script && self.carry.is_empty() {
                if let State::Raw { js, .. } = &mut self.state {
                    for &c in &chars[i..i + read] {
                        js.step(c);
                    }
                }
            }
            i += read;
        }
        // A comment's text that ends where what follows could end it: `<!--` or
        // `<!---` (then `>` or `->`), and `-`, `--` or `--!` (then `->`, `>`...).
        // It is read again in front of the next text, so a statement or empty
        // block between them does not hide the end, and a value may not follow
        // (found by the 1.1.1 audit).
        if self.state == State::Comment && self.carry.is_empty() {
            let body = comment_open.map_or(0, |k| k + 4);
            let rest: String = chars[body.min(chars.len())..].iter().collect();
            if let Some(k) = comment_open.filter(|_| rest.is_empty() || rest == "-") {
                self.carry = chars[k..].iter().collect();
                self.state = State::Text;
            } else if let Some(end) = ["--!", "--", "-"].iter().find(|e| rest.ends_with(**e)) {
                self.carry = (*end).to_string();
            }
        }
        // The end of the text so far, across pieces (a short piece keeps what came
        // before it), for a value inserted after it (`hidden_end_tag_start`) and
        // for an end tag split across pieces (`check_across_pieces`).
        let mut tail = std::mem::take(&mut self.tail);
        tail.extend(chars.iter());
        let skip = tail.chars().count().saturating_sub(16);
        self.tail = tail.chars().skip(skip).collect();
        scripts
    }
}

/// The index after the run of HTML whitespace at `i`.
fn spaces_end(chars: &[char], i: usize) -> usize {
    let mut j = i;
    while j < chars.len() && html_space(chars[j]) {
        j += 1;
    }
    j
}

/// The index after the run of `-` at `i`.
fn dashes_end(chars: &[char], i: usize) -> usize {
    let mut j = i;
    while chars.get(j) == Some(&'-') {
        j += 1;
    }
    j
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

    fn found(html: &str) -> Vec<InlineCode> {
        let mut t = HtmlTracker::default();
        t.feed(html);
        t.take_inline_code().into_iter().map(|(_, c)| c).collect()
    }

    /// Found by the 1.0.0 audit: in SVG and MathML a script's text is markup,
    /// so any tag, comment or bogus comment there could hide its end tag.
    #[test]
    fn markup_in_foreign_raw_text_is_refused() {
        for html in [
            "<svg><script><a title='</script>'></a>",
            "<svg><script><!x </script >",
            "<svg><script></ </script >",
            "<svg><style><a title='</style>'></a>",
            "<math><style><![CDATA[",
            "<svg><script><!--",
        ] {
            assert!(found(html).contains(&InlineCode::ForeignCdata), "{html}");
        }
        // Its own end tag, and `<` that starts nothing, are fine.
        assert!(found("<svg><script>if (a < b) {}</script></svg>").is_empty());
    }

    /// Found by the 1.0.0 audit: `<svg/>` opens nothing, and `</math>` does
    /// not close an `<svg>` (browsers ignore it), so raw-text elements stay
    /// the same for the tracker and the browser.
    #[test]
    fn foreign_content_follows_the_open_elements() {
        assert_eq!(ctx("<svg/><textarea>"), EchoContext::RawText("textarea"));
        let mut t = HtmlTracker::default();
        t.feed("<svg/><textarea><!--</textarea><p>");
        assert!(t.at_top_level(), "the textarea ended for the tracker as it does for the browser");
        // In SVG, `</math>` is ignored: a `<script>` is still SVG's, whose text is markup.
        assert!(found("<svg></math><script><![CDATA[//</script>]]>").contains(&InlineCode::ForeignCdata));
    }

    /// Found by the 1.0.0 audit: where a browser reads an element's text as raw
    /// text (`<noscript>` with scripting on, `<xmp>` in `<foreignObject>`), its
    /// end tag in a comment or attribute value ends it.
    #[test]
    fn end_tags_hidden_in_comments_and_attributes_are_refused() {
        for html in [
            "<noscript><!--</noscript><meta http-equiv=\"refresh\">-->",
            "<svg><foreignObject><xmp><!--</xmp><a href=\"",
            "<svg><a title=\"</textarea >\">",
            "<math><!x </title>",
            // Inside `<noscript>`, its end tag in raw text too (found again by the audit).
            "<noscript><textarea></noscript><a href=\"",
            "<noscript><title></NOSCRIPT ><meta>",
        ] {
            assert!(found(html).contains(&InlineCode::HiddenEndTag), "{html}");
        }
        // Where a browser reads the page as this tracker does, they are fine:
        // a commented-out script, HTML in a `data-*` attribute.
        assert!(found("<!-- <script src=\"/old.js\"></script> --><p data-x=\"<textarea></textarea>\">").is_empty());
        assert!(found("<noscript><style>.js{display:none}</style><p title=\"</b>\"></noscript>").is_empty());
        // A value right after the start of such an end tag could complete it.
        assert_eq!(ctx("<noscript><!-- </noscr"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<svg><a title=\"</"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<noscript><!-- <"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<svg><a title=\"</b"), EchoContext::Attr);
        // Outside them, only `</noscript` is watched (see `hides_noscript_outside_one`).
        assert_eq!(ctx("<!-- </noscr"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<!-- </scr"), EchoContext::Comment { bogus: false, watched: false });
        // Split between pieces of literal text (statements between them), also
        // through short pieces.
        for pieces in [
            &["<noscript><!--</", "noscript><a href=\""][..],
            &["<noscript><!--</nos", "cript>"],
            &["<noscript><!--</", "no", "script>"],
            &["<noscript><!--<", "/", "noscript>"],
            &["<svg><foreignObject><xmp><!--</", "x", "mp>"],
        ] {
            let mut t = HtmlTracker::default();
            let mut found = Vec::new();
            for p in pieces {
                t.feed(p);
                found.extend(t.take_inline_code());
            }
            assert!(found.iter().any(|(_, c)| *c == InlineCode::HiddenEndTag), "{pieces:?}");
        }
        // A value after a short piece that continues the name.
        let mut t = HtmlTracker::default();
        t.feed("<noscript><!--</");
        t.feed("n");
        assert_eq!(t.context(), EchoContext::AfterMarkup);
        // Neither can a block (each way is read after the other).
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        for src in [
            "<noscript><!--</<?phv if ($b) { ?>no<?phv } else { ?>xy<?phv } ?>script>--></noscript>",
            "<noscript><!--</no<?phv if ($b) { ?>script<?phv } ?>>--></noscript>",
        ] {
            assert!(parse(src).iter().any(|d| d.is_error()), "{src}");
        }
        assert!(parse("<noscript><!-- <?phv if ($b) { ?>a<?phv } ?> --></noscript>").iter().all(|d| !d.is_error()));
        // `<noscript/>` opens a noscript (browsers ignore `/>` on HTML elements).
        assert!(found("<noscript/><!--</noscript><a>").contains(&InlineCode::HiddenEndTag));
        assert!(found("<noscript/><textarea></noscript>").contains(&InlineCode::HiddenEndTag));
    }

    /// Found by the 1.0.0 audit: a statement between `</` and the rest of an
    /// end tag hidden from the tracker passed `phv check` (a function it calls
    /// could write the rest); only values (`<?= ?>`) were refused there.
    #[test]
    fn statements_after_a_hidden_end_tag_start_are_refused() {
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        for src in [
            "<noscript><!--</<?phv f(); ?>--></noscript>",
            "<noscript><!--<<?phv f(); ?>--></noscript>",
            "<noscript><!--</nos<?phv $x = 1; ?>--></noscript>",
            "<noscript><textarea></<?phv f(); ?></textarea></noscript>",
            "<svg><a title=\"</<?phv f(); ?>\"></a></svg>",
            "<svg><!--</<?phv f(); ?>--></svg>",
            "<math><!--</<?phv f(); ?>--></math>",
            "<math><mi title='</ti<?phv f(); ?>'></mi></math>",
            // Outside them, before what could complete `</noscript` (the
            // template may be rendered inside one; found by the 1.1.1 audit).
            "<p><!--</<?phv f(); ?>--></p>",
            "<p title=\"</nos<?phv f(); ?>\"></p>",
        ] {
            let diags = parse(src);
            assert!(
                diags.iter().any(|d| d.is_error() && d.message.contains("code right after `<` or `</`")),
                "{src}: {diags:?}"
            );
        }
        // Elsewhere, and after other text, statements are fine.
        for src in [
            "<p><!--</b<?phv f(); ?>--></p>",
            "<noscript><!--</b <?phv f(); ?>--></noscript>",
            "<noscript><!-- <?phv f(); ?> --></noscript>",
            "<svg><a title=\"</b<?phv f(); ?>\"></a></svg>",
            "<noscript><p>a <?phv f(); ?></p></noscript>",
        ] {
            assert!(parse(src).iter().all(|d| !d.is_error()), "{src}");
        }
    }

    /// Found by the 1.1.1 audit: a template or function is checked on its own,
    /// but may write inside a `<noscript>` (`render()`, a layout's body, a
    /// call), where browsers with scripting on read raw text up to the first
    /// `</noscript`. So that end tag is watched everywhere: in a comment, an
    /// attribute value or raw text, and before a value or statement there.
    #[test]
    fn hides_noscript_outside_one() {
        for html in [
            "<!--</noscript><a href=\"-->",
            "<p title=\"</NoScript >\">",
            "<textarea></noscript></textarea>",
            "<script>var s = \"</noscript>\";</script>",
            "<!x </noscript>",
        ] {
            assert!(found(html).contains(&InlineCode::HiddenEndTag), "{html}");
        }
        for html in ["<!--</", "<!--<", "<p title=\"</no", "<textarea></noscrip", "<!-- </NOSCRIPT"] {
            assert_eq!(ctx(html), EchoContext::AfterMarkup, "{html}");
        }
        let mut seen = Vec::new();
        let mut t = HtmlTracker::default();
        for p in ["<!--</nos", "cript>-->"] {
            t.feed(p);
            seen.extend(t.take_inline_code());
        }
        assert!(seen.iter().any(|(_, c)| *c == InlineCode::HiddenEndTag));
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        assert!(parse("<!--</no<?phv if ($b) { ?>script<?phv } ?>>-->").iter().any(|d| d.is_error()));
        // Other end tags there, and `</noscript>` in text, are fine.
        for html in [
            "<!-- <script src=\"/old.js\"></script> -->",
            "<p title=\"</b>\" data-x=\"<textarea></textarea>\">",
            "<noscript><p>Turn on JavaScript.</p></noscript>",
            "</noscript>",
        ] {
            assert!(found(html).is_empty(), "{html}");
        }
        assert_eq!(ctx("<!--</scr"), EchoContext::Comment { bogus: false, watched: false });
        assert_eq!(ctx("<p title=\"a < "), EchoContext::Attr);
    }

    /// Found by the 1.1.1 audit: a value ending with `--` before a literal `>`
    /// ended a comment for browsers while this tracker read on, and so did
    /// `-` + `->` split by a statement.
    #[test]
    fn values_and_statements_cannot_end_a_comment() {
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        let errors = |src: &str| parse(src).iter().filter(|d| d.is_error()).count();
        for src in [
            // Text after a value that a value's `-`, `--` or `--!` would complete.
            "<!-- <?= $v ?>> <a href=x> -->",
            "<!-- <?= $v ?>!> -->",
            "<!-- a<?= $v ?>-> -->",
            "<!-- <?= $v ?>-!> -->",
            "<!-- <?= $a ?><?= $b ?>> -->",
            // A block may or may not run between the value and the text.
            "<!-- <?= $v ?><?phv if ($c) { ?> x<?phv } ?>> -->",
            // A comment's start or a `-` before a block.
            "<!-- a-<?phv if ($c) { ?>-<?phv } ?>> -->",
            "<!--<?phv if ($c) { ?>-<?phv } ?>> -->",
        ] {
            assert!(errors(src) > 0, "{src}");
        }
        // Nor a value there (the type checker refuses `AfterMarkup`).
        for html in ["<!--", "<!---", "<!-- a-", "<!-- a--", "<!-- a--!"] {
            assert_eq!(ctx(html), EchoContext::AfterMarkup, "{html}");
        }
        // Read across a statement: `-` + `->` ends the comment, as for browsers.
        let mut t = HtmlTracker::default();
        t.feed("<!-- a -");
        t.feed("-> <p>");
        assert!(t.at_top_level());
        let mut t = HtmlTracker::default();
        t.feed("<!--");
        t.feed(">");
        assert!(t.at_top_level(), "`<!--` + `>` is an empty comment");
        for src in ["<!-- <?= $v ?> --> <p>", "<!-- <?= $v ?>--> <p>", "<!-- a <?= $v ?> - <?= $w ?> -->"] {
            assert_eq!(errors(src), 0, "{src}");
        }
        assert_eq!(ctx("<!-- a "), EchoContext::Comment { bogus: false, watched: false });
        assert_eq!(ctx("<!x a "), EchoContext::Comment { bogus: true, watched: false });
        assert_eq!(ctx("<!-- a-"), EchoContext::AfterMarkup);
    }

    /// Found by the 1.0.0 audit: spaces after an attribute name were looked
    /// ahead of at each one (quadratic).
    #[test]
    fn spaces_after_an_attribute_name_are_read_in_linear_time() {
        let start = std::time::Instant::now();
        let html = format!("<a b{}=1><p>", " ".repeat(400_000));
        assert_eq!(ctx(&html), EchoContext::Text);
        let html = format!("<a b{}c=\"", " ".repeat(400_000));
        assert_eq!(ctx(&html), EchoContext::Attr);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
    }

    /// Only tab, LF, FF, CR and space separate names and values, as browsers
    /// read them (found by the 1.1 audit: a full-width space let a value leave
    /// what the tracker took for a quoted attribute).
    #[test]
    fn only_html_whitespace_separates_attributes() {
        // Browsers read `btn\u{3000}title="` as one unquoted value.
        assert_eq!(ctx("<a class=btn\u{3000}title=\""), EchoContext::Unquoted);
        // `\u{a0}"` is an unquoted value too.
        assert_eq!(ctx("<a title=\u{a0}\""), EchoContext::Unquoted);
        // The tag name reads on: the value is inside it.
        assert_eq!(ctx("<div\u{3000}title=\""), EchoContext::Tag);
        assert_eq!(ctx("<div\u{3000}title=\"x\">"), EchoContext::Text);
        // `<script\u{3000}x>` is not a script for a browser.
        assert_eq!(ctx("<script\u{3000}x>"), EchoContext::Text);
        // Ordinary whitespace still separates.
        assert_eq!(ctx("<a class=btn\ttitle=\""), EchoContext::Attr);
        assert_eq!(ctx("<a\x0Ctitle=\""), EchoContext::Attr);
    }

    /// The text of a long `<script>` or `<style>` is read in linear time (found
    /// by the 1.0.0 review: the rest of the text was copied at every character).
    #[test]
    fn long_raw_text_is_read_in_linear_time() {
        let body = "var a = 1; // < </scrip\n".repeat(40_000);
        let start = std::time::Instant::now();
        assert_eq!(ctx(&format!("<script>{body}</script><p>")), EchoContext::Text);
        assert_eq!(ctx(&format!("<style>{}</style><p>", "p{color:red}\n".repeat(80_000))), EchoContext::Text);
        assert_eq!(ctx(&format!("<script>{body}var x = ")), EchoContext::Script);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
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
        assert_eq!(ctx("<svg></svg><title><script>"), EchoContext::RawText("title"));
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
        assert_eq!(ctx("<a href=\"&#x20;"), EchoContext::UrlReference);
        assert_eq!(ctx("<a href=\"j"), EchoContext::Url);
        assert_eq!(ctx("<a href=\"&#106;"), EchoContext::UrlReference);
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

    fn structure_errors(html: &str) -> usize {
        found(html).iter().filter(|c| matches!(c, InlineCode::ForeignStructure(_))).count()
    }

    /// Found by the 1.2 audit: browsers ignore `</svg>` while an HTML element
    /// opened inside a `<foreignObject>` is open, and read on as SVG (a
    /// `<script>` there is SVG's, whose text is markup), where this tracker
    /// left the SVG.
    #[test]
    fn svg_end_tags_that_browsers_ignore_are_refused() {
        for html in [
            "<svg><foreignObject><div></svg></div></foreignObject><script>",
            "<svg><desc><i></svg></i></desc>",
            "<svg><title><b></svg>",
            "<math><mtext><b></math></b></mtext>",
            "<math><mi><span></math>",
            // Unclosed: browsers close a `<p>` before a `<div>`, but not before `</svg>`.
            "<svg><foreignObject><p>a</foreignObject></svg>",
            // HTML elements right inside SVG or MathML end it for browsers.
            "<svg><p>a</p></svg>",
            "<svg><g><font>a</font></g></svg>",
            "<math><b>",
            "<svg></p>",
            "<svg><g></br></g></svg>",
            // `<annotation-xml>` holds HTML or MathML depending on `encoding`.
            "<math><annotation-xml encoding=\"text/html\"><p>a</p></annotation-xml></math>",
            // An end tag that closes an HTML element around nested SVG.
            "<svg><foreignObject><div><svg></div></svg></foreignObject></svg>",
        ] {
            assert_eq!(structure_errors(html), 1, "{html}");
        }
        // What browsers read as written: closed HTML in integration points,
        // SVG and MathML elements, nested SVG and void elements.
        for html in [
            "<svg><foreignObject><div><p>a</p><br><img src=x><b>b</b></div></foreignObject></svg>",
            "<svg><title>Icon <b>x</b></title><path d=\"M0 0\"/><g><rect/></g></svg>",
            "<svg><foreignObject><svg><circle r=\"1\"/></svg></foreignObject></svg>",
            "<math><mi>x</mi><mtext><b>t</b><mglyph></mglyph></mtext></math>",
            "<math><annotation-xml encoding=\"image/svg+xml\"><svg></svg></annotation-xml></math>",
            "<svg></math><g></g></svg>",
            "<svg><foreignObject></p></foreignObject></svg>",
        ] {
            assert_eq!(structure_errors(html), 0, "{html}");
            let mut t = HtmlTracker::default();
            t.feed(html);
            assert!(t.at_top_level(), "{html}");
        }
        // A block must leave the same elements open.
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        assert!(parse("<svg><foreignObject><?phv if ($c) { ?><div><?phv } ?></foreignObject></svg>")
            .iter()
            .any(|d| d.is_error()));
        // The elements followed are bounded (reported once), so blocks inside
        // deep SVG are read in linear time.
        let deep = format!("<svg>{}", "<g>".repeat(MAX_OPEN + 1000));
        assert_eq!(structure_errors(&deep), 1);
        let src = format!("<svg>{}{}</svg>", "<g>".repeat(MAX_OPEN), "<?phv if ($c) { ?>x<?phv } ?>".repeat(2000));
        let start = std::time::Instant::now();
        assert!(parse(&src).len() <= 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "{:?}", start.elapsed());
    }

    /// Found by the 1.2 audit: `<![CDATA[` in SVG or MathML was read as a bogus
    /// comment ending at `>`, where browsers read a CDATA section to `]]>`.
    #[test]
    fn cdata_sections_in_foreign_content_are_read_to_their_end() {
        for html in [
            "<svg><![CDATA[ ><a x=\"]]><a href=",
            "<math><![CDATA[ a > b ]]>",
            "<svg><foreignObject><div><![CDATA[ > ]]>",
        ] {
            assert!(found(html).contains(&InlineCode::CdataEnd), "{html}");
        }
        let mut t = HtmlTracker::default();
        t.feed("<svg><![CDATA[ <a x=\"");
        assert_eq!(t.context(), EchoContext::Comment { bogus: true, watched: true });
        t.feed(" ]]]><a title=\"");
        assert_eq!(t.context(), EchoContext::Attr);
        assert!(t.take_inline_code().is_empty());
        // A value before `>` cannot make `]]>`.
        let mut t = HtmlTracker::default();
        t.feed("<svg><![CDATA[ ]]");
        t.inserted();
        t.feed(">");
        assert!(t.take_inline_code().contains(&(0, InlineCode::CdataEnd)));
        // Its start split by a value or statement is read as one; outside SVG
        // and MathML (and in another case) it is a bogus comment.
        assert_eq!(ctx("<svg><![CDA"), EchoContext::AfterMarkup);
        let mut t = HtmlTracker::default();
        t.feed("<svg><![CDA");
        t.feed("TA[ > ]]>");
        assert!(t.take_inline_code().iter().any(|(_, c)| *c == InlineCode::CdataEnd));
        assert_eq!(ctx("<![CDATA[ a > <p title=\""), EchoContext::Attr);
        assert_eq!(ctx("<svg><![cdata[ a > <a title=\""), EchoContext::Attr);
    }

    /// Found by the 1.2 audit: a loop's body that starts with `>` in a comment
    /// follows a value at its end on the next run (`--` + `>` ends the comment).
    #[test]
    fn loop_bodies_cannot_end_a_comment_after_their_own_values() {
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        let errors = |src: &str| parse(src).iter().filter(|d| d.is_error()).count();
        for src in [
            "<!-- <?phv foreach ($xs as $x) { ?>> <a href=<?= $x ?><?phv } ?> -->",
            "<!-- <?phv foreach ($xs as $x): ?>-> <?= $x ?><?phv endforeach; ?> -->",
            "<!-- <?phv for ($i = 0; $i < 2; $i++) { ?>!> <?= $i ?><?phv } ?> -->",
            "<!-- <?phv while ($c) { ?>-!> <?= $v ?><?phv } ?> -->",
            "<!-- <?phv do { ?>> <?= $v ?><?phv } while ($c); ?> -->",
            // At the start through a block, or split by a statement.
            "<!-- <?phv foreach ($xs as $x) { if ($c) { ?>><?phv } ?> <?= $x ?><?phv } ?> -->",
            "<!-- <?phv foreach ($xs as $x) { ?>-<?phv f(); ?>> <?= $x ?><?phv } ?> -->",
            // Nested: the inner loop is at the start of the outer one.
            "<!-- <?phv foreach ($xs as $x) { foreach ($xs as $y) { ?>> <?phv } ?> <?= $x ?><?phv } ?> -->",
            // A `continue` right after a value, and a `break` before the text after the loop.
            "<!-- <?phv foreach ($xs as $x) { ?>> <?= $x ?><?phv if ($c) { continue; } ?> . <?phv } ?> -->",
            "<!-- <?phv foreach ($xs as $x) { ?> <?= $x ?><?phv if ($c) { break; } ?> . <?phv } ?>> -->",
            // `finally` may follow a value before a `break` in its `try` block.
            "<!-- <?phv try { ?> <?= $v ?><?phv } finally { ?>> <?phv } ?> -->",
            // Statements between a value and `!` + `>`.
            "<!-- <?= $v ?>!<?phv f(); ?>> -->",
        ] {
            assert!(errors(src) > 0, "{src}");
        }
        for src in [
            "<!-- <?phv foreach ($xs as $x) { ?> > <?= $x ?><?phv } ?> -->",
            "<!-- <?phv foreach ($xs as $x) { ?>> <?= $x ?> <?phv } ?> -->",
            "<!-- <?phv foreach ($xs as $x) { ?>> <?phv } ?> -->",
            "<!-- <?phv foreach ($xs as $x) { ?><?= $x ?>, <?phv } ?> -->",
            "<?phv foreach ($xs as $x) { ?>> <?= $x ?><?phv } ?>",
        ] {
            assert_eq!(errors(src), 0, "{src}");
        }
    }

    /// Found by the 1.2 audit: older browsers ignore raw-text elements such as
    /// `<title>` in `<select>` and read their text as markup.
    #[test]
    fn raw_text_elements_inside_select_are_refused() {
        for html in ["<select><title>", "<select><option>a<style>", "<SELECT><option><xmp>", "<select><plaintext>"] {
            assert!(found(html).iter().any(|c| matches!(c, InlineCode::RawTextInSelect(_))), "{html}");
        }
        for html in [
            "<select><option>a</option><script>var a = 1;</script></select>",
            "<select></select><title>t</title><style>p {}</style>",
            "<select><textarea></textarea></select>",
        ] {
            assert!(found(html).is_empty(), "{html}");
        }
        // A `<select>` opened in a block may be open after it.
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        assert!(parse("<?phv if ($c) { ?><select><?phv } ?><title>t</title>").iter().any(|d| d.is_error()));
        assert!(parse("<?phv if ($c) { ?><select><?phv } ?><option>o</option></select><title>t</title>")
            .iter()
            .all(|d| !d.is_error()));
    }

    /// Found by the 1.2.1 audit: `Html` written inside a `<select>` (a partial
    /// rendered there) was not checked for the elements above, and a partial
    /// could end inside a `<select>` it opened. Inside one, values land in
    /// `SelectText` (checked at run time), and a template may not end there.
    #[test]
    fn select_is_a_context_and_not_the_top_level() {
        assert_eq!(ctx("<select>"), EchoContext::SelectText);
        assert_eq!(ctx("<select><option>a</option>"), EchoContext::SelectText);
        assert_eq!(ctx("<select><optgroup label=\"g\">"), EchoContext::SelectText);
        assert_eq!(ctx("<select></select>"), EchoContext::Text);
        assert_eq!(ctx("<select><option>a</option></select><p>"), EchoContext::Text);
        // Inside SVG and in attributes, the stricter contexts stay.
        assert_eq!(ctx("<select><svg>"), EchoContext::ForeignText);
        assert_eq!(ctx("<select><option title=\""), EchoContext::Attr);
        assert_eq!(ctx("<select><textarea>"), EchoContext::RawText("textarea"));
        for html in ["<select>", "<select><option>a</option>", "<?phv if ($c) { ?><select><?phv } ?>"] {
            let mut t = HtmlTracker::default();
            t.feed(html);
            assert!(!t.at_top_level(), "{html}");
            assert!(t.only_select_open(), "{html}");
        }
        let mut t = HtmlTracker::default();
        t.feed("<select><option>a</option></select>");
        assert!(t.at_top_level());
        t.feed("<select><option title=\"");
        assert!(!t.at_top_level() && !t.only_select_open(), "not between elements");
        // A template may not end inside a `<select>`, also one a block may have opened.
        let parse = |src: &str| crate::parse_module(src, 0, true).1;
        for src in ["<select><option>a</option>", "<?phv if ($c) { ?><select><?phv } ?><option>o</option>"] {
            let diags = parse(src);
            assert!(diags.iter().any(|d| d.is_error() && d.message.contains("inside an open `<select>`")), "{src}");
        }
        assert!(parse("<select><option>a</option></select>").iter().all(|d| !d.is_error()));
        // Values inside a `<select>` are checked (`HtmlEcho`), with or without `<?= ?>`.
        for src in ["<select><?= $v ?></select>", "<select><?phv echo $v; ?></select>"] {
            let (module, diags) = crate::parse_module(src, 0, true);
            assert!(diags.iter().all(|d| !d.is_error()), "{src}");
            assert!(
                module
                    .stmts
                    .iter()
                    .any(|s| matches!(&s.kind, crate::ast::StmtKind::HtmlEcho(_, EchoContext::SelectText))),
                "{src}"
            );
            // Functions called there may write output (no guard), as in text.
            assert!(module.stmts.iter().all(|s| s.out.is_none()), "{src}");
        }
    }

    /// Found by the 1.2.1 audit: a self-closed `<script/>` or `<style/>` in
    /// SVG or MathML opens nothing for browsers (a self-closing foreign start
    /// tag is popped at once), where the tracker read raw text up to its end
    /// tag and refused everything after as markup in a script.
    #[test]
    fn self_closed_script_in_foreign_content_opens_no_text() {
        assert_eq!(ctx("<svg><script/><g>"), EchoContext::ForeignText);
        assert!(!found("<svg><script/><g>").contains(&InlineCode::ForeignCdata));
        assert_eq!(ctx("<svg><style/><a href=\""), EchoContext::Url);
        assert_eq!(ctx("<math><script/><mi>"), EchoContext::ForeignText);
        assert_eq!(ctx("<svg><script /><g>"), EchoContext::ForeignText);
        // Not self-closed: the script's text, as before.
        assert_eq!(ctx("<svg><script><g>"), EchoContext::Script);
        assert!(found("<svg><script><g>").contains(&InlineCode::ForeignCdata));
        // In an HTML integration point the tag is HTML, whose `/>` browsers
        // ignore: the script's text begins.
        assert_eq!(ctx("<svg><foreignObject><script/>var a = "), EchoContext::Script);
        assert_eq!(ctx("<svg><foreignObject><div><script/>var a = "), EchoContext::Script);
        assert_eq!(ctx("<math><mi><script/>var a = "), EchoContext::Script);
        assert_eq!(ctx("<script/>var a = "), EchoContext::Script);
    }

    /// The tables are well formed: a row always matches, the contexts are in
    /// the order of `Ctx`, the element names are unique and lowercase, and
    /// a row that reports something and goes on never ends a table.
    #[test]
    fn tables_are_well_formed() {
        for (k, (ctx, rules)) in RULES.iter().enumerate() {
            assert_eq!(*ctx as usize, k, "{ctx:?}");
            let last = rules.last().expect("a context has rules");
            assert!(
                matches!(last.on, On::Any) && last.when.is_empty(),
                "{ctx:?} ends with a row that matches anything"
            );
            for rule in *rules {
                if let On::Lit(s) | On::Exact(s) | On::RestStarts(s) | On::RestStartsExact(s) = rule.on {
                    assert!(s.is_ascii() && !s.is_empty(), "{s:?}");
                }
                if let Then::TagName(p) = rule.then {
                    assert!(p.is_empty() || p == "/");
                }
            }
        }
        let mut names: Vec<&str> = ELEMENTS.iter().map(|e| e.name).collect();
        assert!(names.iter().all(|n| *n == n.to_ascii_lowercase() && !n.is_empty()));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ELEMENTS.len(), "element names are unique");
        for e in ELEMENTS {
            // What hides an end tag is exactly the raw-text elements and `<noscript>`.
            assert_eq!(e.flags & HIDES_END != 0, e.flags & (RAW | NOSCRIPT) != 0, "{}", e.name);
            assert!(e.flags & NO_END_TAG == 0 || e.flags & RAW != 0, "{}", e.name);
        }
        for rules in [START_RULES, END_RULES] {
            let last = rules.last().unwrap();
            assert!(matches!(last.tag, TagIs::Any) && matches!(last.wrong, Wrong::None));
            assert!(!rules.iter().any(|r| matches!((r.then, r.wrong), (Stack::Continue, Wrong::None))));
            // Every top, self-closing or not, is covered by a row that matches any tag.
            for top in [Top::None, Top::Html, Top::HtmlPoint, Top::TextPoint, Top::AnnotationXml, Top::Foreign] {
                for closed in [Closed::Yes, Closed::No] {
                    assert!(
                        rules.iter().any(|r| r.top.contains(&top)
                            && matches!(r.tag, TagIs::Any)
                            && (r.self_closing == Closed::Any || r.self_closing == closed)),
                        "{top:?} {closed:?}"
                    );
                }
            }
        }
        assert!(URL_ATTRS
            .iter()
            .chain(ANIMATION_URL_ATTRS)
            .chain(SCRIPT_SRC_ATTRS)
            .all(|a| *a == a.to_ascii_lowercase()));
    }

    /// Each kind of row of the tables decides at least one of these (a row
    /// removed or reordered changes one of them).
    #[test]
    fn every_table_row_decides_something() {
        // Text rows.
        assert_eq!(ctx("<p>a"), EchoContext::Text);
        assert_eq!(ctx("<!-->a"), EchoContext::Text);
        assert_eq!(ctx("<!--->a"), EchoContext::Text);
        assert_eq!(ctx("<!-- a "), EchoContext::Comment { bogus: false, watched: false });
        assert_eq!(ctx("a<"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<a"), EchoContext::Tag);
        assert_eq!(ctx("a</"), EchoContext::AfterMarkup);
        assert_eq!(ctx("a<!-"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<svg><![CDA"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<svg><![CDATA[ "), EchoContext::Comment { bogus: true, watched: true });
        assert_eq!(ctx("<p></p"), EchoContext::Tag);
        assert_eq!(ctx("</>a"), EchoContext::Text);
        assert_eq!(ctx("</ a"), EchoContext::Comment { bogus: true, watched: false });
        assert_eq!(ctx("<!x a"), EchoContext::Comment { bogus: true, watched: false });
        assert_eq!(ctx("<?x a"), EchoContext::Comment { bogus: true, watched: false });
        assert_eq!(ctx("<1"), EchoContext::Text);
        // Tag, attribute and value rows.
        assert_eq!(ctx("<a>"), EchoContext::Text);
        assert_eq!(ctx("<a / "), EchoContext::Tag);
        assert_eq!(ctx("<a x"), EchoContext::Tag);
        assert_eq!(ctx("<a x="), EchoContext::Unquoted);
        assert_eq!(ctx("<a x>"), EchoContext::Text);
        assert_eq!(ctx("<a x  =\""), EchoContext::Attr);
        assert_eq!(ctx("<a x y=\""), EchoContext::Attr);
        assert_eq!(ctx("<a x/y=\""), EchoContext::Attr);
        assert_eq!(ctx("<a x= \""), EchoContext::Attr);
        assert_eq!(ctx("<a x=>"), EchoContext::Text);
        assert_eq!(ctx("<a x=y"), EchoContext::Unquoted);
        assert_eq!(ctx("<a x='y'"), EchoContext::Tag);
        assert_eq!(ctx("<a x=y "), EchoContext::Tag);
        assert_eq!(ctx("<a x=y>"), EchoContext::Text);
        // Comment, bogus comment and CDATA rows.
        assert_eq!(ctx("<!-- a -->"), EchoContext::Text);
        assert_eq!(ctx("<!-- a --!>"), EchoContext::Text);
        assert_eq!(ctx("<!x >"), EchoContext::Text);
        assert_eq!(ctx("<svg><![CDATA[ ]]>"), EchoContext::ForeignText);
        assert!(found("<svg><![CDATA[ ] > ]]>").contains(&InlineCode::CdataEnd));
        // Raw text rows.
        assert_eq!(ctx("<script>a <!-"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<script><!-- <scr"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<script><!-- --"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<textarea></te"), EchoContext::AfterMarkup);
        assert_eq!(ctx("<script><!--<script></script>a"), EchoContext::ScriptLiteral);
        assert_eq!(ctx("<textarea></textarea>"), EchoContext::Text);
        assert!(found("<svg><script><a>").contains(&InlineCode::ForeignCdata));
        assert_eq!(ctx("<style><!-- a"), EchoContext::Style);
        // `<!---->` leaves the escape state alone (`<script>` after it is text, so
        // `</script>` ends the script); `<!--` is a line comment for JavaScript.
        assert_eq!(ctx("<script><!---->\n<script>\n</script>a"), EchoContext::Text);
        assert_eq!(ctx("<script><!---->var a = "), EchoContext::ScriptLiteral);
        assert_eq!(ctx("<script><!-- a"), EchoContext::ScriptLiteral);
        assert_eq!(ctx("<script><!-- a -->\nvar b = "), EchoContext::Script);
        assert_eq!(ctx("<script><!-- <script></script>--></script>"), EchoContext::Text);
        // Open-element rows (the start and end tags of foreign content).
        assert_eq!(ctx("<svg/>a"), EchoContext::Text);
        assert_eq!(ctx("<math><mi><mglyph/>"), EchoContext::ForeignText);
        assert_eq!(structure_errors("<math><mi><mglyph></mglyph></mi></math>"), 0);
        assert_eq!(structure_errors("<math><annotation-xml><b></b></annotation-xml></math>"), 1);
        assert_eq!(ctx("<svg><title><svg/></title></svg>a"), EchoContext::Text);
        assert_eq!(ctx("<svg><title><svg><g/></svg></title></svg>a"), EchoContext::Text);
        assert_eq!(ctx("<svg><title><math><mi/></math></title></svg>a"), EchoContext::Text);
        assert_eq!(ctx("<svg><title><br></title></svg>a"), EchoContext::Text);
        assert_eq!(ctx("<svg><title><b></b></title></svg>a"), EchoContext::Text);
        assert_eq!(structure_errors("<svg><br></svg>"), 1);
        assert_eq!(structure_errors("<svg><b></b></svg>"), 1);
        assert_eq!(ctx("<svg><g/></svg>a"), EchoContext::Text);
        assert_eq!(ctx("<svg><g></g></svg>a"), EchoContext::Text);
        assert_eq!(structure_errors("<svg><title><b></svg>"), 1);
        assert_eq!(ctx("<svg><title><b></i></b></title></svg>a"), EchoContext::Text);
        assert_eq!(structure_errors("<svg></p>"), 1);
        assert_eq!(ctx("<svg><g></svg>a"), EchoContext::Text);
        assert_eq!(structure_errors("<svg><title><b><svg></b></svg></title></svg>"), 1);
        assert_eq!(ctx("<svg></x>"), EchoContext::ForeignText);
        // Rows whose conditions only a corner case tells apart (each of these
        // passed with one condition of a row removed, so they stay).
        // `<scripts` is not `<script` with a delimiter after it: no double escape.
        assert_eq!(ctx("<script><!--<scripts></script>a"), EchoContext::Text);
        // `<![CDATA[` is a bogus comment outside foreign content: no `]]>` end.
        assert!(!found("<![CDATA[ > ]]>").contains(&InlineCode::CdataEnd));
        // An end tag closes the nearest element of its name when the HTML
        // element and the foreign element have the same name.
        assert_eq!(structure_errors("<svg><foreignObject><a><svg><a></a></svg></a></foreignObject></svg>"), 0);
        // Spaces before `=` belong to the attribute `x`, not to `href`.
        assert_eq!(ctx("<a x href=\""), EchoContext::Url);
        // `--` is carried only in the escaped states.
        assert_eq!(ctx("<script>a--"), EchoContext::Script);
    }
}
