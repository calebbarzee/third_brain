//! clap command tree. subcommand groups live in their own files and are wired here.
pub mod answer;
pub mod archive;
pub mod backup;
pub mod check;
pub mod curate;
pub mod gate;
pub mod import;
pub mod index;
pub mod init;
pub mod maintain;
pub mod merge;
pub mod policy;
pub mod provider;
pub mod review;
pub mod run;
pub mod schedule;
pub mod serve;
pub mod session;
pub mod status;
pub mod transcript;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "kb", version, about = "git-backed knowledge base for humans and agents")]
pub struct Cli {
	/// repository root; defaults to the nearest ancestor containing kb.toml or .git
	#[arg(long, global = true)]
	pub repo: Option<PathBuf>,
	/// machine-readable output where supported
	#[arg(long, global = true)]
	pub json: bool,
	#[command(subcommand)]
	pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
	/// create a knowledge base repository skeleton here (or at PATH) and install the pre-commit hook
	Init {
		path: Option<PathBuf>,
		/// overwrite existing config files and hook
		#[arg(long)]
		force: bool,
		/// skip installing the git pre-commit hook
		#[arg(long)]
		no_hook: bool,
	},
	/// print the parsed frontmatter, sections, and links of one file
	Parse { path: PathBuf },
	/// alias for `kb check validate`
	Validate {
		paths: Vec<PathBuf>,
		#[arg(long)]
		fix: bool,
		#[arg(long)]
		staged: bool,
		#[arg(long)]
		no_classify: bool,
	},
	/// alias for `kb check secrets`
	Secrets {
		#[arg(long)]
		staged: bool,
		range: Option<String>,
	},
	/// alias for `kb index search`
	Search {
		query: String,
		#[arg(long, default_value_t = 5)]
		limit: usize,
		#[arg(long)]
		status_any: bool,
	},
	/// alias for `kb index get`
	Get { id: String },
	/// alias for `kb index show`
	Show { id: String },
	/// content store: captures, catalog, export, quarantine
	#[command(subcommand)]
	Archive(archive::ArchiveCommand),
	/// validate, infer, gate, graph, and secrets checks
	#[command(subcommand)]
	Check(check::CheckCommand),
	/// curate: turn captures or a proposal into operations inside a session
	#[command(subcommand)]
	Curate(curate::CurateCommand),
	/// import existing markdown notes as documents through one session
	Import(import::ImportArgs),
	/// systemd user timers (or cron lines) for tick, capture, eod, nightly, backup, serve
	#[command(subcommand)]
	Schedule(schedule::ScheduleCommand),
	/// one screen of repository, session, index, capture, backup, and scheduler state
	Status,
	/// ask a question over the knowledge base, or evaluate retrieval quality
	#[command(subcommand)]
	Answer(answer::AnswerCommand),
	/// http server (rest and mcp), stdio mcp server, and bearer tokens
	#[command(flatten)]
	Serve(serve::ServeCommand),
	/// nightly curation, its escalation queue, and compaction
	#[command(subcommand)]
	Maintain(maintain::MaintainCommand),
	/// mirror push, bundles, and the content-store copy
	#[command(subcommand)]
	Backup(backup::BackupCommand),
	/// index build and read commands: build, search, get, show, refs, ls, blame, graph
	#[command(subcommand)]
	Index(index::IndexCommand),
	/// llm provider commands: test connectivity, run classify on one file
	#[command(subcommand)]
	Provider(provider::ProviderCommand),
	/// full transcripts of every model call and mcp tool call
	#[command(subcommand)]
	Transcript(transcript::TranscriptCommand),
	/// land a session on main: rebase, checks, structure gate, content review, fast-forward
	Merge {
		session: String,
		/// run the checks and report the class without landing
		#[arg(long)]
		check: bool,
		/// land a review-class session (records the approver)
		#[arg(long)]
		approve: bool,
	},
	/// review queue (list, approve, reject) and the review hooks (run, packet, resolve)
	#[command(subcommand)]
	Review(ReviewTop),
	/// revert a commit, run, or session as a forward commit through a new session
	Revert { target: String },
	/// check main history and worktree state
	Doctor,
	/// reconciliation tick: rebuild a stale index, repair sessions, prune worktrees
	Tick,
	/// quick gate: run the cheap classifier configured for role `gate` by hand
	#[command(subcommand)]
	Gate(gate::GateCommand),
	/// agent policy: decide one operation, or list allowed operations for a document
	#[command(subcommand)]
	Policy(policy::PolicyCommand),
	/// run logs: list, show, verify, diff, search
	#[command(subcommand)]
	Run(run::RunCommand),
	/// session lifecycle: open, settle, list, show, hold, abandon, cleanup, eod
	#[command(subcommand)]
	Session(session::SessionCommand),
	/// list open sessions with actor, state, and changed files
	Sessions {
		/// include merged and abandoned sessions
		#[arg(long)]
		all: bool,
	},
	/// end-of-day sweep: commit and settle every open session
	Eod,
}

/// `kb review ...`: the human review queue plus the model-backed review hooks.
#[derive(Subcommand, Debug)]
pub enum ReviewTop {
	/// list sessions waiting for approval
	List,
	/// approve and land a review-class session
	Approve {
		session: String,
		/// approve as this principal instead of the current git identity
		#[arg(long)]
		as_principal: Option<String>,
	},
	/// send a review-class session back to its author
	Reject {
		session: String,
		#[arg(long)]
		reason: String,
	},
	/// content review hooks: run, packet, resolve
	#[command(flatten)]
	Hook(review::ReviewCommand),
}

/// resolve the repository root from --repo or the current directory.
pub fn resolve_root(repo: &Option<PathBuf>) -> anyhow::Result<PathBuf> {
	let start = match repo {
		Some(p) => p.clone(),
		None => std::env::current_dir()?,
	};
	if !start.exists() {
		anyhow::bail!("repository path does not exist: {}", start.display());
	}
	crate::config::Config::find_root(&start).ok_or_else(|| anyhow::anyhow!("no kb repository found from {} (run kb init)", start.display()))
}

pub fn load_config(repo: &Option<PathBuf>) -> anyhow::Result<crate::config::Config> {
	let root = resolve_root(repo)?;
	crate::config::Config::load(&root)
}

/// run a command; returns the process exit code.
pub fn run(cli: Cli) -> anyhow::Result<i32> {
	match cli.command {
		Command::Init { path, force, no_hook } => {
			let root = match path {
				Some(p) => p,
				None => cli.repo.clone().unwrap_or(std::env::current_dir()?),
			};
			std::fs::create_dir_all(&root)?;
			let created = init::init(&root, force, !no_hook)?;
			if cli.json {
				println!("{}", serde_json::to_string_pretty(&created)?);
			} else if created.is_empty() {
				println!("nothing to do: skeleton already present at {}", root.display());
			} else {
				for c in created {
					if let Some(warning) = c.strip_prefix("warning: ") {
						println!("warning: {warning}");
					} else {
						println!("created {c}");
					}
				}
			}
			Ok(0)
		}
		Command::Parse { path } => {
			let text = std::fs::read_to_string(&path)?;
			let doc = crate::parse::parse_document(&text)?;
			if cli.json {
				println!("{}", serde_json::to_string_pretty(&doc)?);
			} else {
				println!("{doc:#?}");
			}
			Ok(0)
		}
		Command::Validate { paths, fix, staged, no_classify } => {
			let cfg = load_config(&cli.repo)?;
			check::run(&cfg, check::CheckCommand::Validate { paths, fix, staged, no_classify }, cli.json)
		}
		Command::Secrets { staged, range } => {
			let cfg = load_config(&cli.repo)?;
			check::run(&cfg, check::CheckCommand::Secrets { staged, range }, cli.json)
		}
		Command::Search { query, limit, status_any } => {
			let cfg = load_config(&cli.repo)?;
			index::run(
				&cfg,
				index::IndexCommand::Search { query, limit, status_any, include_captures: false, mode: None, explain: false, record: false },
				cli.json,
			)
		}
		Command::Get { id } => {
			let cfg = load_config(&cli.repo)?;
			index::run(&cfg, index::IndexCommand::Get { id }, cli.json)
		}
		Command::Show { id } => {
			let cfg = load_config(&cli.repo)?;
			index::run(&cfg, index::IndexCommand::Show { id }, cli.json)
		}
		Command::Archive(cmd) => {
			let cfg = load_config(&cli.repo)?;
			archive::run(&cfg, cmd, cli.json)
		}
		Command::Check(cmd) => {
			let cfg = load_config(&cli.repo)?;
			check::run(&cfg, cmd, cli.json)
		}
		Command::Curate(cmd) => {
			let cfg = load_config(&cli.repo)?;
			curate::run(&cfg, cmd, cli.json)
		}
		Command::Import(args) => {
			let cfg = load_config(&cli.repo)?;
			import::run_import(&cfg, args, cli.json)
		}
		Command::Schedule(cmd) => {
			let cfg = load_config(&cli.repo)?;
			schedule::run(&cfg, cmd, cli.json)
		}
		Command::Status => {
			let cfg = load_config(&cli.repo)?;
			status::run_status(&cfg, cli.json)
		}
		Command::Answer(cmd) => {
			let cfg = load_config(&cli.repo)?;
			answer::run(&cfg, cmd, cli.json)
		}
		Command::Serve(cmd) => {
			let cfg = load_config(&cli.repo)?;
			serve::run(&cfg, cmd, cli.json)
		}
		Command::Maintain(cmd) => {
			let cfg = load_config(&cli.repo)?;
			maintain::run(&cfg, cmd, cli.json)
		}
		Command::Backup(cmd) => {
			let cfg = load_config(&cli.repo)?;
			backup::run(&cfg, cmd, cli.json)
		}
		Command::Index(cmd) => {
			let cfg = load_config(&cli.repo)?;
			index::run(&cfg, cmd, cli.json)
		}
		Command::Provider(cmd) => {
			let cfg = load_config(&cli.repo)?;
			provider::run(&cfg, cmd, cli.json)
		}
		Command::Merge { session, check, approve } => {
			let cfg = load_config(&cli.repo)?;
			merge::run(&cfg, merge::MergeCommand::Merge { session, check, approve }, cli.json)
		}
		Command::Review(cmd) => {
			let cfg = load_config(&cli.repo)?;
			match cmd {
				ReviewTop::List => merge::run(&cfg, merge::MergeCommand::Review { cmd: merge::ReviewCommand::List }, cli.json),
				ReviewTop::Approve { session, as_principal } => {
					merge::run(&cfg, merge::MergeCommand::Review { cmd: merge::ReviewCommand::Approve { session, as_principal } }, cli.json)
				}
				ReviewTop::Reject { session, reason } => {
					merge::run(&cfg, merge::MergeCommand::Review { cmd: merge::ReviewCommand::Reject { session, reason } }, cli.json)
				}
				ReviewTop::Hook(inner) => review::run(&cfg, inner, cli.json),
			}
		}
		Command::Revert { target } => {
			let cfg = load_config(&cli.repo)?;
			merge::run(&cfg, merge::MergeCommand::Revert { target }, cli.json)
		}
		Command::Doctor => {
			let cfg = load_config(&cli.repo)?;
			merge::run(&cfg, merge::MergeCommand::Doctor, cli.json)
		}
		Command::Tick => {
			let cfg = load_config(&cli.repo)?;
			merge::run(&cfg, merge::MergeCommand::Tick, cli.json)
		}
		Command::Gate(cmd) => {
			let cfg = load_config(&cli.repo)?;
			gate::run(&cfg, cmd, cli.json)
		}
		Command::Policy(cmd) => {
			let cfg = load_config(&cli.repo)?;
			policy::run(&cfg, cmd, cli.json)
		}
		Command::Run(cmd) => {
			let cfg = load_config(&cli.repo)?;
			run::run(&cfg, cmd, cli.json)
		}
		Command::Transcript(cmd) => {
			let cfg = load_config(&cli.repo)?;
			transcript::run(&cfg, cmd, cli.json)
		}
		Command::Session(cmd) => {
			let cfg = load_config(&cli.repo)?;
			session::run(&cfg, cmd, cli.json)
		}
		Command::Sessions { all } => {
			let cfg = load_config(&cli.repo)?;
			session::run(&cfg, session::SessionCommand::List { all }, cli.json)
		}
		Command::Eod => {
			let cfg = load_config(&cli.repo)?;
			session::run(&cfg, session::SessionCommand::Eod, cli.json)
		}
	}
}
