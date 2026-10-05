//! The backends that do the operations behind the [`crate::backend::Backend`]
//! trait: fs, process, git and package.

pub mod fs;
pub mod git;
pub mod package;
pub mod process;
