//! `l3-http --write-cases` regenerates `CASES.md` from the case index. It never starts a fixture
//! or a database; the suite itself runs through `cargo test -p l3-http`.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--write-cases" {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("CASES.md");
        return match std::fs::write(&path, l3_http::index::cases_md()) {
            Ok(()) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("could not write {}: {error}", path.display());
                ExitCode::FAILURE
            }
        };
    }
    eprintln!(
        "usage: l3-http --write-cases\n\n  --write-cases  regenerate harness/rust/l3-http/CASES.md from the case index\n\nThe suite itself runs with `cargo test -p l3-http` (see harness/rust/l3-http/README.md)."
    );
    ExitCode::from(2)
}
