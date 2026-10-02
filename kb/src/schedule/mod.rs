//! `kb schedule`: systemd user units (or, with `--cron`, equivalent crontab lines) for the
//! recurring jobs design.md §22 (tick), §12 (end of day), §21 (backups), and nightly curation
//! expect to run unattended. rendering is pure (no I/O) so it is unit-tested directly; `install`,
//! `uninstall`, and `status` are the only functions that touch the filesystem or shell out to
//! `systemctl`, and tests must never call those three (per this crate's brief: "do not run
//! systemctl in tests").

use crate::config::Config;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// one systemd unit file: `name` is its filename under `~/.config/systemd/user/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub content: String,
}

/// timers this crate installs, enables, and later finds again to uninstall. `kb-serve` has no
/// timer (it is a standing service), so it is not in this list; see `SERVICE_ONLY_UNITS`.
const TIMER_STEMS: [&str; 4] = ["kb-capture", "kb-eod", "kb-nightly", "kb-backup"];
/// `kb-backup-full` shares `kb-backup`'s hourly timer's job (a daily full backup) under its own
/// unit pair, since design.md §21 wants both an hourly incremental and a daily full.
const EXTRA_TIMER_STEMS: [&str; 1] = ["kb-backup-full"];
/// units with no timer: long-running or on-demand services, enabled directly.
const SERVICE_ONLY_UNITS: [&str; 1] = ["kb-serve"];

#[derive(Debug, Clone)]
pub struct ScheduleOptions {
    /// "HH:MM", 24-hour.
    pub eod: String,
    /// "HH:MM", 24-hour.
    pub nightly: String,
    pub capture_minutes: u32,
    pub tick_seconds: u32,
    /// a daily full backup fires at this time; the hourly bundle timer runs independently of it.
    pub backup_full_at: String,
    pub serve: bool,
}

impl Default for ScheduleOptions {
    fn default() -> Self {
        Self {
            eod: "02:00".to_string(),
            nightly: "03:00".to_string(),
            capture_minutes: 60,
            tick_seconds: 60,
            backup_full_at: "04:00".to_string(),
            serve: false,
        }
    }
}

/// units plus any companion scripts an ExecStart line points at, keyed by the path to write them
/// under (relative to nothing in particular; `install` decides where each goes).
pub struct Rendered {
    pub units: Vec<Unit>,
    /// (filename, content) helper scripts placed in the repository's own state directory.
    pub scripts: Vec<(String, String)>,
}

/// a short, filesystem- and systemd-instance-safe name for `root`, used as the `kb-tick@<name>`
/// instance so more than one kb repository can schedule ticks on the same account without
/// colliding. deterministic, so `uninstall` can recompute it without stored state.
pub fn instance_name(root: &Path) -> String {
    let base = root.file_name().and_then(|s| s.to_str()).unwrap_or("kb");
    let sanitized: String = base.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    if sanitized.is_empty() { "kb".to_string() } else { sanitized }
}

fn oneshot_service(description: &str, working_dir: &Path, exec_start: &str) -> String {
    format!("[Unit]\nDescription={description}\n\n[Service]\nType=oneshot\nWorkingDirectory={}\nExecStart={exec_start}\n", working_dir.display())
}

fn timer(description: &str, on_calendar: Option<&str>, on_active_sec: Option<u32>) -> String {
    let mut body = format!("[Unit]\nDescription={description}\n\n[Timer]\n");
    if let Some(secs) = on_active_sec {
        body.push_str(&format!("OnActiveSec={secs}\nOnUnitActiveSec={secs}\n"));
    }
    if let Some(cal) = on_calendar {
        body.push_str(&format!("OnCalendar={cal}\n"));
    }
    body.push_str("Persistent=true\n\n[Install]\nWantedBy=timers.target\n");
    body
}

/// `HH:MM` to a systemd `OnCalendar` daily expression (`*-*-* HH:MM:00`); `None` when it does not
/// parse as two colon-separated integers in range.
fn on_calendar_daily(hhmm: &str) -> Option<String> {
    let (h, m) = hhmm.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(format!("*-*-* {h:02}:{m:02}:00"))
}

/// the eod helper script's filename, written under `<state>/schedule/`. `kb eod` only settles
/// open sessions and leaves them `settled` for a merge (design.md §12); it does not merge them
/// itself, so the scheduled job needs a second step. kept as a standalone script rather than an
/// inline `ExecStart` one-liner so the quoting stays simple and the unit stays readable.
pub const EOD_MERGE_SCRIPT: &str = "kb-eod-merge.sh";

fn eod_merge_script(bin: &Path, root: &Path) -> String {
    let lines = [
        "#!/bin/sh".to_string(),
        "# installed by `kb schedule install`. `kb eod` settles every open session but does not".to_string(),
        "# merge it (design.md \u{a7}12); this merges every session it left `settled`.".to_string(),
        "set -e".to_string(),
        format!("KB=\"{}\"", bin.display()),
        format!("ROOT=\"{}\"", root.display()),
        "\"$KB\" --repo \"$ROOT\" eod".to_string(),
        "\"$KB\" --repo \"$ROOT\" sessions --all | awk '$2 == \"settled\" { print $1 }' | while read -r id; do".to_string(),
        "  \"$KB\" --repo \"$ROOT\" merge \"$id\" || true".to_string(),
        "done".to_string(),
    ];
    lines.join("\n") + "\n"
}

/// render every unit (and companion script) `kb schedule install` writes. pure: no filesystem or
/// process access, so this is exactly what the unit tests exercise.
pub fn render(cfg: &Config, bin: &Path, opts: &ScheduleOptions) -> Result<Rendered> {
    let root = &cfg.root;
    let instance = instance_name(root);
    let mut units = Vec::new();
    let mut scripts = Vec::new();

    // tick: a template service (one instance per repository) plus a concrete timer for it.
    units.push(Unit {
        name: "kb-tick@.service".to_string(),
        content: oneshot_service("kb reconciliation tick (%i)", root, &format!("{} --repo {} tick", bin.display(), root.display())),
    });
    units.push(Unit {
        name: format!("kb-tick@{instance}.timer"),
        content: timer(&format!("kb tick every {}s", opts.tick_seconds), None, Some(opts.tick_seconds)),
    });

    // capture: pull every configured source into the content store.
    units.push(Unit {
        name: "kb-capture.service".to_string(),
        content: oneshot_service("kb archive capture", root, &format!("{} --repo {} archive capture", bin.display(), root.display())),
    });
    units.push(Unit {
        name: "kb-capture.timer".to_string(),
        content: timer(&format!("kb archive capture every {} minute(s)", opts.capture_minutes), None, Some(opts.capture_minutes.saturating_mul(60).max(60))),
    });

    // end of day: settle every open session, then merge every session it settled.
    let eod_script = eod_merge_script(bin, root);
    scripts.push((EOD_MERGE_SCRIPT.to_string(), eod_script));
    let eod_calendar = on_calendar_daily(&opts.eod).with_context(|| format!("`--eod {}` is not `HH:MM`", opts.eod))?;
    units.push(Unit {
        name: "kb-eod.service".to_string(),
        content: oneshot_service("kb end of day: settle and merge", root, &format!("/bin/sh {}", schedule_dir(cfg).join(EOD_MERGE_SCRIPT).display())),
    });
    units.push(Unit { name: "kb-eod.timer".to_string(), content: timer(&format!("kb eod daily at {}", opts.eod), Some(&eod_calendar), None) });

    // nightly curation: `kb maintain nightly` is another agent's command, not yet built as of
    // this writing. the unit is written to point at it regardless (see this crate's report).
    let nightly_calendar = on_calendar_daily(&opts.nightly).with_context(|| format!("`--nightly {}` is not `HH:MM`", opts.nightly))?;
    units.push(Unit {
        name: "kb-nightly.service".to_string(),
        content: oneshot_service("kb nightly curation", root, &format!("{} --repo {} maintain nightly", bin.display(), root.display())),
    });
    units.push(Unit { name: "kb-nightly.timer".to_string(), content: timer(&format!("kb maintain nightly daily at {}", opts.nightly), Some(&nightly_calendar), None) });

    // backups: hourly bundle, daily full. `kb backup run` is another agent's command, not yet
    // built as of this writing; see this crate's report.
    units.push(Unit {
        name: "kb-backup.service".to_string(),
        content: oneshot_service("kb hourly backup bundle", root, &format!("{} --repo {} backup run --kind bundle", bin.display(), root.display())),
    });
    units.push(Unit { name: "kb-backup.timer".to_string(), content: timer("kb backup bundle hourly", None, Some(3600)) });
    let backup_calendar = on_calendar_daily(&opts.backup_full_at).with_context(|| format!("`--backup-full-at {}` is not `HH:MM`", opts.backup_full_at))?;
    units.push(Unit {
        name: "kb-backup-full.service".to_string(),
        content: oneshot_service("kb daily full backup", root, &format!("{} --repo {} backup run --kind bundle-full", bin.display(), root.display())),
    });
    units.push(Unit {
        name: "kb-backup-full.timer".to_string(),
        content: timer(&format!("kb full backup daily at {}", opts.backup_full_at), Some(&backup_calendar), None),
    });

    if opts.serve {
        units.push(Unit {
            name: "kb-serve.service".to_string(),
            content: format!(
                "[Unit]\nDescription=kb server\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={} --repo {} serve --no-auth\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
                root.display(),
                bin.display(),
                root.display(),
            ),
        });
    }

    Ok(Rendered { units, scripts })
}

/// `<state>/schedule/`, where companion scripts (the eod merge helper) are written; kept inside
/// the repository's own `.kb` directory so it travels with the repo rather than the account.
pub fn schedule_dir(cfg: &Config) -> PathBuf {
    cfg.state_dir().join("schedule")
}

fn user_unit_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("systemd/user"));
    }
    let home = std::env::var("HOME").context("HOME is not set; cannot locate ~/.config/systemd/user")?;
    Ok(PathBuf::from(home).join(".config/systemd/user"))
}

/// every unit name this crate manages, timers first then their services, so `disable --now` on
/// the timer runs before its service is touched. used by `install` (to enable) and `uninstall`
/// (to disable and remove).
fn managed_unit_names(cfg: &Config, serve: bool) -> Vec<String> {
    let instance = instance_name(&cfg.root);
    let mut names = vec![format!("kb-tick@{instance}.timer"), "kb-tick@.service".to_string()];
    for stem in TIMER_STEMS.iter().chain(EXTRA_TIMER_STEMS.iter()) {
        names.push(format!("{stem}.timer"));
        names.push(format!("{stem}.service"));
    }
    if serve {
        for stem in SERVICE_ONLY_UNITS {
            names.push(format!("{stem}.service"));
        }
    }
    names
}

/// `kb schedule install`: write every unit (and the eod helper script) and, unless `dry_run`,
/// `daemon-reload` then enable every timer (and, with `--serve`, the serve service) `--now`.
/// never calls `systemctl` when `dry_run`.
pub fn install(cfg: &Config, bin: &Path, opts: &ScheduleOptions, dry_run: bool) -> Result<Rendered> {
    let rendered = render(cfg, bin, opts)?;
    if dry_run {
        return Ok(rendered);
    }

    let script_dir = schedule_dir(cfg);
    std::fs::create_dir_all(&script_dir)?;
    for (name, content) in &rendered.scripts {
        let path = script_dir.join(name);
        std::fs::write(&path, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }

    let unit_dir = user_unit_dir()?;
    std::fs::create_dir_all(&unit_dir)?;
    for unit in &rendered.units {
        std::fs::write(unit_dir.join(&unit.name), &unit.content)?;
    }

    run_systemctl(&["--user", "daemon-reload"])?;
    let mut enable_targets: Vec<String> = TIMER_STEMS.iter().chain(EXTRA_TIMER_STEMS.iter()).map(|s| format!("{s}.timer")).collect();
    enable_targets.push(format!("kb-tick@{}.timer", instance_name(&cfg.root)));
    if opts.serve {
        enable_targets.push("kb-serve.service".to_string());
    }
    let mut args = vec!["--user".to_string(), "enable".to_string(), "--now".to_string()];
    args.extend(enable_targets);
    run_systemctl(&args.iter().map(String::as_str).collect::<Vec<_>>())?;

    Ok(rendered)
}

/// `kb schedule uninstall`: disable every managed unit, remove its file, then `daemon-reload`.
/// errors from `systemctl` (a unit was never enabled, say) are tolerated; a missing file is not
/// an error either, so this is safe to run more than once or after a partial install.
pub fn uninstall(cfg: &Config, serve: bool) -> Result<()> {
    let names = managed_unit_names(cfg, serve);
    let timers: Vec<&str> = names.iter().filter(|n| n.ends_with(".timer") || n.ends_with(".service")).map(String::as_str).collect();
    if !timers.is_empty() {
        let mut args = vec!["--user", "disable", "--now"];
        args.extend(timers);
        let _ = run_systemctl(&args);
    }
    let unit_dir = user_unit_dir()?;
    for name in &names {
        let _ = std::fs::remove_file(unit_dir.join(name));
    }
    let _ = run_systemctl(&["--user", "daemon-reload"]);
    Ok(())
}

/// `kb schedule status`: `systemctl --user list-timers`, filtered to `kb-*`. `"systemctl --user
/// unavailable"` when there is no systemd user session (most sandboxes and containers) rather
/// than an error.
pub fn status() -> String {
    match std::process::Command::new("systemctl").args(["--user", "list-timers", "--all"]).output() {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<&str> = text.lines().filter(|l| l.contains("kb-") || l.starts_with("NEXT")).collect();
            if lines.is_empty() { "no kb-* timers found".to_string() } else { lines.join("\n") }
        }
        _ => "systemctl --user unavailable".to_string(),
    }
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("systemctl").args(args).output().context("running systemctl")?;
    if !out.status.success() {
        anyhow::bail!("systemctl {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// `--cron`: crontab lines equivalent to `render`'s units, for a machine without systemd. cron's
/// granularity is one minute, so `tick_seconds` below 60 collapses to "every minute" with a
/// comment noting the approximation.
pub fn cron_lines(cfg: &Config, bin: &Path, opts: &ScheduleOptions) -> Result<Vec<String>> {
    let root = &cfg.root;
    let bin = bin.display();
    let root_display = root.display();
    let mut lines = Vec::new();

    if opts.tick_seconds <= 60 {
        lines.push(format!("# kb tick: cron's granularity is one minute; {}s collapses to every minute", opts.tick_seconds));
        lines.push(format!("* * * * * {bin} --repo {root_display} tick"));
    } else {
        let every_min = (opts.tick_seconds / 60).max(1);
        lines.push(format!("*/{every_min} * * * * {bin} --repo {root_display} tick"));
    }

    lines.push(format!("*/{} * * * * {bin} --repo {root_display} archive capture", opts.capture_minutes.max(1)));

    let (eod_h, eod_m) = split_hhmm(&opts.eod).with_context(|| format!("`--eod {}` is not `HH:MM`", opts.eod))?;
    let script = schedule_dir(cfg).join(EOD_MERGE_SCRIPT);
    lines.push(format!("{eod_m} {eod_h} * * * /bin/sh {}", script.display()));

    let (night_h, night_m) = split_hhmm(&opts.nightly).with_context(|| format!("`--nightly {}` is not `HH:MM`", opts.nightly))?;
    lines.push(format!("{night_m} {night_h} * * * {bin} --repo {root_display} maintain nightly"));

    lines.push(format!("0 * * * * {bin} --repo {root_display} backup run --kind bundle"));
    let (full_h, full_m) = split_hhmm(&opts.backup_full_at).with_context(|| format!("`--backup-full-at {}` is not `HH:MM`", opts.backup_full_at))?;
    lines.push(format!("{full_m} {full_h} * * * {bin} --repo {root_display} backup run --kind bundle-full"));

    Ok(lines)
}

fn split_hhmm(hhmm: &str) -> Option<(u32, u32)> {
    let (h, m) = hhmm.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h <= 23 && m <= 59).then_some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &Path) -> Config {
        Config::load(root).unwrap()
    }

    #[test]
    fn on_calendar_daily_formats_and_rejects_bad_input() {
        assert_eq!(on_calendar_daily("02:00").as_deref(), Some("*-*-* 02:00:00"));
        assert_eq!(on_calendar_daily("9:5").as_deref(), Some("*-*-* 09:05:00"));
        assert!(on_calendar_daily("24:00").is_none());
        assert!(on_calendar_daily("nonsense").is_none());
    }

    #[test]
    fn instance_name_is_stable_and_sanitized() {
        assert_eq!(instance_name(Path::new("/home/caleb/notes kb!")), "notes-kb-");
        assert_eq!(instance_name(Path::new("/")), "kb");
    }

    #[test]
    fn render_produces_the_expected_unit_set_and_valid_calendars() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let bin = Path::new("/usr/local/bin/kb");
        let opts = ScheduleOptions::default();
        let rendered = render(&c, bin, &opts).unwrap();

        let names: Vec<&str> = rendered.units.iter().map(|u| u.name.as_str()).collect();
        assert!(names.contains(&"kb-tick@.service"));
        let instance = instance_name(dir.path());
        assert!(names.contains(&format!("kb-tick@{instance}.timer").as_str()));
        for stem in ["kb-capture", "kb-eod", "kb-nightly", "kb-backup", "kb-backup-full"] {
            assert!(names.contains(&format!("{stem}.service").as_str()), "missing {stem}.service");
            assert!(names.contains(&format!("{stem}.timer").as_str()), "missing {stem}.timer");
        }
        assert!(!names.iter().any(|n| n.starts_with("kb-serve")), "serve unit should be absent by default");

        let eod_service = rendered.units.iter().find(|u| u.name == "kb-eod.service").unwrap();
        assert!(eod_service.content.contains("kb-eod-merge.sh"));
        let eod_timer = rendered.units.iter().find(|u| u.name == "kb-eod.timer").unwrap();
        assert!(eod_timer.content.contains("OnCalendar=*-*-* 02:00:00"));

        let tick_service = rendered.units.iter().find(|u| u.name == "kb-tick@.service").unwrap();
        assert!(tick_service.content.contains(&format!("--repo {}", dir.path().display())));
        assert!(tick_service.content.contains("tick"));

        assert_eq!(rendered.scripts.len(), 1);
        assert_eq!(rendered.scripts[0].0, "kb-eod-merge.sh");
        assert!(rendered.scripts[0].1.contains("kb eod"));
        assert!(rendered.scripts[0].1.contains("merge"));
    }

    #[test]
    fn render_with_serve_adds_the_serve_service_with_no_authentication_flag() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let opts = ScheduleOptions { serve: true, ..ScheduleOptions::default() };
        let rendered = render(&c, Path::new("/usr/local/bin/kb"), &opts).unwrap();
        let serve = rendered.units.iter().find(|u| u.name == "kb-serve.service").expect("kb-serve.service");
        assert!(serve.content.contains("serve --no-auth"));
        assert!(!serve.content.contains("[Timer]"));
    }

    #[test]
    fn every_rendered_backup_kind_parses_in_the_cli() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let bin = Path::new("/usr/local/bin/kb");
        let opts = ScheduleOptions::default();
        let mut texts: Vec<String> = render(&c, bin, &opts).unwrap().units.into_iter().map(|u| u.content).collect();
        texts.extend(cron_lines(&c, bin, &opts).unwrap());
        let mut seen = 0;
        for text in &texts {
            for kind in text.split("--kind ").skip(1).map(|rest| rest.split_whitespace().next().unwrap_or("").trim_end_matches('"')) {
                crate::cli::backup::parse_kind(&Some(kind.to_string())).unwrap_or_else(|e| panic!("rendered `--kind {kind}` is rejected by the cli: {e}"));
                seen += 1;
            }
        }
        assert!(seen >= 4, "expected the hourly and daily backup kinds in both units and cron lines, saw {seen}");
    }

    #[test]
    fn render_rejects_a_malformed_time() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let opts = ScheduleOptions { eod: "not-a-time".to_string(), ..ScheduleOptions::default() };
        assert!(render(&c, Path::new("/usr/bin/kb"), &opts).is_err());
    }

    #[test]
    fn cron_lines_cover_every_job() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let lines = cron_lines(&c, Path::new("/usr/local/bin/kb"), &ScheduleOptions::default()).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("tick"));
        assert!(joined.contains("archive capture"));
        assert!(joined.contains("kb-eod-merge.sh"));
        assert!(joined.contains("maintain nightly"));
        assert!(joined.contains("backup run --kind bundle"));
        assert!(joined.contains("backup run --kind bundle-full"));
    }

    #[test]
    fn managed_unit_names_include_the_instance_tick_timer() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let names = managed_unit_names(&c, false);
        assert!(names.contains(&format!("kb-tick@{}.timer", instance_name(dir.path()))));
        assert!(!names.iter().any(|n| n.starts_with("kb-serve")));
        let with_serve = managed_unit_names(&c, true);
        assert!(with_serve.iter().any(|n| n == "kb-serve.service"));
    }
}
