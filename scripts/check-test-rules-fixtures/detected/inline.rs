pub fn real() {}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    fn a() { let _ = std::time::Instant::now(); }
    fn b() { let _ = std::time::SystemTime::now(); }
    fn c() { std::thread::sleep(std::time::Duration::from_millis(1)); }
    fn d() { let _ = std::process::Command::new("git"); }
    const W: &str = "while [ ! -f go ]; do sleep 1; done";
}

mod outer {
    pub fn x() {}
    #[cfg(test)]
    mod tests {
        fn e() { let _ = std::time::SystemTime::now(); }
    }
    pub fn y() { std::thread::sleep(std::time::Duration::ZERO); }
}
