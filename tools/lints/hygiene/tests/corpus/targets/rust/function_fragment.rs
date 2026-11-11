pub fn grade_group(records: &[i32], boundary: i32) -> i32 {
    let mut result = 0;
    for record in records {
        if *record > boundary && *record % 2 == 0 {
            result += *record;
        } else {
            result -= record.abs();
        }
    }
    if result > 100 {
        result / 2
    } else {
        result
    }
}
