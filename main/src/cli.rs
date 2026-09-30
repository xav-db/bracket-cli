//! Command-line surface. Every leaf subcommand maps onto exactly one handler in `main`.

use clap::{Args, Parser, Subcommand};

/// Connect to, list, and manage Bracket's AWS dev infrastructure.
///
/// Uses your standard AWS credentials and region (`AWS_PROFILE`, `AWS_REGION`, `~/.aws`).
#[derive(Debug, Parser)]
#[command(name = "bracket", version, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open a dev box in Cursor over SSH, starting it first if needed.
    Connect {
        #[command(subcommand)]
        target: ConnectTarget,
    },

    /// Stop a running resource.
    Stop {
        #[command(subcommand)]
        target: StopTarget,
    },

    /// List resources in the current AWS region.
    #[command(visible_alias = "ls")]
    List {
        #[command(subcommand)]
        target: ListTarget,
    },

    /// Set up local credentials.
    Config {
        #[command(subcommand)]
        target: ConfigTarget,
    },

    /// Manage App Runner services.
    AppRunner {
        #[command(subcommand)]
        command: AppRunnerCommand,
    },

    /// Update bracket to the latest release.
    Update,

    /// ProxyCommand used by the SSH host entries `bracket connect` writes.
    #[command(hide = true)]
    SshProxy(SshProxyArgs),
}

#[derive(Debug, Subcommand)]
pub enum ConnectTarget {
    /// Open an EC2 instance in Cursor.
    Ec2 {
        /// The instance's Name tag. Omit it to pick from a list.
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum StopTarget {
    /// Stop an EC2 instance.
    Ec2 {
        /// The instance's Name tag. Omit it to pick from a list.
        name: Option<String>,
    },

    /// Stop a Neptune cluster.
    Neptune {
        /// The cluster identifier. Omit it to pick from a list.
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ListTarget {
    /// List EC2 instances with their state and CPU.
    Ec2,

    /// List Neptune clusters with their status and CPU.
    Neptune,
}

#[derive(Debug, Subcommand)]
pub enum ConfigTarget {
    /// Run `aws configure`, then check the credentials work.
    Aws,
}

#[derive(Debug, Subcommand)]
pub enum AppRunnerCommand {
    /// Start a new deployment and follow it until it finishes.
    Redeploy {
        /// The service name. Omit it to pick from a list.
        name: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct SshProxyArgs {
    pub instance_id: String,

    #[arg(long)]
    pub region: String,

    #[arg(long)]
    pub profile: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn names_are_optional() {
        let cli = Cli::parse_from(["bracket", "connect", "ec2"]);
        assert!(matches!(
            cli.command,
            Command::Connect {
                target: ConnectTarget::Ec2 { name: None }
            }
        ));

        let cli = Cli::parse_from(["bracket", "stop", "neptune", "graph"]);
        let Command::Stop {
            target: StopTarget::Neptune { name },
        } = cli.command
        else {
            panic!("expected stop neptune, got {:?}", cli.command);
        };
        assert_eq!(name.as_deref(), Some("graph"));
    }

    #[test]
    fn ls_is_an_alias_for_list() {
        let cli = Cli::parse_from(["bracket", "ls", "ec2"]);
        assert!(matches!(
            cli.command,
            Command::List {
                target: ListTarget::Ec2
            }
        ));
    }

    #[test]
    fn ssh_proxy_requires_a_region() {
        assert!(Cli::try_parse_from(["bracket", "ssh-proxy", "i-0abc"]).is_err());

        let cli = Cli::parse_from([
            "bracket",
            "ssh-proxy",
            "i-0abc",
            "--region",
            "us-east-2",
            "--profile",
            "dev",
        ]);
        let Command::SshProxy(args) = cli.command else {
            panic!("expected ssh-proxy, got {:?}", cli.command);
        };
        assert_eq!(args.instance_id, "i-0abc");
        assert_eq!(args.region, "us-east-2");
        assert_eq!(args.profile.as_deref(), Some("dev"));
    }

    #[test]
    fn ssh_proxy_is_hidden_from_help() {
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("ssh-proxy"));
        assert!(help.contains("connect"));
    }
}
