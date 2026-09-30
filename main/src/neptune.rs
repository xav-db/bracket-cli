//! Neptune clusters: `bracket list neptune` and `bracket stop neptune`.

use std::collections::HashMap;

use aws_sdk_neptune::{
    Client,
    types::{DbCluster, Filter},
};
use console::Style;

use crate::{
    aws::Aws,
    error::{Error, Resource, Result},
    metrics,
    ui::{self, Cell},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cluster {
    id: String,
    /// Neptune reports status as free text: `available`, `stopped`, `starting`, `modifying`, ...
    status: String,
    instances: usize,
    endpoint: Option<String>,
}

impl Cluster {
    /// Returns `None` for clusters without an identifier, or that aren't Neptune at all.
    fn from_sdk(cluster: &DbCluster) -> Option<Self> {
        cluster.engine().filter(|engine| *engine == "neptune")?;
        Some(Cluster {
            id: cluster.db_cluster_identifier()?.to_string(),
            status: cluster.status().unwrap_or("unknown").to_string(),
            instances: cluster.db_cluster_members().len(),
            endpoint: cluster.endpoint().map(str::to_string),
        })
    }

    fn status_cell(&self) -> Cell {
        let (dot, style) = match self.status.as_str() {
            "available" => ("●", Style::new().green()),
            "stopped" => ("○", Style::new().dim()),
            _ => ("◐", Style::new().yellow()),
        };
        Cell::new(format!("{dot} {}", self.status)).style(style)
    }
}

/// Fetches every Neptune cluster in the region, sorted by identifier.
async fn describe(neptune: &Client) -> Result<Vec<Cluster>> {
    // DescribeDBClusters is shared with RDS: without this filter it returns Aurora clusters too.
    let engine = Filter::builder().name("engine").values("neptune").build();
    let pages = neptune
        .describe_db_clusters()
        .filters(engine)
        .into_paginator()
        .send()
        .try_collect()
        .await
        .map_err(Error::aws("listing Neptune clusters"))?;
    let mut clusters: Vec<Cluster> = pages
        .iter()
        .flat_map(|page| page.db_clusters())
        .filter_map(Cluster::from_sdk)
        .collect();
    clusters.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(clusters)
}

/// `bracket list neptune`.
pub async fn list() -> Result<()> {
    ui::intro("Neptune clusters")?;
    let aws = Aws::load().await?;
    let neptune = Client::new(&aws.config);
    let cloudwatch = aws_sdk_cloudwatch::Client::new(&aws.config);

    let (clusters, cpu) = ui::step(
        "Fetching clusters",
        async {
            let clusters = describe(&neptune).await?;
            let available: Vec<String> = clusters
                .iter()
                .filter(|cluster| cluster.status == "available")
                .map(|cluster| cluster.id.clone())
                .collect();
            let cpu = metrics::latest_cpu(&cloudwatch, metrics::NEPTUNE, &available).await;
            Ok((clusters, cpu))
        },
        |(clusters, _)| format!("{} clusters in {}", clusters.len(), aws.region),
    )
    .await?;

    if clusters.is_empty() {
        cliclack::outro("Nothing to show")?;
        return Ok(());
    }
    // CPU is a nice-to-have: without CloudWatch access the table still lists the clusters.
    let cpu = cpu.unwrap_or_else(|err| {
        let _ = cliclack::log::warning(format!("CPU unavailable: {err}"));
        HashMap::new()
    });

    let rows: Vec<[Cell; 5]> = clusters
        .iter()
        .map(|cluster| row(cluster, cpu.get(&cluster.id).copied(), &aws.region))
        .collect();
    ui::print_table(["CLUSTER", "STATUS", "INSTANCES", "CPU", "ENDPOINT"], &rows);

    let available = clusters.iter().filter(|c| c.status == "available").count();
    cliclack::outro(format!("{available} of {} available", clusters.len()))?;
    Ok(())
}

fn row(cluster: &Cluster, cpu: Option<f64>, region: &str) -> [Cell; 5] {
    let console = format!(
        "https://{region}.console.aws.amazon.com/neptune/home?region={region}#database:id={};is-cluster=true",
        cluster.id
    );
    [
        Cell::new(&cluster.id)
            .style(Style::new().bold())
            .link(console),
        cluster.status_cell(),
        Cell::new(cluster.instances.to_string()),
        cpu.map_or_else(Cell::missing, |percent| Cell::new(format!("{percent:.1}%"))),
        cluster
            .endpoint
            .as_deref()
            .map_or_else(Cell::missing, |endpoint| {
                Cell::new(endpoint).style(Style::new().dim())
            }),
    ]
}

/// `bracket stop neptune`.
pub async fn stop(name: Option<String>) -> Result<()> {
    ui::intro("stop Neptune cluster")?;
    let aws = Aws::load().await?;
    let neptune = Client::new(&aws.config);

    let clusters = ui::step("Fetching clusters", describe(&neptune), |clusters| {
        format!("Found {} clusters", clusters.len())
    })
    .await?;
    let cluster = ui::choose(
        clusters,
        name.as_deref(),
        Resource::NeptuneCluster,
        "Which cluster should be stopped?",
        |cluster| &cluster.id,
        |cluster| cluster.status.clone(),
    )?;

    match cluster.status.as_str() {
        "stopped" | "stopping" => {
            cliclack::outro(format!("{} is already {}", cluster.id, cluster.status))?;
        }
        _ => {
            ui::step(
                format!("Stopping {}", cluster.id),
                async {
                    neptune
                        .stop_db_cluster()
                        .db_cluster_identifier(&cluster.id)
                        .send()
                        .await
                        .map_err(Error::aws("stopping the cluster"))
                },
                |_| format!("Stop requested for {}", cluster.id),
            )
            .await?;
            cliclack::outro(format!(
                "{} is stopping. Neptune restarts stopped clusters automatically after 7 days.",
                cluster.id
            ))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_neptune::{
        operation::describe_db_clusters::DescribeDbClustersOutput, types::DbClusterMember,
    };
    use aws_smithy_mocks::{mock, mock_client};

    fn sdk_cluster(id: &str, engine: &str, status: &str) -> DbCluster {
        DbCluster::builder()
            .db_cluster_identifier(id)
            .engine(engine)
            .status(status)
            .endpoint(format!("{id}.cluster-x.us-east-2.neptune.amazonaws.com"))
            .db_cluster_members(DbClusterMember::builder().build())
            .db_cluster_members(DbClusterMember::builder().build())
            .build()
    }

    #[test]
    fn only_neptune_clusters_convert() {
        let cluster = Cluster::from_sdk(&sdk_cluster("graph", "neptune", "available")).unwrap();
        assert_eq!(cluster.instances, 2);
        assert_eq!(cluster.status_cell().text, "● available");
        assert_eq!(
            Cluster::from_sdk(&sdk_cluster("pg", "aurora-postgresql", "available")),
            None
        );
    }

    #[test]
    fn status_badges_follow_the_status() {
        let mut cluster = Cluster::from_sdk(&sdk_cluster("graph", "neptune", "stopped")).unwrap();
        assert_eq!(cluster.status_cell().text, "○ stopped");
        cluster.status = "starting".into();
        assert_eq!(cluster.status_cell().text, "◐ starting");
    }

    #[test]
    fn rows_link_to_the_console() {
        let cluster = Cluster::from_sdk(&sdk_cluster("graph", "neptune", "available")).unwrap();
        let [name, _, instances, cpu, endpoint] = row(&cluster, Some(3.0), "eu-west-2");
        assert!(
            name.link
                .unwrap()
                .contains("eu-west-2.console.aws.amazon.com/neptune")
        );
        assert_eq!(instances.text, "2");
        assert_eq!(cpu.text, "3.0%");
        assert!(endpoint.text.starts_with("graph.cluster-x"));
    }

    #[tokio::test]
    async fn describe_asks_for_neptune_only_and_sorts() {
        let rule = mock!(Client::describe_db_clusters)
            .match_requests(|req| {
                req.filters().len() == 1
                    && req.filters()[0].name() == Some("engine")
                    && req.filters()[0].values() == ["neptune"]
            })
            .then_output(|| {
                DescribeDbClustersOutput::builder()
                    .db_clusters(sdk_cluster("zeta", "neptune", "available"))
                    .db_clusters(sdk_cluster("rds", "aurora-mysql", "available"))
                    .db_clusters(sdk_cluster("alpha", "neptune", "stopped"))
                    .build()
            });
        let client = mock_client!(aws_sdk_neptune, [&rule]);

        let clusters = describe(&client).await.unwrap();

        assert_eq!(
            clusters.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
    }
}
