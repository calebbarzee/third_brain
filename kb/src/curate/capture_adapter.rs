//! adapter over the capture catalog (planning notes §5-6, design.md §25). `src/store::catalog`
//! landed (from a concurrently written module) with exactly the api this crate was written
//! against, so every function here is a thin, one-line forward: this module exists so every
//! caller in `curate` goes through one name (`capture_adapter::*`) rather than `crate::store`
//! directly, which would have made swapping in the real store, when it was still absent, a
//! multi-call-site change instead of a one-file one.
use crate::config::Config;
use crate::store::catalog::Capture;
use anyhow::Result;

pub fn get(cfg: &Config, id_or_prefix: &str) -> Result<Option<Capture>> {
	crate::store::catalog::get(cfg, id_or_prefix)
}

pub fn text(cfg: &Config, capture: &Capture) -> Result<String> {
	crate::store::catalog::text(cfg, capture)
}

pub fn list(cfg: &Config, status: Option<&str>, source: Option<&str>, limit: usize) -> Result<Vec<Capture>> {
	crate::store::catalog::list(cfg, status, source, limit)
}

pub fn set_status(cfg: &Config, id: &str, status: &str, run_id: Option<&str>) -> Result<()> {
	crate::store::catalog::set_status(cfg, id, status, run_id)
}
