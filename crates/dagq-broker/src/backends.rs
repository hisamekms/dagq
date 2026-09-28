//! The backends that do the operations behind the [`crate::backend::Backend`]
//! trait. fs and process are here; git is still
//! [`crate::backend::Unimplemented`].

pub mod fs;
pub mod process;
