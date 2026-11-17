//! Prints the stub catalogue as JSON, the input shape of `contracts/management-api/check.py`:
//! `cargo run -p spot-mgmt-api --example catalogue_json`.

#![allow(missing_docs)]

fn main() {
    match spot_mgmt_api::stubs::stub_catalogue() {
        Some(catalogue) => {
            let value = spot_mgmt_api::view::catalogue_json(&catalogue);
            match serde_json::to_string_pretty(&value) {
                Ok(text) => println!("{text}"),
                Err(error) => {
                    eprintln!("could not encode the catalogue: {error}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            eprintln!("the stub catalogue did not build");
            std::process::exit(1);
        }
    }
}
