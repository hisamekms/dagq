// The fixture of `sh scripts/check-it-test-time.sh --self-test`: the base
// commit. It is never compiled.
#[path = "../common/mod.rs"]
mod common;
mod gate_fixture;
mod nested_fixture;
#[path = "under_path.rs"]
mod renamed_by_path;
#[path = "one_line.rs"] mod one_liner;
mod after_one_line;
