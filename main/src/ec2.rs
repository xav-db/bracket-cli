//! EC2 dev boxes: finding them by Name tag, listing them, and moving them between
//! stopped and running.

use std::{collections::HashMap, time::Duration, time::Instant};

use aws_config::Region;
use aws_sdk_ec2::{
    Client,
    client::Waiters,
    types::{Filter, InstanceStateName},
};
use console::Style;

use crate::{
    aws::Aws,
    error::{self, Error, Resource, Result},
    metrics, state,
    ui::{self, Cell},
};

/// How long to wait for a box to boot or finish stopping before giving up.
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// The states bracket works with. Terminated and shutting-down instances are filtered out
/// server-side (see [`LIVE_STATES`]) so they can never be listed, picked, or started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Running,
    Pending,
    Stopping,
    Stopped,
}

/// The `instance-state-name` values matching [`State`]'s variants.
const LIVE_STATES: [&str; 4] = ["running", "pending", "stopping", "stopped"];

impl State {
    fn from_sdk(name: &InstanceStateName) -> Option<Self> {
        match name {
            InstanceStateName::Running => Some(State::Running),
            InstanceStateName::Pending => Some(State::Pending),
            InstanceStateName::Stopping => Some(State::Stopping),
            InstanceStateName::Stopped => Some(State::Stopped),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Pending => "pending",
            State::Stopping => "stopping",
            State::Stopped => "stopped",
        }
    }

    fn cell(self) -> Cell {
        let (dot, style) = match self {
            State::Running => ("●", Style::new().green()),
            State::Pending | State::Stopping => ("◐", Style::new().yellow()),
            State::Stopped => ("○", Style::new().dim()),
        };
        Cell::new(format!("{dot} {}", self.label())).style(style)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    pub name: Option<String>,
    pub state: State,
    pub instance_type: String,
    pub public_ip: Option<String>,
    pub private_ip: Option<String>,
}

impl Instance {
    /// Returns `None` for instances without an id or outside [`State`].
    fn from_sdk(instance: &aws_sdk_ec2::types::Instance) -> Option<Self> {
        Some(Instance {
            id: instance.instance_id()?.to_string(),
            name: instance
                .tags()
                .iter()
                .find(|tag| tag.key() == Some("Name"))
                .and_then(|tag| tag.value())
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            state: State::from_sdk(instance.state()?.name()?)?,
            instance_type: instance
                .instance_type()
                .map(|kind| kind.as_str().to_string())
                .unwrap_or_default(),
            public_ip: instance.public_ip_address().map(str::to_string),
            private_ip: instance.private_ip_address().map(str::to_string),
        })
    }

    /// The Name tag, or the instance id for untagged instances.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    fn summary(&self) -> String {
        format!(
            "{} · {} · {} · {}",
            self.display_name(),
            self.state.label(),
            self.instance_type,
            self.id
        )
    }
}

/// Fetches every live instance, or only those whose Name tag is exactly `name`.
/// Running instances sort first, then by name.
pub async fn describe(ec2: &Client, name: Option<&str>) -> Result<Vec<Instance>> {
    let states = Filter::builder()
        .name("instance-state-name")
        .set_values(Some(LIVE_STATES.map(String::from).to_vec()))
        .build();
    let filters = std::iter::once(states)
        .chain(name.map(|name| Filter::builder().name("tag:Name").values(name).build()))
        .collect();

    let pages = ec2
        .describe_instances()
        .set_filters(Some(filters))
        .into_paginator()
        .send()
        .try_collect()
        .await
        .map_err(Error::aws("listing EC2 instances"))?;

    let mut instances: Vec<Instance> = pages
        .iter()
        .flat_map(|page| page.reservations())
        .flat_map(|reservation| reservation.instances())
        .filter_map(Instance::from_sdk)
        .collect();
    instances.sort_by(|a, b| (a.state, a.display_name()).cmp(&(b.state, b.display_name())));
    Ok(instances)
}

/// Resolves the instance a command should act on: the one named `name`, or the user's pick.
///
/// A name shared by several instances also gets a picker rather than a guess.
pub async fn pick(ec2: &Client, name: Option<String>, prompt: &str) -> Result<Instance> {
    let start = match &name {
        Some(name) => format!("Looking up {name}"),
        None => "Fetching instances".to_string(),
    };
    let mut instances = ui::step(
        start,
        async {
            let instances = describe(ec2, name.as_deref()).await?;
            match (instances.is_empty(), &name) {
                (true, Some(name)) => Err(Error::NotFound {
                    kind: Resource::Ec2Instance,
                    name: name.clone(),
                }),
                (true, None) => Err(Error::Empty(Resource::Ec2Instance)),
                (false, _) => Ok(instances),
            }
        },
        |instances| match (instances.as_slice(), &name) {
            ([only], Some(_)) => only.summary(),
            (_, Some(name)) => format!("{} instances are named {name}", instances.len()),
            (_, None) => format!("Found {} instances", instances.len()),
        },
    )
    .await?;

    if name.is_some() && instances.len() == 1 {
        return Ok(instances.remove(0));
    }
    let index = instances
        .iter()
        .enumerate()
        .fold(cliclack::select(prompt), |select, (index, instance)| {
            let hint = format!(
                "{} · {} · {}",
                instance.state.label(),
                instance.instance_type,
                instance.id
            );
            select.item(index, instance.display_name(), hint)
        })
        .interact()?;
    Ok(instances.swap_remove(index))
}

/// Brings `instance` to `running`, asking before it starts a stopped box.
///
/// Returns `true` if the instance wasn't already running, since sshd may then still be booting.
pub async fn ensure_running(ec2: &Client, instance: &Instance) -> Result<bool> {
    let name = instance.display_name();
    let needs_start = match instance.state {
        State::Running => return Ok(false),
        State::Pending => false,
        State::Stopped => true,
        State::Stopping => {
            ui::step(
                format!("Waiting for {name} to finish stopping"),
                async {
                    ec2.wait_until_instance_stopped()
                        .instance_ids(&instance.id)
                        .wait(MAX_WAIT)
                        .await
                        .map_err(|err| Error::Aws {
                            action: "waiting for the instance to stop",
                            message: error::root_cause(&err),
                        })
                },
                |_| format!("{name} has stopped"),
            )
            .await?;
            true
        }
    };

    if needs_start
        && !cliclack::confirm(format!("{name} is stopped. Start it?"))
            .initial_value(true)
            .interact()?
    {
        return Err(Error::Cancelled);
    }

    let started = Instant::now();
    ui::step(
        format!("Starting {name}"),
        async {
            if needs_start {
                ec2.start_instances()
                    .instance_ids(&instance.id)
                    .send()
                    .await
                    .map_err(Error::aws("starting the instance"))?;
            }
            ec2.wait_until_instance_running()
                .instance_ids(&instance.id)
                .wait(MAX_WAIT)
                .await
                .map_err(|err| Error::Aws {
                    action: "waiting for the instance to start",
                    message: error::root_cause(&err),
                })
        },
        |_| format!("{name} is running ({})", ui::elapsed(started.elapsed())),
    )
    .await?;
    Ok(true)
}

/// The live instances in one region.
#[derive(Debug)]
struct Fleet {
    region: String,
    instances: Vec<Instance>,
}

/// Every region enabled for the account, which is what `DescribeRegions` returns by default.
async fn regions(ec2: &Client) -> Result<Vec<String>> {
    let output = ec2
        .describe_regions()
        .send()
        .await
        .map_err(Error::aws("listing regions"))?;
    Ok(output
        .regions()
        .iter()
        .filter_map(|region| region.region_name())
        .map(str::to_string)
        .collect())
}

/// Describes every region at once and keeps those with instances, sorted by region.
///
/// Also returns the regions that failed, so one blocked region (by an SCP, say) doesn't hide
/// the rest. Only when every region fails is that an error, usually bad credentials.
async fn scan(clients: Vec<(String, Client)>) -> Result<(Vec<Fleet>, Vec<String>)> {
    let total = clients.len();
    let mut tasks = tokio::task::JoinSet::new();
    for (region, client) in clients {
        tasks.spawn(async move {
            let found = describe(&client, None).await;
            (region, found)
        });
    }
    let mut results = tasks.join_all().await;
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let mut fleets = Vec::new();
    let mut failed = Vec::new();
    let mut first_error = None;
    for (region, found) in results {
        match found {
            Ok(instances) if instances.is_empty() => {}
            Ok(instances) => fleets.push(Fleet { region, instances }),
            Err(err) => {
                failed.push(region);
                first_error.get_or_insert(err);
            }
        }
    }
    match first_error {
        Some(err) if failed.len() == total => Err(err),
        _ => Ok((fleets, failed)),
    }
}

/// The fleet to start the region picker on: the region shown last time, else the configured
/// region, else the first.
fn preselect(fleets: &[Fleet], last: Option<&str>, configured: &str) -> usize {
    [last, Some(configured)]
        .into_iter()
        .flatten()
        .find_map(|region| fleets.iter().position(|fleet| fleet.region == region))
        .unwrap_or_default()
}

/// `bracket list ec2`: finds the regions with instances, asks which to show (starting on the
/// one shown last time), then lists it.
pub async fn list() -> Result<()> {
    ui::intro("EC2 instances")?;
    let aws = Aws::load().await?;
    let in_region = |region: &str| {
        aws_sdk_ec2::config::Builder::from(&aws.config)
            .region(Region::new(region.to_string()))
            .build()
    };

    let (mut fleets, failed, _) = ui::step(
        "Scanning regions",
        async {
            let regions = regions(&Client::new(&aws.config)).await?;
            let total = regions.len();
            let clients = regions
                .into_iter()
                .map(|region| {
                    let client = Client::from_conf(in_region(&region));
                    (region, client)
                })
                .collect();
            let (fleets, failed) = scan(clients).await?;
            Ok((fleets, failed, total))
        },
        |(fleets, _, total)| format!("Found instances in {} of {total} regions", fleets.len()),
    )
    .await?;

    if !failed.is_empty() {
        cliclack::log::warning(format!(
            "Skipped regions that didn't answer: {}",
            failed.join(", ")
        ))?;
    }
    if fleets.is_empty() {
        cliclack::outro("No instances in any region")?;
        return Ok(());
    }

    let store = state::Store::from_config_dir();
    let initial = preselect(
        &fleets,
        store
            .as_ref()
            .and_then(state::Store::last_region)
            .as_deref(),
        &aws.region,
    );
    // Only ask when there's a choice and someone to ask; otherwise take the preselection.
    let index = match fleets.len() > 1 && console::Term::stderr().is_term() {
        true => {
            // Counts go in the label, since Clack only shows hints on the focused row.
            let width = fleets
                .iter()
                .map(|fleet| fleet.region.len())
                .max()
                .unwrap_or_default();
            fleets
                .iter()
                .enumerate()
                .fold(
                    cliclack::select("Which region?").initial_value(initial),
                    |select, (index, fleet)| {
                        let counts = console::style(state_counts(&fleet.instances))
                            .dim()
                            .for_stderr();
                        select.item(index, format!("{:<width$}  {counts}", fleet.region), "")
                    },
                )
                .interact()?
        }
        false => initial,
    };
    let Fleet { region, instances } = fleets.swap_remove(index);
    store
        .as_ref()
        .map_or(Ok(()), |store| store.set_last_region(&region))
        .or_else(|err| {
            cliclack::log::warning(format!("Couldn't remember {region} for next time: {err}"))
        })?;

    let cloudwatch = aws_sdk_cloudwatch::Client::from_conf(
        aws_sdk_cloudwatch::config::Builder::from(&aws.config)
            .region(Region::new(region.clone()))
            .build(),
    );
    let running: Vec<String> = instances
        .iter()
        .filter(|instance| instance.state == State::Running)
        .map(|instance| instance.id.clone())
        .collect();
    let cpu = ui::step(
        format!("Fetching CPU in {region}"),
        async { Ok(metrics::latest_cpu(&cloudwatch, metrics::EC2, &running).await) },
        |_| format!("{} instances in {region}", instances.len()),
    )
    .await?
    // CPU is a nice-to-have: without CloudWatch access the table still lists the fleet.
    .unwrap_or_else(|err| {
        let _ = cliclack::log::warning(format!("CPU unavailable: {err}"));
        HashMap::new()
    });

    let rows: Vec<[Cell; 6]> = instances
        .iter()
        .map(|instance| row(instance, cpu.get(&instance.id).copied(), &region))
        .collect();
    ui::print_table(["NAME", "STATE", "TYPE", "CPU", "INSTANCE ID", "IP"], &rows);

    cliclack::outro(state_counts(&instances))?;
    Ok(())
}

fn row(instance: &Instance, cpu: Option<f64>, region: &str) -> [Cell; 6] {
    let console = format!(
        "https://{region}.console.aws.amazon.com/ec2/home?region={region}#InstanceDetails:instanceId={}",
        instance.id
    );
    let name = match &instance.name {
        Some(name) => Cell::new(name).style(Style::new().bold()),
        None => Cell::new("(unnamed)").style(Style::new().dim()),
    };
    let cpu = match cpu {
        Some(percent) => Cell::new(format!("{percent:.1}%")).style(match percent {
            80.0.. => Style::new().red(),
            50.0.. => Style::new().yellow(),
            _ => Style::new(),
        }),
        None => Cell::missing(),
    };
    let ip = match (&instance.public_ip, &instance.private_ip) {
        (Some(public), _) => Cell::new(public),
        (None, Some(private)) => {
            Cell::new(format!("{private} (private)")).style(Style::new().dim())
        }
        (None, None) => Cell::missing(),
    };
    [
        name.link(console),
        instance.state.cell(),
        Cell::new(&instance.instance_type),
        cpu,
        Cell::new(&instance.id).style(Style::new().dim()),
        ip,
    ]
}

/// "2 running · 1 stopped", in [`State`] order, skipping states with no instances.
fn state_counts(instances: &[Instance]) -> String {
    [
        State::Running,
        State::Pending,
        State::Stopping,
        State::Stopped,
    ]
    .into_iter()
    .map(|state| (state, instances.iter().filter(|i| i.state == state).count()))
    .filter(|(_, count)| *count > 0)
    .map(|(state, count)| format!("{count} {}", state.label()))
    .collect::<Vec<_>>()
    .join(" · ")
}

/// `bracket stop ec2`.
pub async fn stop(name: Option<String>) -> Result<()> {
    ui::intro("stop EC2 instance")?;
    let aws = Aws::load().await?;
    stop_with(&Client::new(&aws.config), name).await
}

/// `bracket stop ec2` after setup, taking the client so tests can mock it.
async fn stop_with(ec2: &Client, name: Option<String>) -> Result<()> {
    let instance = pick(ec2, name, "Which instance should be stopped?").await?;
    let name = instance.display_name();

    match instance.state {
        State::Stopping | State::Stopped => {
            cliclack::outro(format!("{name} is already {}", instance.state.label()))?;
        }
        State::Running | State::Pending => {
            ui::step(
                format!("Stopping {name}"),
                async {
                    ec2.stop_instances()
                        .instance_ids(&instance.id)
                        .send()
                        .await
                        .map_err(Error::aws("stopping the instance"))
                },
                |_| format!("Stop requested for {name}"),
            )
            .await?;
            cliclack::outro(format!(
                "{name} is stopping. It's usually down within a minute."
            ))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ec2::{
        operation::describe_instances::DescribeInstancesOutput,
        types::{InstanceState, InstanceType, Reservation, Tag},
    };
    use aws_smithy_mocks::{mock, mock_client};

    fn sdk_instance(
        id: &str,
        name: Option<&str>,
        state: InstanceStateName,
    ) -> aws_sdk_ec2::types::Instance {
        let builder = aws_sdk_ec2::types::Instance::builder()
            .instance_id(id)
            .instance_type(InstanceType::T3Large)
            .state(InstanceState::builder().name(state).build())
            .private_ip_address("10.0.0.5");
        match name {
            Some(name) => builder.tags(Tag::builder().key("Name").value(name).build()),
            None => builder,
        }
        .build()
    }

    fn instance(name: &str, state: State) -> Instance {
        Instance {
            id: format!("i-{name}"),
            name: Some(name.into()),
            state,
            instance_type: "t3.large".into(),
            public_ip: None,
            private_ip: None,
        }
    }

    #[test]
    fn sdk_instances_convert_with_their_name_tag() {
        let converted = Instance::from_sdk(&sdk_instance(
            "i-1",
            Some("dev"),
            InstanceStateName::Running,
        ))
        .unwrap();
        assert_eq!(
            converted,
            Instance {
                id: "i-1".into(),
                name: Some("dev".into()),
                state: State::Running,
                instance_type: "t3.large".into(),
                public_ip: None,
                private_ip: Some("10.0.0.5".into()),
            }
        );
        assert_eq!(converted.display_name(), "dev");
    }

    #[test]
    fn untagged_instances_fall_back_to_their_id() {
        let converted =
            Instance::from_sdk(&sdk_instance("i-2", None, InstanceStateName::Stopped)).unwrap();
        assert_eq!(converted.name, None);
        assert_eq!(converted.display_name(), "i-2");

        let blank =
            Instance::from_sdk(&sdk_instance("i-3", Some(""), InstanceStateName::Stopped)).unwrap();
        assert_eq!(blank.display_name(), "i-3");
    }

    #[test]
    fn dead_and_malformed_instances_are_dropped() {
        for state in [
            InstanceStateName::Terminated,
            InstanceStateName::ShuttingDown,
        ] {
            assert_eq!(
                Instance::from_sdk(&sdk_instance("i-1", Some("x"), state)),
                None
            );
        }
        let no_id = aws_sdk_ec2::types::Instance::builder()
            .state(
                InstanceState::builder()
                    .name(InstanceStateName::Running)
                    .build(),
            )
            .build();
        assert_eq!(Instance::from_sdk(&no_id), None);
    }

    #[test]
    fn live_state_filter_matches_the_state_enum() {
        let parsed: Vec<_> = LIVE_STATES
            .iter()
            .map(|name| State::from_sdk(&InstanceStateName::from(*name)))
            .collect();
        assert_eq!(
            parsed,
            [
                State::Running,
                State::Pending,
                State::Stopping,
                State::Stopped
            ]
            .map(Some)
        );
        assert!(
            parsed
                .iter()
                .zip(LIVE_STATES)
                .all(|(state, name)| state.unwrap().label() == name)
        );
    }

    #[test]
    fn state_counts_skip_empty_states() {
        let fleet = [
            instance("a", State::Stopped),
            instance("b", State::Running),
            instance("c", State::Running),
        ];
        assert_eq!(state_counts(&fleet), "2 running · 1 stopped");
        assert_eq!(state_counts(&[]), "");
    }

    #[test]
    fn rows_prefer_the_public_ip_and_flag_high_cpu() {
        let mut box_ = instance("dev", State::Running);
        box_.private_ip = Some("10.0.0.1".into());
        let [name, state, _, cpu, _, ip] = row(&box_, Some(91.25), "us-east-2");
        assert_eq!(
            name.link
                .as_deref()
                .map(|url| url.ends_with("instanceId=i-dev")),
            Some(true)
        );
        assert_eq!(state.text, "● running");
        assert_eq!(cpu.text, "91.2%");
        assert_eq!(ip.text, "10.0.0.1 (private)");

        box_.public_ip = Some("3.3.3.3".into());
        let [.., ip] = row(&box_, None, "us-east-2");
        assert_eq!(ip.text, "3.3.3.3");
    }

    #[tokio::test]
    async fn describe_filters_server_side_and_sorts_running_first() {
        let rule = mock!(Client::describe_instances)
            .match_requests(|req| {
                let names: Vec<_> = req.filters().iter().filter_map(|f| f.name()).collect();
                names == ["instance-state-name", "tag:Name"] && req.filters()[1].values() == ["dev"]
            })
            .then_output(|| {
                DescribeInstancesOutput::builder()
                    .reservations(
                        Reservation::builder()
                            .instances(sdk_instance("i-2", Some("dev"), InstanceStateName::Stopped))
                            .instances(sdk_instance("i-1", Some("dev"), InstanceStateName::Running))
                            .build(),
                    )
                    .build()
            });
        let client = mock_client!(aws_sdk_ec2, [&rule]);

        let found = describe(&client, Some("dev")).await.unwrap();

        assert_eq!(
            found.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["i-1", "i-2"]
        );
    }

    #[tokio::test]
    async fn describe_follows_pagination() {
        let rule = mock!(Client::describe_instances)
            .sequence()
            .output(|| {
                DescribeInstancesOutput::builder()
                    .reservations(
                        Reservation::builder()
                            .instances(sdk_instance("i-1", Some("b"), InstanceStateName::Stopped))
                            .build(),
                    )
                    .next_token("page-2")
                    .build()
            })
            .output(|| {
                DescribeInstancesOutput::builder()
                    .reservations(
                        Reservation::builder()
                            .instances(sdk_instance("i-2", Some("a"), InstanceStateName::Stopped))
                            .build(),
                    )
                    .build()
            })
            .build();
        let client = mock_client!(aws_sdk_ec2, [&rule]);

        let found = describe(&client, None).await.unwrap();

        assert_eq!(rule.num_calls(), 2);
        assert_eq!(
            found.iter().map(Instance::display_name).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[tokio::test]
    async fn picking_an_unknown_name_is_not_found() {
        let rule = mock!(Client::describe_instances)
            .then_output(|| DescribeInstancesOutput::builder().build());
        let client = mock_client!(aws_sdk_ec2, [&rule]);

        let err = pick(&client, Some("ghost".into()), "?").await.unwrap_err();

        assert!(
            matches!(err, Error::NotFound { kind: Resource::Ec2Instance, ref name } if name == "ghost")
        );
    }

    #[tokio::test]
    async fn picking_a_unique_name_needs_no_prompt() {
        let rule = mock!(Client::describe_instances).then_output(|| {
            DescribeInstancesOutput::builder()
                .reservations(
                    Reservation::builder()
                        .instances(sdk_instance("i-9", Some("dev"), InstanceStateName::Running))
                        .build(),
                )
                .build()
        });
        let client = mock_client!(aws_sdk_ec2, [&rule]);

        let picked = pick(&client, Some("dev".into()), "?").await.unwrap();

        assert_eq!(picked.id, "i-9");
    }

    #[tokio::test]
    async fn an_empty_region_is_reported_as_empty() {
        let rule = mock!(Client::describe_instances)
            .then_output(|| DescribeInstancesOutput::builder().build());
        let client = mock_client!(aws_sdk_ec2, [&rule]);

        let err = pick(&client, None, "?").await.unwrap_err();

        assert!(matches!(err, Error::Empty(Resource::Ec2Instance)));
    }

    #[tokio::test]
    async fn running_instances_are_left_alone() {
        let client = mock_client!(aws_sdk_ec2, []);
        let booted = ensure_running(&client, &instance("dev", State::Running))
            .await
            .unwrap();
        assert!(!booted);
    }

    fn fleet(region: &str) -> Fleet {
        Fleet {
            region: region.into(),
            instances: vec![instance("dev", State::Running)],
        }
    }

    #[test]
    fn the_last_region_is_preselected_then_the_configured_one() {
        let fleets = [fleet("ap-south-1"), fleet("eu-west-2"), fleet("us-east-2")];
        assert_eq!(preselect(&fleets, Some("us-east-2"), "eu-west-2"), 2);
        assert_eq!(preselect(&fleets, None, "eu-west-2"), 1);
        // A remembered region that has since emptied falls back to the configured one...
        assert_eq!(preselect(&fleets, Some("sa-east-1"), "eu-west-2"), 1);
        // ...and failing that, the first region.
        assert_eq!(preselect(&fleets, Some("sa-east-1"), "us-west-1"), 0);
    }

    #[tokio::test]
    async fn regions_come_from_describe_regions() {
        use aws_sdk_ec2::{
            operation::describe_regions::DescribeRegionsOutput, types::Region as SdkRegion,
        };
        let rule = mock!(Client::describe_regions).then_output(|| {
            DescribeRegionsOutput::builder()
                .regions(SdkRegion::builder().region_name("eu-west-2").build())
                .regions(SdkRegion::builder().build())
                .regions(SdkRegion::builder().region_name("us-east-2").build())
                .build()
        });
        let client = mock_client!(aws_sdk_ec2, [&rule]);

        assert_eq!(regions(&client).await.unwrap(), ["eu-west-2", "us-east-2"]);
    }

    fn region_client(instances: Vec<aws_sdk_ec2::types::Instance>) -> Client {
        let rule = mock!(Client::describe_instances).then_output(move || {
            DescribeInstancesOutput::builder()
                .reservations(
                    Reservation::builder()
                        .set_instances(Some(instances.clone()))
                        .build(),
                )
                .build()
        });
        mock_client!(aws_sdk_ec2, aws_smithy_mocks::RuleMode::MatchAny, [&rule])
    }

    fn failing_client() -> Client {
        let rule = mock!(Client::describe_instances).then_error(|| {
            aws_sdk_ec2::operation::describe_instances::DescribeInstancesError::generic(
                aws_sdk_ec2::error::ErrorMetadata::builder()
                    .code("UnauthorizedOperation")
                    .message("denied by SCP")
                    .build(),
            )
        });
        mock_client!(aws_sdk_ec2, aws_smithy_mocks::RuleMode::MatchAny, [&rule])
    }

    #[tokio::test]
    async fn scan_keeps_regions_with_instances_and_reports_failures() {
        let clients = vec![
            (
                "us-east-2".to_string(),
                region_client(vec![sdk_instance(
                    "i-1",
                    Some("a"),
                    InstanceStateName::Running,
                )]),
            ),
            ("ap-south-1".to_string(), failing_client()),
            ("eu-west-1".to_string(), region_client(vec![])),
            (
                "eu-west-2".to_string(),
                region_client(vec![sdk_instance(
                    "i-2",
                    Some("b"),
                    InstanceStateName::Stopped,
                )]),
            ),
        ];

        let (fleets, failed) = scan(clients).await.unwrap();

        let regions: Vec<_> = fleets.iter().map(|fleet| fleet.region.as_str()).collect();
        assert_eq!(regions, ["eu-west-2", "us-east-2"]);
        assert_eq!(fleets[1].instances[0].id, "i-1");
        assert_eq!(failed, ["ap-south-1"]);
    }

    #[tokio::test]
    async fn scan_fails_only_when_every_region_fails() {
        let clients = vec![
            ("eu-west-2".to_string(), failing_client()),
            ("us-east-2".to_string(), failing_client()),
        ];
        let err = scan(clients).await.unwrap_err();
        assert!(err.to_string().contains("UnauthorizedOperation"), "{err}");

        let (fleets, failed) = scan(vec![]).await.unwrap();
        assert!(fleets.is_empty() && failed.is_empty());
    }

    fn stop_rule() -> aws_smithy_mocks::Rule {
        mock!(Client::stop_instances)
            .match_requests(|req| req.instance_ids() == ["i-1"])
            .then_output(|| {
                aws_sdk_ec2::operation::stop_instances::StopInstancesOutput::builder().build()
            })
    }

    fn describe_rule(state: InstanceStateName) -> aws_smithy_mocks::Rule {
        mock!(Client::describe_instances).then_output(move || {
            DescribeInstancesOutput::builder()
                .reservations(
                    Reservation::builder()
                        .instances(sdk_instance("i-1", Some("dev"), state.clone()))
                        .build(),
                )
                .build()
        })
    }

    #[tokio::test]
    async fn stopping_a_running_instance_sends_one_stop() {
        let (describe, stop) = (describe_rule(InstanceStateName::Running), stop_rule());
        let client = mock_client!(
            aws_sdk_ec2,
            aws_smithy_mocks::RuleMode::MatchAny,
            [&describe, &stop]
        );

        stop_with(&client, Some("dev".into())).await.unwrap();

        assert_eq!(stop.num_calls(), 1);
    }

    #[tokio::test]
    async fn stopping_a_stopped_instance_does_nothing() {
        let (describe, stop) = (describe_rule(InstanceStateName::Stopped), stop_rule());
        let client = mock_client!(
            aws_sdk_ec2,
            aws_smithy_mocks::RuleMode::MatchAny,
            [&describe, &stop]
        );

        stop_with(&client, Some("dev".into())).await.unwrap();

        assert_eq!(stop.num_calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_instances_are_waited_for_not_started() {
        let describe = describe_rule(InstanceStateName::Running);
        let start = mock!(Client::start_instances).then_output(|| {
            aws_sdk_ec2::operation::start_instances::StartInstancesOutput::builder().build()
        });
        let client = mock_client!(
            aws_sdk_ec2,
            aws_smithy_mocks::RuleMode::MatchAny,
            [&describe, &start]
        );

        let booted = ensure_running(&client, &instance("dev", State::Pending))
            .await
            .unwrap();

        assert!(booted);
        assert_eq!(start.num_calls(), 0);
        assert!(describe.num_calls() >= 1);
    }
}
