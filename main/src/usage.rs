//! `bracket usage ec2`: CPU and estimated cost over time, by instance, region, or family.
//!
//! Everything comes from 5-minute CloudWatch CPU datapoints. A running instance publishes one
//! every 5 minutes and a stopped one doesn't, so each datapoint is also 5 minutes of running
//! time: estimated cost = running slots × on-demand $/h ÷ 12. That leaves out EBS, data transfer,
//! and discounts, and counts spot instances at the on-demand price.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_config::Region;

use crate::{
    aws::Aws,
    ec2::{self, Instance, State},
    error::{Error, Result},
    metrics::{self, Series},
    pricing, tui, ui,
};

/// One slot per CloudWatch datapoint.
pub const SLOT: Duration = metrics::PERIOD;

/// Slots loaded: 14 days. CloudWatch keeps 5-minute data for 15.
pub const HISTORY: usize = 14 * 24 * 12;

/// Basic monitoring publishes a few minutes late. Ending the timeline this far back keeps
/// running instances from looking stopped in the last slot.
const PUBLISH_DELAY: Duration = Duration::from_secs(10 * 60);

/// One instance's usage over the loaded history.
#[derive(Debug, Clone)]
pub struct Track {
    pub instance: Instance,
    pub region: String,
    pub usd_per_hour: Option<f64>,
    /// CPU percent per slot, oldest first, `None` where the instance wasn't running.
    /// Always [`HISTORY`] long.
    pub cpu: Vec<Option<f64>>,
}

impl Track {
    /// Lays `points` onto the [`HISTORY`] slots that end at `end`, dropping any outside them.
    pub fn new(
        instance: Instance,
        region: String,
        usd_per_hour: Option<f64>,
        points: &[(SystemTime, f64)],
        end: SystemTime,
    ) -> Self {
        let start = end - SLOT * HISTORY as u32;
        let mut cpu = vec![None; HISTORY];
        points
            .iter()
            .filter_map(|(time, value)| {
                let offset = time.duration_since(start).ok()?;
                Some(((offset.as_secs() / SLOT.as_secs()) as usize, *value))
            })
            .filter(|(slot, _)| *slot < HISTORY)
            .for_each(|(slot, value)| cpu[slot] = Some(value));
        Track {
            instance,
            region,
            usd_per_hour,
            cpu,
        }
    }

    /// `c7i` for a `c7i.8xlarge`.
    fn family(&self) -> &str {
        family(&self.instance.instance_type)
    }

    /// CPU averages weight each instance by its size, so one idle 32-vCPU box outweighs a busy
    /// 2-vCPU one.
    fn weight(&self) -> f64 {
        f64::from(self.instance.vcpus.unwrap_or(1))
    }
}

/// The family part of an instance type: `c7i` for `c7i.8xlarge`.
pub fn family(instance_type: &str) -> &str {
    instance_type
        .split_once('.')
        .map_or(instance_type, |(family, _)| family)
}

/// Every instance's track, all on the same timeline.
#[derive(Debug)]
pub struct Dataset {
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Day,
    Week,
    Fortnight,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::Day, Range::Week, Range::Fortnight];

    pub fn label(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Fortnight => "14d",
        }
    }

    pub fn slots(self) -> usize {
        match self {
            Range::Day => 24 * 12,
            Range::Week => 7 * 24 * 12,
            Range::Fortnight => HISTORY,
        }
    }

    /// Slots per chart point (15 minutes, 1 hour, 2 hours), so every range draws 96–168 points.
    pub fn bucket(self) -> usize {
        match self {
            Range::Day => 3,
            Range::Week => 12,
            Range::Fortnight => 24,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    Instance,
    Region,
    Family,
}

impl Grouping {
    pub const ALL: [Grouping; 3] = [Grouping::Instance, Grouping::Region, Grouping::Family];

    pub fn label(self) -> &'static str {
        match self {
            Grouping::Instance => "Instance",
            Grouping::Region => "Region",
            Grouping::Family => "Family",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Cost,
    Cpu,
    Name,
}

impl Sort {
    pub fn next(self) -> Self {
        match self {
            Sort::Cost => Sort::Cpu,
            Sort::Cpu => Sort::Name,
            Sort::Name => Sort::Cost,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Cost => "cost",
            Sort::Cpu => "cpu",
            Sort::Name => "name",
        }
    }
}

/// Usage for one group of instances over a range.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub label: String,
    pub detail: String,
    /// Estimated USD over the range. `None` when no instance in the group has a price.
    pub cost: Option<f64>,
    /// Instances in the group without a price, so left out of `cost`.
    pub unpriced: usize,
    /// vCPU-weighted mean CPU percent while running. `None` if nothing ran.
    pub cpu: Option<f64>,
    /// $/h for the group's priced instances that are running now.
    pub rate_now: f64,
    /// Chart points, oldest first: vCPU-weighted mean CPU per [`Range::bucket`].
    pub cpu_points: Vec<Option<f64>>,
    /// Chart points, oldest first: mean $/h per [`Range::bucket`].
    pub rate_points: Vec<f64>,
}

/// One slot's totals across a group's running instances.
#[derive(Debug, Default, Clone, Copy)]
struct Slot {
    weighted_cpu: f64,
    weight: f64,
    usd_per_hour: f64,
}

impl Slot {
    fn add(self, other: Slot) -> Slot {
        Slot {
            weighted_cpu: self.weighted_cpu + other.weighted_cpu,
            weight: self.weight + other.weight,
            usd_per_hour: self.usd_per_hour + other.usd_per_hour,
        }
    }

    fn cpu(self) -> Option<f64> {
        (self.weight > 0.0).then(|| self.weighted_cpu / self.weight)
    }
}

/// Totals `tracks` over the last `range` of the history.
fn summarize(label: String, detail: String, tracks: &[&Track], range: Range) -> Summary {
    let first = HISTORY - range.slots();
    let slots: Vec<Slot> = (first..HISTORY)
        .map(|slot| {
            tracks
                .iter()
                .filter_map(|track| Some((track, track.cpu[slot]?)))
                .map(|(track, cpu)| Slot {
                    weighted_cpu: cpu * track.weight(),
                    weight: track.weight(),
                    usd_per_hour: track.usd_per_hour.unwrap_or_default(),
                })
                .fold(Slot::default(), Slot::add)
        })
        .collect();
    let total = slots.iter().copied().fold(Slot::default(), Slot::add);
    let slot_hours = SLOT.as_secs_f64() / 3600.0;
    let unpriced = tracks
        .iter()
        .filter(|track| track.usd_per_hour.is_none())
        .count();

    Summary {
        label,
        detail,
        cost: (unpriced < tracks.len()).then_some(total.usd_per_hour * slot_hours),
        unpriced,
        cpu: total.cpu(),
        rate_now: tracks
            .iter()
            .filter(|track| track.instance.state == State::Running)
            .filter_map(|track| track.usd_per_hour)
            .sum(),
        cpu_points: slots
            .chunks(range.bucket())
            .map(|bucket| {
                bucket
                    .iter()
                    .copied()
                    .fold(Slot::default(), Slot::add)
                    .cpu()
            })
            .collect(),
        rate_points: slots
            .chunks(range.bucket())
            .map(|bucket| {
                bucket.iter().map(|slot| slot.usd_per_hour).sum::<f64>() / bucket.len() as f64
            })
            .collect(),
    }
}

fn instances(count: usize) -> String {
    match count {
        1 => "1 instance".to_string(),
        _ => format!("{count} instances"),
    }
}

/// Every instance together.
pub fn total(dataset: &Dataset, range: Range) -> Summary {
    let tracks: Vec<&Track> = dataset.tracks.iter().collect();
    let regions: BTreeSet<&str> = tracks.iter().map(|track| track.region.as_str()).collect();
    let detail = match regions.len() {
        1 => format!("{} in 1 region", instances(tracks.len())),
        count => format!("{} in {count} regions", instances(tracks.len())),
    };
    summarize("All instances".into(), detail, &tracks, range)
}

/// One summary per instance, region, or family, in name order.
pub fn groups(dataset: &Dataset, grouping: Grouping, range: Range) -> Vec<Summary> {
    let mut by_key: BTreeMap<&str, Vec<&Track>> = BTreeMap::new();
    for track in &dataset.tracks {
        let key = match grouping {
            Grouping::Instance => &track.instance.id,
            Grouping::Region => &track.region,
            Grouping::Family => track.family(),
        };
        by_key.entry(key).or_default().push(track);
    }
    by_key
        .into_values()
        .map(|tracks| {
            let first = tracks[0];
            let (label, detail) = match grouping {
                Grouping::Instance => {
                    let spot = if first.instance.spot { " · spot" } else { "" };
                    (
                        first.instance.display_name().to_string(),
                        format!("{} · {}{spot}", first.region, first.instance.instance_type),
                    )
                }
                Grouping::Region => (first.region.clone(), instances(tracks.len())),
                Grouping::Family => (first.family().to_string(), instances(tracks.len())),
            };
            summarize(label, detail, &tracks, range)
        })
        .collect()
}

/// Orders summaries: most expensive or busiest first, or by name. Unknowns go last.
pub fn sort(summaries: &mut [Summary], by: Sort) {
    let descending = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (a, b) => b.is_some().cmp(&a.is_some()),
    };
    summaries.sort_by(|a, b| {
        match by {
            Sort::Cost => descending(a.cost, b.cost),
            Sort::Cpu => descending(a.cpu, b.cpu),
            Sort::Name => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.label.cmp(&b.label))
    });
}

/// The timeline's end: the start of the current 5-minute slot, less the publishing delay.
fn timeline_end(now: SystemTime) -> SystemTime {
    let secs = (now - PUBLISH_DELAY)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    UNIX_EPOCH + Duration::from_secs(secs - secs % SLOT.as_secs())
}

/// `bracket usage ec2`: loads 14 days of usage for every instance, then opens the dashboard.
pub async fn run() -> Result<()> {
    if !(console::Term::stdout().is_term() && console::Term::stderr().is_term()) {
        return Err(Error::Unexpected(
            "`bracket usage` needs an interactive terminal".into(),
        ));
    }
    ui::intro("EC2 usage")?;
    let aws = Aws::load().await?;
    let fleets = ec2::survey(&aws).await?;
    if fleets.is_empty() {
        cliclack::outro("No instances in any region")?;
        return Ok(());
    }

    let end = timeline_end(SystemTime::now());
    let start = end - SLOT * HISTORY as u32;
    let series = ui::step(
        "Fetching 14 days of CPU",
        async {
            let mut tasks = tokio::task::JoinSet::new();
            for fleet in &fleets {
                let cloudwatch = aws_sdk_cloudwatch::Client::from_conf(
                    aws_sdk_cloudwatch::config::Builder::from(&aws.config)
                        .region(Region::new(fleet.region.clone()))
                        .build(),
                );
                let ids: Vec<String> = fleet.instances.iter().map(|i| i.id.clone()).collect();
                tasks.spawn(async move {
                    metrics::cpu_series(&cloudwatch, metrics::EC2, &ids, start, end).await
                });
            }
            // Instance ids are unique across regions, so one map holds every series.
            tasks.join_all().await.into_iter().try_fold(
                HashMap::new(),
                |mut all: HashMap<String, Series>, found| {
                    all.extend(found?);
                    Ok(all)
                },
            )
        },
        |series| {
            let points: usize = series.values().map(Vec::len).sum();
            format!("Fetched {points} datapoints")
        },
    )
    .await?;

    let keys: BTreeSet<pricing::Key> = fleets
        .iter()
        .flat_map(|fleet| {
            fleet.instances.iter().map(|instance| pricing::Key {
                region: fleet.region.clone(),
                instance_type: instance.instance_type.clone(),
            })
        })
        .collect();
    let pricing = aws_sdk_pricing::Client::from_conf(
        aws_sdk_pricing::config::Builder::from(&aws.config)
            .region(Region::new(pricing::API_REGION))
            .build(),
    );
    let cache = pricing::Cache::from_cache_dir();
    let prices = ui::step(
        "Looking up on-demand prices",
        async { Ok(pricing::hourly(&pricing, cache.as_ref(), keys, SystemTime::now()).await) },
        |prices| match prices {
            Ok(prices) => format!("Priced {} instance types", prices.len()),
            Err(_) => "Prices unavailable".to_string(),
        },
    )
    .await?
    // Without prices the dashboard still has CPU, so carry on.
    .unwrap_or_else(|err| {
        let _ = cliclack::log::warning(format!("Showing CPU only: {err}"));
        HashMap::new()
    });

    let tracks = fleets
        .into_iter()
        .flat_map(|fleet| {
            let region = fleet.region;
            fleet
                .instances
                .into_iter()
                .map(move |instance| (region.clone(), instance))
        })
        .map(|(region, instance)| {
            let price = prices
                .get(&pricing::Key {
                    region: region.clone(),
                    instance_type: instance.instance_type.clone(),
                })
                .copied();
            let points = series.get(&instance.id).map_or(&[][..], Vec::as_slice);
            Track::new(instance, region, price, points, end)
        })
        .collect();

    cliclack::outro("Opening the dashboard. Press q to quit.")?;
    tui::run(Dataset { tracks })?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn instance(id: &str, name: &str, instance_type: &str, vcpus: u32) -> Instance {
        Instance {
            id: id.into(),
            name: Some(name.into()),
            state: State::Running,
            instance_type: instance_type.into(),
            public_ip: None,
            private_ip: None,
            vcpus: Some(vcpus),
            spot: false,
        }
    }

    /// A track with `cpu` in the last slots of the history, and nothing before.
    pub(crate) fn track(
        instance: Instance,
        region: &str,
        usd_per_hour: Option<f64>,
        recent: &[Option<f64>],
    ) -> Track {
        let mut cpu = vec![None; HISTORY - recent.len()];
        cpu.extend_from_slice(recent);
        Track {
            instance,
            region: region.into(),
            usd_per_hour,
            cpu,
        }
    }

    pub(crate) fn dataset() -> Dataset {
        Dataset {
            tracks: vec![
                // 2 vCPUs, $0.12/h, running at 10% for the last hour.
                track(
                    instance("i-a", "api", "t4g.small", 2),
                    "us-east-2",
                    Some(0.12),
                    &[Some(10.0); 12],
                ),
                // 32 vCPUs, $1.20/h, running at 50% for the last 2 hours.
                track(
                    instance("i-b", "build", "c7i.8xlarge", 32),
                    "us-east-1",
                    Some(1.2),
                    &[Some(50.0); 24],
                ),
                // Unpriced, ran at 90% for 5 minutes a quarter of an hour ago, stopped now.
                track(
                    Instance {
                        state: State::Stopped,
                        ..instance("i-c", "cache", "t4g.medium", 2)
                    },
                    "us-east-2",
                    None,
                    &[Some(90.0), None, None, None],
                ),
            ],
        }
    }

    fn at(slot: usize) -> SystemTime {
        UNIX_EPOCH + SLOT * slot as u32
    }

    #[test]
    fn datapoints_land_in_their_slots() {
        let end = at(HISTORY + 100);
        let points = [
            (at(100), 1.0),                            // first slot
            (at(100) + Duration::from_secs(299), 2.0), // still the first slot (last write wins)
            (at(HISTORY + 99), 3.0),                   // last slot
            (at(99), 4.0),                             // before the window
            (at(HISTORY + 100), 5.0),                  // at the end: outside
        ];
        let track = Track::new(
            instance("i-a", "a", "t4g.small", 2),
            "r".into(),
            None,
            &points,
            end,
        );
        assert_eq!(track.cpu.len(), HISTORY);
        assert_eq!(track.cpu[0], Some(2.0));
        assert_eq!(track.cpu[HISTORY - 1], Some(3.0));
        assert_eq!(track.cpu.iter().flatten().count(), 2);
    }

    #[test]
    fn families_come_from_the_instance_type() {
        assert_eq!(family("c7i.8xlarge"), "c7i");
        assert_eq!(family("u-6tb1.metal"), "u-6tb1");
        assert_eq!(family("weird"), "weird");
    }

    #[test]
    fn ranges_divide_into_whole_buckets() {
        for range in Range::ALL {
            assert_eq!(range.slots() % range.bucket(), 0, "{range:?}");
            assert!(
                (96..=168).contains(&(range.slots() / range.bucket())),
                "{range:?}"
            );
        }
        assert_eq!(Range::Fortnight.slots(), HISTORY);
    }

    #[test]
    fn cost_is_running_time_times_price() {
        let summary = total(&dataset(), Range::Day);
        // api: 1h × $0.12, build: 2h × $1.20, cache: unpriced.
        assert!((summary.cost.unwrap() - (0.12 + 2.4)).abs() < 1e-9);
        assert_eq!(summary.unpriced, 1);
        assert!((summary.rate_now - 1.32).abs() < 1e-9);
        assert_eq!(summary.detail, "3 instances in 2 regions");
    }

    #[test]
    fn cpu_is_weighted_by_vcpus() {
        let summary = total(&dataset(), Range::Day);
        // Slot-weighted: api 12 slots × 2 vCPU × 10%, build 24 × 32 × 50%, cache 1 × 2 × 90%.
        let expected = (12.0 * 2.0 * 10.0 + 24.0 * 32.0 * 50.0 + 2.0 * 90.0)
            / (12.0 * 2.0 + 24.0 * 32.0 + 2.0);
        assert!((summary.cpu.unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn chart_points_average_each_bucket() {
        let summary = total(&dataset(), Range::Day);
        assert_eq!(summary.cpu_points.len(), 96);
        assert_eq!(summary.rate_points.len(), 96);
        // The last 15 minutes: api and build both running.
        let last = summary.cpu_points[95].unwrap();
        assert!((last - (2.0 * 10.0 + 32.0 * 50.0) / 34.0).abs() < 1e-9);
        assert!((summary.rate_points[95] - 1.32).abs() < 1e-9);
        // Before anything ran there's no CPU and no spend.
        assert_eq!(summary.cpu_points[0], None);
        assert_eq!(summary.rate_points[0], 0.0);
    }

    #[test]
    fn nothing_ran_means_no_cpu_and_no_cost() {
        let idle = Dataset {
            tracks: vec![track(
                instance("i-z", "z", "t4g.small", 2),
                "r",
                Some(1.0),
                &[],
            )],
        };
        let summary = total(&idle, Range::Week);
        assert_eq!(summary.cpu, None);
        assert_eq!(summary.cost, Some(0.0));
        assert!(summary.cpu_points.iter().all(Option::is_none));
    }

    #[test]
    fn groups_split_by_instance_region_and_family() {
        let data = dataset();

        let by_instance = groups(&data, Grouping::Instance, Range::Day);
        let labels: Vec<_> = by_instance.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["api", "build", "cache"]);
        assert_eq!(by_instance[0].detail, "us-east-2 · t4g.small");
        assert_eq!(by_instance[2].cost, None);

        let by_region = groups(&data, Grouping::Region, Range::Day);
        let labels: Vec<_> = by_region
            .iter()
            .map(|s| (s.label.as_str(), s.detail.as_str()))
            .collect();
        assert_eq!(
            labels,
            [("us-east-1", "1 instance"), ("us-east-2", "2 instances")]
        );
        // A region with one unpriced instance still has a cost for the others.
        assert!((by_region[1].cost.unwrap() - 0.12).abs() < 1e-9);
        assert_eq!(by_region[1].unpriced, 1);

        let by_family = groups(&data, Grouping::Family, Range::Day);
        let labels: Vec<_> = by_family.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["c7i", "t4g"]);
    }

    #[test]
    fn spot_instances_are_marked() {
        let mut data = dataset();
        data.tracks[0].instance.spot = true;
        let by_instance = groups(&data, Grouping::Instance, Range::Day);
        assert_eq!(by_instance[0].detail, "us-east-2 · t4g.small · spot");
    }

    #[test]
    fn ranges_only_count_their_own_slots() {
        let mut data = dataset();
        // Ten days ago, api ran for an hour.
        let ten_days_ago = HISTORY - 10 * 24 * 12;
        (ten_days_ago..ten_days_ago + 12).for_each(|slot| data.tracks[0].cpu[slot] = Some(10.0));

        let day = total(&data, Range::Day).cost.unwrap();
        let week = total(&data, Range::Week).cost.unwrap();
        let fortnight = total(&data, Range::Fortnight).cost.unwrap();
        assert!((day - week).abs() < 1e-9);
        assert!((fortnight - week - 0.12).abs() < 1e-9);
    }

    #[test]
    fn sorting_puts_the_biggest_first_and_unknowns_last() {
        let mut summaries = groups(&dataset(), Grouping::Instance, Range::Day);

        sort(&mut summaries, Sort::Cost);
        let order: Vec<_> = summaries.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(order, ["build", "api", "cache"]);

        sort(&mut summaries, Sort::Cpu);
        let order: Vec<_> = summaries.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(order, ["cache", "build", "api"]);

        sort(&mut summaries, Sort::Name);
        let order: Vec<_> = summaries.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(order, ["api", "build", "cache"]);
    }

    #[test]
    fn the_timeline_ends_on_a_slot_boundary_before_the_publishing_delay() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000_123);
        let end = timeline_end(now);
        let secs = end.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs % 300, 0);
        let behind = (now - PUBLISH_DELAY).duration_since(end).unwrap();
        assert!(behind < SLOT);
    }

    #[test]
    fn sort_cycles_through_every_key() {
        assert_eq!(Sort::Cost.next().next().next(), Sort::Cost);
        assert_eq!(
            [Sort::Cost, Sort::Cpu, Sort::Name].map(Sort::label),
            ["cost", "cpu", "name"]
        );
    }
}
