//! SSH plumbing for `bracket connect`. Everything bracket owns lives under `~/.ssh/bracket/`:
//!
//! - `id_ed25519` and `id_ed25519.pub`: generated once. EC2 Instance Connect trusts a pushed key
//!   for only 60 seconds, so `bracket ssh-proxy` pushes it again on every connection. That's what
//!   lets Cursor reconnect hours later without going back through bracket.
//! - `hosts/<alias>`: one `Host` block per instance, rewritten on every connect.
//! - `known_hosts`: host keys pinned to the instance id through `HostKeyAlias`, so the new IP an
//!   instance gets after a restart isn't mistaken for a new host.
//!
//! The only change outside that directory is a single `Include` line at the top of
//! `~/.ssh/config`, so OpenSSH, and Cursor through it, can see the hosts.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::error::{Error, Result};

/// Amazon Linux's default login user.
pub const USER: &str = "ec2-user";

/// The line bracket adds to `~/.ssh/config`. It has to come before any `Host` block,
/// because an `Include` inside one only applies to that host.
const INCLUDE: &str = "Include ~/.ssh/bracket/hosts/*";

/// Where bracket's SSH files live, rooted at a `.ssh` directory.
pub struct Paths {
    ssh: PathBuf,
}

impl Paths {
    /// Paths under the current user's `~/.ssh`.
    pub fn from_home() -> Result<Self> {
        dirs::home_dir()
            .map(|home| Paths::at(home.join(".ssh")))
            .ok_or_else(|| Error::Unexpected("could not find your home directory".into()))
    }

    /// Paths under an arbitrary `.ssh` directory.
    pub fn at(ssh: PathBuf) -> Self {
        Paths { ssh }
    }

    fn root(&self) -> PathBuf {
        self.ssh.join("bracket")
    }

    fn key(&self) -> PathBuf {
        self.root().join("id_ed25519")
    }

    pub fn public_key(&self) -> PathBuf {
        self.root().join("id_ed25519.pub")
    }

    fn hosts(&self) -> PathBuf {
        self.root().join("hosts")
    }

    fn config(&self) -> PathBuf {
        self.ssh.join("config")
    }
}

/// The SSH host alias for an instance name: `Dev Box` becomes `bracket-dev-box`.
///
/// OpenSSH lowercases host names before matching `Host` lines, and the alias doubles as a file
/// name and part of a `vscode-remote://` URI, so anything beyond `[a-z0-9._-]` becomes `-`.
///
/// ```ignore
/// assert_eq!(ssh::alias("Dev Box"), "bracket-dev-box");
/// ```
pub fn alias(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .collect();
    format!("bracket-{slug}")
}

/// Everything needed to reach one instance through `bracket ssh-proxy`.
pub struct Host<'a> {
    pub alias: &'a str,
    pub instance_id: &'a str,
    pub region: &'a str,
    pub profile: Option<&'a str>,
    /// The bracket binary that ssh runs as the ProxyCommand.
    pub bracket: &'a Path,
}

impl Host<'_> {
    /// Renders the `Host` block. The instance id stands in for the host name: the proxy looks up
    /// the live address on each connection, so a restart that changes the IP needs no rewrite.
    fn block(&self) -> String {
        // ProxyCommand runs through a shell and expands `%` tokens, so quote each argument and
        // escape `%`.
        let quote = |arg: &str| format!("\"{}\"", arg.replace('%', "%%"));
        let bracket = quote(&self.bracket.to_string_lossy());
        let profile = self
            .profile
            .map(|profile| format!(" --profile {}", quote(profile)))
            .unwrap_or_default();
        format!(
            "# Written by `bracket connect ec2`, which rewrites it on every connect.\n\
             Host {alias}\n  \
               HostName {id}\n  \
               User {USER}\n  \
               IdentityFile ~/.ssh/bracket/id_ed25519\n  \
               IdentitiesOnly yes\n  \
               HostKeyAlias {id}\n  \
               StrictHostKeyChecking accept-new\n  \
               UserKnownHostsFile ~/.ssh/bracket/known_hosts\n  \
               ProxyCommand {bracket} ssh-proxy {id} --region {region}{profile}\n",
            alias = self.alias,
            id = self.instance_id,
            region = quote(self.region),
        )
    }
}

/// Returns `config` with bracket's `Include` prepended, or `None` if it's already there.
fn with_include(config: &str) -> Option<String> {
    let present = config.lines().any(|line| line.trim() == INCLUDE);
    (!present).then(|| format!("{INCLUDE}\n\n{config}"))
}

/// Writes everything `ssh <alias>` needs: the key pair, the host's block, and the `Include`.
/// Safe to run repeatedly; the host block is replaced and nothing else is duplicated.
pub fn install(paths: &Paths, host: &Host) -> Result<()> {
    create_private_dir(&paths.hosts())?;
    ensure_key(paths)?;
    fs::write(paths.hosts().join(host.alias), host.block())?;
    ensure_include(paths)
}

/// Creates `dir` and any missing parents, readable only by the user on Unix, as ssh expects.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Generates the key pair unless both halves already exist.
fn ensure_key(paths: &Paths) -> Result<()> {
    let (key, public) = (paths.key(), paths.public_key());
    if key.exists() && public.exists() {
        return Ok(());
    }
    // Half a pair is useless, and ssh-keygen would stop to ask before overwriting it.
    [&key, &public]
        .into_iter()
        .try_for_each(|path| match fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        })?;

    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "bracket", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .status()
        .map_err(Error::launch("ssh-keygen", "Install OpenSSH."))?;
    match status.success() {
        true => Ok(()),
        false => Err(Error::CommandFailed {
            command: "ssh-keygen".into(),
            status,
        }),
    }
}

/// Adds the `Include` to `~/.ssh/config`, creating the file (mode 600) if needed. Writing in
/// place keeps the file's permissions and, if it's a symlink into a dotfiles repo, the link.
fn ensure_include(paths: &Paths) -> Result<()> {
    let path = paths.config();
    let current = match fs::read_to_string(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        read => read?,
    };
    let Some(updated) = with_include(&current) else {
        return Ok(());
    };
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // Only applies when the file is created; an existing file keeps its mode.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(updated.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host<'a>(bracket: &'a Path, profile: Option<&'a str>) -> Host<'a> {
        Host {
            alias: "bracket-dev",
            instance_id: "i-0abc",
            region: "us-east-2",
            profile,
            bracket,
        }
    }

    #[test]
    fn aliases_are_safe_lowercase_slugs() {
        assert_eq!(alias("dev-box"), "bracket-dev-box");
        assert_eq!(alias("Dev Box"), "bracket-dev-box");
        assert_eq!(alias("api_v2.staging"), "bracket-api_v2.staging");
        assert_eq!(alias("../etc/passwd"), "bracket-..-etc-passwd");
        assert_eq!(alias("café"), "bracket-caf-");
        assert!(!alias("a/b\\c d").contains(['/', '\\', ' ']));
    }

    #[test]
    fn host_block_routes_through_the_proxy() {
        let block = host(Path::new("/usr/local/bin/bracket"), None).block();
        assert_eq!(
            block,
            "# Written by `bracket connect ec2`, which rewrites it on every connect.\n\
             Host bracket-dev\n  \
               HostName i-0abc\n  \
               User ec2-user\n  \
               IdentityFile ~/.ssh/bracket/id_ed25519\n  \
               IdentitiesOnly yes\n  \
               HostKeyAlias i-0abc\n  \
               StrictHostKeyChecking accept-new\n  \
               UserKnownHostsFile ~/.ssh/bracket/known_hosts\n  \
               ProxyCommand \"/usr/local/bin/bracket\" ssh-proxy i-0abc --region \"us-east-2\"\n"
        );
    }

    #[test]
    fn host_block_quotes_awkward_paths_and_passes_the_profile() {
        let block = host(Path::new("/Users/a b/100%/bracket"), Some("dev team")).block();
        assert!(block.contains(
            "ProxyCommand \"/Users/a b/100%%/bracket\" ssh-proxy i-0abc --region \"us-east-2\" --profile \"dev team\"\n"
        ));
    }

    #[test]
    fn include_is_prepended_once() {
        let config = "Host github.com\n  User git\n";
        let updated = with_include(config).unwrap();
        assert_eq!(updated, format!("{INCLUDE}\n\n{config}"));
        assert_eq!(with_include(&updated), None);
        assert_eq!(with_include(&format!("  {INCLUDE}  \n")), None);
        assert_eq!(with_include("").unwrap(), format!("{INCLUDE}\n\n"));
    }

    #[test]
    fn install_writes_everything_ssh_needs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join(".ssh"));
        let bracket = Path::new("/bin/bracket");

        install(&paths, &host(bracket, None)).unwrap();

        let block = fs::read_to_string(paths.hosts().join("bracket-dev")).unwrap();
        assert!(block.contains("Host bracket-dev\n"));
        let public = fs::read_to_string(paths.public_key()).unwrap();
        assert!(public.starts_with("ssh-ed25519 ") && public.trim_end().ends_with(" bracket"));
        // EC2 Instance Connect rejects keys shorter than 80 characters.
        assert!(public.trim().len() >= 80);
        assert_eq!(
            fs::read_to_string(paths.config()).unwrap(),
            format!("{INCLUDE}\n\n")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&paths.config()), 0o600);
            assert_eq!(mode(&paths.root()), 0o700);
            assert_eq!(mode(&paths.hosts()), 0o700);
            assert_eq!(mode(&paths.key()), 0o600);
        }
    }

    #[test]
    fn reinstalling_keeps_the_key_and_the_users_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join(".ssh"));
        create_private_dir(&dir.path().join(".ssh")).unwrap();
        fs::write(paths.config(), "Host github.com\n  User git\n").unwrap();
        let bracket = Path::new("/bin/bracket");

        install(&paths, &host(bracket, None)).unwrap();
        let key = fs::read_to_string(paths.public_key()).unwrap();
        let config = fs::read_to_string(paths.config()).unwrap();

        let mut moved = host(bracket, Some("dev"));
        moved.instance_id = "i-0def";
        install(&paths, &moved).unwrap();

        assert_eq!(fs::read_to_string(paths.public_key()).unwrap(), key);
        assert_eq!(fs::read_to_string(paths.config()).unwrap(), config);
        assert!(config.ends_with("Host github.com\n  User git\n"));
        let block = fs::read_to_string(paths.hosts().join("bracket-dev")).unwrap();
        assert!(block.contains("HostName i-0def") && !block.contains("i-0abc"));
        assert_eq!(fs::read_dir(paths.hosts()).unwrap().count(), 1);
    }

    #[test]
    fn a_half_written_key_pair_is_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().to_path_buf());
        create_private_dir(&paths.root()).unwrap();
        fs::write(paths.key(), "stale").unwrap();

        ensure_key(&paths).unwrap();

        assert_ne!(fs::read_to_string(paths.key()).unwrap(), "stale");
        assert!(paths.public_key().exists());
    }
}
