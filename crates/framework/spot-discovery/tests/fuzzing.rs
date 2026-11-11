//! The fuzz-the-decoder test: random byte sequences, never a panic. Runs the TXT
//! decoder (`Advertisement::from_txt`) and the manual parser (`parse_manual`) against
//! pseudo-random inputs built only from `std` (no RNG dependency is allowed), plus
//! the deterministic malformed-record table.

use spot_discovery::manual::parse_manual;
use spot_discovery::record::Advertisement;

/// xorshift64* — deterministic, tiny, no dependency.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

const KEYS: [&str; 12] = [
    "instance", "port", "pv", "tls", "fp", "ver", "site", "sitedesc", "tenant", "company", "db",
    "user",
];
const VALUES: [&str; 13] = [
    "srv-1",
    "8080",
    "1",
    "off",
    "self-signed",
    "provided",
    "quantum",
    "abcd",
    "ABCD",
    "",
    "-1",
    "99999999",
    "Acme Trading",
];

#[test]
fn decoder_never_panics_on_random_txt_pairs() {
    let mut rng = Xorshift(0x5EED_1234_ABCD_0001);
    for _case in 0..5_000u32 {
        let n = (rng.next() % 6) as usize;
        let mut txt: Vec<(String, String)> = Vec::with_capacity(n);
        for _ in 0..n {
            let k = KEYS[(rng.next() as usize) % KEYS.len()];
            let v = VALUES[(rng.next() as usize) % VALUES.len()];
            txt.push((k.to_string(), v.to_string()));
        }
        // The contract: any result at all, never a panic.
        let _ = Advertisement::from_txt(&txt);
    }
}

#[test]
fn decoder_never_panics_on_random_bytes_in_values() {
    let mut rng = Xorshift(0x00B5_ADC0_FFEE_0002);
    for _case in 0..5_000u32 {
        let n = (rng.next() % 4 + 1) as usize;
        let mut txt = Vec::with_capacity(n);
        for _ in 0..n {
            let k = KEYS[(rng.next() as usize) % KEYS.len()];
            let vlen = (rng.next() % 40) as usize;
            let mut v = String::new();
            for _ in 0..vlen {
                // Printable ASCII, plus occasionally a raw high byte.
                let byte = if rng.next().is_multiple_of(16) {
                    (rng.next() % 256) as u8
                } else {
                    0x20 + (rng.next() % 95) as u8
                };
                v.push(byte as char);
            }
            txt.push((k.to_string(), v));
        }
        let _ = Advertisement::from_txt(&txt);
    }
}

#[test]
fn manual_parser_never_panics_on_random_input() {
    let mut rng = Xorshift(0xFEED_FACE_0003_BEEF);
    let alphabet: Vec<char> = "0123456789abcdefABCDEFghijklmnop:.[\\]-_ "
        .chars()
        .collect();
    for _case in 0..5_000u32 {
        let len = (rng.next() % 30) as usize;
        let mut s = String::new();
        for _ in 0..len {
            s.push(alphabet[(rng.next() as usize) % alphabet.len()]);
        }
        // Any result, never a panic.
        let _ = parse_manual(&s);
    }
}

#[test]
fn deterministic_malformed_records_are_refused() {
    let base = vec![
        ("instance".to_string(), "srv-1".to_string()),
        ("port".to_string(), "8080".to_string()),
        ("pv".to_string(), "1".to_string()),
        ("tls".to_string(), "off".to_string()),
    ];
    let mut port_overflow = base.clone();
    port_overflow[1] = ("port".to_string(), "70000".to_string());
    assert!(matches!(
        Advertisement::from_txt(&port_overflow),
        Err(spot_discovery::DiscoveryError::RecordInvalid { .. })
    ));
    let mut pv_negative = base.clone();
    pv_negative[2] = ("pv".to_string(), "-1".to_string());
    assert!(matches!(
        Advertisement::from_txt(&pv_negative),
        Err(spot_discovery::DiscoveryError::RecordInvalid { .. })
    ));
    let mut duplicate_instance = base.clone();
    duplicate_instance.insert(1, ("instance".to_string(), "srv-2".to_string()));
    // Last wins deterministically; still decodes.
    let dup = Advertisement::from_txt(&duplicate_instance)
        .unwrap_or_else(|e| panic!("duplicate decode failed: {e:?}"));
    assert_eq!(dup.service_instance.as_str(), "srv-2");
}
