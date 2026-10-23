pub fn now() -> std::time::Instant { std::time::Instant::now() }

#[cfg(all(test, unix))]
mod unix_tests {
    fn a() { let _ = std::time::Instant::now(); }
}

#[cfg(test)]
fn helper() { let _ = std::time::Instant::now(); }

#[cfg(test)]
mod tests {
    // std::thread::sleep(...) in a comment
    fn a() { super::helper(); }
    const W: &str = "await_file go; echo done";
    fn b() { let _ = std::time::Instant::now(); }
    // test-rules: allow A-262 the test checks the wait of the real clock
    fn c() { std::thread::sleep(std::time::Duration::ZERO); }
    fn d() { let _ = std::process::Command::new("true"); } // test-rules: allow A-262 a boundary named here
}

#[cfg(test)]
mod closed_with_a_comment {
    fn e() {}
} // mod closed_with_a_comment

pub fn after_the_comment() {
    let _ = std::time::SystemTime::now();
}

pub fn after() { std::thread::sleep(std::time::Duration::ZERO); }
