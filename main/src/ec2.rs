//! EC2 dev boxes: finding them by Name tag, listing them, and moving them between
//! stopped and running.

use std::{collections::HashMap, time::Duration, time::Instant};

use aws_sdk_ec2::{
    Client,
    client::Waiters,
    types::{Filter, InstanceStateName},
};
use console::Style;

use crate::{
    aws::Aws,
    error::{self, Error, Resource, Result},
    metrics,
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

/// `bracket list ec2`.
pub async fn list() -> Result<()> {
    ui::intro("EC2 instances")?;
    let aws = Aws::load().await?;
    let ec2 = Client::new(&aws.config);
    let cloudwatch = aws_sdk_cloudwatch::Client::new(&aws.config);

    let (instances, cpu) = ui::step(
        "Fetching instances",
        async {
            let instances = describe(&ec2, None).await?;
            let running: Vec<String> = instances
                .iter()
                .filter(|instance| instance.state == State::Running)
                .map(|instance| instance.id.clone())
                .collect();
            let cpu = metrics::latest_cpu(&cloudwatch, metrics::EC2, &running).await;
            Ok((instances, cpu))
        },
        |(instances, _)| format!("{} instances in {}", instances.len(), aws.region),
    )
    .await?;

    if instances.is_empty() {
        cliclack::outro("Nothing to show")?;
        return Ok(());
    }
    // CPU is a nice-to-have: without CloudWatch access the table still lists the fleet.
    let cpu = cpu.unwrap_or_else(|err| {
        let _ = cliclack::log::warning(format!("CPU unavailable: {err}"));
        HashMap::new()
    });

    let rows: Vec<[Cell; 6]> = instances
        .iter()
        .map(|instance| row(instance, cpu.get(&instance.id).copied(), &aws.region))
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
    let ec2 = Client::new(&aws.config);
    let instance = pick(&ec2, name, "Which instance should be stopped?").await?;
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
}
