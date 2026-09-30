//! `bracket update`: compare against the latest GitHub release and reinstall in place if behind.

use std::process::Command;

use semver::Version;

use crate::{
    error::{Error, Result},
    ui,
};

const REPO: &str = "xav-db/bracket-cli";

/// `bracket update`.
pub async fn update() -> Result<()> {
    ui::intro("update")?;
    let current =
        Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml's version is valid semver");
    let latest = ui::step("Checking for a new release", latest_release(), |latest| {
        format!("Latest release is v{latest}")
    })
    .await?;

    if latest <= current {
        cliclack::outro(format!("You're up to date on v{current}"))?;
        return Ok(());
    }

    // Reinstall over this binary, wherever it lives, rather than wherever the installer defaults to.
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Unexpected("could not find bracket's install directory".into()))?;
    ui::step(format!("Installing v{latest}"), install(dir), |_| {
        format!("Installed v{latest}")
    })
    .await?;

    let output = Command::new(&exe).arg("--version").output()?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if !reported.contains(&latest.to_string()) {
        return Err(Error::Unexpected(format!(
            "the new binary reports `{}` instead of v{latest}",
            reported.trim()
        )));
    }
    cliclack::outro(format!("Updated v{current} → v{latest}"))?;
    Ok(())
}

async fn latest_release() -> Result<Version> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let output = tokio::process::Command::new("curl")
        .args(["-fsSI", &url])
        .output()
        .await
        .map_err(Error::launch("curl", "Install curl."))?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            command: format!("curl -fsSI {url}"),
            status: output.status,
        });
    }
    parse_latest(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| Error::Unexpected("GitHub didn't say which release is latest".into()))
}

/// GitHub answers `/releases/latest` with a redirect to `/releases/tag/<tag>`. Reading the
/// redirect avoids the REST API and its limit of 60 anonymous requests an hour.
fn parse_latest(headers: &str) -> Option<Version> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let (_, tag) = name
            .trim()
            .eq_ignore_ascii_case("location")
            .then_some(value)?
            .trim()
            .rsplit_once("/tag/")?;
        Version::parse(tag.trim_start_matches('v')).ok()
    })
}

/// Runs the platform's install script, pointed at `dir`.
async fn install(dir: &std::path::Path) -> Result<()> {
    let base = format!("https://raw.githubusercontent.com/{REPO}/main/install");
    let mut command = match cfg!(windows) {
        true => {
            let mut command = tokio::process::Command::new("powershell");
            command.args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!("irm {base}/install.ps1 | iex"),
            ]);
            command
        }
        false => {
            // pipefail, so a failed download fails the update instead of running an empty script.
            let mut command = tokio::process::Command::new("bash");
            command.args([
                "-c",
                "set -euo pipefail; curl -fsSL \"$0\" | bash",
                &format!("{base}/install.sh"),
            ]);
            command
        }
    };
    let output = command
        .env("BRACKET_INSTALL_DIR", dir)
        .output()
        .await
        .map_err(match cfg!(windows) {
            true => Error::launch("powershell", "Install PowerShell."),
            false => Error::launch("bash", "Install bash."),
        })?;
    match output.status.success() {
        true => Ok(()),
        false => Err(Error::Unexpected(format!(
            "the installer failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_tag_comes_from_the_redirect() {
        let headers = "HTTP/2 302 \r\n\
                       date: Tue, 30 Sep 2026 10:00:00 GMT\r\n\
                       Location: https://github.com/xav-db/bracket-cli/releases/tag/v1.2.3\r\n\
                       \r\n";
        assert_eq!(parse_latest(headers), Some(Version::new(1, 2, 3)));
    }

    #[test]
    fn missing_or_malformed_redirects_are_rejected() {
        assert_eq!(
            parse_latest("HTTP/2 200 \r\ncontent-type: text/html\r\n"),
            None
        );
        assert_eq!(
            parse_latest("location: https://github.com/xav-db/bracket-cli/releases\r\n"),
            None
        );
        assert_eq!(
            parse_latest(
                "location: https://github.com/xav-db/bracket-cli/releases/tag/nightly\r\n"
            ),
            None
        );
        // A header whose value merely mentions a tag URL isn't the redirect.
        assert_eq!(
            parse_latest("link: https://github.com/x/releases/tag/v9.9.9\r\n"),
            None
        );
    }

    #[test]
    fn the_crate_version_parses() {
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_ok());
    }
}
