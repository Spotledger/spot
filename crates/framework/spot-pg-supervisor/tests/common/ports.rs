//! Race-free test port allocation, shared by the supervisor and snapshot tests.
//!
//! A bare "bind :0, read the port, drop it" probe loses the port to any other process
//! that binds `:0` or opens an outgoing connection before the fake postmaster binds it
//! (parallel CI jobs on one runner host). So ports come from a range below the OS
//! ephemeral range (nothing hands those out implicitly), and each one is claimed with an
//! exclusive marker file in the shared temp dir, so no other test process picks it too.

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Below every OS ephemeral range (Linux 32768+, Windows/BSD 49152+).
const FIRST: u16 = 20_000;
const LAST: u16 = 30_000;
/// A claim older than this belongs to a finished test run and may be reused.
const CLAIM_TTL: Duration = Duration::from_secs(30 * 60);

/// The directory claims live in. It must be the same for every runner on the host: GitHub runners
/// each get their own `TMPDIR`, so `std::env::temp_dir()` would give each its own claims and
/// let two jobs pick one port. On Unix the shared `/tmp` is used when it exists.
fn claim_dir() -> PathBuf {
    let shared = PathBuf::from("/tmp");
    if cfg!(unix) && shared.is_dir() {
        return shared.join("spot-test-ports");
    }
    std::env::temp_dir().join("spot-test-ports")
}

/// Claims `port` for this process; `false` when another live claim holds it.
fn claim(port: u16) -> bool {
    let dir = claim_dir();
    let _ = std::fs::create_dir_all(&dir);
    let marker = dir.join(port.to_string());
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => return true,
            Err(_) => {
                let stale = std::fs::metadata(&marker)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age > CLAIM_TTL);
                if !stale || std::fs::remove_file(&marker).is_err() {
                    return false;
                }
            }
        }
    }
    false
}

/// A port nobody else will take: claimed across processes and bindable right now.
pub fn claimed_free_port() -> u16 {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        ^ (u64::from(std::process::id()) << 16);
    let span = u64::from(LAST - FIRST);
    for step in 0..span {
        let port = FIRST + ((seed.wrapping_add(step.wrapping_mul(7919))) % span) as u16;
        if !claim(port) {
            continue;
        }
        // Bindable now (nothing else listens on it); the claim keeps other tests off it.
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no unclaimed test port left in {FIRST}..{LAST}");
}
