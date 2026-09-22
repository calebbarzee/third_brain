//! `kb status`: clap wiring over the `status` module. not yet mounted in `cli::Command`; see
//! this crate's report for the snippet to add there and to `cli::run`.
use crate::config::Config;
use crate::status;
use anyhow::Result;

pub fn run_status(cfg: &Config, json: bool) -> Result<i32> {
    let report = status::gather(cfg)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }

    println!("root: {}", report.root.display());
    println!("main: {}", report.main_head.as_deref().unwrap_or("(no commits)"));
    println!();

    println!("documents: {} total", report.total_documents);
    for (layer, by_status) in &report.documents {
        let parts: Vec<String> = by_status.iter().map(|(s, n)| format!("{s}={n}")).collect();
        println!("  {layer:<10} {}", parts.join(" "));
    }
    println!();

    println!("open sessions: {}", report.open_sessions.len());
    for s in &report.open_sessions {
        println!("  {}  {:<12} {:<20} {:.1}h", s.id, s.state, s.actor, s.age_hours);
    }
    println!();

    println!("review queue: {}", report.review_queue);
    println!("escalations:  {}", report.escalations);
    println!();

    println!(
        "index: {} ({})",
        report.index.artifact.as_deref().unwrap_or("none built"),
        if report.index.current { "current" } else { "stale" }
    );
    println!("captures: new={} curated={} quarantined={}", report.captures.new, report.captures.curated, report.captures.quarantined);
    println!();

    if report.backups.is_empty() {
        println!("backups: none recorded");
    } else {
        println!("backups:");
        for b in &report.backups {
            println!("  {:<10} {}", b.kind, b.at);
        }
    }

    println!("provider roles: {}", if report.provider_roles.is_empty() { "none configured".to_string() } else { report.provider_roles.join(", ") });

    if report.scheduler_units.is_empty() {
        println!("scheduler: no kb-* timers found (or systemctl --user unavailable)");
    } else {
        println!("scheduler:");
        for line in &report.scheduler_units {
            println!("  {line}");
        }
    }

    Ok(0)
}
