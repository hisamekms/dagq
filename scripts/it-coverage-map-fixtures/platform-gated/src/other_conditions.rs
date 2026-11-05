#[cfg(unix)]
fn on_unix() {}
#[cfg(target_vendor = "unknown")]
fn on_unknown() {}
fn target_os() -> bool {
    cfg!(test)
}
