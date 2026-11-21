//! `spot-agent-priv`: the privileged helper. Invoked as `sudo -n spot-agent-priv run <name>`
//! with a JSON parameter line on stdin, or as `spot-agent-priv --print-sudoers --user U`.

use std::io::{Read as _, Write as _};

use spot_agent::privilege::exec::StdChildRunner;
use spot_agent::privilege::helper::{MAX_STDIN, run_helper};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stdin = String::new();
    if args.first().map(String::as_str) == Some("run") {
        let limit = u64::try_from(MAX_STDIN + 1).unwrap_or(u64::MAX);
        let _ = std::io::stdin().take(limit).read_to_string(&mut stdin);
    }
    let (code, out) = run_helper(&args, &stdin, &StdChildRunner);
    let _ = std::io::stdout().write_all(out.as_bytes());
    std::process::exit(code);
}
