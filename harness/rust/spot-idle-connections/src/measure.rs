//! What the harness reads from the operating system: resident memory, open descriptors and the
//! descriptor limit. No `unsafe` and no extra dependency: Linux reads `/proc`, other systems ask
//! a standard tool, and a number that cannot be read is `None` (reported as `null`).
//!
//! The server runs in the harness's own process, so the resident size is that of the process:
//! server and client together. The clients hold one socket each and no task, so their share is a
//! small, constant amount per connection; the numbers are an upper bound on the server's cost.

/// Resident memory of this process in bytes, or `None` when the system cannot say.
#[must_use]
pub fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib.saturating_mul(1024))
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kib.saturating_mul(1024))
    }
    #[cfg(target_os = "windows")]
    {
        let script = format!("(Get-Process -Id {}).WorkingSet64", std::process::id());
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Open file descriptors of this process, or `None` when the system cannot say.
#[must_use]
pub fn open_descriptors() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let count = std::fs::read_dir("/proc/self/fd").ok()?.count();
        u64::try_from(count).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The soft `RLIMIT_NOFILE` of this process, or `None` when there is no such limit or it cannot
/// be read (Windows has none that applies). The harness reads it and never raises it.
#[must_use]
pub fn descriptor_limit() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
        let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
        let soft = line.split_whitespace().nth(3)?;
        if soft == "unlimited" {
            return None;
        }
        soft.parse().ok()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let out = std::process::Command::new("sh")
            .args(["-c", "ulimit -n"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_numbers_are_readable_and_plausible() {
        assert!(rss_bytes().is_some_and(|b| b > 1024 * 1024));
        assert!(open_descriptors().is_some_and(|n| n >= 3));
        assert!(descriptor_limit().is_some_and(|n| n >= 16));
    }
}
