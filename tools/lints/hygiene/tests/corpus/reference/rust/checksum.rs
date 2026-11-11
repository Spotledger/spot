pub fn checksum(values: &[u8]) -> u32 {
    let mut result = 2166136261u32;
    for value in values {
        result ^= *value as u32;
        result = result.wrapping_mul(16777619);
    }
    result
}

pub fn has_marker(values: &[u8], marker: u8) -> bool {
    values.iter().any(|value| *value == marker)
}

pub fn count_odd(values: &[i32]) -> usize {
    values.iter().filter(|value| **value % 2 != 0).count()
}
