fn f() {
    if cfg!(any(
        target_vendor = "unknown",
        target_os = "android"
    )) {}
}
