//! criticality percentile ranks and banding. design.md §17.4. pure functions over plain numbers so
//! the math is testable without a database or a repository.

/// the five raw metrics for one document, before percentile ranking. `reads_90d` and `cites_90d`
/// are usage metrics; step 1 has no usage table, so callers always pass `None` for them and their
/// weight is redistributed to the other three.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawMetrics {
	pub in_canon: f64,
	pub in_code: f64,
	pub pagerank: f64,
	pub reads_90d: Option<f64>,
	pub cites_90d: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Weights {
	pub in_canon: f64,
	pub in_code: f64,
	pub pagerank: f64,
	pub reads_90d: f64,
	pub cites_90d: f64,
}

/// percentile rank of `value` within `population`: the fraction of the population strictly below
/// it. `population.len() <= 1` has no spread, so every value ranks at 0. the formula assumes
/// `value` is itself a population member (so the top member ranks at exactly 1.0); a non-canon
/// document's raw metric can exceed every canon value, which would otherwise push the ratio above
/// 1.0, so the result is clamped to `[0, 1]`.
fn percentile_rank(value: f64, population: &[f64]) -> f64 {
	if population.len() <= 1 {
		return 0.0;
	}
	let below = population.iter().filter(|v| **v < value).count();
	(below as f64 / (population.len() - 1) as f64).min(1.0)
}

/// effective weights after redistributing the weight of any always-absent usage metric
/// proportionally across the present metrics, so the present weights sum to the original total.
fn effective_weights(w: &Weights, usage_present: bool) -> (f64, f64, f64, f64, f64) {
	if usage_present {
		return (w.in_canon, w.in_code, w.pagerank, w.reads_90d, w.cites_90d);
	}
	let present_sum = w.in_canon + w.in_code + w.pagerank;
	if present_sum <= 0.0 {
		return (0.0, 0.0, 0.0, 0.0, 0.0);
	}
	let total = w.in_canon + w.in_code + w.pagerank + w.reads_90d + w.cites_90d;
	let scale = total / present_sum;
	(w.in_canon * scale, w.in_code * scale, w.pagerank * scale, 0.0, 0.0)
}

/// `criticality_pct` (0..100) for one document, given the canon population's raw metrics to rank
/// against. `metrics` may belong to a non-canon document; it is still ranked against `canon_population`
/// per design.md §17.4 ("drafts and superseded documents are banded against the canon cutoff").
pub fn criticality_pct(metrics: &RawMetrics, canon_population: &CanonPopulation, weights: &Weights) -> f64 {
	let usage_present = canon_population.reads.iter().any(|v| v.is_some()) || canon_population.cites.iter().any(|v| v.is_some());
	let (w_canon, w_code, w_pr, w_reads, w_cites) = effective_weights(weights, usage_present);
	let pct_canon = percentile_rank(metrics.in_canon, &canon_population.in_canon);
	let pct_code = percentile_rank(metrics.in_code, &canon_population.in_code);
	let pct_pr = percentile_rank(metrics.pagerank, &canon_population.pagerank);
	let pct_reads = if usage_present { percentile_rank(metrics.reads_90d.unwrap_or(0.0), &canon_population.reads_present()) } else { 0.0 };
	let pct_cites = if usage_present { percentile_rank(metrics.cites_90d.unwrap_or(0.0), &canon_population.cites_present()) } else { 0.0 };
	100.0 * (w_canon * pct_canon + w_code * pct_code + w_pr * pct_pr + w_reads * pct_reads + w_cites * pct_cites)
}

/// the canon documents' raw metric values, used as the ranking population for every document.
#[derive(Debug, Clone, Default)]
pub struct CanonPopulation {
	pub in_canon: Vec<f64>,
	pub in_code: Vec<f64>,
	pub pagerank: Vec<f64>,
	pub reads: Vec<Option<f64>>,
	pub cites: Vec<Option<f64>>,
}

impl CanonPopulation {
	fn reads_present(&self) -> Vec<f64> {
		self.reads.iter().filter_map(|v| *v).collect()
	}
	fn cites_present(&self) -> Vec<f64> {
		self.cites.iter().filter_map(|v| *v).collect()
	}
}

/// the band cutoff: the highest `criticality_pct` among the lowest `band_pct` percent of canon
/// documents (by count, rounded down). documents at or below the cutoff are `low`.
pub fn band_cutoff(mut canon_pcts: Vec<f64>, band_pct: f64) -> f64 {
	if canon_pcts.is_empty() {
		return f64::NEG_INFINITY;
	}
	canon_pcts.sort_by(|a, b| a.partial_cmp(b).unwrap());
	let low_count = ((canon_pcts.len() as f64) * band_pct / 100.0).floor() as usize;
	let low_count = low_count.max(1).min(canon_pcts.len());
	canon_pcts[low_count - 1]
}

/// `low` when `pct` is at or below the cutoff, `review` otherwise.
pub fn band_for(pct: f64, cutoff: f64) -> &'static str {
	if pct <= cutoff { "low" } else { "review" }
}

#[cfg(test)]
mod tests {
	use super::*;

	fn w() -> Weights {
		Weights { in_canon: 0.25, in_code: 0.30, pagerank: 0.20, reads_90d: 0.15, cites_90d: 0.10 }
	}

	#[test]
	fn percentile_rank_basic() {
		assert_eq!(percentile_rank(5.0, &[1.0, 2.0, 3.0, 4.0, 5.0]), 4.0 / 4.0);
		assert_eq!(percentile_rank(1.0, &[1.0, 2.0, 3.0, 4.0, 5.0]), 0.0);
		assert_eq!(percentile_rank(3.0, &[1.0, 2.0, 3.0, 4.0, 5.0]), 2.0 / 4.0);
		assert_eq!(percentile_rank(3.0, &[3.0]), 0.0);
	}

	#[test]
	fn absent_usage_weight_redistributes() {
		let pop = CanonPopulation {
			in_canon: vec![0.0, 1.0, 2.0, 3.0],
			in_code: vec![0.0, 1.0, 2.0, 3.0],
			pagerank: vec![0.1, 0.2, 0.3, 0.4],
			reads: vec![None, None, None, None],
			cites: vec![None, None, None, None],
		};
		let top = RawMetrics { in_canon: 3.0, in_code: 3.0, pagerank: 0.4, reads_90d: None, cites_90d: None };
		let pct = criticality_pct(&top, &pop, &w());
		// top of every present metric: weight 0.25+0.30+0.20 = 0.75 is redistributed to sum to the
		// original 1.0, so a document that tops every present metric should land at exactly 100.
		assert!((pct - 100.0).abs() < 1e-9, "got {pct}");

		let bottom = RawMetrics { in_canon: 0.0, in_code: 0.0, pagerank: 0.1, reads_90d: None, cites_90d: None };
		assert_eq!(criticality_pct(&bottom, &pop, &w()), 0.0);

		// a metric with no spread at all (every canon document tied, including this one) contributes
		// 0 to everyone, since nobody ranks "below" a tie; its weight is not separately redistributed
		// away, it is simply unrealized. this is a direct consequence of the percentile-rank formula,
		// not a bug.
		let pop_tied_code = CanonPopulation { in_code: vec![0.0, 0.0, 0.0, 0.0], ..pop };
		let top_tied_code = RawMetrics { in_code: 0.0, ..top };
		let pct_tied = criticality_pct(&top_tied_code, &pop_tied_code, &w());
		assert!(pct_tied < 100.0, "a tied metric cannot contribute, so the top document should score below 100, got {pct_tied}");
	}

	#[test]
	fn band_cutoff_and_assignment() {
		// 10 canon docs, band_pct = 20 -> lowest 2 are "low".
		let pcts: Vec<f64> = (0..10).map(|i| i as f64 * 10.0).collect();
		let cutoff = band_cutoff(pcts.clone(), 20.0);
		assert_eq!(cutoff, 10.0);
		let bands: Vec<&str> = pcts.iter().map(|p| band_for(*p, cutoff)).collect();
		assert_eq!(bands.iter().filter(|b| **b == "low").count(), 2);
	}

	#[test]
	fn small_population_still_bands_at_least_one() {
		let cutoff = band_cutoff(vec![5.0, 50.0, 95.0], 20.0);
		assert_eq!(cutoff, 5.0);
	}
}
