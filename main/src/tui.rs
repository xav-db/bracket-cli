//! The `bracket usage ec2` dashboard: groups in a table on the left, and CPU and cost over time
//! for the selected one on the right.

use std::io;

use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, Cell, Chart, Dataset as Series, GraphType, Paragraph, Row, Table, TableState,
    },
};

use crate::usage::{self, Dataset, Grouping, Range, Sort, Summary};

/// Hours in an average month, for the monthly projection.
const HOURS_PER_MONTH: f64 = 730.0;

const ACCENT: Color = Color::Cyan;

/// Runs the dashboard until the user quits.
pub fn run(dataset: Dataset) -> io::Result<()> {
    let mut app = App::new(dataset);
    ratatui::run(|terminal| {
        loop {
            terminal.draw(|frame| app.render(frame))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Press && app.handle(key) == Flow::Quit {
                return Ok(());
            }
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

struct App {
    dataset: Dataset,
    grouping: Grouping,
    range: Range,
    sort: Sort,
    /// "All instances" first, then the groups in `sort` order. Never empty.
    rows: Vec<Summary>,
    table: TableState,
}

impl App {
    fn new(dataset: Dataset) -> Self {
        let mut app = App {
            dataset,
            grouping: Grouping::Instance,
            range: Range::Week,
            sort: Sort::Cost,
            rows: Vec::new(),
            table: TableState::default().with_selected(0),
        };
        app.refresh();
        app
    }

    /// Recomputes the rows, keeping the same group selected if it's still there.
    fn refresh(&mut self) {
        let selected = self.rows.get(self.selected()).map(|row| row.label.clone());
        let mut groups = usage::groups(&self.dataset, self.grouping, self.range);
        usage::sort(&mut groups, self.sort);
        self.rows = std::iter::once(usage::total(&self.dataset, self.range))
            .chain(groups)
            .collect();
        let index = selected
            .and_then(|label| self.rows.iter().position(|row| row.label == label))
            .unwrap_or_default();
        self.select(index);
    }

    fn selected(&self) -> usize {
        self.table.selected().unwrap_or_default()
    }

    fn select(&mut self, index: usize) {
        self.table
            .select(Some(index.min(self.rows.len().saturating_sub(1))));
    }

    fn cycle<T: Copy + PartialEq, const N: usize>(all: [T; N], current: T, step: isize) -> T {
        let index = all
            .iter()
            .position(|item| *item == current)
            .unwrap_or_default();
        all[(index as isize + step).rem_euclid(N as isize) as usize]
    }

    fn set_grouping(&mut self, grouping: Grouping) {
        self.grouping = grouping;
        self.select(0);
        self.refresh();
    }

    fn handle(&mut self, key: KeyEvent) -> Flow {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Flow::Quit;
            }
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected() + 1),
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected().saturating_sub(1)),
            KeyCode::PageDown => self.select(self.selected() + 10),
            KeyCode::PageUp => self.select(self.selected().saturating_sub(10)),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(']') => {
                self.range = Self::cycle(Range::ALL, self.range, 1);
                self.refresh();
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('[') => {
                self.range = Self::cycle(Range::ALL, self.range, -1);
                self.refresh();
            }
            KeyCode::Tab => self.set_grouping(Self::cycle(Grouping::ALL, self.grouping, 1)),
            KeyCode::BackTab => self.set_grouping(Self::cycle(Grouping::ALL, self.grouping, -1)),
            KeyCode::Char(digit @ '1'..='3') => {
                self.set_grouping(Grouping::ALL[digit as usize - '1' as usize]);
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.refresh();
            }
            _ => {}
        }
        Flow::Continue
    }

    fn render(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(2),
        ])
        .areas(frame.area());
        let [list, charts] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .areas(body);
        let [cpu, cost] =
            Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(charts);

        frame.render_widget(self.header(), header);
        self.render_table(frame, list);
        let selected = &self.rows[self.selected()];
        render_cpu(frame, cpu, selected, self.range);
        render_cost(frame, cost, selected, self.range);
        frame.render_widget(footer_text(self.sort), footer);
    }

    fn header(&self) -> Line<'static> {
        let tab = |label: String, active: bool| match active {
            true => Span::styled(label, Style::new().fg(Color::Black).bg(ACCENT).bold()),
            false => Span::styled(label, Style::new().dim()),
        };
        let mut spans = vec![
            Span::styled(" bracket ", Style::new().fg(Color::Black).bg(ACCENT)),
            Span::raw(" "),
            Span::styled("EC2 usage", Style::new().bold()),
            Span::raw("    "),
        ];
        spans.extend(
            Range::ALL
                .into_iter()
                .map(|range| tab(format!(" {} ", range.label()), range == self.range)),
        );
        spans.push(Span::raw("    "));
        spans.extend(
            Grouping::ALL
                .into_iter()
                .enumerate()
                .map(|(index, grouping)| {
                    tab(
                        format!(" {} {} ", index + 1, grouping.label()),
                        grouping == self.grouping,
                    )
                }),
        );
        Line::from(spans)
    }

    fn render_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(
            ["NAME", "DETAIL", "CPU", "COST", "NOW"]
                .map(|title| Cell::from(title).style(Style::new().dim())),
        );
        let rows = self.rows.iter().enumerate().map(|(index, row)| {
            let name = Cell::from(row.label.clone());
            let style = match index {
                0 => Style::new().bold(),
                _ => Style::new(),
            };
            Row::new([
                name,
                Cell::from(row.detail.clone()).style(Style::new().dim()),
                Cell::from(row.cpu.map_or("—".into(), |cpu| format!("{cpu:.1}%")))
                    .style(cpu_style(row.cpu)),
                Cell::from(cost_label(row)),
                Cell::from(match row.rate_now {
                    0.0 => "—".to_string(),
                    rate => format!("{}/h", usd(rate)),
                }),
            ])
            .style(style)
        });
        let table = Table::new(
            rows,
            [
                Constraint::Fill(2),
                Constraint::Fill(3),
                Constraint::Length(6),
                Constraint::Length(9),
                Constraint::Length(9),
            ],
        )
        .header(header)
        .row_highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▶ ")
        .block(Block::bordered().title(format!(
            " {} · last {} · by {} ",
            self.grouping.label(),
            self.range.label(),
            self.sort.label()
        )));
        frame.render_stateful_widget(table, area, &mut self.table);
    }
}

fn cpu_style(cpu: Option<f64>) -> Style {
    match cpu {
        Some(80.0..) => Style::new().fg(Color::Red),
        Some(50.0..) => Style::new().fg(Color::Yellow),
        _ => Style::new(),
    }
}

/// The range's cost, with `+` when some instances couldn't be priced and are missing from it.
fn cost_label(row: &Summary) -> String {
    match (row.cost, row.unpriced) {
        (None, _) => "—".into(),
        (Some(cost), 0) => usd(cost),
        (Some(cost), _) => format!("{}+", usd(cost)),
    }
}

/// `$0.017`, `$12.40`, `$1,234`.
fn usd(amount: f64) -> String {
    match amount {
        amount if amount < 1.0 => format!("${amount:.3}"),
        amount if amount < 100.0 => format!("${amount:.2}"),
        amount => {
            let digits = (amount.round() as u64).to_string();
            let grouped: Vec<String> = digits
                .as_bytes()
                .rchunks(3)
                .rev()
                .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                .collect();
            format!("${}", grouped.join(","))
        }
    }
}

/// Splits chart points into runs of known values, so gaps (nothing running) aren't drawn over.
fn runs(points: &[Option<f64>]) -> Vec<Vec<(f64, f64)>> {
    points
        .iter()
        .enumerate()
        .fold(vec![Vec::new()], |mut runs, (index, point)| {
            match point {
                Some(value) => runs
                    .last_mut()
                    .expect("starts with one run")
                    .push((index as f64, *value)),
                None if runs.last().is_some_and(|run| !run.is_empty()) => runs.push(Vec::new()),
                None => {}
            }
            runs
        })
        .into_iter()
        .filter(|run| !run.is_empty())
        .collect()
}

/// `-7d`, `-3.5d`, `now`: relative times under the chart.
fn x_axis(range: Range, points: usize) -> Axis<'static> {
    let labels = match range {
        Range::Day => ["-24h", "-12h", "now"],
        Range::Week => ["-7d", "-3.5d", "now"],
        Range::Fortnight => ["-14d", "-7d", "now"],
    };
    Axis::default()
        .bounds([0.0, points.saturating_sub(1) as f64])
        .labels(labels.map(|label| Span::styled(label, Style::new().dim())))
}

fn render_cpu(frame: &mut Frame, area: Rect, row: &Summary, range: Range) {
    let runs = runs(&row.cpu_points);
    let peak = runs
        .iter()
        .flatten()
        .map(|(_, cpu)| *cpu)
        .fold(0.0, f64::max);
    // Zoom in on quiet fleets, in steps of 10%.
    let top = ((peak * 1.2 / 10.0).ceil() * 10.0).clamp(10.0, 100.0);
    let datasets = runs
        .iter()
        .map(|run| {
            Series::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(ACCENT))
                .data(run)
        })
        .collect();
    let title = match row.cpu {
        Some(cpu) => format!(" CPU · {} · avg {cpu:.1}% ", row.label),
        None => format!(" CPU · {} · not running ", row.label),
    };
    let chart = Chart::new(datasets)
        .block(Block::bordered().title(title))
        .x_axis(x_axis(range, row.cpu_points.len()))
        .y_axis(Axis::default().bounds([0.0, top]).labels([
            "0%".to_string(),
            format!("{}%", top / 2.0),
            format!("{top}%"),
        ]));
    frame.render_widget(chart, area);
}

fn render_cost(frame: &mut Frame, area: Rect, row: &Summary, range: Range) {
    let points: Vec<(f64, f64)> = row
        .rate_points
        .iter()
        .enumerate()
        .map(|(index, rate)| (index as f64, *rate))
        .collect();
    let peak = row.rate_points.iter().copied().fold(0.0, f64::max);
    let top = match peak {
        0.0 => 1.0,
        peak => peak * 1.2,
    };
    let title = match row.cost {
        Some(_) => format!(
            " $/h · {} over {} · now {}/h ≈ {}/mo ",
            cost_label(row),
            range.label(),
            usd(row.rate_now),
            usd(row.rate_now * HOURS_PER_MONTH)
        ),
        None => " $/h · no price for this instance type ".to_string(),
    };
    let chart = Chart::new(vec![
        Series::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(Color::Green))
            .data(&points),
    ])
    .block(Block::bordered().title(title))
    .x_axis(x_axis(range, points.len()))
    .y_axis(
        Axis::default()
            .bounds([0.0, top])
            .labels(["$0".to_string(), format!("{}/h", usd(top))]),
    );
    frame.render_widget(chart, area);
}

fn footer_text(sort: Sort) -> Paragraph<'static> {
    let key = |key: &'static str| Span::styled(key, Style::new().fg(ACCENT).bold());
    let help = Line::from(vec![
        key(" ↑↓"),
        Span::raw(" select  "),
        key("←→"),
        Span::raw(" range  "),
        key("tab"),
        Span::raw(" group  "),
        key("s"),
        Span::raw(format!(" sort ({})  ", sort.label())),
        key("q"),
        Span::raw(" quit"),
    ]);
    let note = Line::styled(
        " Estimated: on-demand Linux price × running time. Leaves out EBS, data transfer, and \
         discounts; spot runs at on-demand price. + means some instances have no price.",
        Style::new().add_modifier(Modifier::DIM),
    );
    Paragraph::new(vec![help, note])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::tests::dataset;
    use ratatui::{Terminal, backend::TestBackend};

    fn press(app: &mut App, code: KeyCode) -> Flow {
        app.handle(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn money_reads_naturally() {
        assert_eq!(usd(0.0168), "$0.017");
        assert_eq!(usd(12.4), "$12.40");
        assert_eq!(usd(1234.4), "$1,234");
        assert_eq!(usd(1_234_567.0), "$1,234,567");
        assert_eq!(usd(100.0), "$100");
    }

    #[test]
    fn runs_split_at_gaps() {
        let points = [None, Some(1.0), Some(2.0), None, None, Some(3.0), None];
        assert_eq!(
            runs(&points),
            vec![vec![(1.0, 1.0), (2.0, 2.0)], vec![(5.0, 3.0)]]
        );
        assert!(runs(&[None, None]).is_empty());
    }

    #[test]
    fn it_opens_on_all_instances_for_the_last_week_by_cost() {
        let app = App::new(dataset());
        assert_eq!(app.range, Range::Week);
        assert_eq!(app.grouping, Grouping::Instance);
        let labels: Vec<_> = app.rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, ["All instances", "build", "api", "cache"]);
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn selection_stays_in_bounds() {
        let mut app = App::new(dataset());
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selected(), 0);
        press(&mut app, KeyCode::End);
        assert_eq!(app.selected(), 3);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected(), 3);
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn ranges_cycle_both_ways_and_keep_the_selection() {
        let mut app = App::new(dataset());
        press(&mut app, KeyCode::Down); // build
        press(&mut app, KeyCode::Right);
        assert_eq!(app.range, Range::Fortnight);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.range, Range::Day);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.range, Range::Fortnight);
        assert_eq!(app.rows[app.selected()].label, "build");
    }

    #[test]
    fn groupings_switch_by_tab_or_number() {
        let mut app = App::new(dataset());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.grouping, Grouping::Region);
        assert_eq!(app.selected(), 0);
        press(&mut app, KeyCode::BackTab);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.grouping, Grouping::Family);
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.grouping, Grouping::Instance);
        let labels: Vec<_> = app.rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels.len(), 4);
    }

    #[test]
    fn sorting_cycles_and_follows_the_selected_row() {
        let mut app = App::new(dataset());
        press(&mut app, KeyCode::Down); // build, most expensive
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.sort, Sort::Cpu);
        assert_eq!(app.rows[app.selected()].label, "build");
        let labels: Vec<_> = app
            .rows
            .iter()
            .skip(1)
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(labels, ["cache", "build", "api"]);
    }

    #[test]
    fn q_escape_and_ctrl_c_quit() {
        let mut app = App::new(dataset());
        assert_eq!(press(&mut app, KeyCode::Char('x')), Flow::Continue);
        assert_eq!(press(&mut app, KeyCode::Char('q')), Flow::Quit);
        assert_eq!(press(&mut app, KeyCode::Esc), Flow::Quit);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle(ctrl_c), Flow::Quit);
        assert_eq!(press(&mut app, KeyCode::Char('c')), Flow::Continue);
    }

    #[test]
    fn the_dashboard_shows_the_table_charts_and_help() {
        let mut app = App::new(dataset());
        let text = screen(&mut app, 140, 40);
        for expected in [
            "bracket",
            "EC2 usage",
            " 24h ",
            " 7d ",
            "1 Instance",
            "2 Region",
            "3 Family",
            "All instances",
            "3 instances in 2 regions",
            "build",
            "us-east-1 · c7i.8xlarge",
            "$2.52+",
            "CPU · All instances",
            "now $1.32/h",
            "≈ $964/mo",
            "-7d",
            "now",
            "sort (cost)",
            "Estimated: on-demand",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
    }

    #[test]
    fn unpriced_and_idle_rows_render_placeholders() {
        let mut app = App::new(dataset());
        press(&mut app, KeyCode::End); // cache: unpriced and stopped
        let text = screen(&mut app, 140, 40);
        assert!(text.contains("no price for this instance type"), "{text}");
        let cache_row = text.lines().find(|line| line.contains("▶ cache")).unwrap();
        assert!(cache_row.contains('—'), "{cache_row}");
    }

    #[test]
    fn a_small_terminal_does_not_panic() {
        let mut app = App::new(dataset());
        screen(&mut app, 30, 8);
        screen(&mut app, 1, 1);
    }
}
