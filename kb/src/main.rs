use clap::Parser;

fn main() {
    let cli = kb::cli::Cli::parse();
    match kb::cli::run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
