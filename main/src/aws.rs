//! Loading the AWS SDK configuration, and `bracket config aws`.

use std::process::Command;

use aws_config::SdkConfig;

use crate::{
    error::{Error, Result},
    ui,
};

pub const INSTALL_CLI: &str = "Install the AWS CLI: https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html";

/// Resolved SDK configuration with a region guaranteed to be set.
pub struct Aws {
    pub config: SdkConfig,
    pub region: String,
}

impl Aws {
    /// Loads credentials and region the same way the AWS CLI does: environment, then `~/.aws`.
    ///
    /// Credentials resolve lazily on the first call, so this is cheap; only the region is
    /// checked up front, since nothing works without one.
    pub async fn load() -> Result<Self> {
        let config = aws_config::load_from_env().await;
        // An exported-but-empty AWS_REGION reaches the SDK as `Some("")`, which only fails later
        // with an unreadable endpoint error.
        let region = config
            .region()
            .map(ToString::to_string)
            .filter(|region| !region.is_empty())
            .ok_or(Error::NoRegion)?;
        Ok(Aws { config, region })
    }
}

/// `bracket config aws`: hands the terminal to `aws configure`, then proves the result works.
pub async fn configure() -> Result<()> {
    ui::intro("configure AWS")?;
    cliclack::log::step("Running `aws configure`. Press Enter to keep a current value.")?;

    let status = Command::new("aws")
        .arg("configure")
        .status()
        .map_err(Error::launch("aws", INSTALL_CLI))?;
    if !status.success() {
        return Err(Error::CommandFailed {
            command: "aws configure".into(),
            status,
        });
    }

    // Load after `aws configure` so a freshly entered region is picked up.
    let aws = Aws::load().await?;
    let sts = aws_sdk_sts::Client::new(&aws.config);
    let identity = ui::step(
        "Checking your credentials",
        async {
            sts.get_caller_identity()
                .send()
                .await
                .map_err(Error::aws("checking credentials"))
        },
        |identity| {
            format!(
                "Signed in as {}",
                identity.arn().unwrap_or("an unknown identity")
            )
        },
    )
    .await?;

    cliclack::outro(format!(
        "AWS is ready: account {} in {}",
        identity.account().unwrap_or("unknown"),
        aws.region
    ))?;
    Ok(())
}
