use clap::Parser;

fn main() {
	// sqlite-vec must be registered before any artifact is opened for vector search
	kb::index::vectors::register_extension();
	let cli = kb::cli::Cli::parse();
	match kb::cli::run(cli) {
		Ok(code) => std::process::exit(code),
		Err(e) => {
			eprintln!("error: {e:#}");
			std::process::exit(1);
		}
	}
}
