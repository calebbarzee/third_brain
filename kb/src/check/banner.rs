//! the fixed-template forward-pointer banner for superseded and archived documents.
//! contract in design.md §8: "no document leaves canon without one", written with no model involved.

use crate::model::Status;

/// render the banner line for `status` (superseded or archived), dated `date` (`YYYY-MM-DD`),
/// pointing at `target` (title, `kb:` uri) when there is a successor. `target = None` matches
/// `superseded_by = "none"`.
pub fn banner(status: Status, date: &str, target: Option<(&str, &str)>) -> String {
	let verb = match status {
		Status::Archived => "archived",
		_ => "superseded",
	};
	match target {
		Some((title, uri)) => format!("> {verb} {date} by [{title}]({uri}). kept for history; not current.\n"),
		None => format!("> {verb} {date}. kept for history; not current.\n"),
	}
}

/// true when the first non-blank line of the body is a superseded or archived banner.
pub fn has_banner(body: &str) -> bool {
	body.lines().map(str::trim).find(|l| !l.is_empty()).is_some_and(|l| l.starts_with("> superseded") || l.starts_with("> archived"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn banner_with_target() {
		let b = banner(Status::Superseded, "2026-09-14", Some(("token refresh moved to the retry path", "kb:01K8A")));
		assert_eq!(b, "> superseded 2026-09-14 by [token refresh moved to the retry path](kb:01K8A). kept for history; not current.\n");
		assert!(has_banner(&b));
	}

	#[test]
	fn banner_without_target() {
		let b = banner(Status::Archived, "2026-09-14", None);
		assert_eq!(b, "> archived 2026-09-14. kept for history; not current.\n");
		assert!(has_banner(&b));
	}

	#[test]
	fn has_banner_requires_first_line() {
		assert!(!has_banner("\n\ntext\n> superseded 2026-01-01. kept for history; not current.\n"));
		assert!(has_banner("\n> archived 2026-01-01. kept for history; not current.\n\nbody\n"));
		assert!(!has_banner("body only\n"));
	}
}
