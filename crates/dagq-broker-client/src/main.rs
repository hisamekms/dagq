//! `dagq-broker-client`: kept thin; everything is in the library.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = dagq_broker_client::cli::run(
        &args,
        &|name| std::env::var(name).ok(),
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    std::process::exit(code);
}
