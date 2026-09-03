//! Small shared building blocks: panels, inline meters, overlays.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::theme;

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border())
        .title(Span::styled(format!(" {} ", title.into()), theme::title()))
}

/// `label   value` on one line, label muted and value bold.
pub fn kv<'a>(label: &'a str, value: impl Into<String>, style: Style) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), theme::label()),
        Span::styled(value.into(), style),
    ])
}

/// A compact inline bar: `label ██████░░░░  42%`.
///
/// Cheaper and denser than a stack of `Gauge` widgets, which matters when we
/// draw one per core. `avail` is the total width the line may occupy; the bar
/// takes whatever the label and the trailing text leave over, so the value
/// never gets clipped off the right edge.
pub fn meter(label: &str, ratio: f64, text: &str, avail: usize, color: Color) -> Line<'static> {
    let ratio = ratio.clamp(0.0, 1.0);
    let label_width = label.chars().count().max(7);
    let width = avail
        .saturating_sub(label_width + text.chars().count() + 1)
        .clamp(3, 40);
    let filled = (ratio * width as f64).round() as usize;
    let filled = filled.min(width);
    Line::from(vec![
        Span::styled(format!("{label:<7}"), theme::label()),
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "░".repeat(width.saturating_sub(filled)),
            Style::default().fg(theme::BORDER),
        ),
        Span::styled(format!(" {text}"), theme::value()),
    ])
}

/// Braille-ish sparkline rendered as text so it can sit inside a paragraph.
pub fn spark(data: &[u64], width: usize) -> Line<'static> {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if data.is_empty() || width == 0 {
        return Line::from(Span::styled("no samples yet", theme::muted()));
    }
    let slice = &data[data.len().saturating_sub(width)..];
    let max = slice.iter().copied().max().unwrap_or(1).max(1);
    let spans: Vec<Span> = slice
        .iter()
        .map(|v| {
            let level = ((*v as f64 / max as f64) * (LEVELS.len() - 1) as f64).round() as usize;
            Span::styled(
                LEVELS[level.min(LEVELS.len() - 1)].to_string(),
                Style::default().fg(theme::ACCENT),
            )
        })
        .collect();
    Line::from(spans)
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    cell
}

pub fn help_overlay(frame: &mut Frame, area: Rect) {
    let entries: &[(&str, &str)] = &[
        ("1-7 / Tab", "switch view"),
        ("↑ ↓ / j k", "move selection (scroll, in Logs)"),
        ("a", "add a rig / wallet / endpoint / machine, per the view"),
        (
            "c",
            "mine another coin on the selected rig, at the same time",
        ),
        ("d", "remove the selected item from the config"),
        ("e", "enable or disable the selected rig"),
        ("s / x", "start / stop the selected rig"),
        ("S / X", "start all enabled rigs / stop all rigs"),
        ("+ / -", "add or remove a thread on the selected rig"),
        ("p", "run the selected endpoint check now"),
        ("t", "in Nodes: reconnect to the selected machine now"),
        ("r", "reload the config file"),
        ("f", "freeze the display (mining is unaffected)"),
        ("Q", "shut the daemon down and stop mining"),
        ("q / Esc / Ctrl-C", "close the dashboard, keep mining"),
    ];
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Closing this dashboard does not stop mining. The daemon keeps",
            theme::muted(),
        )),
        Line::from(Span::styled(
            "  running in the background; reopen with `cryptocli` any time.",
            theme::muted(),
        )),
        Line::from(""),
    ];
    for (key, description) in entries {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<18}"), theme::accent()),
            Span::styled(*description, theme::base()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press any key to close",
        theme::muted(),
    )));

    let height = lines.len() as u16 + 2;
    let target = centered(area, 78, height);
    frame.render_widget(Clear, target);
    frame.render_widget(Paragraph::new(lines).block(panel("Keys")), target);
}

pub fn confirm_overlay(frame: &mut Frame, area: Rect, prompt: &str) {
    let target = centered(area, 60, 7);
    frame.render_widget(Clear, target);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(format!("  {prompt}"), theme::base())),
        Line::from(""),
        Line::from(vec![
            Span::styled("  y", theme::accent()),
            Span::styled(" confirm    ", theme::muted()),
            Span::styled("any other key", theme::accent()),
            Span::styled(" cancel", theme::muted()),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::WARN))
                .title(Span::styled(" Confirm ", theme::title())),
        ),
        target,
    );
}
