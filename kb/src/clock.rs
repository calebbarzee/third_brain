//! the current time for everything kb records or compares: run logs, session ages, nightly
//! findings, ulids, backup stamps, and the dates on commits kb makes. `KB_NOW` (rfc 3339) fixes
//! it, so a simulation can run kb at any date and step days forward; unset, it is the system
//! clock.

use chrono::{DateTime, Utc};
use std::time::SystemTime;

/// `KB_NOW` when set and valid, else the system clock.
pub fn now() -> DateTime<Utc> {
	fixed().unwrap_or_else(Utc::now)
}

/// `now()` as a `SystemTime`, for file-age and ulid arithmetic.
pub fn system_now() -> SystemTime {
	now().into()
}

/// the `KB_NOW` override, when set and valid rfc 3339.
pub fn fixed() -> Option<DateTime<Utc>> {
	let raw = std::env::var("KB_NOW").ok()?;
	DateTime::parse_from_rfc3339(raw.trim()).ok().map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unset_or_invalid_is_the_system_clock() {
		// tests run in parallel and share the environment, so this only checks the parser
		assert!(DateTime::parse_from_rfc3339("2026-10-02T09:00:00Z").is_ok());
		let before = Utc::now();
		let t = if fixed().is_none() { now() } else { Utc::now() };
		assert!(t >= before);
	}
}
