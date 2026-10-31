// The helpers of the fixture's tests: few is used by one module, named
// through the name this file re-exports, and mod.rs itself by every module
// that names `common`.
pub mod few;
pub mod named;
pub use named::Named;

pub fn within() {}
