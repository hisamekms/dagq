#[cfg(test)]
mod tests {
    fn spawn() { let _ = std::process::Command::new("/bin/sh"); }
    fn wait() { std::thread::sleep(std::time::Duration::ZERO); }
    fn marked() { std::thread::sleep(std::time::Duration::ZERO); } // test-rules: allow A-262 the wait is the boundary under test
    fn next() { let _ = std::process::Command::new("true"); }
}
