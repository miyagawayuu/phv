//! The message catalogs of a program that uses `std.i18n`: read and checked
//! by the loader when the program is compiled (`phv_std::i18n::load`), kept
//! in the program, and read by the builtins of `std.i18n` at run time.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// The catalogs of `[i18n]`: one per locale, the default one first.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalogs {
    pub catalogs: Vec<Catalog>,
    /// SHA-256 of what they were built from (the settings and the files), so
    /// that the pages of a process share one copy (`phv_std::i18n::intern`).
    pub hash: [u8; 32],
}

impl Catalogs {
    /// The catalog of `locale` (as written in `[i18n] locales`).
    pub fn index_of(&self, locale: &str) -> Option<usize> {
        self.catalogs.iter().position(|c| c.locale == locale)
    }
}

/// The messages of one locale.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// As written in `[i18n] locales` (`pt-BR`).
    pub locale: String,
    /// The catalogs looked in after this one, in order: the locales it falls
    /// back to (`pt-BR` to `pt`, when it is listed), then the default one.
    pub fallback: Vec<u32>,
    pub messages: HashMap<String, Message>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    Text(Vec<Piece>),
    /// The forms of a plural message, by CLDR category (0 zero, 1 one, 2 two,
    /// 3 few, 4 many, 5 other).
    Plural(Vec<(u8, Vec<Piece>)>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Piece {
    Text(String),
    /// `{name}`: the parameter `name`.
    Param(String),
}

/// The catalogs as a program holds them: shared by the pages of a process.
#[derive(Debug, Clone)]
pub struct Shared(pub Arc<Catalogs>);

impl Serialize for Shared {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Shared {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Shared, D::Error> {
        Catalogs::deserialize(d).map(|c| Shared(Arc::new(c)))
    }
}
