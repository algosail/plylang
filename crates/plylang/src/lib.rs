mod anf;
mod ast;
mod builtin;
mod build;
mod check;
mod codes;
mod complete;
mod definition;
mod diagnostic;
mod docs;
mod hover;
mod lexer;
mod parser;
mod paths;
mod span;
mod surface;
mod test_run;
mod token;

pub const LANGUAGE_VERSION: u32 = 1;

pub use ast::Module;
pub use build::{
    build, buffer_notes, build_exports, check_buffer, check_package, javascript_file, stack_sites,
    BufferNotes, BuildError,
};
pub use check::{check, javascript, StackAt};
pub use codes::Error;
pub use complete::{complete, Completion, CompletionKind};
pub use definition::{definition, Definition};
pub use diagnostic::Diagnostic;
pub use docs::write_docs;
pub use hover::{hover, hover_with_sites, Hover};
pub use parser::parse;
pub use paths::{mode_from_path, module_name_from_path, ParseMode};
pub use span::{byte_offset, utf16_position};
pub use test_run::test_package;
