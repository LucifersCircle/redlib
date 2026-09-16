use std::time::Duration;

pub(crate) fn positive_jitter(base: Duration, max_extra: Duration) -> Duration {
	let max_millis = max_extra.as_millis().min(u128::from(u64::MAX)) as u64;
	if max_millis == 0 {
		return base;
	}
	positive_jitter_with_sample(base, max_extra, fastrand::u64(0..=max_millis))
}

pub(crate) fn proportional_positive_jitter(base: Duration) -> Duration {
	positive_jitter(base, base / 4)
}

fn positive_jitter_with_sample(base: Duration, max_extra: Duration, sample_millis: u64) -> Duration {
	let max_millis = max_extra.as_millis().min(u128::from(u64::MAX)) as u64;
	base.saturating_add(Duration::from_millis(sample_millis.min(max_millis)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn positive_jitter_never_shortens_and_respects_bounds() {
		let base = Duration::from_secs(20);
		let max_extra = Duration::from_secs(5);
		assert_eq!(positive_jitter_with_sample(base, max_extra, 0), base);
		assert_eq!(positive_jitter_with_sample(base, max_extra, 5_000), Duration::from_secs(25));
		assert_eq!(positive_jitter_with_sample(base, max_extra, 9_000), Duration::from_secs(25));
	}
}
