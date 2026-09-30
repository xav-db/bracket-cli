//! The one error type every command returns. Each variant renders as a one-line
//! message, and [`Error::hint`] adds the next step to take where there is one.

use std::{fmt, io, process::ExitStatus};

use aws_sdk_ec2::error::ProvideErrorMetadata;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An AWS API call failed. `action` says what bracket was doing at the time.
    #[error("{action} failed: {message}")]
    Aws {
        action: &'static str,
        message: String,
    },

    #[error("no AWS region is configured")]
    NoRegion,

    #[error("no {kind} named `{name}`")]
    NotFound { kind: Resource, name: String },

    #[error("no {0}s in this region")]
    Empty(Resource),

    #[error("`{program}` was not found on your PATH")]
    MissingProgram {
        program: &'static str,
        install: &'static str,
    },

    #[error("`{command}` failed ({status})")]
    CommandFailed { command: String, status: ExitStatus },

    #[error("SSH to `{alias}` failed: {reason}")]
    Ssh { alias: String, reason: String },

    #[error("gave up waiting for {what} after {}", crate::ui::elapsed(*.after))]
    TimedOut {
        what: &'static str,
        after: std::time::Duration,
    },

    #[error("deployment finished as {0}")]
    Deploy(String),

    #[error("{0}")]
    Unexpected(String),

    #[error("cancelled")]
    Cancelled,

    #[error(transparent)]
    Io(io::Error),
}

impl Error {
    /// Maps an AWS SDK error to [`Error::Aws`], preferring the service's own error code and
    /// message. Otherwise (no credentials, network trouble) it uses the root cause, since the
    /// SDK's top-level `Display` says just "service error" or "dispatch failure".
    ///
    /// ```ignore
    /// ec2.describe_instances().send().await.map_err(Error::aws("listing instances"))?;
    /// ```
    pub fn aws<E>(action: &'static str) -> impl FnOnce(E) -> Self
    where
        E: ProvideErrorMetadata + std::error::Error + 'static,
    {
        move |err| {
            let message = match (err.code(), err.message()) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                _ => root_cause(&err),
            };
            Error::Aws { action, message }
        }
    }

    /// Maps a failure to launch `program`, explaining how to install it when it's missing.
    pub fn launch(program: &'static str, install: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |err| match err.kind() {
            io::ErrorKind::NotFound => Error::MissingProgram { program, install },
            _ => err.into(),
        }
    }

    /// The next step to suggest alongside this error, if there's an obvious one.
    pub fn hint(&self) -> Option<String> {
        match self {
            Error::NoRegion => Some("Run `bracket config aws`, or set AWS_REGION.".into()),
            Error::Aws { message, .. } if is_auth_failure(message) => {
                Some("Check your credentials with `bracket config aws`.".into())
            }
            Error::NotFound { kind, .. } | Error::Empty(kind) => Some(kind.hint().into()),
            Error::MissingProgram { install, .. } => Some((*install).into()),
            _ => None,
        }
    }
}

/// The innermost error in `err`'s source chain, which is where the specific reason lives.
pub fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    std::iter::successors(Some(err), |err| err.source())
        .last()
        .map(ToString::to_string)
        .unwrap_or_default()
}

/// Converts I/O errors, turning a cancelled Clack prompt (Esc or Ctrl-C) into [`Error::Cancelled`].
impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::Interrupted => Error::Cancelled,
            _ => Error::Io(err),
        }
    }
}

/// Error codes AWS returns for missing, expired, or rejected credentials.
const AUTH_CODES: [&str; 6] = [
    "AuthFailure",
    "ExpiredToken",
    "ExpiredTokenException",
    "InvalidClientTokenId",
    "UnrecognizedClientException",
    "InvalidSignatureException",
];

fn is_auth_failure(message: &str) -> bool {
    message.contains("credentials")
        || AUTH_CODES
            .iter()
            .any(|code| message.starts_with(&format!("{code}:")))
}

/// Kinds of AWS resource bracket looks up by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Ec2Instance,
    NeptuneCluster,
    AppRunnerService,
}

impl Resource {
    fn hint(self) -> &'static str {
        match self {
            Resource::Ec2Instance => "Run `bracket list ec2` to see what's available.",
            Resource::NeptuneCluster => "Run `bracket list neptune` to see what's available.",
            Resource::AppRunnerService => {
                "Run `bracket app-runner redeploy` without a name to pick from a list."
            }
        }
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Resource::Ec2Instance => "EC2 instance",
            Resource::NeptuneCluster => "Neptune cluster",
            Resource::AppRunnerService => "App Runner service",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ec2::error::ErrorMetadata;
    use aws_sdk_ec2::operation::describe_instances::DescribeInstancesError;

    fn service_error(code: &str, message: &str) -> DescribeInstancesError {
        DescribeInstancesError::generic(
            ErrorMetadata::builder().code(code).message(message).build(),
        )
    }

    #[test]
    fn aws_errors_use_the_service_code_and_message() {
        let err = Error::aws("listing instances")(service_error(
            "UnauthorizedOperation",
            "You are not authorized to perform this operation.",
        ));
        assert_eq!(
            err.to_string(),
            "listing instances failed: UnauthorizedOperation: You are not authorized to perform this operation."
        );
        assert_eq!(err.hint(), None);
    }

    #[test]
    fn aws_errors_without_metadata_fall_back_to_the_error_chain() {
        let err = Error::aws("listing instances")(DescribeInstancesError::generic(
            ErrorMetadata::builder().build(),
        ));
        let Error::Aws { message, .. } = err else {
            panic!("expected an AWS error");
        };
        assert!(!message.is_empty());
    }

    #[test]
    fn root_cause_is_the_innermost_source() {
        #[derive(Debug, thiserror::Error)]
        #[error("dispatch failure")]
        struct Outer(#[source] io::Error);

        let err = Outer(io::Error::new(
            io::ErrorKind::TimedOut,
            "connection timed out",
        ));
        assert_eq!(root_cause(&err), "connection timed out");
        assert_eq!(root_cause(&io::Error::other("alone")), "alone");
    }

    #[test]
    fn auth_failures_suggest_reconfiguring() {
        let expired = Error::aws("x")(service_error("ExpiredToken", "The token has expired."));
        assert!(expired.hint().unwrap().contains("bracket config aws"));

        let missing = Error::Aws {
            action: "x",
            message: "dispatch failure: no credentials in the property bag".into(),
        };
        assert!(missing.hint().unwrap().contains("bracket config aws"));

        // A code that merely contains an auth code as a substring is not an auth failure.
        let other = Error::aws("x")(service_error("NotExpiredTokenish", "nope"));
        assert_eq!(other.hint(), None);
    }

    #[test]
    fn cancelled_prompts_become_cancelled() {
        let err = Error::from(io::Error::from(io::ErrorKind::Interrupted));
        assert!(matches!(err, Error::Cancelled));

        let err = Error::from(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn lookups_point_at_the_matching_list_command() {
        let err = Error::NotFound {
            kind: Resource::Ec2Instance,
            name: "dev".into(),
        };
        assert_eq!(err.to_string(), "no EC2 instance named `dev`");
        assert_eq!(
            err.hint().unwrap(),
            "Run `bracket list ec2` to see what's available."
        );

        let err = Error::Empty(Resource::NeptuneCluster);
        assert_eq!(err.to_string(), "no Neptune clusters in this region");
        assert!(err.hint().unwrap().contains("bracket list neptune"));

        let err = Error::Empty(Resource::AppRunnerService);
        assert!(err.hint().unwrap().contains("app-runner redeploy"));
    }

    #[test]
    fn launch_failures_distinguish_missing_programs() {
        let err =
            Error::launch("ssh", "Install OpenSSH.")(io::Error::from(io::ErrorKind::NotFound));
        assert!(matches!(err, Error::MissingProgram { program: "ssh", .. }));

        let err = Error::launch("ssh", "Install OpenSSH.")(io::Error::from(
            io::ErrorKind::PermissionDenied,
        ));
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn timeouts_say_how_long_they_waited() {
        let err = Error::TimedOut {
            what: "the deployment",
            after: std::time::Duration::from_secs(1800),
        };
        assert_eq!(
            err.to_string(),
            "gave up waiting for the deployment after 30m 00s"
        );
    }

    #[test]
    fn missing_programs_explain_how_to_install_them() {
        let err = Error::MissingProgram {
            program: "cursor",
            install: "Install Cursor.",
        };
        assert_eq!(err.to_string(), "`cursor` was not found on your PATH");
        assert_eq!(err.hint().as_deref(), Some("Install Cursor."));
    }
}
