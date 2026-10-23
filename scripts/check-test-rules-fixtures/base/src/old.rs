#[cfg(test)]
mod tests {
    fn a() { let a = 1; }

    #[test]
    fn t() {
        a();
        std::thread::sleep(std::time::Duration::ZERO);
        let _ = "while [ ! -f go ]; do sleep 1; done";
    }
}
