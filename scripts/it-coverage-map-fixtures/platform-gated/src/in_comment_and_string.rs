// #[cfg(target_os = "macos")] and cfg!(target_os = "linux") are in a comment.
/* #[cfg(target_os = "macos")] */
const TEXT: &str = "#[cfg(target_os = \"macos\")]";
const RAW: &str = r#"cfg!(target_os = "linux")"#;
const QUOTE: char = '"';
const NAME: &str = "target_os";
