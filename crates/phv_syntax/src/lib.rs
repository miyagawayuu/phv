//! Source handling, lexer, parser and AST for PHV.

pub mod ast;
pub mod diag;
pub mod format;
pub mod htmlctx;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;

pub use diag::{Diagnostic, Severity};
pub use parser::{normalize_template_path, parse_module, parse_type_str};
pub use span::{SourceFile, SourceMap, Span};
