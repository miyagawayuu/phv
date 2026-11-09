//! Type checker for PHV: resolves names, checks types and produces HIR.

pub mod builtins;
mod check;
pub mod hir;
pub mod index;
pub mod query;
pub mod types;

pub use check::{check, check_for_editor, CheckInput, MessageKey, Messages, ModuleInput, ModuleKind};
