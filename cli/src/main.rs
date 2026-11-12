/// Run the CLI and preserve its conventional process exit code.
fn main() {
    std::process::exit(copyrust_cli::Cli::run(std::env::args_os()));
}
