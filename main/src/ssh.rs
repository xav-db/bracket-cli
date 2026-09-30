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

/// The alias to use for `instance_id`: [`alias`] of its name, unless that host already belongs to
/// another instance (the same Name tag in another region, say). Then the id is appended, so
/// connecting to one never silently repoints the other's Cursor window.
pub fn alias_for(paths: &Paths, name: &str, instance_id: &str) -> String {
    let alias = alias(name);
    let owner = format!("HostKeyAlias {instance_id}");
    match fs::read_to_string(paths.hosts().join(&alias)) {
        Ok(block) if !block.lines().any(|line| line.trim() == owner) => {
            format!("{alias}-{instance_id}")
        }
        _ => alias,
    }
}

/// Quotes one ProxyCommand argument. ssh expands `%` tokens first, then runs the line with
/// `/bin/sh -c` on Unix, where single quotes stop every expansion, or directly on Windows.
fn quote(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    match cfg!(windows) {
        true => format!("\"{arg}\""),
        false => format!("'{}'", arg.replace('\'', r"'\''")),
    }
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
    // OpenSSH refuses to run at all if an included file is group- or world-writable, which a
    // umask of 002 would otherwise produce. So host files are always written as 600.
    write_atomically(&paths.hosts().join(host.alias), &host.block(), None)?;
    ensure_include(paths)
}

/// Replaces `path` with `contents` in one step, so a crash or full disk mid-write leaves the old
/// file intact. The result gets `permissions`, or mode 600 when that's `None`.
fn write_atomically(
    path: &Path,
    contents: &str,
    permissions: Option<fs::Permissions>,
) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} is not a file path", path.display())))?;
    let temp = path.with_file_name(format!(".{}.bracket-tmp", name.to_string_lossy()));
    // A leftover temp file could carry any mode; start fresh so ours applies.
    if let Err(err) = fs::remove_file(&temp)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    file.write_all(contents.as_bytes())?;
    permissions.map_or(Ok(()), |permissions| file.set_permissions(permissions))?;
    file.sync_all()?;
    fs::rename(&temp, path)
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

/// Adds the `Include` to `~/.ssh/config`, creating it (mode 600) if needed.
///
/// The file is replaced atomically so it can never be left half-written, keeps its permissions,
/// and, when it's a symlink into a dotfiles repo, is written through the link.
fn ensure_include(paths: &Paths) -> Result<()> {
    let (target, current, permissions) = match fs::canonicalize(paths.config()) {
        Ok(target) => {
            let current = fs::read_to_string(&target)?;
            let permissions = fs::metadata(&target)?.permissions();
            (target, current, Some(permissions))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => (paths.config(), String::new(), None),
        Err(err) => return Err(err.into()),
    };
    let Some(updated) = with_include(&current) else {
        return Ok(());
    };
    write_atomically(&target, &updated, permissions)?;
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

    #[cfg(unix)]
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
               ProxyCommand '/usr/local/bin/bracket' ssh-proxy i-0abc --region 'us-east-2'\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn proxy_arguments_survive_the_shell() {
        assert_eq!(quote("/usr/bin/bracket"), "'/usr/bin/bracket'");
        assert_eq!(
            quote("/Users/a b/100%/$HOME/`x`"),
            "'/Users/a b/100%%/$HOME/`x`'"
        );
        assert_eq!(quote("it's"), r"'it'\''s'");

        // Round-trip through a real shell to prove nothing expands.
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "printf %s {}",
                quote("a b $HOME `id` \\ \" it's").replace("%%", "%")
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "a b $HOME `id` \\ \" it's"
        );
    }

    #[test]
    fn host_block_passes_the_profile() {
        let block = host(Path::new("/bin/bracket"), Some("dev team")).block();
        let proxy = block
            .lines()
            .find(|line| line.contains("ProxyCommand"))
            .unwrap();
        assert!(proxy.ends_with(&format!(
            "ssh-proxy i-0abc --region {} --profile {}",
            quote("us-east-2"),
            quote("dev team")
        )));
    }

    #[test]
    fn aliases_stay_with_their_instance() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().to_path_buf());
        assert_eq!(alias_for(&paths, "dev", "i-1"), "bracket-dev");

        install(
            &paths,
            &Host {
                alias: "bracket-dev",
                instance_id: "i-1",
                ..host(Path::new("/b"), None)
            },
        )
        .unwrap();
        assert_eq!(alias_for(&paths, "dev", "i-1"), "bracket-dev");
        assert_eq!(alias_for(&paths, "Dev", "i-2"), "bracket-dev-i-2");
        // An id that merely starts with the owner's id is a different instance.
        assert_eq!(alias_for(&paths, "dev", "i-10"), "bracket-dev-i-10");
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

    #[cfg(unix)]
    #[test]
    fn host_files_are_private_even_if_they_were_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().to_path_buf());
        create_private_dir(&paths.hosts()).unwrap();
        let file = paths.hosts().join("bracket-dev");
        fs::write(&file, "old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o664)).unwrap();

        install(&paths, &host(Path::new("/b"), None)).unwrap();

        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!paths.hosts().join(".bracket-dev.bracket-tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_keeps_its_link_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join(".ssh"));
        create_private_dir(&dir.path().join(".ssh")).unwrap();
        let dotfiles = dir.path().join("dotfiles-ssh-config");
        fs::write(&dotfiles, "Host *\n  AddKeysToAgent yes\n").unwrap();
        fs::set_permissions(&dotfiles, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&dotfiles, paths.config()).unwrap();

        install(&paths, &host(Path::new("/b"), None)).unwrap();

        assert!(
            fs::symlink_metadata(paths.config())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let config = fs::read_to_string(&dotfiles).unwrap();
        assert_eq!(
            config,
            format!("{INCLUDE}\n\nHost *\n  AddKeysToAgent yes\n")
        );
        assert_eq!(
            fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn stale_temp_files_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file");
        fs::write(dir.path().join(".file.bracket-tmp"), "junk").unwrap();

        write_atomically(&target, "fresh", None).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "fresh");
        assert!(!dir.path().join(".file.bracket-tmp").exists());
    }
}
