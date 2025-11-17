use clap::{Parser, Subcommand};
use std::ffi::OsString;

#[derive(Parser, Debug)]
#[command(name = "typervox", about = "Voice keyboard daemon and CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
enum Command {
    Daemon,
    Status,
}

fn parse_cli<I, T>(args: I) -> Cli
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::parse_from(args)
}

fn main() {
    let cli = parse_cli(std::env::args_os());

    match cli.command {
        Command::Daemon => {
            // Daemon logic arrives in later milestones.
        }
        Command::Status => {
            // Status logic arrives in later milestones.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_daemon_subcommand() {
        let cli = parse_cli(["typervox", "daemon"]);
        assert_eq!(cli.command, Command::Daemon);
    }

    #[test]
    fn parses_status_subcommand() {
        let cli = parse_cli(["typervox", "status"]);
        assert_eq!(cli.command, Command::Status);
    }
}
