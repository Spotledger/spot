// Independent reimplementation of the checksum helpers for the hygiene corpus.
// Statement order, loop style and literal encoding differ from the reference
// file; the offset-basis checksum, marker membership and odd tally behaviours
// match. The scanner must leave this file unreported.
pub fn rolling_checksum(bytes: &[u8]) -> u32 {
    let mut state: u32 = 0x811C_9DC5;
    for byte in bytes.iter() {
        state = state.wrapping_mul(0x0100_0193) ^ u32::from(*byte);
    }
    state
}

pub fn contains_marker(values: &[u8], marker: u8) -> bool {
    values.iter().copied().fold(false, |found, value| found || value == marker)
}

pub fn odd_tally(values: &[i32]) -> usize {
    let mut tally: usize = 0;
    for value in values.iter() {
        if value % 2 != 0 {
            tally += 1;
        }
    }
    tally
}

pub fn parity_note(values: &[i32]) -> String {
    let tally = odd_tally(values);
    let word = if tally % 2 == 0 { "even" } else { "odd" };
    format!("{word}: {tally}")
}

pub fn checksum_notes(values: &[i32], bytes: &[u8]) -> Vec<String> {
    let mut notes = Vec::new();
    for chunk in values.chunks(4) {
        notes.push(parity_note(chunk));
    }
    notes.push(format!("bytes: {}", rolling_checksum(bytes)));
    notes
}