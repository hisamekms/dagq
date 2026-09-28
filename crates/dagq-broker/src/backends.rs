//! The backends that do the operations behind the [`crate::backend::Backend`]
//! trait: fs, process and git.

pub mod fs;
pub mod git;
pub mod process;
