//! `bracket app-runner redeploy`: start a deployment and follow it to the end.

use std::time::Duration;

use aws_sdk_apprunner::{Client, types::OperationStatus};
use tokio::time::Instant;

use crate::{
    aws::Aws,
    error::{Error, Resource, Result},
    ui,
};

const POLL: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Service {
    name: String,
    arn: String,
    status: String,
    url: Option<String>,
}

/// Fetches every App Runner service in the region, sorted by name.
async fn describe(client: &Client) -> Result<Vec<Service>> {
    let pages = client
        .list_services()
        .into_paginator()
        .send()
        .try_collect()
        .await
        .map_err(Error::aws("listing App Runner services"))?;
    let mut services: Vec<Service> = pages
        .iter()
        .flat_map(|page| page.service_summary_list())
        .filter_map(|summary| {
            Some(Service {
                name: summary.service_name()?.to_string(),
                arn: summary.service_arn()?.to_string(),
                status: summary
                    .status()
                    .map_or("unknown", |status| status.as_str())
                    .to_lowercase(),
                url: summary.service_url().map(str::to_string),
            })
        })
        .collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

/// `bracket app-runner redeploy`.
pub async fn redeploy(name: Option<String>) -> Result<()> {
    ui::intro("redeploy App Runner service")?;
    let aws = Aws::load().await?;
    let client = Client::new(&aws.config);

    let services = ui::step("Fetching services", describe(&client), |services| {
        format!("Found {} services", services.len())
    })
    .await?;
    let service = ui::choose(
        services,
        name.as_deref(),
        Resource::AppRunnerService,
        "Which service should be redeployed?",
        |service| &service.name,
        |service| service.status.clone(),
    )?;

    let operation = ui::step(
        format!("Starting a deployment of {}", service.name),
        async {
            client
                .start_deployment()
                .service_arn(&service.arn)
                .send()
                .await
                .map(|output| output.operation_id)
                .map_err(Error::aws("starting the deployment"))
        },
        |_| "Deployment started".to_string(),
    )
    .await?;

    let started = Instant::now();
    ui::step(
        format!(
            "Deploying {} (Ctrl-C stops watching; the deployment carries on)",
            service.name
        ),
        follow(&client, &service.arn, &operation),
        |_| {
            format!(
                "Deployed {} in {}",
                service.name,
                ui::elapsed(started.elapsed())
            )
        },
    )
    .await?;

    cliclack::outro(match &service.url {
        Some(url) => format!("{} is live at https://{url}", service.name),
        None => format!("{} is live", service.name),
    })?;
    Ok(())
}

/// Where a deployment stands.
#[derive(Debug, PartialEq, Eq)]
enum Progress {
    Running,
    Succeeded,
    Failed(String),
}

impl Progress {
    fn of(status: &OperationStatus) -> Self {
        match status {
            OperationStatus::Succeeded => Progress::Succeeded,
            OperationStatus::Failed
            | OperationStatus::RollbackFailed
            | OperationStatus::RollbackInProgress
            | OperationStatus::RollbackSucceeded => {
                Progress::Failed(status.as_str().to_lowercase().replace('_', " "))
            }
            // Pending, in progress, and anything newer than this SDK: keep watching.
            _ => Progress::Running,
        }
    }
}

/// Polls the service's operations until `operation_id` succeeds, fails, or [`MAX_WAIT`] passes.
async fn follow(client: &Client, service_arn: &str, operation_id: &str) -> Result<()> {
    let deadline = Instant::now() + MAX_WAIT;
    loop {
        tokio::time::sleep(POLL).await;
        let operations = client
            .list_operations()
            .service_arn(service_arn)
            .send()
            .await
            .map_err(Error::aws("checking the deployment"))?;
        let progress = operations
            .operation_summary_list()
            .iter()
            .find(|operation| operation.id() == Some(operation_id))
            .and_then(|operation| operation.status())
            .map_or(Progress::Running, Progress::of);
        match progress {
            Progress::Succeeded => return Ok(()),
            Progress::Failed(status) => return Err(Error::Deploy(status)),
            Progress::Running if Instant::now() < deadline => {}
            Progress::Running => {
                return Err(Error::TimedOut {
                    what: "the deployment",
                    after: MAX_WAIT,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_apprunner::{
        operation::{list_operations::ListOperationsOutput, list_services::ListServicesOutput},
        types::{OperationSummary, ServiceStatus, ServiceSummary},
    };
    use aws_smithy_mocks::{RuleMode, mock, mock_client};

    fn operations(id: &str, status: OperationStatus) -> ListOperationsOutput {
        ListOperationsOutput::builder()
            .operation_summary_list(
                OperationSummary::builder()
                    .id("older")
                    .status(OperationStatus::Failed)
                    .build(),
            )
            .operation_summary_list(OperationSummary::builder().id(id).status(status).build())
            .build()
    }

    #[test]
    fn statuses_map_to_progress() {
        assert_eq!(
            Progress::of(&OperationStatus::Succeeded),
            Progress::Succeeded
        );
        assert_eq!(Progress::of(&OperationStatus::Pending), Progress::Running);
        assert_eq!(
            Progress::of(&OperationStatus::InProgress),
            Progress::Running
        );
        assert_eq!(
            Progress::of(&OperationStatus::RollbackSucceeded),
            Progress::Failed("rollback succeeded".into())
        );
        assert_eq!(
            Progress::of(&OperationStatus::from("SOMETHING_NEW")),
            Progress::Running
        );
    }

    #[tokio::test(start_paused = true)]
    async fn follow_waits_for_this_operation_to_succeed() {
        let rule = mock!(Client::list_operations)
            .sequence()
            .output(|| operations("op-1", OperationStatus::InProgress))
            .times(3)
            .output(|| operations("op-1", OperationStatus::Succeeded))
            .build();
        let client = mock_client!(aws_sdk_apprunner, [&rule]);

        follow(&client, "arn", "op-1").await.unwrap();

        assert_eq!(rule.num_calls(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn follow_reports_failed_deployments() {
        let rule = mock!(Client::list_operations)
            .then_output(|| operations("op-1", OperationStatus::RollbackInProgress));
        let client = mock_client!(aws_sdk_apprunner, [&rule]);

        let err = follow(&client, "arn", "op-1").await.unwrap_err();

        assert_eq!(
            err.to_string(),
            "deployment finished as rollback in progress"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn follow_gives_up_eventually() {
        let rule = mock!(Client::list_operations)
            .then_output(|| operations("op-1", OperationStatus::InProgress));
        let client = mock_client!(aws_sdk_apprunner, RuleMode::MatchAny, [&rule]);

        let err = follow(&client, "arn", "op-1").await.unwrap_err();

        assert!(matches!(err, Error::TimedOut { .. }));
        assert_eq!(rule.num_calls() as u64, MAX_WAIT.as_secs() / POLL.as_secs());
    }

    #[tokio::test]
    async fn describe_reads_every_page() {
        let summary = |name: &str| {
            ServiceSummary::builder()
                .service_name(name)
                .service_arn(format!("arn:{name}"))
                .status(ServiceStatus::Running)
                .service_url(format!("{name}.awsapprunner.com"))
                .build()
        };
        let rule = mock!(Client::list_services)
            .sequence()
            .output(move || {
                ListServicesOutput::builder()
                    .service_summary_list(summary("web"))
                    .next_token("2")
                    .build()
                    .unwrap()
            })
            .output(move || {
                ListServicesOutput::builder()
                    .service_summary_list(summary("api"))
                    .build()
                    .unwrap()
            })
            .build();
        let client = mock_client!(aws_sdk_apprunner, [&rule]);

        let services = describe(&client).await.unwrap();

        assert_eq!(services.len(), 2);
        assert_eq!(
            services[0],
            Service {
                name: "api".into(),
                arn: "arn:api".into(),
                status: "running".into(),
                url: Some("api.awsapprunner.com".into()),
            }
        );
    }
}
