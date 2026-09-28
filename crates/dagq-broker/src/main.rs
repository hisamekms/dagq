//! `dagq-broker`: kept thin; everything is in the library.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = dagq_broker::run(&args, &mut std::io::stdout().lock()) {
        eprintln!("{message}");
        std::process::exit(2);
    }
}
