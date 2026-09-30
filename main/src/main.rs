//! `bracket`: connect to, list, and manage Bracket's AWS dev infrastructure.

mod app_runner;
mod aws;
mod cli;
mod connect;
mod ec2;
mod error;
mod metrics;
mod neptune;
mod ssh;
mod state;
mod ui;
mod update;

use std::process::ExitCode;

use clap::Parser;

use cli::{AppRunnerCommand, Cli, Command, ConfigTarget, ConnectTarget, ListTarget, StopTarget};
use error::Error;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let result = match Cli::parse().command {
        // ssh owns this process's stdio, so exit directly: nothing may print after the pipe
        // closes, and a lingering blocking stdin read must not hold the runtime open.
        Command::SshProxy(args) => std::process::exit(match connect::proxy(args).await {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("bracket ssh-proxy: {err}");
                255
            }
        }),
        Command::Connect {
            target: ConnectTarget::Ec2 { name },
        } => connect::connect(name).await,
        Command::Stop {
            target: StopTarget::Ec2 { name },
        } => ec2::stop(name).await,
        Command::Stop {
            target: StopTarget::Neptune { name },
        } => neptune::stop(name).await,
        Command::List {
            target: ListTarget::Ec2,
        } => ec2::list().await,
        Command::List {
            target: ListTarget::Neptune,
        } => neptune::list().await,
        Command::Config {
            target: ConfigTarget::Aws,
        } => aws::configure().await,
        Command::AppRunner {
            command: AppRunnerCommand::Redeploy { name },
        } => app_runner::redeploy(name).await,
        Command::Update => update::update().await,
    };

    let Err(err) = result else {
        return ExitCode::SUCCESS;
    };
    // Reporting is best effort: if stderr is gone there's nobody left to tell.
    let _ = match &err {
        Error::Cancelled => cliclack::outro_cancel("Cancelled"),
        _ => err
            .hint()
            .map_or(Ok(()), cliclack::log::info)
            .and_then(|()| cliclack::outro_cancel(&err)),
    };
    ExitCode::FAILURE
}
