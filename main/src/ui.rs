//! Terminal presentation shared by every command: the intro banner, spinner-wrapped
//! steps, and a borderless table that lines up with Clack's `│` gutter.
//!
//! Clack chrome (intro, spinners, prompts, outro) goes to stderr. Tables go to stdout, so
//! `bracket list ec2 | grep running` sees plain rows with no gutter or colour.

use std::{fmt::Display, future::Future, io, time::Duration};

use console::{Style, measure_text_width, style};

use crate::error::{Error, Resource, Result};

/// Prints the `┌  bracket <title>` banner that opens every command.
pub fn intro(title: &str) -> io::Result<()> {
    cliclack::intro(format!(
        "{} {}",
        style(" bracket ").black().on_cyan().for_stderr(),
        style(title).bold().for_stderr(),
    ))
}

/// Runs `work` under a spinner labelled `start`.
///
/// On success the spinner collapses to `done(&value)`. On failure it stays on screen marked as
/// the step that failed, and the error is returned for `main` to report. Never prompt inside
/// `work`: the spinner owns the line until it finishes.
pub async fn step<T>(
    start: impl Display,
    work: impl Future<Output = Result<T>>,
    done: impl FnOnce(&T) -> String,
) -> Result<T> {
    let start = start.to_string();
    let spinner = cliclack::spinner();
    spinner.start(&start);
    let result = work.await;
    match &result {
        Ok(value) => spinner.stop(done(value)),
        Err(_) => spinner.error(&start),
    }
    result
}

/// Picks the item whose `key` is `name`, or asks the user to choose when `name` is `None`.
pub fn choose<T>(
    mut items: Vec<T>,
    name: Option<&str>,
    kind: Resource,
    prompt: &str,
    key: impl Fn(&T) -> &str,
    hint: impl Fn(&T) -> String,
) -> Result<T> {
    let index = match name {
        Some(name) => items
            .iter()
            .position(|item| key(item) == name)
            .ok_or_else(|| Error::NotFound {
                kind,
                name: name.to_string(),
            })?,
        None if items.is_empty() => return Err(Error::Empty(kind)),
        None => items
            .iter()
            .enumerate()
            .fold(cliclack::select(prompt), |select, (index, item)| {
                select.item(index, key(item), hint(item))
            })
            .interact()?,
    };
    Ok(items.swap_remove(index))
}

/// Formats a wait as `42s` or `3m 05s`.
pub fn elapsed(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        _ => format!("{}m {:02}s", secs / 60, secs % 60),
    }
}

/// One table cell. Column widths come from `text` alone, so styling and hyperlinks
/// never push the columns out of line.
#[derive(Debug, Clone)]
pub struct Cell {
    pub(crate) text: String,
    style: Style,
    pub(crate) link: Option<String>,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Self {
        Cell {
            text: text.into(),
            style: Style::new(),
            link: None,
        }
    }

    /// A dimmed dash for a value that doesn't exist, like the IP of a stopped instance.
    pub fn missing() -> Self {
        Cell::new("—").style(Style::new().dim())
    }

    pub fn style(self, style: Style) -> Self {
        Cell { style, ..self }
    }

    /// Makes the cell a clickable link in terminals that support OSC 8 hyperlinks.
    pub fn link(self, url: String) -> Self {
        Cell {
            link: Some(url),
            ..self
        }
    }
}

/// Columns never shrink below this when fitting a table to the terminal.
const MIN_COLUMN: usize = 8;

/// Prints a table to stdout. `N` fixes the column count, so a row can't have too few cells.
pub fn print_table<const N: usize>(headers: [&str; N], rows: &[[Cell; N]]) {
    let stdout = console::Term::stdout();
    let columns = stdout.is_term().then(|| usize::from(stdout.size().1));
    print!("{}", render_table(headers, rows, columns));
}

/// Renders a table. On a terminal `columns` wide, rows get Clack's gutter and their
/// hyperlinks, and the widest columns are cut short with `…` so each row fits on one line.
/// Piped output (`columns` is `None`) is never truncated.
fn render_table<const N: usize>(
    headers: [&str; N],
    rows: &[[Cell; N]],
    columns: Option<usize>,
) -> String {
    let natural: [usize; N] = std::array::from_fn(|col| {
        rows.iter()
            .map(|row| measure_text_width(&row[col].text))
            .chain([headers[col].len()])
            .max()
            .unwrap_or_default()
    });
    const GUTTER: usize = 3;
    let widths = match columns {
        Some(columns) => fit(natural, columns.saturating_sub(GUTTER + 2 * (N - 1))),
        None => natural,
    };
    let gutter = match columns {
        Some(_) => format!("{}  ", Style::new().bright().black().apply_to("│")),
        None => String::new(),
    };
    let header = headers.map(|title| Cell::new(title).style(Style::new().dim()));

    std::iter::once(&header)
        .chain(rows)
        .map(|row| {
            let cells = row
                .iter()
                .zip(widths)
                .enumerate()
                .map(|(col, (cell, width))| {
                    let visible = console::truncate_str(&cell.text, width, "…");
                    let styled = cell.style.apply_to(&visible).to_string();
                    let text = match (&cell.link, columns) {
                        (Some(url), Some(_)) => {
                            format!("\x1b]8;;{url}\x1b\\{styled}\x1b]8;;\x1b\\")
                        }
                        _ => styled,
                    };
                    // The last column isn't padded, so lines carry no trailing whitespace.
                    let pad = match col + 1 == N {
                        true => 0,
                        false => width.saturating_sub(measure_text_width(&visible)),
                    };
                    format!("{text}{}", " ".repeat(pad))
                });
            format!("{gutter}{}\n", cells.collect::<Vec<_>>().join("  "))
        })
        .collect()
}

/// Narrows the widest column one character at a time until the columns fit in `budget`,
/// stopping at [`MIN_COLUMN`]. A table that still doesn't fit is left to wrap.
fn fit<const N: usize>(mut widths: [usize; N], budget: usize) -> [usize; N] {
    while widths.iter().sum::<usize>() > budget {
        let Some((widest, &width)) = widths.iter().enumerate().max_by_key(|(_, width)| **width)
        else {
            break;
        };
        if width <= MIN_COLUMN {
            break;
        }
        widths[widest] -= 1;
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_switches_to_minutes_at_sixty_seconds() {
        assert_eq!(elapsed(Duration::from_millis(900)), "0s");
        assert_eq!(elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(elapsed(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn choose_finds_items_by_key() {
        let items = vec!["api", "worker"];
        let picked = choose(
            items,
            Some("worker"),
            Resource::AppRunnerService,
            "?",
            |s| s,
            |_| String::new(),
        );
        assert_eq!(picked.unwrap(), "worker");
    }

    #[test]
    fn choose_reports_unknown_names_and_empty_lists() {
        let err = choose(
            vec!["api"],
            Some("web"),
            Resource::NeptuneCluster,
            "?",
            |s| *s,
            |_| String::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::NotFound { kind: Resource::NeptuneCluster, ref name } if name == "web")
        );

        let err = choose(
            Vec::<&str>::new(),
            None,
            Resource::NeptuneCluster,
            "?",
            |s| *s,
            |_| String::new(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Empty(Resource::NeptuneCluster)));
    }

    #[test]
    fn columns_align_on_the_widest_cell() {
        let rows = [
            [Cell::new("dev-box"), Cell::new("● running")],
            [Cell::new("a"), Cell::missing()],
        ];
        assert_eq!(
            render_table(["NAME", "STATE"], &rows, None),
            "NAME     STATE\n\
             dev-box  ● running\n\
             a        —\n"
        );
    }

    #[test]
    fn headers_set_the_minimum_width() {
        let rows = [[Cell::new("x"), Cell::new("y")]];
        assert_eq!(
            render_table(["INSTANCE ID", "IP"], &rows, None),
            "INSTANCE ID  IP\nx            y\n"
        );
    }

    #[test]
    fn empty_tables_render_just_the_header() {
        let rows: [[Cell; 2]; 0] = [];
        assert_eq!(render_table(["A", "B"], &rows, None), "A  B\n");
    }

    #[test]
    fn links_and_gutter_only_appear_on_a_terminal() {
        let rows = [[Cell::new("dev").link("https://example.com".into())]];

        let piped = render_table(["NAME"], &rows, None);
        assert!(!piped.contains("\x1b]8;;"));
        assert!(!piped.contains('│'));

        let tty = render_table(["NAME"], &rows, Some(80));
        assert!(tty.contains("\x1b]8;;https://example.com\x1b\\dev\x1b]8;;\x1b\\"));
        assert!(tty.lines().all(|line| line.contains('│')));
    }

    #[test]
    fn hyperlinks_do_not_change_column_widths() {
        let rows = [
            [
                Cell::new("ab").link("https://a.example".into()),
                Cell::new("x"),
            ],
            [Cell::new("abcd"), Cell::new("y")],
        ];
        let rendered = render_table(["N", "V"], &rows, Some(80));
        let visible: Vec<usize> = rendered
            .lines()
            .map(|line| {
                let plain =
                    console::strip_ansi_codes(line).replace("\x1b]8;;https://a.example\x1b\\", "");
                plain
                    .replace("\x1b]8;;\x1b\\", "")
                    .find(['x', 'y', 'V'])
                    .unwrap()
            })
            .collect();
        assert!(
            visible.windows(2).all(|pair| pair[0] == pair[1]),
            "{rendered:?}"
        );
    }

    #[test]
    fn fit_narrows_the_widest_column_first() {
        assert_eq!(fit([40, 10, 20], 70), [40, 10, 20]);
        assert_eq!(fit([40, 10, 20], 60), [30, 10, 20]);
        assert_eq!(fit([40, 10, 38], 60), [25, 10, 25]);
        assert_eq!(fit([40, 10], 5), [MIN_COLUMN, MIN_COLUMN]);
        assert_eq!(fit([3, 4], 0), [3, 4]);
    }

    #[test]
    fn long_cells_are_truncated_to_fit_the_terminal() {
        let rows = [[
            Cell::new("an-instance-with-a-very-long-name-indeed"),
            Cell::new("● running"),
        ]];
        let rendered = render_table(["NAME", "STATE"], &rows, Some(40));
        let line = console::strip_ansi_codes(rendered.lines().nth(1).unwrap()).to_string();
        assert_eq!(measure_text_width(&line), 40);
        assert!(line.contains("an-instance-with-a-very-l…  ● running"));

        let piped = render_table(["NAME", "STATE"], &rows, None);
        assert!(piped.contains("an-instance-with-a-very-long-name-indeed"));
    }
}
