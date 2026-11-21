fn warm_up(samples: &[i32]) -> i32 {
    samples.iter().sum()
}

pub fn assess_samples(values: &[i32], limit: i32) -> i32 {
    let mut total = 0;
    for value in values {
        if *value > limit && *value % 2 == 0 {
            total += *value;
        } else {
            total -= value.abs();
        }
    }
    if total > 100 {
        total / 2
    } else {
        total
    }
}

fn cool_down(score: i32) -> bool {
    score < 0
}
