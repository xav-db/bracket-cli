//! `bracket connect ec2`, and `bracket ssh-proxy`, the ProxyCommand behind every host entry
//! it writes.

use std::{
    fs, io,
    process::{Command, Stdio},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, ToSocketAddrs},
    time::Instant,
};

use crate::{
    aws::{self, Aws},
    cli::SshProxyArgs,
    ec2,
    error::{Error, Result},
    ssh, ui,
};

/// How long a freshly started box gets to bring sshd up.
const SSH_READY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SSH_RETRY: Duration = Duration::from_secs(3);
const TCP_TIMEOUT: Duration = Duration::from_secs(10);

/// `bracket connect ec2`: start the box if needed, wire up SSH, check it works, open Cursor.
pub async fn connect(name: Option<String>) -> Result<()> {
    ui::intro("connect")?;
    let aws = Aws::load().await?;
    let ec2 = aws_sdk_ec2::Client::new(&aws.config);
    let instance = ec2::pick(&ec2, name, "Which instance do you want to open?").await?;
    let booted = ec2::ensure_running(&ec2, &instance).await?;

    let alias = ssh::alias(instance.display_name());
    let bracket = std::env::current_exe()?;
    let profile = std::env::var("AWS_PROFILE").ok();
    ssh::install(
        &ssh::Paths::from_home()?,
        &ssh::Host {
            alias: &alias,
            instance_id: &instance.id,
            region: &aws.region,
            profile: profile.as_deref(),
            bracket: &bracket,
        },
    )?;

    // A box that was already up answers first time. One that just booted needs a moment for
    // sshd, and handing Cursor a half-booted box only gets you its connection error dialog.
    let started = Instant::now();
    let deadline = booted.then(|| started + SSH_READY_TIMEOUT);
    ui::step(
        format!("Checking SSH to {alias}"),
        wait_for_ssh(&alias, deadline),
        |_| format!("SSH is ready ({})", ui::elapsed(started.elapsed())),
    )
    .await?;

    let status = Command::new("cursor")
        .arg("--folder-uri")
        .arg(format!("vscode-remote://ssh-remote+{alias}/home/{}", ssh::USER))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(Error::launch(
            "cursor",
            "In Cursor, run \"Shell Command: Install 'cursor' command in PATH\" from the command palette.",
        ))?;
    if !status.success() {
        return Err(Error::CommandFailed {
            command: "cursor".into(),
            status,
        });
    }

    cliclack::log::remark(format!("From a terminal: ssh {alias}"))?;
    cliclack::outro(format!("Opened {} in Cursor", instance.display_name()))?;
    Ok(())
}

/// Runs `ssh <alias> true` until it succeeds. Without a deadline it gets one attempt.
async fn wait_for_ssh(alias: &str, deadline: Option<Instant>) -> Result<()> {
    loop {
        let output = tokio::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                alias,
                "true",
            ])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(Error::launch("ssh", "Install OpenSSH."))?;
        if output.status.success() {
            return Ok(());
        }
        match deadline {
            Some(deadline) if Instant::now() < deadline => tokio::time::sleep(SSH_RETRY).await,
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let reason = stderr
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .map_or_else(|| output.status.to_string(), |line| line.trim().to_string());
                return Err(Error::Ssh {
                    alias: alias.to_string(),
                    reason,
                });
            }
        }
    }
}

/// `bracket ssh-proxy`: authorise our key for the next 60 seconds, then carry ssh's stdio to
/// the instance. Public instances get a direct TCP connection; private ones go through an EC2
/// Instance Connect Endpoint via `aws ec2-instance-connect open-tunnel`.
///
/// ssh shows this command's stderr to the user, so errors are plain lines, with no Clack chrome.
pub async fn proxy(args: SshProxyArgs) -> Result<()> {
    let loader = aws_config::from_env().region(aws_config::Region::new(args.region.clone()));
    let config = match &args.profile {
        Some(profile) => loader.profile_name(profile),
        None => loader,
    }
    .load()
    .await;
    let public_key = fs::read_to_string(ssh::Paths::from_home()?.public_key())?;
    let ec2 = aws_sdk_ec2::Client::new(&config);
    let instance_connect = aws_sdk_ec2instanceconnect::Client::new(&config);

    // Finding the address and pushing the key don't depend on each other.
    let (described, _) = tokio::try_join!(
        async {
            ec2.describe_instances()
                .instance_ids(&args.instance_id)
                .send()
                .await
                .map_err(Error::aws("describing the instance"))
        },
        async {
            instance_connect
                .send_ssh_public_key()
                .instance_id(&args.instance_id)
                .instance_os_user(ssh::USER)
                .ssh_public_key(public_key.trim())
                .send()
                .await
                .map_err(Error::aws("pushing the SSH key"))
        },
    )?;

    let public_ip = described
        .reservations()
        .iter()
        .flat_map(|reservation| reservation.instances())
        .find_map(|instance| instance.public_ip_address());
    match public_ip {
        Some(ip) => {
            let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
            Ok(pipe(stdio, (ip, 22)).await?)
        }
        None => tunnel(&args),
    }
}

/// Copies bytes both ways between `stdio` and a TCP connection to `addr` until both sides close.
async fn pipe(
    mut stdio: impl AsyncRead + AsyncWrite + Unpin,
    addr: impl ToSocketAddrs,
) -> io::Result<()> {
    let mut stream = tokio::time::timeout(TCP_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "timed out connecting to port 22")
        })??;
    stream.set_nodelay(true)?;
    tokio::io::copy_bidirectional(&mut stdio, &mut stream).await?;
    Ok(())
}

/// Hands stdio to the AWS CLI's Instance Connect Endpoint tunnel for instances without a public IP.
fn tunnel(args: &SshProxyArgs) -> Result<()> {
    let status = Command::new("aws")
        .args(["ec2-instance-connect", "open-tunnel", "--instance-id"])
        .arg(&args.instance_id)
        .args(["--region", &args.region])
        .args(
            args.profile
                .iter()
                .flat_map(|profile| ["--profile", profile]),
        )
        .status()
        .map_err(Error::launch("aws", aws::INSTALL_CLI))?;
    match status.success() {
        true => Ok(()),
        false => Err(Error::CommandFailed {
            command: "aws ec2-instance-connect open-tunnel".into(),
            status,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn pipe_carries_bytes_both_ways_until_both_sides_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            socket.read_to_end(&mut request).await.unwrap();
            socket.write_all(b"SSH-2.0-test\r\n").await.unwrap();
            request
        });

        let (mut ssh, stdio) = tokio::io::duplex(64);
        let proxy = tokio::spawn(pipe(stdio, addr));
        ssh.write_all(b"SSH-2.0-client\r\n").await.unwrap();
        ssh.shutdown().await.unwrap();
        let mut reply = Vec::new();
        ssh.read_to_end(&mut reply).await.unwrap();

        proxy.await.unwrap().unwrap();
        assert_eq!(server.await.unwrap(), b"SSH-2.0-client\r\n");
        assert_eq!(reply, b"SSH-2.0-test\r\n");
    }

    #[tokio::test]
    async fn pipe_reports_unreachable_hosts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (_ssh, stdio) = tokio::io::duplex(64);
        assert!(pipe(stdio, addr).await.is_err());
    }
}
