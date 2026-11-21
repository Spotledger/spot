//! The `spot-store-server` binary: `spot-store-server serve --config <file>`.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(spot_store_server::run_cli(&args));
}
