use clap::{Parser, Subcommand};
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

mod state;
mod server;
mod capture;

#[derive(Parser, Debug)]
#[command(name = "typervox", about = "Voice keyboard daemon and CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
enum Command {
    Daemon {
        #[arg(long, default_value_os = "/tmp/typervox.sock")]
        socket: PathBuf,
    },
    Status,
}

fn parse_cli<I, T>(args: I) -> Cli
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::parse_from(args)
}

#[tokio::main]
async fn main() {
    let cli = parse_cli(std::env::args_os());

    match cli.command {
        Command::Daemon { socket } => {
            let queue = Arc::new(Mutex::new(state::QueueState::default()));
            let router = server::router(queue);
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            if let Err(err) = server::serve_unix(socket, router, shutdown).await {
                eprintln!("server error: {err}");
                std::process::exit(1);
            }
        }
        Command::Status => {
            // Status command will be implemented in later milestones.
            println!("status command is not yet implemented");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_daemon_subcommand() {
        let cli = parse_cli(["typervox", "daemon"]);
        assert_eq!(
            cli.command,
            Command::Daemon {
                socket: PathBuf::from("/tmp/typervox.sock")
            }
        );
    }

    #[test]
    fn parses_status_subcommand() {
        let cli = parse_cli(["typervox", "status"]);
        assert_eq!(cli.command, Command::Status);
    }
}
