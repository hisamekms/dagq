//! The backends that do the operations behind the [`crate::backend::Backend`]
//! trait. The fs backend is here; process and git are still
//! [`crate::backend::Unimplemented`].

pub mod fs;
