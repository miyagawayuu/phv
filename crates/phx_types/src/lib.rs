//! Type checker for PHX: resolves names, checks types and produces HIR.

pub mod builtins;
mod check;
pub mod hir;
pub mod query;
pub mod types;

pub use check::{check, CheckInput, ModuleInput, ModuleKind};
