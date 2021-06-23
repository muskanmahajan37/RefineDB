use lalrpop_util::lalrpop_mod;

pub mod ast;
pub mod codegen;
mod state;

lalrpop_mod!(pub language, "/data/treewalker/asm/language.rs");

use thiserror::Error;

#[derive(Error, Debug)]
pub enum TwAsmError {
  #[error("invalid literal")]
  InvalidLiteral,

  #[error("type unsupported in table")]
  TypeUnsupportedInTable,

  #[error("node not found: {0}")]
  NodeNotFound(String),

  #[error("identifier not found: {0}")]
  IdentifierNotFound(String),

  #[error("duplicate return")]
  DuplicateReturn,

  #[error("param not found: {0}")]
  ParamNotFound(String),

  #[error("duplicate param: {0}")]
  DuplicateParam(String),
}
