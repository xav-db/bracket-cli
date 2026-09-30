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

/// Basic monitoring's resolution, which bracket uses everywhere.
pub const PERIOD: Duration = Duration::from_secs(300);

/// Look back far enough to always catch one basic-monitoring datapoint.
const LATEST_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Datapoints for one resource, oldest first.
pub type Series = Vec<(SystemTime, f64)>;

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
    let series = cpu_series(cloudwatch, source, ids, end - LATEST_WINDOW, end).await?;
    Ok(series
        .into_iter()
        .filter_map(|(id, points)| Some((id, points.last()?.1)))
        .collect())
}

/// Returns each resource's average CPU utilisation (percent) per [`PERIOD`] from `start` to
/// `end`. Resources without any datapoints are left out of the map.
pub async fn cpu_series(
    cloudwatch: &Client,
    source: Source,
    ids: &[String],
    start: SystemTime,
    end: SystemTime,
) -> Result<HashMap<String, Series>> {
    let mut series: HashMap<String, Series> = HashMap::with_capacity(ids.len());

    // Chunks run one after another: past 500 dev boxes the extra round trip is the least of it.
    // Within a chunk the paginator splits long series into pages of 100,800 datapoints.
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
            .scan_by(ScanBy::TimestampAscending)
            .into_paginator()
            .send()
            .try_collect()
            .await
            .map_err(Error::aws("fetching CPU metrics"))?;

        let results = pages.iter().flat_map(|page| page.metric_data_results());
        for (id, points) in datapoints(chunk, results) {
            series.entry(id).or_default().extend(points);
        }
    }
    // One series can span pages; keep each in time order regardless.
    series
        .values_mut()
        .for_each(|points| points.sort_by_key(|(time, _)| *time));
    Ok(series)
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
                .period(PERIOD.as_secs() as i32)
                .stat("Average")
                .build(),
        )
        .build()
}

/// Maps each result back to its resource id, paired with its datapoints.
fn datapoints<'a>(
    chunk: &'a [String],
    results: impl Iterator<Item = &'a MetricDataResult> + 'a,
) -> impl Iterator<Item = (String, Series)> + 'a {
    results.filter_map(|result| {
        let index: usize = result.id()?.strip_prefix('q')?.parse().ok()?;
        let points = result
            .timestamps()
            .iter()
            .zip(result.values())
            .filter_map(|(time, value)| Some((SystemTime::try_from(*time).ok()?, *value)))
            .collect();
        Some((chunk.get(index)?.clone(), points))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_cloudwatch::operation::get_metric_data::GetMetricDataOutput;
    use aws_smithy_mocks::{mock, mock_client};

    /// A result whose values sit at 5-minute marks `minutes` after the epoch.
    fn result(id: &str, points: &[(u64, f64)]) -> MetricDataResult {
        MetricDataResult::builder()
            .id(id)
            .set_timestamps(Some(
                points
                    .iter()
                    .map(|(minutes, _)| DateTime::from_secs(*minutes as i64 * 60))
                    .collect(),
            ))
            .set_values(Some(points.iter().map(|(_, value)| *value).collect()))
            .build()
    }

    fn at(minutes: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(minutes * 60)
    }

    #[test]
    fn results_map_back_to_resource_ids_by_position() {
        let chunk = ["i-a".to_string(), "i-b".to_string()];
        let results = [
            result("q1", &[(5, 12.5), (10, 90.0)]),
            result("q7", &[(5, 1.0)]),
            result("bogus", &[(5, 1.0)]),
        ];
        let values: Vec<_> = datapoints(&chunk, results.iter()).collect();
        assert_eq!(
            values,
            vec![("i-b".to_string(), vec![(at(5), 12.5), (at(10), 90.0)])]
        );
    }

    #[test]
    fn queries_target_the_right_metric() {
        let query = query(NEPTUNE, 3, "graph");
        assert_eq!(query.id(), Some("q3"));
        let stat = query.metric_stat().unwrap();
        assert_eq!(stat.period(), Some(300));
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
                    .metric_data_results(result("q0", &[(5, 5.0)]))
                    .metric_data_results(result("q2", &[(5, 10.0), (10, 75.0)]))
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
                .metric_data_results(result("q0", &[(5, 1.0)]))
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

    #[tokio::test]
    async fn series_spanning_pages_are_joined_in_time_order() {
        let rule = mock!(Client::get_metric_data)
            .sequence()
            .output(|| {
                GetMetricDataOutput::builder()
                    .metric_data_results(result("q0", &[(10, 2.0), (15, 3.0)]))
                    .next_token("more")
                    .build()
            })
            .output(|| {
                GetMetricDataOutput::builder()
                    .metric_data_results(result("q0", &[(5, 1.0)]))
                    .metric_data_results(result("q1", &[(5, 9.0)]))
                    .build()
            })
            .build();
        let client = mock_client!(aws_sdk_cloudwatch, [&rule]);
        let ids = ["a", "b"].map(String::from);

        let series = cpu_series(&client, EC2, &ids, at(0), at(20)).await.unwrap();

        assert_eq!(rule.num_calls(), 2);
        assert_eq!(
            series["a"],
            vec![(at(5), 1.0), (at(10), 2.0), (at(15), 3.0)]
        );
        assert_eq!(series["b"], vec![(at(5), 9.0)]);
    }
}
