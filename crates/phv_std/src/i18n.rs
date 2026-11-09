//! Translations (`std.i18n`): message catalogs, plural rules, the locale of a
//! request, and numbers and dates written as a locale writes them.
//!
//! The catalogs (`lang/<locale>.toml`, one per locale of `[i18n]`) are read
//! and checked when a program that imports `std.i18n` is compiled (`build`,
//! called by the loader), so a missing or malformed catalog is a compile
//! error, never an error at a visitor's request. The program keeps them
//! (`Program::i18n`), shared by the pages of a process (`intern`).
//!
//! Messages are plain text with `{name}` placeholders. Parameters and results
//! are strings, escaped on output like any string; `t_html()` escapes the
//! message's text and inserts `Html` parameters, so a translator never writes
//! markup.
//!
//! The plural rules and the formats of numbers and dates are those of CLDR 49
//! (`plurals.json`, `numbers.json` and `ca-gregorian.json` of cldr-json
//! 49.0.0), copied for the locales in `PLURALS` and `FORMATS`; nothing is
//! downloaded.

use super::{int, map, s};
use phv_vm::i18n::{Catalog, Catalogs, Message, Piece};
use phv_vm::{BuiltinFn, Key, Value, Vm, VmError};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// The locales a site may list in `[i18n] locales`.
pub const MAX_LOCALES: usize = 32;
/// Bytes of one catalog file, and of all of them together.
pub const MAX_CATALOG_BYTES: usize = 2 << 20;
pub const MAX_CATALOGS_BYTES: usize = 16 << 20;
/// Messages in one catalog.
pub const MAX_MESSAGES: usize = 20_000;
/// Bytes of a key (`checkout.errors.card_declined`), and its parts at most.
pub const MAX_KEY: usize = 200;
pub const MAX_KEY_DEPTH: usize = 8;
/// Bytes of one message (of each form of a plural message).
pub const MAX_MESSAGE: usize = 8 << 10;
/// Different placeholders in one message, and bytes of a placeholder's name.
pub const MAX_PLACEHOLDERS: usize = 16;
pub const MAX_PARAM_NAME: usize = 64;
/// Entries of the parameters given to one call.
pub const MAX_PARAMS: usize = 64;
/// Bytes of `Accept-Language` read (the rest is ignored, at the last comma
/// before it), entries of it looked at, and bytes of one of its tags.
pub const MAX_ACCEPT: usize = 1024;
pub const MAX_ACCEPT_ENTRIES: usize = 32;
pub const MAX_ACCEPT_TAG: usize = 35;
/// Decimals `number()` and `percent()` write at most.
pub const MAX_DECIMALS: i64 = 20;
/// Errors reported for the catalogs of one program at most.
const MAX_ERRORS: usize = 50;

// ----- locales ---------------------------------------------------------------

/// Whether `tag` is a locale as `[i18n] locales` takes it: a language of two
/// or three lowercase letters, then optionally a script (`Hant`) and a region
/// (`BR`, `419`), joined by `-`.
pub fn valid_locale(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let lang = parts.next().unwrap_or("");
    if !(2..=3).contains(&lang.len()) || !lang.bytes().all(|b| b.is_ascii_lowercase()) {
        return false;
    }
    let mut rest: Vec<&str> = parts.collect();
    if rest.first().is_some_and(|p| {
        p.len() == 4 && p.as_bytes()[0].is_ascii_uppercase() && p[1..].bytes().all(|b| b.is_ascii_lowercase())
    }) {
        rest.remove(0);
    }
    match rest.as_slice() {
        [] => true,
        [r] => {
            (r.len() == 2 && r.bytes().all(|b| b.is_ascii_uppercase()))
                || (r.len() == 3 && r.bytes().all(|b| b.is_ascii_digit()))
        }
        _ => false,
    }
}

/// The locales `locale` falls back to, most specific first, without itself:
/// `zh-Hant-TW` gives `zh-Hant`, `zh`.
fn parents(locale: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut s = locale;
    while let Some(i) = s.rfind('-') {
        s = &s[..i];
        out.push(s);
    }
    out
}

/// The language of a locale (`pt` of `pt-BR`).
fn language(locale: &str) -> &str {
    locale.split('-').next().unwrap_or(locale)
}

/// `tag` in the form locales are compared in: lowercase, with `-`.
fn folded(tag: &str) -> String {
    tag.trim().replace('_', "-").to_ascii_lowercase()
}

/// The listed locale `tag` names (in any case, with `-` or `_`), if any.
fn listed<'a>(locales: &'a [String], tag: &str) -> Option<&'a str> {
    let t = folded(tag);
    locales.iter().find(|l| l.to_ascii_lowercase() == t).map(String::as_str)
}

/// The best of `locales` for an `Accept-Language` header, or `default`.
///
/// The header is read up to `MAX_ACCEPT` bytes (to the last comma before
/// them), and up to `MAX_ACCEPT_ENTRIES` entries. An entry is a tag (or `*`)
/// with an optional `q` between 0 and 1 of at most three decimals; one with
/// another `q`, or with a tag that is not letters, digits and `-` (at most
/// `MAX_ACCEPT_TAG` bytes) is skipped, and `q=0` refuses a tag. The entries
/// are tried by `q`, then in the order written: a locale equal to the tag,
/// then to a shorter form of it (`de-CH` to `de`), then the first locale of
/// the tag's language (`pt` to `pt-BR`). `*` takes the default.
pub fn negotiate<'a>(header: &str, locales: &'a [String], default: &'a str) -> &'a str {
    let mut text = header;
    if text.len() > MAX_ACCEPT {
        let mut cut = MAX_ACCEPT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text = &text[..cut];
        text = &text[..text.rfind(',').unwrap_or(0)];
    }
    let mut entries: Vec<(u16, usize, String)> = Vec::new();
    for (i, entry) in text.split(',').take(MAX_ACCEPT_ENTRIES).enumerate() {
        let mut parts = entry.split(';');
        let tag = parts.next().unwrap_or("").trim();
        if tag.is_empty()
            || tag.len() > MAX_ACCEPT_TAG
            || !(tag == "*" || tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        {
            continue;
        }
        let mut q = Some(1000u16);
        for p in parts {
            let p = p.trim();
            if let Some(v) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                q = quality(v.trim());
            }
        }
        if let Some(q) = q {
            entries.push((q, i, folded(tag)));
        }
    }
    // By q, highest first; equal ones in the order written.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let refused: HashSet<&str> = entries.iter().filter(|e| e.0 == 0).map(|e| e.2.as_str()).collect();
    let ok = |l: &&String| !refused.contains(l.to_ascii_lowercase().as_str());
    for (q, _, tag) in &entries {
        if *q == 0 {
            continue;
        }
        if tag == "*" {
            return default;
        }
        let found = std::iter::once(tag.as_str())
            .chain(parents(tag))
            .find_map(|t| locales.iter().filter(ok).find(|l| l.to_ascii_lowercase() == t));
        if let Some(l) = found {
            return l;
        }
        let lang = language(tag);
        if let Some(l) = locales.iter().filter(ok).find(|l| language(&l.to_ascii_lowercase()) == lang) {
            return l;
        }
    }
    default
}

/// A `q` value in thousandths: `0` to `1`, with at most three decimals.
fn quality(v: &str) -> Option<u16> {
    let (int, frac) = v.split_once('.').unwrap_or((v, ""));
    if frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac_value = format!("{frac:0<3}").parse::<u16>().ok()?;
    match int {
        "0" => Some(frac_value),
        "1" if frac_value == 0 => Some(1000),
        _ => None,
    }
}

// ----- plural rules (CLDR 49) ----------------------------------------------

/// The CLDR plural categories, by the number messages keep them as.
pub const CATEGORIES: [&str; 6] = ["zero", "one", "two", "few", "many", "other"];
const ZERO: u8 = 0;
const ONE: u8 = 1;
const TWO: u8 = 2;
const FEW: u8 = 3;
const MANY: u8 = 4;
const OTHER: u8 = 5;

/// The locales with plural rules, and the categories each uses (CLDR 49,
/// `plurals.json`, cardinal). A locale not listed uses its language's rules.
const PLURALS: &[(&str, &[u8])] = &[
    ("ar", &[ZERO, ONE, TWO, FEW, MANY, OTHER]),
    ("de", &[ONE, OTHER]),
    ("en", &[ONE, OTHER]),
    ("es", &[ONE, MANY, OTHER]),
    ("fr", &[ONE, MANY, OTHER]),
    ("it", &[ONE, MANY, OTHER]),
    ("ja", &[OTHER]),
    ("ko", &[OTHER]),
    ("pl", &[ONE, FEW, MANY, OTHER]),
    ("pt", &[ONE, MANY, OTHER]),
    ("pt-PT", &[ONE, MANY, OTHER]),
    ("ru", &[ONE, FEW, MANY, OTHER]),
    ("zh", &[OTHER]),
];

/// The entry of `PLURALS` for `locale`: its own, or its language's.
fn plural_rules(locale: &str) -> Option<(&'static str, &'static [u8])> {
    let find = |t: &str| PLURALS.iter().find(|(l, _)| *l == t).copied();
    find(locale).or_else(|| parents(locale).into_iter().find_map(find))
}

/// The locales with plural rules, for messages.
fn plural_locales() -> String {
    PLURALS.iter().map(|(l, _)| *l).collect::<Vec<_>>().join(", ")
}

/// The category of the whole number `n` in `rules` (an entry of `PLURALS`).
/// The rules are CLDR's for integers: the operands `v` (visible decimals)
/// and `e` (compact exponent) are 0, and `i` = `n` = |n|.
fn category(rules: &str, n: i64) -> u8 {
    let n = n.unsigned_abs();
    let (n10, n100) = (n % 10, n % 100);
    // `many` of es, fr, it and pt: e = 0 and i != 0 and i % 1000000 = 0 and v = 0.
    let millions = n != 0 && n.is_multiple_of(1_000_000);
    match rules {
        "ja" | "ko" | "zh" => OTHER,
        // one: i = 1 and v = 0
        "en" | "de" => {
            if n == 1 {
                ONE
            } else {
                OTHER
            }
        }
        // one: n = 1 (es) / i = 1 and v = 0 (it, pt-PT)
        "es" | "it" | "pt-PT" => match n {
            1 => ONE,
            _ if millions => MANY,
            _ => OTHER,
        },
        // one: i = 0..1
        "fr" | "pt" => match n {
            0 | 1 => ONE,
            _ if millions => MANY,
            _ => OTHER,
        },
        // one: v = 0 and i % 10 = 1 and i % 100 != 11
        // few: v = 0 and i % 10 = 2..4 and i % 100 != 12..14
        // many: v = 0 and (i % 10 = 0 or i % 10 = 5..9 or i % 100 = 11..14)
        "ru" => {
            if n10 == 1 && n100 != 11 {
                ONE
            } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
                FEW
            } else {
                MANY
            }
        }
        // one: i = 1 and v = 0
        // few: v = 0 and i % 10 = 2..4 and i % 100 != 12..14
        // many: v = 0 and (i != 1 and i % 10 = 0..1 or i % 10 = 5..9 or i % 100 = 12..14)
        "pl" => {
            if n == 1 {
                ONE
            } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
                FEW
            } else {
                MANY
            }
        }
        // zero: n = 0; one: n = 1; two: n = 2; few: n % 100 = 3..10; many: n % 100 = 11..99
        "ar" => match (n, n100) {
            (0, _) => ZERO,
            (1, _) => ONE,
            (2, _) => TWO,
            (_, 3..=10) => FEW,
            (_, 11..=99) => MANY,
            _ => OTHER,
        },
        _ => OTHER,
    }
}

// ----- numbers and dates (CLDR 49) ---------------------------------------------

/// How a locale writes numbers and dates (CLDR 49: the `latn` symbols, the
/// percent pattern, `minimumGroupingDigits`, and the Gregorian calendar's
/// month and day names, format context, and date formats).
struct Format {
    tag: &'static str,
    decimal: &'static str,
    group: &'static str,
    /// Digits the integer part needs before a group separator is written
    /// beyond 3 (`minimumGroupingDigits`): with 2, `1234` but `12.345`.
    min_group: usize,
    minus: &'static str,
    /// What follows the number of a percentage (`%`, ` %`).
    percent: &'static str,
    months_short: [&'static str; 12],
    months: [&'static str; 12],
    /// Sunday first.
    weekdays: [&'static str; 7],
    /// The `full`, `long`, `medium` and `short` date patterns.
    dates: [&'static str; 4],
}

const EN_MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const EN_DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const PT_MONTHS_SHORT: [&str; 12] =
    ["jan.", "fev.", "mar.", "abr.", "mai.", "jun.", "jul.", "ago.", "set.", "out.", "nov.", "dez."];
const PT_MONTHS: [&str; 12] = [
    "janeiro",
    "fevereiro",
    "março",
    "abril",
    "maio",
    "junho",
    "julho",
    "agosto",
    "setembro",
    "outubro",
    "novembro",
    "dezembro",
];
const PT_DAYS: [&str; 7] =
    ["domingo", "segunda-feira", "terça-feira", "quarta-feira", "quinta-feira", "sexta-feira", "sábado"];
const CJK_MONTHS: [&str; 12] = ["1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月"];
const AR_MONTHS: [&str; 12] =
    ["يناير", "فبراير", "مارس", "أبريل", "مايو", "يونيو", "يوليو", "أغسطس", "سبتمبر", "أكتوبر", "نوفمبر", "ديسمبر"];

const FORMATS: &[Format] = &[
    Format {
        tag: "en",
        decimal: ".",
        group: ",",
        min_group: 1,
        minus: "-",
        percent: "%",
        months_short: ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"],
        months: EN_MONTHS,
        weekdays: EN_DAYS,
        dates: ["EEEE, MMMM d, y", "MMMM d, y", "MMM d, y", "M/d/yy"],
    },
    Format {
        tag: "en-GB",
        decimal: ".",
        group: ",",
        min_group: 1,
        minus: "-",
        percent: "%",
        months_short: ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sept", "Oct", "Nov", "Dec"],
        months: EN_MONTHS,
        weekdays: EN_DAYS,
        dates: ["EEEE, d MMMM y", "d MMMM y", "d MMM y", "dd/MM/y"],
    },
    Format {
        tag: "fr",
        decimal: ",",
        group: "\u{202F}",
        min_group: 1,
        minus: "-",
        percent: "\u{202F}%",
        months_short: [
            "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc.",
        ],
        months: [
            "janvier",
            "février",
            "mars",
            "avril",
            "mai",
            "juin",
            "juillet",
            "août",
            "septembre",
            "octobre",
            "novembre",
            "décembre",
        ],
        weekdays: ["dimanche", "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi"],
        dates: ["EEEE d MMMM y", "d MMMM y", "d MMM y", "dd/MM/y"],
    },
    Format {
        tag: "de",
        decimal: ",",
        group: ".",
        min_group: 1,
        minus: "-",
        percent: "\u{A0}%",
        months_short: ["Jan.", "Feb.", "März", "Apr.", "Mai", "Juni", "Juli", "Aug.", "Sept.", "Okt.", "Nov.", "Dez."],
        months: [
            "Januar",
            "Februar",
            "März",
            "April",
            "Mai",
            "Juni",
            "Juli",
            "August",
            "September",
            "Oktober",
            "November",
            "Dezember",
        ],
        weekdays: ["Sonntag", "Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag"],
        dates: ["EEEE, d. MMMM y", "d. MMMM y", "dd.MM.y", "dd.MM.yy"],
    },
    Format {
        tag: "es",
        decimal: ",",
        group: ".",
        min_group: 2,
        minus: "-",
        percent: "\u{A0}%",
        months_short: ["ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sept", "oct", "nov", "dic"],
        months: [
            "enero",
            "febrero",
            "marzo",
            "abril",
            "mayo",
            "junio",
            "julio",
            "agosto",
            "septiembre",
            "octubre",
            "noviembre",
            "diciembre",
        ],
        weekdays: ["domingo", "lunes", "martes", "miércoles", "jueves", "viernes", "sábado"],
        dates: ["EEEE, d 'de' MMMM 'de' y", "d 'de' MMMM 'de' y", "d MMM y", "d/M/yy"],
    },
    Format {
        tag: "it",
        decimal: ",",
        group: ".",
        min_group: 2,
        minus: "-",
        percent: "%",
        months_short: ["gen", "feb", "mar", "apr", "mag", "giu", "lug", "ago", "set", "ott", "nov", "dic"],
        months: [
            "gennaio",
            "febbraio",
            "marzo",
            "aprile",
            "maggio",
            "giugno",
            "luglio",
            "agosto",
            "settembre",
            "ottobre",
            "novembre",
            "dicembre",
        ],
        weekdays: ["domenica", "lunedì", "martedì", "mercoledì", "giovedì", "venerdì", "sabato"],
        dates: ["EEEE d MMMM y", "d MMMM y", "d MMM y", "dd/MM/yy"],
    },
    Format {
        tag: "pt",
        decimal: ",",
        group: ".",
        min_group: 1,
        minus: "-",
        percent: "%",
        months_short: PT_MONTHS_SHORT,
        months: PT_MONTHS,
        weekdays: PT_DAYS,
        dates: ["EEEE, d 'de' MMMM 'de' y", "d 'de' MMMM 'de' y", "d 'de' MMM 'de' y", "dd/MM/y"],
    },
    Format {
        tag: "pt-PT",
        decimal: ",",
        group: "\u{A0}",
        min_group: 2,
        minus: "-",
        percent: "%",
        months_short: PT_MONTHS_SHORT,
        months: PT_MONTHS,
        weekdays: PT_DAYS,
        dates: ["EEEE, d 'de' MMMM 'de' y", "d 'de' MMMM 'de' y", "dd/MM/y", "dd/MM/yy"],
    },
    Format {
        tag: "ru",
        decimal: ",",
        group: "\u{A0}",
        min_group: 1,
        minus: "-",
        percent: "\u{A0}%",
        months_short: [
            "янв.",
            "февр.",
            "мар.",
            "апр.",
            "мая",
            "июн.",
            "июл.",
            "авг.",
            "сент.",
            "окт.",
            "нояб.",
            "дек.",
        ],
        months: [
            "января",
            "февраля",
            "марта",
            "апреля",
            "мая",
            "июня",
            "июля",
            "августа",
            "сентября",
            "октября",
            "ноября",
            "декабря",
        ],
        weekdays: ["воскресенье", "понедельник", "вторник", "среда", "четверг", "пятница", "суббота"],
        dates: ["EEEE, d MMMM y 'г'.", "d MMMM y 'г'.", "d MMM y 'г'.", "dd.MM.y"],
    },
    Format {
        tag: "pl",
        decimal: ",",
        group: "\u{A0}",
        min_group: 2,
        minus: "-",
        percent: "%",
        months_short: ["sty", "lut", "mar", "kwi", "maj", "cze", "lip", "sie", "wrz", "paź", "lis", "gru"],
        months: [
            "stycznia",
            "lutego",
            "marca",
            "kwietnia",
            "maja",
            "czerwca",
            "lipca",
            "sierpnia",
            "września",
            "października",
            "listopada",
            "grudnia",
        ],
        weekdays: ["niedziela", "poniedziałek", "wtorek", "środa", "czwartek", "piątek", "sobota"],
        dates: ["EEEE, d MMMM y", "d MMMM y", "d MMM y", "d.MM.y"],
    },
    Format {
        tag: "ar",
        decimal: ".",
        group: ",",
        min_group: 1,
        minus: "\u{200E}-",
        percent: "\u{200E}%\u{200E}",
        months_short: AR_MONTHS,
        months: AR_MONTHS,
        weekdays: ["الأحد", "الاثنين", "الثلاثاء", "الأربعاء", "الخميس", "الجمعة", "السبت"],
        dates: ["EEEE، d MMMM y", "d MMMM y", "dd\u{200F}/MM\u{200F}/y", "d\u{200F}/M\u{200F}/y"],
    },
    Format {
        tag: "ja",
        decimal: ".",
        group: ",",
        min_group: 1,
        minus: "-",
        percent: "%",
        months_short: CJK_MONTHS,
        months: CJK_MONTHS,
        weekdays: ["日曜日", "月曜日", "火曜日", "水曜日", "木曜日", "金曜日", "土曜日"],
        dates: ["y年M月d日EEEE", "y年M月d日", "y/MM/dd", "y/MM/dd"],
    },
    Format {
        tag: "zh",
        decimal: ".",
        group: ",",
        min_group: 1,
        minus: "-",
        percent: "%",
        months_short: CJK_MONTHS,
        months: ["一月", "二月", "三月", "四月", "五月", "六月", "七月", "八月", "九月", "十月", "十一月", "十二月"],
        weekdays: ["星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"],
        dates: ["y年M月d日EEEE", "y年M月d日", "y年M月d日", "y/M/d"],
    },
    Format {
        tag: "ko",
        decimal: ".",
        group: ",",
        min_group: 1,
        minus: "-",
        percent: "%",
        months_short: ["1월", "2월", "3월", "4월", "5월", "6월", "7월", "8월", "9월", "10월", "11월", "12월"],
        months: ["1월", "2월", "3월", "4월", "5월", "6월", "7월", "8월", "9월", "10월", "11월", "12월"],
        weekdays: ["일요일", "월요일", "화요일", "수요일", "목요일", "금요일", "토요일"],
        dates: ["y년 MMMM d일 EEEE", "y년 MMMM d일", "y. M. d.", "yy. M. d."],
    },
];

/// For a locale without an entry: CLDR's root symbols, and dates as ISO 8601
/// in every style.
const ROOT: Format = Format {
    tag: "",
    decimal: ".",
    group: ",",
    min_group: 1,
    minus: "-",
    percent: "%",
    months_short: CJK_MONTHS,
    months: CJK_MONTHS,
    weekdays: EN_DAYS,
    dates: ["y-MM-dd", "y-MM-dd", "y-MM-dd", "y-MM-dd"],
};

/// The entry of `FORMATS` for `locale`: its own, its language's, or `ROOT`.
fn format_of(locale: &str) -> &'static Format {
    let find = |t: &str| FORMATS.iter().find(|f| f.tag == t);
    find(locale).or_else(|| parents(locale).into_iter().find_map(find)).unwrap_or(&ROOT)
}

/// `digits` (ASCII, the integer part of a number) with the group separators
/// of `f`: every three digits from the right, once there are at least
/// `3 + min_group` of them.
fn grouped(f: &Format, digits: &str, out: &mut String) {
    let len = digits.len();
    let group = len >= 3 + f.min_group;
    for (i, ch) in digits.char_indices() {
        if group && i > 0 && (len - i).is_multiple_of(3) {
            out.push_str(f.group);
        }
        out.push(ch);
    }
}

/// An integer as `f` writes it, with `decimals` zeros after the separator.
fn format_int(f: &Format, n: i64, decimals: usize) -> String {
    let mut out = String::new();
    if n < 0 {
        out.push_str(f.minus);
    }
    grouped(f, &n.unsigned_abs().to_string(), &mut out);
    if decimals > 0 {
        out.push_str(f.decimal);
        out.extend(std::iter::repeat_n('0', decimals));
    }
    out
}

/// A number as `f` writes it, rounded half away from zero to `decimals`
/// places (as `number_format()`).
fn format_float(f: &Format, x: f64, decimals: usize) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { format!("{}∞", f.minus) } else { "∞".into() };
    }
    let p = 10f64.powi(decimals as i32);
    let scaled = x.abs() * p;
    let rounded = if scaled.is_finite() { scaled.round() / p } else { x.abs() };
    let text = format!("{rounded:.decimals$}");
    let (int_part, frac) = text.split_once('.').unwrap_or((&text, ""));
    let mut out = String::new();
    if x < 0.0 && text.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        out.push_str(f.minus);
    }
    grouped(f, int_part, &mut out);
    if !frac.is_empty() {
        out.push_str(f.decimal);
        out.push_str(frac);
    }
    out
}

/// A date in the style `style` (0 full, 1 long, 2 medium, 3 short) of `f`.
/// `fields` are the year, month (1-12), day and weekday (1 Monday to 7
/// Sunday). The patterns use `y`, `yy`, `M` to `MMMM`, `d`, `dd`, `EEEE` and
/// quoted text.
fn format_date(f: &Format, style: usize, fields: (i64, u32, u32, u32)) -> String {
    let (year, month, day, weekday) = fields;
    let pattern: Vec<char> = f.dates[style].chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < pattern.len() {
        let c = pattern[i];
        if c == '\'' {
            // Quoted text; `''` is a quote.
            i += 1;
            while i < pattern.len() {
                if pattern[i] == '\'' {
                    if pattern.get(i + 1) == Some(&'\'') {
                        out.push('\'');
                        i += 2;
                        continue;
                    }
                    break;
                }
                out.push(pattern[i]);
                i += 1;
            }
            i += 1;
            continue;
        }
        let mut n = 1;
        while pattern.get(i + n) == Some(&c) {
            n += 1;
        }
        match (c, n) {
            ('y', 2) => out.push_str(&format!("{:02}", year.rem_euclid(100))),
            ('y', _) => out.push_str(&year.to_string()),
            ('M', 1) => out.push_str(&month.to_string()),
            ('M', 2) => out.push_str(&format!("{month:02}")),
            ('M', 3) => out.push_str(f.months_short[month as usize - 1]),
            ('M', _) => out.push_str(f.months[month as usize - 1]),
            ('d', 1) => out.push_str(&day.to_string()),
            ('d', _) => out.push_str(&format!("{day:02}")),
            ('E', _) => out.push_str(f.weekdays[weekday as usize % 7]),
            _ => out.extend(std::iter::repeat_n(c, n)),
        }
        i += n;
    }
    out
}

// ----- catalogs ------------------------------------------------------------------

/// An error in a catalog: the index of its locale, where, and what.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogError {
    pub catalog: usize,
    pub span: Range<usize>,
    pub message: String,
}

/// What the type checker learns of a message of the default catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageInfo {
    pub plural: bool,
    /// The placeholders of the message (of all its forms), in order.
    pub params: Vec<String>,
}

/// The pieces of a message: its text and `{name}` placeholders. `{{` and
/// `}}` write a brace; any other brace is an error.
pub fn parse_message(text: &str) -> Result<Vec<Piece>, String> {
    if text.len() > MAX_MESSAGE {
        return Err(format!("a message has at most {MAX_MESSAGE} bytes"));
    }
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let mut names: Vec<&str> = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' if b.get(i + 1) == Some(&b'{') => {
                lit.push('{');
                i += 2;
            }
            b'}' if b.get(i + 1) == Some(&b'}') => {
                lit.push('}');
                i += 2;
            }
            b'{' => {
                let end = text[i + 1..].find('}').map(|e| i + 1 + e);
                let name = end.map(|e| &text[i + 1..e]).filter(|n| param_name(n));
                let (Some(end), Some(name)) = (end, name) else {
                    return Err("a placeholder is `{name}` (letters, digits and `_`, not starting with a digit); \
                         write `{{` for a brace"
                        .into());
                };
                if !lit.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut lit)));
                }
                if !names.contains(&name) {
                    if names.len() == MAX_PLACEHOLDERS {
                        return Err(format!("a message has at most {MAX_PLACEHOLDERS} different placeholders"));
                    }
                    names.push(name);
                }
                pieces.push(Piece::Param(name.to_string()));
                i = end + 1;
            }
            b'}' => return Err("a `}` that ends no placeholder; write `}}` for a brace".into()),
            _ => {
                let len = text[i..].chars().next().map_or(1, char::len_utf8);
                lit.push_str(&text[i..i + len]);
                i += len;
            }
        }
    }
    if !lit.is_empty() || pieces.is_empty() {
        pieces.push(Piece::Text(lit));
    }
    Ok(pieces)
}

fn param_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_PARAM_NAME
        && !n.as_bytes()[0].is_ascii_digit()
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn key_part(k: &str) -> bool {
    !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The placeholders of a message, each once, in order.
fn placeholders(m: &Message) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let forms: Vec<&Vec<Piece>> = match m {
        Message::Text(p) => vec![p],
        Message::Plural(forms) => forms.iter().map(|(_, p)| p).collect(),
    };
    for p in forms.into_iter().flatten() {
        if let Piece::Param(n) = p {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
    }
    out
}

/// One catalog as parsed: its messages and where each key is written.
struct Parsed {
    messages: HashMap<String, Message>,
    spans: HashMap<String, Range<usize>>,
}

/// Parses the catalog of `locale` (the catalog number `index`), adding what
/// is wrong with it to `errors`.
fn parse_catalog(index: usize, locale: &str, text: &str, errors: &mut Vec<CatalogError>) -> Parsed {
    let mut out = Parsed { messages: HashMap::new(), spans: HashMap::new() };
    let mut err = |span: Range<usize>, message: String| {
        errors.push(CatalogError { catalog: index, span, message });
    };
    if text.len() > MAX_CATALOG_BYTES {
        err(0..0, format!("a catalog has at most {} MB", MAX_CATALOG_BYTES >> 20));
        return out;
    }
    let doc = match toml::de::DeTable::parse(text) {
        Ok(d) => d,
        Err(e) => {
            err(e.span().unwrap_or(0..0), format!("this catalog is not valid TOML: {}", e.message()));
            return out;
        }
    };
    let rules = plural_rules(locale);
    // Tables still to read: (table, its key, its depth).
    let mut work: Vec<(&toml::de::DeTable, String, usize)> = vec![(doc.get_ref(), String::new(), 0)];
    let mut too_many = false;
    while let Some((table, prefix, depth)) = work.pop() {
        for (k, v) in table.iter() {
            let part: &str = k.get_ref();
            let key = if prefix.is_empty() { part.to_string() } else { format!("{prefix}.{part}") };
            if !key_part(part) {
                err(k.span(), format!("`{part}`: a key is made of letters, digits, `_` and `-`"));
                continue;
            }
            if key.len() > MAX_KEY {
                err(k.span(), format!("a key has at most {MAX_KEY} bytes"));
                continue;
            }
            let message = match v.get_ref() {
                toml::de::DeValue::String(text) => match parse_message(text) {
                    Ok(p) => Message::Text(p),
                    Err(e) => {
                        err(v.span(), format!("`{key}`: {e}"));
                        continue;
                    }
                },
                toml::de::DeValue::Table(t)
                    if !t.is_empty() && t.keys().all(|c| CATEGORIES.contains(&&**c.get_ref())) =>
                {
                    let Some((_, cats)) = rules else {
                        err(
                            v.span(),
                            format!(
                                "`{key}` is a plural message, and there are no plural rules for `{locale}` \
                                 (there are for {})",
                                plural_locales()
                            ),
                        );
                        continue;
                    };
                    let mut forms = Vec::new();
                    let mut bad = false;
                    for (c, form) in t.iter() {
                        let cat = CATEGORIES.iter().position(|x| *x == &**c.get_ref()).unwrap_or(5) as u8;
                        let toml::de::DeValue::String(text) = form.get_ref() else {
                            err(
                                form.span(),
                                format!("`{key}.{}`: a plural form is a string (forms do not nest)", c.get_ref()),
                            );
                            bad = true;
                            continue;
                        };
                        if !cats.contains(&cat) {
                            err(
                                c.span(),
                                format!(
                                    "`{key}`: `{locale}` has no plural form `{}` (it has {})",
                                    c.get_ref(),
                                    names_of(cats)
                                ),
                            );
                            bad = true;
                            continue;
                        }
                        match parse_message(text) {
                            Ok(p) => forms.push((cat, p)),
                            Err(e) => {
                                err(form.span(), format!("`{key}.{}`: {e}", c.get_ref()));
                                bad = true;
                            }
                        }
                    }
                    let missing: Vec<&str> = cats
                        .iter()
                        .filter(|c| !forms.iter().any(|(f, _)| f == *c))
                        .map(|c| CATEGORIES[*c as usize])
                        .collect();
                    if !bad && !missing.is_empty() {
                        err(
                            k.span(),
                            format!(
                                "`{key}` has no plural form {} (`{locale}` has {})",
                                missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", "),
                                names_of(cats)
                            ),
                        );
                        bad = true;
                    }
                    if bad {
                        continue;
                    }
                    forms.sort_by_key(|(c, _)| *c);
                    Message::Plural(forms)
                }
                toml::de::DeValue::Table(t) => {
                    if depth + 1 >= MAX_KEY_DEPTH {
                        err(k.span(), format!("keys have at most {MAX_KEY_DEPTH} parts"));
                    } else {
                        work.push((t, key, depth + 1));
                    }
                    continue;
                }
                _ => {
                    err(v.span(), format!("`{key}`: a message is a string, or a table of plural forms"));
                    continue;
                }
            };
            if out.messages.len() == MAX_MESSAGES {
                if !too_many {
                    err(k.span(), format!("a catalog has at most {MAX_MESSAGES} messages"));
                    too_many = true;
                }
                continue;
            }
            out.spans.insert(key.clone(), k.span());
            out.messages.insert(key, message);
        }
    }
    out
}

/// `one, few, many, other`.
fn names_of(cats: &[u8]) -> String {
    cats.iter().map(|c| CATEGORIES[*c as usize]).collect::<Vec<_>>().join(", ")
}

/// Builds the catalogs of `locales` (`[i18n] locales`, with `default` among
/// them) from their texts, in the same order, and checks them together: the
/// default catalog has every key, a message is plural in every catalog or
/// in none, and a translation uses only the placeholders of the default
/// message (and `{count}` in a plural one). `hash` identifies the input.
pub fn build(locales: &[String], default: &str, texts: &[&str], hash: [u8; 32]) -> Result<Catalogs, Vec<CatalogError>> {
    let mut errors = Vec::new();
    let parsed: Vec<Parsed> =
        texts.iter().enumerate().map(|(i, t)| parse_catalog(i, &locales[i], t, &mut errors)).collect();
    let d = locales.iter().position(|l| l == default).unwrap_or(0);
    let base = &parsed[d];
    for (i, p) in parsed.iter().enumerate() {
        if i == d {
            continue;
        }
        let mut keys: Vec<&String> = p.messages.keys().collect();
        keys.sort_by_key(|k| p.spans[*k].start);
        for key in keys {
            let span = p.spans[key].clone();
            let m = &p.messages[key];
            let Some(dm) = base.messages.get(key) else {
                errors.push(CatalogError {
                    catalog: i,
                    span,
                    message: format!("`{key}` is not in the catalog of the default locale `{default}`"),
                });
                continue;
            };
            let plural = matches!(m, Message::Plural(_));
            if plural != matches!(dm, Message::Plural(_)) {
                let what = if plural { "plural" } else { "not plural" };
                errors.push(CatalogError {
                    catalog: i,
                    span,
                    message: format!("`{key}` is {what} here, and not so in the catalog of `{default}`"),
                });
                continue;
            }
            let known = placeholders(dm);
            if let Some(extra) = placeholders(m).into_iter().find(|n| !known.contains(n) && !(plural && n == "count")) {
                errors.push(CatalogError {
                    catalog: i,
                    span,
                    message: format!(
                        "`{key}` uses the placeholder `{{{extra}}}`, which the message of `{default}` does not"
                    ),
                });
            }
        }
    }
    if !errors.is_empty() {
        errors.sort_by_key(|e| (e.catalog, e.span.start));
        errors.truncate(MAX_ERRORS);
        return Err(errors);
    }
    // The order of `[i18n] locales`; each falls back to the listed locales
    // its tag shortens to, then to the default.
    let catalogs = parsed
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let mut fallback: Vec<u32> = parents(&locales[i])
                .into_iter()
                .filter_map(|t| locales.iter().position(|l| l == t))
                .map(|j| j as u32)
                .collect();
            if i != d && !fallback.contains(&(d as u32)) {
                fallback.push(d as u32);
            }
            Catalog { locale: locales[i].clone(), fallback, messages: p.messages }
        })
        .collect();
    let mut c = Catalogs { catalogs, hash };
    // The default first (`catalogs[0]`), the others in the order listed.
    let moved = |j: u32| match (j as usize).cmp(&d) {
        std::cmp::Ordering::Less => j + 1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => j,
    };
    let default_catalog = c.catalogs.remove(d);
    c.catalogs.insert(0, default_catalog);
    for cat in &mut c.catalogs {
        for f in &mut cat.fallback {
            *f = moved(*f);
        }
    }
    Ok(c)
}

/// What the type checker learns of the default catalog: each key's kind and
/// placeholders.
pub fn message_index(c: &Catalogs) -> HashMap<String, MessageInfo> {
    let Some(d) = c.catalogs.first() else { return HashMap::new() };
    d.messages
        .iter()
        .map(|(k, m)| (k.clone(), MessageInfo { plural: matches!(m, Message::Plural(_)), params: placeholders(m) }))
        .collect()
}

/// Catalogs built in this process, by `Catalogs::hash`: the pages of a site
/// share one copy while any of them holds it.
#[allow(clippy::type_complexity)]
static INTERNED: LazyLock<Mutex<HashMap<[u8; 32], Weak<Catalogs>>>> = LazyLock::new(Default::default);

/// The catalogs of `hash` built before in this process and still held, or
/// those `make` builds (then kept for the next time).
pub fn intern<E>(hash: [u8; 32], make: impl FnOnce() -> Result<Catalogs, E>) -> Result<Arc<Catalogs>, E> {
    if let Some(c) = INTERNED.lock().unwrap_or_else(|e| e.into_inner()).get(&hash).and_then(Weak::upgrade) {
        return Ok(c);
    }
    let mut c = make()?;
    c.hash = hash;
    Ok(share(Arc::new(c)))
}

/// The copy of `c` this process holds already (the same `hash`), or `c`, then
/// kept: catalogs read from the bytecode cache or a built executable are
/// shared like those built here.
pub fn share(c: Arc<Catalogs>) -> Arc<Catalogs> {
    let mut kept = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|_, w| w.strong_count() > 0);
    if let Some(old) = kept.get(&c.hash).and_then(Weak::upgrade) {
        return old;
    }
    kept.insert(c.hash, Arc::downgrade(&c));
    c
}

// ----- at run time -----------------------------------------------------------------

fn catalogs(vm: &Vm) -> Result<Arc<Catalogs>, VmError> {
    vm.catalogs()
        .cloned()
        .ok_or_else(|| vm.throw("RuntimeException", "std.i18n: this program has no catalogs ([i18n] in phv.toml)"))
}

fn locales_of(c: &Catalogs) -> Vec<String> {
    c.catalogs.iter().map(|c| c.locale.clone()).collect()
}

/// The locale of the request: set by `set_locale()`, or the best for its
/// `Accept-Language` (several fields are read as one), kept for the rest of
/// the request.
fn request_locale(vm: &mut Vm) -> Result<String, VmError> {
    if let Some(l) = &vm.http.locale {
        return Ok(l.clone());
    }
    let c = catalogs(vm)?;
    let locales = locales_of(&c);
    let header: Vec<&str> = vm
        .http
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("accept-language"))
        .map(|(_, v)| v.as_str())
        .collect();
    let chosen = negotiate(&header.join(","), &locales, &locales[0]).to_string();
    vm.http.locale = Some(chosen.clone());
    Ok(chosen)
}

/// The index of the catalog of `locale` (as listed), or ValueError.
fn catalog_index(vm: &Vm, c: &Catalogs, locale: &str) -> Result<usize, VmError> {
    c.index_of(locale).ok_or_else(|| {
        vm.throw("ValueError", format!("`{locale}` is not one of the locales of [i18n] ({})", locales_of(c).join(", ")))
    })
}

/// The message `key` for the catalog `i`: from it, or from the first of its
/// fallbacks that has it, with the index of the catalog it came from.
fn find<'a>(c: &'a Catalogs, i: usize, key: &str) -> Option<(usize, &'a Message)> {
    let cat = &c.catalogs[i];
    if let Some(m) = cat.messages.get(key) {
        return Some((i, m));
    }
    cat.fallback.iter().find_map(|&j| c.catalogs[j as usize].messages.get(key).map(|m| (j as usize, m)))
}

/// The parameters of a call (a `map<string, string>` or `map<string, Html>`).
fn params<'a>(vm: &Vm, v: &'a Value) -> Result<&'a phv_vm::Map, VmError> {
    let m: &phv_vm::Map = map(v);
    if m.len() > MAX_PARAMS {
        return Err(vm.throw("ValueError", format!("a message takes at most {MAX_PARAMS} parameters")));
    }
    Ok(m)
}

/// Writes `pieces` with the values of `given` (escaped text and `Html`
/// values when `html`), and `count` for a `{count}` not given.
fn render(
    vm: &Vm,
    key: &str,
    pieces: &[Piece],
    given: &phv_vm::Map,
    count: Option<&str>,
    html: bool,
) -> Result<String, VmError> {
    let mut size = 0;
    for p in pieces {
        size += match p {
            Piece::Text(t) if html => t.len() * 6,
            Piece::Text(t) => t.len(),
            Piece::Param(n) => {
                given.get(&Key::Str(Rc::new(n.clone()))).map_or(count.map_or(0, str::len), |v| s(v).len())
            }
        };
    }
    vm.reserve(size)?;
    let mut out = String::with_capacity(size);
    for p in pieces {
        match p {
            Piece::Text(t) if html => phv_vm::value::html_escape_into(t, &mut out),
            Piece::Text(t) => out.push_str(t),
            Piece::Param(n) => match (given.get(&Key::Str(Rc::new(n.clone()))), count) {
                (Some(v), _) => out.push_str(s(v)),
                (None, Some(c)) if n == "count" => out.push_str(c),
                (None, _) => {
                    return Err(vm.throw("ValueError", format!("the message `{key}` needs the parameter `{n}`")));
                }
            },
        }
    }
    Ok(out)
}

/// `t()`, `tn()` (with `count`) and `t_html()` (with `html`): the message
/// `key` for `locale`, with the parameters `given`. A key in no catalog
/// gives the key itself (escaped for `t_html()`).
fn translate(
    vm: &mut Vm,
    locale: &str,
    key: &str,
    count: Option<i64>,
    given: &Value,
    html: bool,
) -> Result<String, VmError> {
    let c = catalogs(vm)?;
    let i = catalog_index(vm, &c, locale)?;
    let given = params(vm, given)?;
    let Some((from, m)) = find(&c, i, key) else {
        vm.reserve(key.len() * if html { 6 } else { 1 })?;
        return Ok(if html { phv_vm::value::html_escape(key) } else { key.to_string() });
    };
    match (m, count) {
        (Message::Text(p), None) => render(vm, key, p, given, None, html),
        (Message::Plural(forms), Some(n)) => {
            let loc = &c.catalogs[from].locale;
            let rules = plural_rules(loc).map_or("", |(r, _)| r);
            let cat = category(rules, n);
            let pieces = forms.iter().find(|(f, _)| *f == cat).or_else(|| forms.iter().find(|(f, _)| *f == OTHER));
            let Some((_, pieces)) = pieces else { return Ok(key.to_string()) };
            let shown = format_int(format_of(loc), n, 0);
            render(vm, key, pieces, given, Some(&shown), html)
        }
        (Message::Plural(_), None) => {
            Err(vm.throw("ValueError", format!("the message `{key}` is plural: use tn() with a count")))
        }
        (Message::Text(_), Some(_)) => {
            Err(vm.throw("ValueError", format!("the message `{key}` is not plural: use t()")))
        }
    }
}

fn decimals(vm: &Vm, v: &Value) -> Result<usize, VmError> {
    let d = int(v);
    if !(0..=MAX_DECIMALS).contains(&d) {
        return Err(vm.throw("ValueError", format!("decimals must be 0 to {MAX_DECIMALS}")));
    }
    Ok(d as usize)
}

/// The style of a date: `full`, `long`, `medium` or `short`.
fn date_style(vm: &Vm, style: &str) -> Result<usize, VmError> {
    ["full", "long", "medium", "short"].iter().position(|s| *s == style).ok_or_else(|| {
        vm.throw(
            "ValueError",
            format!("unknown date style `{style}` (use \"full\", \"long\", \"medium\" or \"short\")"),
        )
    })
}

pub(crate) fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__i18n_locales" => |vm, _| {
            let c = catalogs(vm)?;
            Ok(Value::list(c.catalogs.iter().map(|c| Value::str(c.locale.as_str())).collect()))
        },
        "__i18n_locale" => |vm, _| Ok(Value::str(request_locale(vm)?)),
        // __i18n_set_locale(tag): whether it is listed (it is then the request's).
        "__i18n_set_locale" => |vm, a| {
            let c = catalogs(vm)?;
            let locales = locales_of(&c);
            match listed(&locales, s(&a[0])) {
                Some(l) => {
                    vm.http.locale = Some(l.to_string());
                    Ok(Value::Bool(true))
                }
                None => Ok(Value::Bool(false)),
            }
        },
        // __i18n_check(tag): the listed spelling of a locale, or ValueError.
        "__i18n_check" => |vm, a| {
            let c = catalogs(vm)?;
            let locales = locales_of(&c);
            match listed(&locales, s(&a[0])) {
                Some(l) => Ok(Value::str(l)),
                None => Err(vm.throw(
                    "ValueError",
                    format!("`{}` is not one of the locales of [i18n] ({})", s(&a[0]), locales.join(", ")),
                )),
            }
        },
        "__i18n_negotiate" => |vm, a| {
            let c = catalogs(vm)?;
            let locales = locales_of(&c);
            Ok(Value::str(negotiate(s(&a[0]), &locales, &locales[0])))
        },
        "__i18n_t" => |vm, a| {
            let (locale, key) = (s(&a[0]).to_string(), s(&a[1]).to_string());
            Ok(Value::str(translate(vm, &locale, &key, None, &a[2], false)?))
        },
        "__i18n_tn" => |vm, a| {
            let (locale, key) = (s(&a[0]).to_string(), s(&a[1]).to_string());
            Ok(Value::str(translate(vm, &locale, &key, Some(int(&a[2])), &a[3], false)?))
        },
        "__i18n_html" => |vm, a| {
            let (locale, key) = (s(&a[0]).to_string(), s(&a[1]).to_string());
            Ok(Value::html(translate(vm, &locale, &key, None, &a[2], true)?))
        },
        "__i18n_has" => |vm, a| {
            let c = catalogs(vm)?;
            let i = catalog_index(vm, &c, s(&a[0]))?;
            Ok(Value::Bool(find(&c, i, s(&a[1])).is_some()))
        },
        "__i18n_int" => |vm, a| {
            let d = decimals(vm, &a[2])?;
            Ok(Value::str(format_int(format_of(s(&a[0])), int(&a[1]), d)))
        },
        "__i18n_float" => |vm, a| {
            let d = decimals(vm, &a[2])?;
            Ok(Value::str(format_float(format_of(s(&a[0])), super::float(&a[1]), d)))
        },
        "__i18n_percent" => |vm, a| {
            let d = decimals(vm, &a[2])?;
            let f = format_of(s(&a[0]));
            let mut out = format_float(f, super::float(&a[1]) * 100.0, d);
            out.push_str(f.percent);
            Ok(Value::str(out))
        },
        // __i18n_date(locale, timestamp, zone, style)
        "__i18n_date" => |vm, a| {
            use chrono::Datelike;
            let style = date_style(vm, s(&a[3]))?;
            let tz = super::timez::zone(vm, s(&a[2]))?;
            let t = super::timez::instant(vm, int(&a[1]), tz)?;
            let fields = (t.year() as i64, t.month(), t.day(), t.weekday().number_from_monday());
            Ok(Value::str(format_date(format_of(s(&a[0])), style, fields)))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn locale_tags() {
        for ok in ["en", "pt-BR", "zh-Hant", "zh-Hant-TW", "es-419", "ast"] {
            assert!(valid_locale(ok), "{ok}");
        }
        for bad in ["", "EN", "en_US", "en-us", "e", "engl", "zh-hant", "en-US-x", "en--US", "pt-BR ", "../x"] {
            assert!(!valid_locale(bad), "{bad}");
        }
    }

    #[test]
    fn accept_language() {
        let l = locs(&["en", "ja", "pt-BR", "de"]);
        assert_eq!(negotiate("ja,en;q=0.8", &l, "en"), "ja");
        assert_eq!(negotiate("fr-CH, fr;q=0.9, de;q=0.7, *;q=0.5", &l, "en"), "de");
        assert_eq!(negotiate("pt-PT,pt;q=0.9", &l, "en"), "pt-BR");
        assert_eq!(negotiate("de-AT", &l, "en"), "de");
        assert_eq!(negotiate("JA-jp", &l, "en"), "ja");
        assert_eq!(negotiate("en;q=0.1, ja;q=0.9", &l, "en"), "ja");
        // Equal q: the order written.
        assert_eq!(negotiate("de;q=0.5, ja;q=0.5", &l, "en"), "de");
        // q=0 refuses a locale, also one a later entry would match.
        assert_eq!(negotiate("ja;q=0, *", &l, "en"), "en");
        assert_eq!(negotiate("pt;q=0.9, pt-BR;q=0", &l, "en"), "en");
        // Malformed entries are skipped, never an error.
        assert_eq!(negotiate("ja;q=2, de;q=0.5", &l, "en"), "de");
        assert_eq!(negotiate("ja;q=0.1234, de;q=abc, pt-BR;q=1.5", &l, "en"), "en");
        assert_eq!(negotiate("<script>, ja", &l, "en"), "ja");
        assert_eq!(negotiate("", &l, "en"), "en");
        assert_eq!(negotiate(",,,;;;q=", &l, "en"), "en");
        assert_eq!(negotiate("xx-YY", &l, "en"), "en");
        // Never an unlisted locale.
        assert_eq!(negotiate("fr", &l, "en"), "en");
    }

    #[test]
    fn accept_language_limits() {
        let l = locs(&["en", "ja"]);
        // An entry past MAX_ACCEPT_ENTRIES is not read.
        let many = format!("{}ja", "xx,".repeat(MAX_ACCEPT_ENTRIES));
        assert_eq!(negotiate(&many, &l, "en"), "en");
        let fits = format!("{}ja", "xx,".repeat(MAX_ACCEPT_ENTRIES - 1));
        assert_eq!(negotiate(&fits, &l, "en"), "ja");
        // Past MAX_ACCEPT bytes, at the last comma before them.
        let long = format!("{},ja", "x".repeat(MAX_ACCEPT));
        assert_eq!(negotiate(&long, &l, "en"), "en");
        let tag = format!("ja,{}", "a-".repeat(MAX_ACCEPT));
        assert_eq!(negotiate(&tag, &l, "en"), "ja");
        // A tag longer than MAX_ACCEPT_TAG is skipped.
        let t = format!("ja-{}", "x".repeat(MAX_ACCEPT_TAG));
        assert_eq!(negotiate(&t, &l, "en"), "en");
        // Multi-byte text at the cut does not panic.
        let wide = format!("{},ja", "é".repeat(MAX_ACCEPT));
        assert_eq!(negotiate(&wide, &l, "en"), "en");
    }

    /// The sample integers of each category in CLDR 49's `plurals.json`.
    #[test]
    fn plural_rules_match_cldr_samples() {
        let cases: &[(&str, u8, &[i64])] = &[
            ("en", ONE, &[1]),
            ("en", OTHER, &[0, 2, 3, 16, 100, 1000, 1000000]),
            ("de", ONE, &[1]),
            ("de", OTHER, &[0, 2, 17, 100]),
            ("ja", OTHER, &[0, 1, 2, 15, 100]),
            ("fr", ONE, &[0, 1]),
            ("fr", MANY, &[1000000, 2000000, 10000000]),
            ("fr", OTHER, &[2, 3, 17, 100, 1000, 10000, 100000]),
            ("pt", ONE, &[0, 1]),
            ("pt", MANY, &[1000000]),
            ("pt", OTHER, &[2, 17, 100, 1000]),
            ("pt-PT", ONE, &[1]),
            ("pt-PT", OTHER, &[0, 2, 16, 100]),
            ("pt-PT", MANY, &[1000000]),
            ("es", ONE, &[1]),
            ("es", MANY, &[1000000]),
            ("es", OTHER, &[0, 2, 16, 100, 1000]),
            ("it", ONE, &[1]),
            ("it", MANY, &[1000000]),
            ("it", OTHER, &[0, 2, 16, 100]),
            ("ru", ONE, &[1, 21, 31, 41, 51, 61, 71, 81, 101, 1001]),
            ("ru", FEW, &[2, 3, 4, 22, 23, 24, 32, 33, 34, 42, 102, 1002]),
            ("ru", MANY, &[0, 5, 6, 10, 11, 12, 13, 14, 15, 19, 100, 1000, 10000, 100000, 1000000]),
            ("pl", ONE, &[1]),
            ("pl", FEW, &[2, 3, 4, 22, 23, 24, 32, 33, 34, 42, 102, 1002]),
            ("pl", MANY, &[0, 5, 6, 11, 12, 13, 14, 19, 21, 100, 1000, 10000, 100000, 1000000]),
            ("ar", ZERO, &[0]),
            ("ar", ONE, &[1]),
            ("ar", TWO, &[2]),
            ("ar", FEW, &[3, 4, 9, 10, 103, 104, 110, 1003]),
            ("ar", MANY, &[11, 12, 26, 99, 111, 1011]),
            ("ar", OTHER, &[100, 101, 102, 200, 201, 202, 300, 1000, 10000, 100000, 1000000]),
        ];
        for (locale, cat, samples) in cases {
            let (rules, cats) = plural_rules(locale).unwrap();
            assert!(cats.contains(cat), "{locale} has {}", CATEGORIES[*cat as usize]);
            for n in *samples {
                assert_eq!(category(rules, *n), *cat, "{locale} {n}");
                // The rules are those of the number's absolute value.
                assert_eq!(category(rules, -*n), *cat, "{locale} -{n}");
            }
        }
        assert_eq!(category("ru", i64::MIN), category("ru", 8));
        assert_eq!(plural_rules("pt-BR").unwrap().0, "pt");
        assert_eq!(plural_rules("zh-Hant-TW").unwrap().0, "zh");
        assert_eq!(plural_rules("en-GB").unwrap().0, "en");
        assert!(plural_rules("nl").is_none());
    }

    #[test]
    fn numbers() {
        let f = format_of;
        assert_eq!(format_float(f("en"), 1234567.891, 2), "1,234,567.89");
        assert_eq!(format_float(f("de"), 1234567.891, 2), "1.234.567,89");
        assert_eq!(format_float(f("fr"), 1234.5, 1), "1\u{202F}234,5");
        assert_eq!(format_float(f("ru"), -1234.5, 0), "-1\u{A0}235");
        // minimumGroupingDigits 2: four digits are not grouped.
        assert_eq!(format_int(f("es"), 1234, 0), "1234");
        assert_eq!(format_int(f("es"), 12345, 0), "12.345");
        assert_eq!(format_int(f("pl"), -12345, 2), "-12\u{A0}345,00");
        assert_eq!(format_int(f("ja"), i64::MIN, 0), "-9,223,372,036,854,775,808");
        assert_eq!(format_int(f("ar"), -5, 0), "\u{200E}-5");
        assert_eq!(format_float(f("en"), -0.004, 2), "0.00");
        assert_eq!(format_float(f("en"), 0.5, 0), "1");
        assert_eq!(format_float(f("en"), f64::NAN, 2), "NaN");
        assert_eq!(format_float(f("de"), f64::NEG_INFINITY, 2), "-∞");
        assert!(format_float(f("en"), 1e300, 20).starts_with("1,000,000,000,000,000,0"));
        // pt-BR has no entry: pt's. A locale with none: CLDR root.
        assert_eq!(format_float(f("pt-BR"), 1234.5, 1), "1.234,5");
        assert_eq!(format_float(f("nl"), 1234.5, 1), "1,234.5");
    }

    #[test]
    fn dates() {
        let d = (2026, 11, 5, 4);
        let at = |l: &str, s: usize| format_date(format_of(l), s, d);
        assert_eq!(at("en", 0), "Thursday, November 5, 2026");
        assert_eq!(at("en", 2), "Nov 5, 2026");
        assert_eq!(at("en", 3), "11/5/26");
        assert_eq!(at("en-GB", 1), "5 November 2026");
        assert_eq!(at("fr", 0), "jeudi 5 novembre 2026");
        assert_eq!(at("de", 1), "5. November 2026");
        assert_eq!(at("es", 1), "5 de noviembre de 2026");
        assert_eq!(at("pt-BR", 2), "5 de nov. de 2026");
        assert_eq!(at("pt-PT", 2), "05/11/2026");
        assert_eq!(at("ru", 1), "5 ноября 2026 г.");
        assert_eq!(at("pl", 1), "5 listopada 2026");
        assert_eq!(at("ja", 0), "2026年11月5日木曜日");
        assert_eq!(at("zh", 1), "2026年11月5日");
        assert_eq!(at("ko", 1), "2026년 11월 5일");
        assert_eq!(at("ar", 3), "5\u{200F}/11\u{200F}/2026");
        assert_eq!(at("nl", 0), "2026-11-05");
        assert_eq!(format_date(format_of("en"), 0, (2026, 11, 8, 7)), "Sunday, November 8, 2026");
        assert_eq!(format_date(format_of("de"), 3, (5, 1, 2, 1)), "02.01.05");
    }

    #[test]
    fn messages() {
        assert_eq!(
            parse_message("Hi {name}, {{x}} {name}").unwrap(),
            vec![
                Piece::Text("Hi ".into()),
                Piece::Param("name".into()),
                Piece::Text(", {x} ".into()),
                Piece::Param("name".into())
            ]
        );
        assert_eq!(parse_message("").unwrap(), vec![Piece::Text(String::new())]);
        assert_eq!(parse_message("こんにちは {n}").unwrap()[0], Piece::Text("こんにちは ".into()));
        for bad in ["{", "}", "{1x}", "{a b}", "{}", "{name", "a } b", "{<b>}"] {
            assert!(parse_message(bad).is_err(), "{bad}");
        }
        let many: String = (0..=MAX_PLACEHOLDERS).map(|i| format!("{{p{i}}}")).collect();
        assert!(parse_message(&many).unwrap_err().contains("placeholders"));
        assert!(parse_message(&"x".repeat(MAX_MESSAGE)).is_ok());
        assert!(parse_message(&"x".repeat(MAX_MESSAGE + 1)).is_err());
        assert!(parse_message(&format!("{{{}}}", "a".repeat(MAX_PARAM_NAME + 1))).is_err());
    }

    fn build_of(cats: &[(&str, &str)]) -> Result<Catalogs, Vec<CatalogError>> {
        let locales: Vec<String> = cats.iter().map(|(l, _)| l.to_string()).collect();
        let texts: Vec<&str> = cats.iter().map(|(_, t)| *t).collect();
        build(&locales, &locales[0], &texts, [0; 32])
    }

    fn first_error(cats: &[(&str, &str)]) -> String {
        build_of(cats).unwrap_err()[0].message.clone()
    }

    #[test]
    fn catalogs_are_checked() {
        let c = build_of(&[
            ("en", "hello = \"Hello, {name}\"\n[cart.items]\none = \"{count} item\"\nother = \"{count} items\"\n"),
            ("ja", "hello = \"こんにちは、{name}さん\"\n[cart.items]\nother = \"{count} 個\"\n"),
        ])
        .unwrap();
        assert_eq!(c.catalogs[1].fallback, vec![0]);
        let idx = message_index(&c);
        assert_eq!(idx["hello"], MessageInfo { plural: false, params: vec!["name".into()] });
        assert!(idx["cart.items"].plural);

        assert!(first_error(&[("en", "a = ")]).contains("not valid TOML"));
        assert!(first_error(&[("en", "a = 1")]).contains("a message is a string"));
        assert!(first_error(&[("en", "a = [\"x\"]")]).contains("a message is a string"));
        assert!(first_error(&[("en", "\"a b\" = \"x\"")]).contains("a key is made of"));
        assert!(first_error(&[("en", "a = \"{oops\"")]).contains("placeholder"));
        assert!(first_error(&[("en", "[a]\none = \"x\"")]).contains("no plural form `other`"));
        assert!(first_error(&[("ja", "[a]\none = \"x\"\nother = \"y\"")]).contains("has no plural form `one`"));
        assert!(first_error(&[("ru", "[a]\none = \"x\"\nother = \"y\"")]).contains("`few`, `many`"));
        assert!(first_error(&[("nl", "[a]\none = \"x\"\nother = \"y\"")]).contains("no plural rules for `nl`"));
        assert!(first_error(&[("en", "[a.one]\nx = \"y\"\n[a.other]\nx = \"z\"")]).contains("forms do not nest"));
        assert!(first_error(&[("en", "a = \"x\""), ("ja", "b = \"y\"")]).contains("not in the catalog of the default"));
        assert!(first_error(&[("en", "a = \"x\""), ("ja", "a.other = \"y\"")]).contains("is plural here"));
        assert!(first_error(&[("en", "a = \"{x}\""), ("ja", "a = \"{y}\"")]).contains("placeholder `{y}`"));
        // A translation may leave a placeholder out, and a plural one use {count}.
        assert!(build_of(&[
            ("en", "a = \"{x} {y}\"\nb.one = \"one\"\nb.other = \"{n}\""),
            ("ja", "a = \"{y}\"\nb.other = \"{count} {n}\"")
        ])
        .is_ok());
        // An empty catalog, and an empty table, are fine.
        assert!(build_of(&[("en", ""), ("ja", "[empty]\n")]).is_ok());
    }

    #[test]
    fn catalog_limits() {
        let deep = format!("{} = \"x\"", ["a"; MAX_KEY_DEPTH + 1].join("."));
        assert!(first_error(&[("en", &deep)]).contains("at most 8 parts"));
        let ok = format!("{} = \"x\"", ["a"; MAX_KEY_DEPTH].join("."));
        assert!(build_of(&[("en", &ok)]).is_ok());
        let long = format!("{} = \"x\"", "k".repeat(MAX_KEY + 1));
        assert!(first_error(&[("en", &long)]).contains("at most 200 bytes"));
        let fits = format!("{} = \"x\"", "k".repeat(MAX_KEY));
        assert!(build_of(&[("en", &fits)]).is_ok());
        let msg = format!("a = \"{}\"", "x".repeat(MAX_MESSAGE + 1));
        assert!(first_error(&[("en", &msg)]).contains("at most 8192 bytes"));
        let many: String = (0..=MAX_MESSAGES).map(|i| format!("k{i} = \"x\"\n")).collect();
        assert!(first_error(&[("en", &many)]).contains("at most 20000 messages"));
        let max: String = (0..MAX_MESSAGES).map(|i| format!("k{i} = \"x\"\n")).collect();
        assert_eq!(build_of(&[("en", &max)]).unwrap().catalogs[0].messages.len(), MAX_MESSAGES);
        let big = format!("# {}", "x".repeat(MAX_CATALOG_BYTES));
        assert!(first_error(&[("en", &big)]).contains("at most 2 MB"));
        // Errors are reported up to MAX_ERRORS.
        let bad: String = (0..100).map(|i| format!("k{i} = 1\n")).collect();
        assert_eq!(build_of(&[("en", &bad)]).unwrap_err().len(), MAX_ERRORS);
        // Deeply nested TOML is refused by the parser, without a crash.
        let nested = format!("a = {}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(build_of(&[("en", &nested)]).is_err());
        let tables = format!("a = {}", "{ b = ".repeat(10_000));
        assert!(build_of(&[("en", &tables)]).is_err());
    }

    #[test]
    fn fallbacks() {
        let locales = locs(&["ja", "pt", "pt-BR", "en"]);
        let c = build(&locales, "en", &["", "a = \"pt\"", "", "a = \"en\"\nb = \"en b\""], [0; 32]).unwrap();
        // The default first, then the others in order.
        let order: Vec<&str> = c.catalogs.iter().map(|c| c.locale.as_str()).collect();
        assert_eq!(order, ["en", "ja", "pt", "pt-BR"]);
        assert_eq!(c.catalogs[3].fallback, vec![2, 0]);
        let br = c.index_of("pt-BR").unwrap();
        assert_eq!(find(&c, br, "a").unwrap().0, c.index_of("pt").unwrap());
        assert_eq!(find(&c, br, "b").unwrap().0, 0);
        assert!(find(&c, br, "c").is_none());
        assert_eq!(c.catalogs[c.index_of("ja").unwrap()].fallback, vec![0]);
    }

    #[test]
    fn catalogs_are_shared() {
        let a = intern::<()>([7; 32], || Ok(Catalogs::default())).unwrap();
        let b = intern::<()>([7; 32], || panic!("built again")).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        drop((a, b));
        // Once no program holds them, they are built again.
        let c = intern::<()>([7; 32], || Ok(Catalogs { catalogs: vec![Catalog::default()], ..Default::default() }));
        let c = c.unwrap();
        assert_eq!((c.hash, c.catalogs.len()), ([7; 32], 1));
        // Catalogs read from a cache are shared with those built here.
        let read = Arc::new(Catalogs { hash: [7; 32], ..Default::default() });
        assert!(Arc::ptr_eq(&share(read), &c));
    }
}
