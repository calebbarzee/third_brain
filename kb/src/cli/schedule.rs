//! `kb schedule`: clap wiring over the `schedule` module. not yet mounted in `cli::Command`; see
//! this crate's report for the snippet to add there and to `cli::run`.
use crate::config::Config;
use crate::schedule::{self, ScheduleOptions};
use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ScheduleCommand {
    /// write (and enable) the systemd user units, or print equivalent crontab lines with --cron
    Install {
        /// accepted for clarity: units always install at user scope (~/.config/systemd/user/)
        #[arg(long)]
        user: bool,
        #[arg(long, default_value = "02:00")]
        eod: String,
        #[arg(long, default_value = "03:00")]
        nightly: String,
        #[arg(long, default_value_t = 60)]
        capture_minutes: u32,
        #[arg(long, default_value_t = 60)]
        tick_seconds: u32,
        /// also install and enable kb-serve.service (`kb serve --no-auth`)
        #[arg(long)]
        serve: bool,
        /// print the unit files (or, with --cron, crontab lines) instead of writing and enabling them
        #[arg(long)]
        dry_run: bool,
        /// print equivalent crontab lines instead of writing systemd units
        #[arg(long)]
        cron: bool,
    },
    /// disable and remove every unit this crate installed
    Uninstall {
        /// also remove kb-serve.service
        #[arg(long)]
        serve: bool,
    },
    /// `systemctl --user list-timers`, filtered to kb-*
    Status,
}

pub fn run(cfg: &Config, cmd: ScheduleCommand, json: bool) -> Result<i32> {
    match cmd {
        ScheduleCommand::Install { user: _, eod, nightly, capture_minutes, tick_seconds, serve, dry_run, cron } => {
            let opts = ScheduleOptions { eod, nightly, capture_minutes, tick_seconds, serve, ..ScheduleOptions::default() };
            let bin = std::env::current_exe().unwrap_or_else(|_| "kb".into());

            if cron {
                let lines = schedule::cron_lines(cfg, &bin, &opts)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&lines)?);
                } else {
                    for l in &lines {
                        println!("{l}");
                    }
                }
                return Ok(0);
            }

            let rendered = schedule::install(cfg, &bin, &opts, dry_run)?;
            if dry_run {
                if json {
                    let units: Vec<_> = rendered.units.iter().map(|u| serde_json::json!({"name": u.name, "content": u.content})).collect();
                    println!("{}", serde_json::to_string_pretty(&units)?);
                } else {
                    for u in &rendered.units {
                        println!("# {}\n{}", u.name, u.content);
                    }
                    for (name, content) in &rendered.scripts {
                        println!("# {name}\n{content}");
                    }
                }
            } else if json {
                let names: Vec<&str> = rendered.units.iter().map(|u| u.name.as_str()).collect();
                println!("{}", serde_json::to_string_pretty(&names)?);
            } else {
                println!("installed and enabled:");
                for u in &rendered.units {
                    println!("  {}", u.name);
                }
            }
            Ok(0)
        }
        ScheduleCommand::Uninstall { serve } => {
            schedule::uninstall(cfg, serve)?;
            if !json {
                println!("uninstalled");
            }
            Ok(0)
        }
        ScheduleCommand::Status => {
            let text = schedule::status();
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "status": text }))?);
            } else {
                println!("{text}");
            }
            Ok(0)
        }
    }
}
