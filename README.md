# bracket

Connect to, list, and manage Bracket's AWS dev infrastructure from the terminal.

## Install

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/xav-db/bracket-cli/main/install/install.sh | bash
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/xav-db/bracket-cli/main/install/install.ps1 | iex
```

Run `bracket update` to upgrade later.

## Setup

bracket uses your normal AWS credentials and region, so whatever works for the AWS CLI works here: `AWS_PROFILE`, `AWS_REGION`, SSO, or `~/.aws`. Starting from scratch? Run `bracket config aws`.

`bracket connect` also needs:

- **Cursor** with its `cursor` shell command. In Cursor's command palette, run *Shell Command: Install 'cursor' command in PATH*.
- **The AWS CLI**, for instances without a public IP. bracket reaches them through an EC2 Instance Connect Endpoint.

## Commands

| Command | What it does |
| --- | --- |
| `bracket connect ec2 [name]` | Starts the instance if needed, waits for SSH, and opens it in Cursor. |
| `bracket list ec2` | Asks which region to show (only regions with instances, starting on the one you picked last), then lists instances with state, type, CPU, and IP. `bracket ls ec2` does the same. |
| `bracket usage ec2` | Opens a dashboard of CPU and estimated cost over the last 24h, 7d, or 14d, by instance, region, or family. |
| `bracket list neptune` | Lists Neptune clusters with status, size, and CPU. |
| `bracket stop ec2 [name]` | Stops an instance. |
| `bracket stop neptune [name]` | Stops a Neptune cluster. |
| `bracket app-runner redeploy [name]` | Starts a deployment and follows it until it finishes. |
| `bracket config aws` | Runs `aws configure`, then checks that the credentials work. |
| `bracket update` | Installs the latest release over the current binary. |

Leave out `[name]` to pick from a list. Names are EC2 `Name` tags, Neptune cluster identifiers, and App Runner service names.

## How `usage` estimates cost

A running instance publishes a CPU datapoint to CloudWatch every 5 minutes, so each datapoint also counts 5 minutes of running time. Cost is that running time multiplied by the instance type's on-demand Linux price, which comes from the AWS Price List API and is cached for a week. The estimate leaves out EBS, data transfer, and Reserved Instance or Savings Plan discounts, counts spot instances at the on-demand price, and only covers instances that still exist.

## How `connect` works

`bracket connect ec2 dev-box` writes an SSH host called `bracket-dev-box` to `~/.ssh/bracket/hosts/`, and adds one `Include` line to the top of `~/.ssh/config` so OpenSSH and Cursor can find it.

The host's `ProxyCommand` is `bracket ssh-proxy`. On every connection it pushes bracket's key (`~/.ssh/bracket/id_ed25519`) through EC2 Instance Connect, then carries the connection to the instance's public IP, or through an Instance Connect Endpoint when the instance has none. Pushed keys expire after 60 seconds, so this is what lets Cursor reconnect hours later and `ssh bracket-dev-box` work from any terminal.

Host keys are pinned to the instance ID, so a restart that changes the IP doesn't trigger a host-key warning.

## Development

```bash
cd main
cargo test
cargo clippy --all-targets -- -D warnings
./build.sh   # install this checkout into ~/.cargo/bin
```

To release, bump `version` in `main/Cargo.toml` and merge to `main`. CI builds every platform and publishes `v<version>`.
