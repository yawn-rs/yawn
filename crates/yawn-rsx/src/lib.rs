//! The `.rsx` compiler.
//!
//! An `.rsx` file is Rust plus `component` items whose `view` is declarative markup:
//!
//! ```text
//! use crate::stepper::Stepper;
//!
//! component Counter() {
//!     let count = signal(0);
//!     view {
//!         <button on:click={move || count.update(|c| *c += 1)}>"clicked {count} times"</button>
//!     }
//! }
//! ```
//!
//! [`compile`] turns one file into a Rust module (plus a [`SourceMap`] so rustc errors can be
//! shown at their `.rsx` location); [`project::compile_project`] does it for every `.rsx` in a
//! project and writes the generated modules. The output only calls the target-agnostic
//! `yawn::ui` runtime, so the same components can later be rendered on the server.

pub mod api;
pub mod ast;
pub mod codegen;
pub mod diag;
pub mod manifest;
pub mod parser;
pub mod project;
pub mod routes;
pub mod scan;
pub mod sourcemap;
pub mod split;
pub mod split_nodes;

pub use diag::Diagnostic;
pub use sourcemap::SourceMap;

/// Result of compiling one `.rsx` file
#[derive(Debug, Clone)]
pub struct Compiled {
    pub rust: String,
    pub map: SourceMap,
    /// CSS from the file's `style { }` blocks, one commented section per component
    pub css: String,
}

/// Compiles the text of one `.rsx` file
pub fn compile(source: &str, path: &str) -> Result<Compiled, Diagnostic> {
    compile_with(source, path, &codegen::Registry::new())
}

/// [`compile`] for a file of a project: `known` says which props of the project's components may be
/// left out, so that `<Switch />` can omit them
pub fn compile_with(
    source: &str,
    path: &str,
    known: &codegen::Registry,
) -> Result<Compiled, Diagnostic> {
    let file = parser::parse(source)?;
    let (rust, map) = codegen::generate(source, &file, path, known)?;
    let mut css = String::new();
    for item in &file.items {
        if let ast::Item::Component(c) = item
            && let Some(span) = &c.style
        {
            css.push_str(&format!("/* {path}: {} */\n{}\n", c.name, source[span.clone()].trim()));
        }
    }
    Ok(Compiled { rust, map, css })
}
