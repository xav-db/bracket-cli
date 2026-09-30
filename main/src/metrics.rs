//! Batched CloudWatch CPU lookups.
//!
//! One `GetMetricData` call covers up to 500 resources, instead of a `GetMetricStatistics`
//! round trip per resource, so listing stays one request deep however many boxes exist.

use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

use aws_sdk_cloudwatch::{
    Client,
    primitives::DateTime,
    types::{Dimension, Metric, MetricDataQuery, MetricDataResult, MetricStat, ScanBy},
};

use crate::error::{Error, Result};

/// `GetMetricData` accepts at most this many queries per request.
const MAX_QUERIES: usize = 500;

/// Basic monitoring publishes every 5 minutes, so look back far enough to always catch one.
const WINDOW: Duration = Duration::from_secs(15 * 60);
const PERIOD_SECS: i32 = 300;

/// A CloudWatch namespace together with the dimension that identifies one resource in it.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub namespace: &'static str,
    pub dimension: &'static str,
}

pub const EC2: Source = Source {
    namespace: "AWS/EC2",
    dimension: "InstanceId",
};

pub const NEPTUNE: Source = Source {
    namespace: "AWS/Neptune",
    dimension: "DBClusterIdentifier",
};

/// Returns the most recent average CPU utilisation (percent) for each resource in `ids`.
/// Resources with no datapoint in the last 15 minutes are left out of the map.
pub async fn latest_cpu(
    cloudwatch: &Client,
    source: Source,
    ids: &[String],
) -> Result<HashMap<String, f64>> {
    let end = SystemTime::now();
    let start = end - WINDOW;
    let mut cpu = HashMap::with_capacity(ids.len());

    // Chunks run one after another: past 500 dev boxes the extra round trip is the least of it.
    for chunk in ids.chunks(MAX_QUERIES) {
        let queries = chunk
            .iter()
            .enumerate()
            .map(|(index, id)| query(source, index, id))
            .collect();
        let pages = cloudwatch
            .get_metric_data()
            .set_metric_data_queries(Some(queries))
            .start_time(DateTime::from(start))
            .end_time(DateTime::from(end))
            .scan_by(ScanBy::TimestampDescending)
            .into_paginator()
            .send()
            .try_collect()
            .await
            .map_err(Error::aws("fetching CPU metrics"))?;

        let results = pages.iter().flat_map(|page| page.metric_data_results());
        for (id, value) in latest_values(chunk, results) {
            cpu.entry(id).or_insert(value);
        }
    }
    Ok(cpu)
}

/// Query ids must match `^[a-z][a-zA-Z0-9_]*$`, so resource ids are referenced by position.
fn query(source: Source, index: usize, id: &str) -> MetricDataQuery {
    let metric = Metric::builder()
        .namespace(source.namespace)
        .metric_name("CPUUtilization")
        .dimensions(
            Dimension::builder()
                .name(source.dimension)
                .value(id)
                .build(),
        )
        .build();
    MetricDataQuery::builder()
        .id(format!("q{index}"))
        .metric_stat(
            MetricStat::builder()
                .metric(metric)
                .period(PERIOD_SECS)
                .stat("Average")
                .build(),
        )
        .build()
}

/// Maps each result back to its resource id and takes its newest value. Results arrive
/// newest-first because the request scans `TimestampDescending`.
fn latest_values<'a>(
    chunk: &'a [String],
    results: impl Iterator<Item = &'a MetricDataResult> + 'a,
) -> impl Iterator<Item = (String, f64)> + 'a {
    results.filter_map(|result| {
        let index: usize = result.id()?.strip_prefix('q')?.parse().ok()?;
        Some((chunk.get(index)?.clone(), *result.values().first()?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_cloudwatch::operation::get_metric_data::GetMetricDataOutput;
    use aws_smithy_mocks::{mock, mock_client};

    fn result(id: &str, values: &[f64]) -> MetricDataResult {
        MetricDataResult::builder()
            .id(id)
            .set_values(Some(values.to_vec()))
            .build()
    }

    #[test]
    fn results_map_back_to_resource_ids_by_position() {
        let chunk = ["i-a".to_string(), "i-b".to_string()];
        let results = [
            result("q1", &[12.5, 90.0]),
            result("q0", &[]),
            result("q7", &[1.0]),
            result("bogus", &[1.0]),
        ];
        let values: Vec<_> = latest_values(&chunk, results.iter()).collect();
        assert_eq!(values, vec![("i-b".to_string(), 12.5)]);
    }

    #[test]
    fn queries_target_the_right_metric() {
        let query = query(NEPTUNE, 3, "graph");
        assert_eq!(query.id(), Some("q3"));
        let stat = query.metric_stat().unwrap();
        assert_eq!(stat.period(), Some(PERIOD_SECS));
        assert_eq!(stat.stat(), Some("Average"));
        let metric = stat.metric().unwrap();
        assert_eq!(metric.namespace(), Some("AWS/Neptune"));
        assert_eq!(metric.metric_name(), Some("CPUUtilization"));
        assert_eq!(metric.dimensions()[0].name(), Some("DBClusterIdentifier"));
        assert_eq!(metric.dimensions()[0].value(), Some("graph"));
    }

    #[tokio::test]
    async fn one_request_covers_every_resource() {
        let rule = mock!(Client::get_metric_data)
            .match_requests(|req| req.metric_data_queries().len() == 3)
            .then_output(|| {
                GetMetricDataOutput::builder()
                    .metric_data_results(result("q0", &[5.0]))
                    .metric_data_results(result("q2", &[75.0, 10.0]))
                    .build()
            });
        let client = mock_client!(aws_sdk_cloudwatch, [&rule]);
        let ids = ["a", "b", "c"].map(String::from);

        let cpu = latest_cpu(&client, EC2, &ids).await.unwrap();

        assert_eq!(rule.num_calls(), 1);
        assert_eq!(cpu.len(), 2);
        assert_eq!(cpu["a"], 5.0);
        assert_eq!(cpu["c"], 75.0);
    }

    #[tokio::test]
    async fn large_fleets_are_split_into_batches_of_five_hundred() {
        let rule = mock!(Client::get_metric_data).then_compute_output(|req| {
            assert!(req.metric_data_queries().len() <= MAX_QUERIES);
            GetMetricDataOutput::builder()
                .metric_data_results(result("q0", &[1.0]))
                .build()
        });
        let client = mock_client!(
            aws_sdk_cloudwatch,
            aws_smithy_mocks::RuleMode::MatchAny,
            [&rule]
        );
        let ids: Vec<String> = (0..1001).map(|i| format!("i-{i}")).collect();

        let cpu = latest_cpu(&client, EC2, &ids).await.unwrap();

        assert_eq!(rule.num_calls(), 3);
        assert_eq!(cpu.len(), 3);
        assert!(
            ["i-0", "i-500", "i-1000"]
                .iter()
                .all(|id| cpu.contains_key(*id))
        );
    }

    #[tokio::test]
    async fn no_resources_means_no_requests() {
        let rule =
            mock!(Client::get_metric_data).then_output(|| GetMetricDataOutput::builder().build());
        let client = mock_client!(aws_sdk_cloudwatch, [&rule]);

        let cpu = latest_cpu(&client, EC2, &[]).await.unwrap();

        assert!(cpu.is_empty());
        assert_eq!(rule.num_calls(), 0);
    }
}
