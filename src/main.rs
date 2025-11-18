use clap::{Parser, Subcommand};
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

mod capture;
mod client;
mod engine;
mod server;
mod state;

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
    Status {
        #[arg(long, default_value_os = "/tmp/typervox.sock")]
        socket: PathBuf,
    },
    Start {
        #[arg(long)]
        app: String,
        #[arg(long, default_value = "")]
        hint: String,
        #[arg(long, default_value_os = "/tmp/typervox.sock")]
        socket: PathBuf,
        #[arg(long, default_value_t = true)]
        stream: bool,
    },
    StopActive {
        #[arg(long, default_value_os = "/tmp/typervox.sock")]
        socket: PathBuf,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
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
        Command::Status { socket } => match client::get_status(&socket).await {
            Ok(status) => {
                println!("{}", serde_json::to_string(&status).unwrap());
            }
            Err(err) => {
                eprintln!("status failed: {err}");
                std::process::exit(1);
            }
        },
        Command::Start {
            app,
            hint,
            socket,
            stream: _,
        } => {
            if let Err(err) = client::start_stream(&socket, &app, &hint).await {
                eprintln!("start failed: {err}");
                std::process::exit(1);
            }
        }
        Command::StopActive { socket, raw } => match client::stop_active(&socket, raw).await {
            Ok(body) => {
                print!("{body}");
            }
            Err(err) => {
                eprintln!("stop_active failed: {err}");
                std::process::exit(1);
            }
        },
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
        assert_eq!(
            cli.command,
            Command::Status {
                socket: PathBuf::from("/tmp/typervox.sock")
            }
        );
    }
}
