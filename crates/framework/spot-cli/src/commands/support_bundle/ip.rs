//! IP-address redaction for log text (split out of the parent module to keep it small).

use std::net::{IpAddr, Ipv4Addr};

fn ip_char(c: char) -> bool {
    c.is_ascii_hexdigit() || c == ':' || c == '.'
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Replaces every IPv4 / IPv6 address in `text` with `[redacted:ip]`, keeping loopback
/// (`127.0.0.1`, `::1`) and unspecified (`0.0.0.0`, `::`) addresses readable. A candidate is a
/// maximal run of hex digits, `:` and `.`, standing alone (not glued to letters, digits or
/// `_`, so `std::io` and `v1.2.3` are left alone); an IPv4 address followed by `:port` is
/// handled.
pub(super) fn redact_ips(text: &str) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !ip_char(chars[i].1) {
            out.push(chars[i].1);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && ip_char(chars[i].1) {
            i += 1;
        }
        let byte_start = chars[start].0;
        let byte_end = chars.get(i).map_or(text.len(), |c| c.0);
        let run = &text[byte_start..byte_end];
        let before_ok = start == 0 || !word_char(chars[start - 1].1);
        let after_ok = chars.get(i).is_none_or(|c| !word_char(c.1));
        match (before_ok && after_ok).then(|| ip_prefix_len(run)).flatten() {
            Some((len, redact)) => {
                if redact {
                    out.push_str("[redacted:ip]");
                } else {
                    out.push_str(&run[..len]);
                }
                out.push_str(&run[len..]);
            }
            None => out.push_str(run),
        }
    }
    out
}

/// The length of the address at the start of `run` and whether it is to be redacted
/// (non-loopback, non-unspecified), allowing trailing `.`/`:` punctuation and an IPv4 `:port`.
fn ip_prefix_len(run: &str) -> Option<(usize, bool)> {
    let verdict = |ip: IpAddr| !(ip.is_loopback() || ip.is_unspecified());
    if let Ok(ip) = run.parse::<IpAddr>() {
        return Some((run.len(), verdict(ip)));
    }
    let trimmed = run.trim_end_matches(['.', ':']);
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Some((trimmed.len(), verdict(ip)));
    }
    if let Some(colon) = run.find(':')
        && let Ok(ip) = run[..colon].parse::<Ipv4Addr>()
    {
        return Some((colon, verdict(IpAddr::V4(ip))));
    }
    None
}
