//! The individual screens. Each one renders straight from the daemon snapshot.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Paragraph, Row, Sparkline, Table, TableState, Wrap};

use super::widgets::{kv, meter, panel, spark};
use super::{App, theme};
use crate::model::{
    EndpointStatus, HardwareSnapshot, LogLevel, RigStatus, Snapshot, fmt_bytes, fmt_count,
    fmt_difficulty, fmt_duration, fmt_hashrate,
};

// ------------------------------------------------------------- dashboard ---

pub fn dashboard(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let [main, log_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(9)]).areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(main);
    let [chart_area, table_area] =
        Layout::vertical([Constraint::Length(8), Constraint::Min(6)]).areas(left);
    let node_height = if snapshot.nodes.len() > 1 {
        (snapshot.nodes.len() as u16 + 2).min(8)
    } else {
        0
    };
    let [node_area, system_area, endpoint_area] = Layout::vertical([
        Constraint::Length(node_height),
        Constraint::Length(12),
        Constraint::Min(4),
    ])
    .areas(right);

    hashrate_panel(frame, &snapshot, chart_area);
    rig_table(frame, &snapshot, app.rig_selected, table_area, true);
    if node_height > 0 {
        node_summary(frame, &snapshot, node_area);
    }
    system_panel(frame, &snapshot, system_area);
    endpoint_summary(frame, &snapshot, endpoint_area);
    log_panel(frame, &snapshot, 0, log_area);
}

fn hashrate_panel(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let totals = &snapshot.totals;
    let block = panel("Total hashrate");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let headline_height = if snapshot.totals.coins.len() > 1 {
        3
    } else {
        2
    };
    let [headline, chart] =
        Layout::vertical([Constraint::Length(headline_height), Constraint::Min(1)]).areas(inner);

    let peak = totals.history.iter().copied().max().unwrap_or(0) as f64;
    let summary = Line::from(vec![
        Span::styled(
            fmt_hashrate(totals.hashrate),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   avg ", theme::label()),
        Span::styled(fmt_hashrate(totals.hashrate_avg), theme::value()),
        Span::styled("   peak ", theme::label()),
        Span::styled(fmt_hashrate(peak), theme::value()),
        Span::styled("   total ", theme::label()),
        Span::styled(
            format!("{} hashes", fmt_count(totals.hashes_total as f64)),
            theme::value(),
        ),
    ]);
    let shares = Line::from(vec![
        Span::styled("accepted ", theme::label()),
        Span::styled(totals.accepted.to_string(), theme::good()),
        Span::styled("   rejected ", theme::label()),
        Span::styled(
            totals.rejected.to_string(),
            if totals.rejected > 0 {
                theme::bad()
            } else {
                theme::muted()
            },
        ),
        Span::styled("   stale ", theme::label()),
        Span::styled(
            totals.stale.to_string(),
            if totals.stale > 0 {
                theme::warn()
            } else {
                theme::muted()
            },
        ),
    ]);
    let mut header_lines = vec![summary, shares];
    if totals.coins.len() > 1 {
        // Several coins in flight: show how the hashrate divides between them.
        let mut spans = vec![Span::styled("coins    ", theme::label())];
        for (i, coin) in totals.coins.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", theme::muted()));
            }
            spans.push(Span::styled(coin.coin.clone(), theme::accent()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(fmt_hashrate(coin.hashrate), theme::value()));
            spans.push(Span::styled(
                format!(" ({}x)", coin.sessions),
                theme::muted(),
            ));
        }
        header_lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(header_lines), headline);

    let data: Vec<u64> = totals
        .history
        .iter()
        .rev()
        .take(chart.width as usize)
        .rev()
        .copied()
        .collect();
    // Leave headroom above the peak so a steady hashrate reads as a line
    // rather than a solid block.
    let ceiling = ((peak * 1.25) as u64).max(1);
    frame.render_widget(
        Sparkline::default()
            .data(data)
            .max(ceiling)
            .style(Style::default().fg(theme::ACCENT)),
        chart,
    );
}

/// One line per machine, for the dashboard's right column.
fn node_summary(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let block = panel(format!(
        "Nodes  {}/{} online",
        snapshot.totals.nodes_online, snapshot.totals.nodes_total
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines: Vec<Line> = snapshot
        .nodes
        .iter()
        .map(|node| {
            let style = if node.online {
                theme::good()
            } else {
                theme::bad()
            };
            Line::from(vec![
                Span::styled(if node.online { "● " } else { "✗ " }, style),
                Span::styled(format!("{:<14}", truncate(&node.name, 14)), theme::base()),
                Span::styled(
                    format!("{:>11}", fmt_hashrate(node.hashrate)),
                    theme::value(),
                ),
                Span::styled(
                    format!("  {}/{}", node.rigs_active, node.rigs_total),
                    theme::muted(),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn system_panel(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let hardware = &snapshot.hardware;
    let block = panel("System");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;

    let mem_ratio = ratio(hardware.mem_used, hardware.mem_total);
    let hottest = hardware.temps.first();
    let mut lines = vec![
        meter(
            "cpu",
            hardware.cpu_usage as f64 / 100.0,
            &format!("{:.0}%", hardware.cpu_usage),
            width,
            theme::threshold(hardware.cpu_usage as f64, 75.0, 92.0),
        ),
        meter(
            "mem",
            mem_ratio,
            &format!(
                "{} / {}",
                fmt_bytes(hardware.mem_used),
                fmt_bytes(hardware.mem_total)
            ),
            width,
            theme::threshold(mem_ratio * 100.0, 75.0, 90.0),
        ),
    ];
    if let Some(temp) = hottest {
        lines.push(meter(
            "temp",
            (temp.celsius as f64 / 100.0).clamp(0.0, 1.0),
            &format!("{:.0}°C {}", temp.celsius, truncate(&temp.label, 12)),
            width,
            theme::threshold(temp.celsius as f64, 70.0, 85.0),
        ));
    }
    let threads_ratio = ratio(
        snapshot.totals.threads_active as u64,
        snapshot.totals.threads_budget.max(1) as u64,
    );
    lines.push(meter(
        "threads",
        threads_ratio,
        &format!(
            "{} / {}",
            snapshot.totals.threads_active, snapshot.totals.threads_budget
        ),
        width,
        theme::ACCENT,
    ));
    lines.push(Line::from(""));
    lines.push(kv("cpu", truncate(&hardware.cpu_brand, 34), theme::value()));
    lines.push(kv(
        "cores",
        format!(
            "{} physical / {} logical",
            hardware.cores_physical, hardware.cores_logical
        ),
        theme::value(),
    ));
    lines.push(kv(
        "load",
        format!(
            "{:.2}  {:.2}  {:.2}",
            hardware.load_avg[0], hardware.load_avg[1], hardware.load_avg[2]
        ),
        theme::value(),
    ));
    lines.push(kv(
        "backend",
        snapshot.totals.backend.clone(),
        theme::accent(),
    ));
    lines.push(kv(
        "work",
        format!(
            "{} units · {} space(s){}",
            fmt_count(snapshot.totals.work_units as f64),
            snapshot.totals.work_spaces,
            if snapshot.totals.shared_spaces > 0 {
                format!(", {} shared", snapshot.totals.shared_spaces)
            } else {
                String::new()
            }
        ),
        theme::value(),
    ));
    if !hardware.gpus.is_empty() {
        let gpu = &hardware.gpus[0];
        lines.push(kv(
            "gpu",
            format!(
                "{} {}",
                truncate(&gpu.name, 22),
                gpu.util_percent
                    .map(|u| format!("{u:.0}%"))
                    .unwrap_or_default()
            ),
            theme::value(),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn endpoint_summary(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let block = panel("Endpoints");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if snapshot.endpoints.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("nothing registered yet", theme::muted())),
                Line::from(""),
                Line::from(Span::styled(
                    "cryptocli endpoint add pool \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "  --url https://pool/api/me \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "  --field balance=data.balance",
                    theme::base(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    let mut lines = Vec::new();
    for endpoint in &snapshot.endpoints {
        lines.push(Line::from(vec![
            Span::styled(status_dot(endpoint.ok), status_style(endpoint.ok)),
            Span::styled(
                format!(" {:<14}", truncate(&endpoint.name, 14)),
                theme::base(),
            ),
            Span::styled(
                endpoint
                    .latency_ms
                    .map(|v| format!("{v:>5}ms"))
                    .unwrap_or_else(|| "     -".into()),
                theme::muted(),
            ),
            Span::styled(
                format!("  {:.0}%", endpoint.uptime_pct),
                if endpoint.uptime_pct >= 99.0 {
                    theme::muted()
                } else {
                    theme::warn()
                },
            ),
        ]));
        for (label, value) in endpoint.fields.iter().take(2) {
            lines.push(Line::from(vec![
                Span::styled(format!("   {label}: "), theme::label()),
                Span::styled(truncate(value, 22), theme::value()),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ rigs ---

pub fn rigs(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Min(10)]).areas(area);
    rig_table(frame, &snapshot, app.rig_selected, table_area, false);

    match snapshot.rigs.get(app.rig_selected) {
        Some(rig) => rig_detail(frame, rig, detail_area),
        None => frame.render_widget(panel("Rig"), detail_area),
    }
}

fn rig_table(frame: &mut Frame, snapshot: &Snapshot, selected: usize, area: Rect, compact: bool) {
    let block = panel(if compact {
        "Rigs"
    } else {
        "Rigs (multi-mining)"
    });
    if snapshot.rigs.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no rigs configured", theme::muted())),
                Line::from(""),
                Line::from(Span::styled(
                    "cryptocli wallet add main --coin BTC --address <addr>",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "cryptocli rig add btc --url stratum+tcp://pool:3333 --wallet main",
                    theme::base(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "then press S to start everything",
                    theme::muted(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    // The node column only earns its width once more than one machine reports.
    let multi = snapshot.nodes.len() > 1;
    let header = if compact {
        Row::new(vec![
            "NODE", "RIG", "COIN", "STATE", "HASHRATE", "THR", "A/R", "TREND",
        ])
    } else {
        Row::new(vec![
            "NODE", "RIG", "COIN", "STATE", "HASHRATE", "THR", "DIFF", "A/R/S", "BEST", "PING",
            "UP", "POOL",
        ])
    }
    .style(theme::label())
    .height(1);

    let rows: Vec<Row> = snapshot
        .rigs
        .iter()
        .map(|rig| {
            let state = Span::styled(
                rig.state.label(),
                Style::default().fg(theme::state_color(rig.state)),
            );
            let shares = Line::from(vec![
                Span::styled(rig.accepted.to_string(), theme::good()),
                Span::styled("/", theme::muted()),
                Span::styled(
                    rig.rejected.to_string(),
                    if rig.rejected > 0 {
                        theme::bad()
                    } else {
                        theme::muted()
                    },
                ),
            ]);
            let coin = Cell::from(Line::from(Span::styled(
                if rig.coin.is_empty() {
                    "-".to_string()
                } else {
                    rig.coin.clone()
                },
                theme::accent(),
            )));
            let node_cell = Cell::from(Line::from(Span::styled(
                truncate(&rig.node, 12),
                theme::muted(),
            )));
            if compact {
                Row::new(vec![
                    node_cell,
                    Cell::from(truncate(&rig.name, 18)),
                    coin,
                    Cell::from(Line::from(state)),
                    Cell::from(Line::from(Span::styled(
                        fmt_hashrate(rig.hashrate),
                        theme::value(),
                    ))),
                    Cell::from(rig.threads.to_string()),
                    Cell::from(shares),
                    Cell::from(spark(&rig.history, 18)),
                ])
            } else {
                Row::new(vec![
                    node_cell,
                    Cell::from(truncate(&rig.name, 18)),
                    coin,
                    Cell::from(Line::from(state)),
                    Cell::from(Line::from(Span::styled(
                        fmt_hashrate(rig.hashrate),
                        theme::value(),
                    ))),
                    Cell::from(rig.threads.to_string()),
                    Cell::from(fmt_difficulty(rig.difficulty)),
                    Cell::from(Line::from(vec![
                        Span::styled(rig.accepted.to_string(), theme::good()),
                        Span::styled("/", theme::muted()),
                        Span::styled(
                            rig.rejected.to_string(),
                            if rig.rejected > 0 {
                                theme::bad()
                            } else {
                                theme::muted()
                            },
                        ),
                        Span::styled("/", theme::muted()),
                        Span::styled(
                            rig.stale.to_string(),
                            if rig.stale > 0 {
                                theme::warn()
                            } else {
                                theme::muted()
                            },
                        ),
                    ])),
                    Cell::from(if rig.best_share > 0.0 {
                        fmt_count(rig.best_share)
                    } else {
                        "-".to_string()
                    }),
                    Cell::from(
                        rig.latency_ms
                            .map(|v| format!("{v}ms"))
                            .unwrap_or_else(|| "-".into()),
                    ),
                    Cell::from(if rig.uptime_secs > 0 {
                        fmt_duration(rig.uptime_secs)
                    } else {
                        "-".into()
                    }),
                    Cell::from(truncate(&rig.pool, 28)),
                ])
            }
        })
        .collect();

    // Collapse the node column to nothing when it is not needed.
    let node_width = if multi { 13 } else { 0 };
    let widths: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(node_width),
            Constraint::Length(18),
            Constraint::Length(5),
            Constraint::Length(11),
            Constraint::Length(12),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Min(10),
        ]
    } else {
        vec![
            Constraint::Length(node_width),
            Constraint::Length(18),
            Constraint::Length(5),
            Constraint::Length(11),
            Constraint::Length(12),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Length(12),
            Constraint::Min(16),
        ]
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(theme::selected())
        .highlight_symbol("");
    let mut state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn rig_detail(frame: &mut Frame, rig: &RigStatus, area: Rect) {
    let block = panel(format!("Rig · {}", rig.name));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("state      ", theme::label()),
            Span::styled(
                rig.state.label(),
                Style::default()
                    .fg(theme::state_color(rig.state))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if rig.enabled {
                    "  (enabled)"
                } else {
                    "  (disabled)"
                },
                theme::muted(),
            ),
        ]),
        kv(
            "coin",
            if rig.coin.is_empty() {
                "-".to_string()
            } else {
                rig.coin.clone()
            },
            theme::accent(),
        ),
        kv(
            "rig",
            if rig.group == rig.name {
                rig.group.clone()
            } else {
                format!("{} (one of several coins)", rig.group)
            },
            theme::value(),
        ),
        kv("pool", rig.pool.clone(), theme::value()),
        kv("worker", truncate(&rig.user, 40), theme::value()),
        kv("algo", rig.algo.clone(), theme::value()),
        kv("threads", rig.threads.to_string(), theme::value()),
        kv(
            "job",
            if rig.job_id.is_empty() {
                "-".into()
            } else {
                rig.job_id.clone()
            },
            theme::value(),
        ),
        kv("difficulty", fmt_difficulty(rig.difficulty), theme::value()),
        kv(
            "reconnects",
            rig.reconnects.to_string(),
            if rig.reconnects > 0 {
                theme::warn()
            } else {
                theme::value()
            },
        ),
    ];
    if let Some(error) = &rig.last_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("last error: {}", truncate(error, 60)),
            theme::bad(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), left);

    let [stats_area, chart_area, _] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Max(6),
        Constraint::Min(0),
    ])
    .areas(right);
    let stats = vec![
        Line::from(vec![
            Span::styled(fmt_hashrate(rig.hashrate), theme::accent()),
            Span::styled("   avg ", theme::label()),
            Span::styled(fmt_hashrate(rig.hashrate_avg), theme::value()),
        ]),
        kv(
            "shares",
            format!(
                "{} accepted · {} rejected · {} stale",
                rig.accepted, rig.rejected, rig.stale
            ),
            theme::value(),
        ),
        kv(
            "last share",
            rig.last_share_secs
                .map(|s| format!("{} ago", fmt_duration(s)))
                .unwrap_or_else(|| "none yet".into()),
            theme::value(),
        ),
        kv(
            "best share",
            if rig.best_share > 0.0 {
                format!("difficulty {}", fmt_count(rig.best_share))
            } else {
                "-".into()
            },
            theme::value(),
        ),
        kv("hashes", fmt_count(rig.hashes_total as f64), theme::value()),
    ];
    frame.render_widget(Paragraph::new(stats), stats_area);
    let data: Vec<u64> = rig
        .history
        .iter()
        .rev()
        .take(chart_area.width as usize)
        .rev()
        .copied()
        .collect();
    let ceiling = ((rig.history.iter().copied().max().unwrap_or(1) as f64 * 1.25) as u64).max(1);
    frame.render_widget(
        Sparkline::default()
            .data(data)
            .max(ceiling)
            .style(Style::default().fg(theme::state_color(rig.state))),
        chart_area,
    );
}

// -------------------------------------------------------------- hardware ---

pub fn hardware(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let hardware = &snapshot.hardware;
    let [summary_area, rest] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(6)]).areas(area);
    let [cores_area, side] =
        Layout::horizontal([Constraint::Min(30), Constraint::Length(46)]).areas(rest);

    // Summary strip.
    let block = panel("Host");
    let inner = block.inner(summary_area);
    frame.render_widget(block, summary_area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(hardware.cpu_brand.clone(), theme::value()),
                Span::styled(
                    format!(
                        "  ·  {} cores / {} threads  ·  {} MHz  ·  {}",
                        hardware.cores_physical,
                        hardware.cores_logical,
                        hardware.freq_mhz,
                        hardware.cpu_arch
                    ),
                    theme::muted(),
                ),
            ]),
            Line::from(vec![
                Span::styled(format!("{}  ·  ", hardware.os), theme::muted()),
                Span::styled("up ", theme::label()),
                Span::styled(fmt_duration(hardware.host_uptime_secs), theme::value()),
                Span::styled("  ·  miner ", theme::label()),
                Span::styled(
                    format!(
                        "{:.0}% cpu, {} rss",
                        hardware.proc_cpu,
                        fmt_bytes(hardware.proc_mem)
                    ),
                    theme::value(),
                ),
            ]),
        ]),
        inner,
    );

    core_grid(frame, hardware, cores_area);

    let [mem_area, temp_area, gpu_area] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(6),
        Constraint::Length(if hardware.gpus.is_empty() {
            3
        } else {
            3 + hardware.gpus.len() as u16
        }),
    ])
    .areas(side);

    // Memory.
    let block = panel("Memory");
    let inner = block.inner(mem_area);
    frame.render_widget(block, mem_area);
    let width = inner.width as usize;
    let mem_ratio = ratio(hardware.mem_used, hardware.mem_total);
    let swap_ratio = ratio(hardware.swap_used, hardware.swap_total);
    frame.render_widget(
        Paragraph::new(vec![
            meter(
                "ram",
                mem_ratio,
                &format!(
                    "{} / {}",
                    fmt_bytes(hardware.mem_used),
                    fmt_bytes(hardware.mem_total)
                ),
                width,
                theme::threshold(mem_ratio * 100.0, 75.0, 90.0),
            ),
            meter(
                "swap",
                swap_ratio,
                &format!(
                    "{} / {}",
                    fmt_bytes(hardware.swap_used),
                    fmt_bytes(hardware.swap_total)
                ),
                width,
                theme::threshold(swap_ratio * 100.0, 25.0, 60.0),
            ),
            Line::from(vec![
                Span::styled("load     ", theme::label()),
                Span::styled(
                    format!(
                        "{:.2}  {:.2}  {:.2}",
                        hardware.load_avg[0], hardware.load_avg[1], hardware.load_avg[2]
                    ),
                    theme::value(),
                ),
            ]),
        ]),
        inner,
    );

    // Thermals.
    let block = panel("Thermals");
    let inner = block.inner(temp_area);
    frame.render_widget(block, temp_area);
    if hardware.temps.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("no sensors visible", theme::muted())),
            inner,
        );
    } else {
        let width = inner.width as usize;
        let lines: Vec<Line> = hardware
            .temps
            .iter()
            .take(inner.height as usize)
            .map(|sensor| {
                meter(
                    &format!("{:<16}", truncate(&sensor.label, 16)),
                    (sensor.celsius as f64 / 100.0).clamp(0.0, 1.0),
                    &format!("{:.0}°C", sensor.celsius),
                    width,
                    theme::threshold(sensor.celsius as f64, 70.0, 85.0),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    // GPUs.
    let block = panel("GPU");
    let inner = block.inner(gpu_area);
    frame.render_widget(block, gpu_area);
    if hardware.gpus.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no NVIDIA GPU detected (CPU mining only)",
                theme::muted(),
            )),
            inner,
        );
    } else {
        let lines: Vec<Line> = hardware
            .gpus
            .iter()
            .map(|gpu| {
                Line::from(vec![
                    Span::styled(format!("{} ", gpu.index), theme::label()),
                    Span::styled(format!("{:<18}", truncate(&gpu.name, 18)), theme::value()),
                    Span::styled(
                        gpu.util_percent
                            .map(|u| format!("{u:>3.0}%"))
                            .unwrap_or_else(|| "  -".into()),
                        theme::base(),
                    ),
                    Span::styled(
                        gpu.temp_c
                            .map(|t| format!(" {t:>3.0}°C"))
                            .unwrap_or_default(),
                        Style::default().fg(theme::threshold(
                            gpu.temp_c.unwrap_or(0.0) as f64,
                            70.0,
                            85.0,
                        )),
                    ),
                    Span::styled(
                        gpu.power_w
                            .map(|p| format!(" {p:>3.0}W"))
                            .unwrap_or_default(),
                        theme::muted(),
                    ),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn core_grid(frame: &mut Frame, hardware: &HardwareSnapshot, area: Rect) {
    let block = panel("Cores");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if hardware.per_core.is_empty() || inner.height == 0 {
        return;
    }

    // Fit as many columns of meters as the panel is wide.
    let column_width = 26u16;
    let columns = (inner.width / column_width).max(1) as usize;
    let rows = inner.height as usize;
    let capacity = columns * rows;
    let cores = &hardware.per_core[..hardware.per_core.len().min(capacity)];
    let per_column = cores.len().div_ceil(columns);

    let constraints: Vec<Constraint> = (0..columns)
        .map(|_| Constraint::Length(column_width))
        .collect();
    let areas = Layout::horizontal(constraints).split(inner);

    for (column, target) in areas.iter().enumerate() {
        let start = column * per_column;
        if start >= cores.len() {
            break;
        }
        let end = (start + per_column).min(cores.len());
        let lines: Vec<Line> = cores[start..end]
            .iter()
            .enumerate()
            .map(|(offset, usage)| {
                meter(
                    &format!("c{:<2}", start + offset),
                    *usage as f64 / 100.0,
                    &format!("{usage:>3.0}%"),
                    // Leave a two-column gutter between core columns.
                    target.width.saturating_sub(2) as usize,
                    theme::threshold(*usage as f64, 75.0, 93.0),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), *target);
    }
}

// ------------------------------------------------------------- endpoints ---

pub fn endpoints(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Min(8)]).areas(area);

    let block = panel("Endpoints");
    if snapshot.endpoints.is_empty() {
        let inner = block.inner(table_area);
        frame.render_widget(block, table_area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no endpoints registered", theme::muted())),
                Line::from(""),
                Line::from(Span::styled(
                    "Any site with a status URL — or any stratum pool — can be watched here.",
                    theme::base(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "cryptocli endpoint add unmineable \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "  --url stratum+tcp://sha256.unmineable.com:3333 \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "  --user 'BTC:youraddress.worker' --password x",
                    theme::base(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "cryptocli endpoint add pool-stats \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "  --url https://pool.example.com/api/worker/rig1 \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "  --header 'Authorization: Bearer TOKEN' \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "  --interval 30 --field hashrate=data.hashrate",
                    theme::base(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        frame.render_widget(panel("Detail"), detail_area);
        return;
    }

    let rows: Vec<Row> = snapshot
        .endpoints
        .iter()
        .map(|endpoint| {
            Row::new(vec![
                Cell::from(Line::from(Span::styled(
                    status_dot(endpoint.ok),
                    status_style(endpoint.ok),
                ))),
                Cell::from(truncate(&endpoint.name, 18)),
                Cell::from(Line::from(Span::styled(
                    endpoint.kind.label(),
                    theme::accent(),
                ))),
                Cell::from(
                    endpoint
                        .http_status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::from(
                    endpoint
                        .latency_ms
                        .map(|v| format!("{v}ms"))
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::from(format!("{:.1}%", endpoint.uptime_pct)),
                Cell::from(
                    endpoint
                        .last_check_secs
                        .map(|s| format!("{}s ago", s))
                        .unwrap_or_else(|| "never".into()),
                ),
                Cell::from(
                    endpoint
                        .next_check_secs
                        .map(|s| format!("in {s}s"))
                        .unwrap_or_else(|| "off".into()),
                ),
                Cell::from(truncate(&endpoint.url, 40)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        vec![
            Constraint::Length(2),
            Constraint::Length(18),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec![
            "", "NAME", "KIND", "HTTP", "LATENCY", "UPTIME", "CHECKED", "NEXT", "URL",
        ])
        .style(theme::label()),
    )
    .block(block)
    .row_highlight_style(theme::selected());
    let mut state = TableState::default().with_selected(Some(app.endpoint_selected));
    frame.render_stateful_widget(table, table_area, &mut state);

    match snapshot.endpoints.get(app.endpoint_selected) {
        Some(endpoint) => endpoint_detail(frame, endpoint, detail_area),
        None => frame.render_widget(panel("Detail"), detail_area),
    }
}

fn endpoint_detail(frame: &mut Frame, endpoint: &EndpointStatus, area: Rect) {
    let block = panel(format!("Endpoint · {}", endpoint.name));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![
        kv("url", endpoint.url.clone(), theme::value()),
        kv(
            "kind",
            match endpoint.kind {
                crate::model::EndpointKind::Stratum => {
                    "stratum pool (connect, subscribe, authorize)"
                }
                crate::model::EndpointKind::Http => "http request",
            },
            theme::accent(),
        ),
        kv(
            "interval",
            format!("every {}s", endpoint.interval_secs),
            theme::value(),
        ),
        kv(
            "checks",
            format!(
                "{} total · {} failed · {:.2}% up",
                endpoint.checks, endpoint.failures, endpoint.uptime_pct
            ),
            theme::value(),
        ),
    ];
    if !endpoint.fields.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("extracted", theme::title())));
        for (label, value) in &endpoint.fields {
            lines.push(Line::from(vec![
                Span::styled(format!("  {label:<16}"), theme::label()),
                Span::styled(value.clone(), theme::value()),
            ]));
        }
    }
    if let Some(error) = &endpoint.last_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            theme::bad(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// --------------------------------------------------------------- wallets ---

pub fn wallets(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let block = panel("Wallets");
    if snapshot.wallets.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no wallets connected", theme::muted())),
                Line::from(""),
                Line::from(Span::styled(
                    "cryptocli wallet add main --coin BTC --address bc1q...",
                    theme::base(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Only payout addresses are stored — cryptocli never asks for,",
                    theme::muted(),
                )),
                Line::from(Span::styled(
                    "holds, or transmits a private key or seed phrase.",
                    theme::muted(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    let [table_area, detail_area] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Min(7)]).areas(area);

    let rows: Vec<Row> = snapshot
        .wallets
        .iter()
        .map(|wallet| {
            Row::new(vec![
                Cell::from(truncate(&wallet.name, 16)),
                Cell::from(wallet.coin.clone()),
                Cell::from(truncate(&wallet.address, 46)),
                Cell::from(if wallet.rigs.is_empty() {
                    "-".to_string()
                } else {
                    wallet.rigs.join(", ")
                }),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        vec![
            Constraint::Length(16),
            Constraint::Length(6),
            Constraint::Length(48),
            Constraint::Min(12),
        ],
    )
    .header(Row::new(vec!["NAME", "COIN", "ADDRESS", "RIGS"]).style(theme::label()))
    .block(block)
    .row_highlight_style(theme::selected());
    let mut state = TableState::default().with_selected(Some(app.wallet_selected));
    frame.render_stateful_widget(table, table_area, &mut state);

    let block = panel("Wallet");
    let inner = block.inner(detail_area);
    frame.render_widget(block, detail_area);
    if let Some(wallet) = snapshot.wallets.get(app.wallet_selected) {
        let earning: Vec<&RigStatus> = snapshot
            .rigs
            .iter()
            .filter(|rig| wallet.rigs.contains(&rig.name))
            .collect();
        let combined: f64 = earning.iter().map(|rig| rig.hashrate).sum();
        let accepted: u64 = earning.iter().map(|rig| rig.accepted).sum();
        let lines = vec![
            kv("name", wallet.name.clone(), theme::value()),
            kv("coin", wallet.coin.clone(), theme::value()),
            kv("address", wallet.address.clone(), theme::value()),
            kv(
                "label",
                wallet.label.clone().unwrap_or_else(|| "-".into()),
                theme::value(),
            ),
            kv(
                "mining",
                format!(
                    "{} rig(s) · {} · {} shares accepted",
                    earning.len(),
                    fmt_hashrate(combined),
                    accepted
                ),
                theme::value(),
            ),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

// ------------------------------------------------------------------ logs ---

pub fn logs(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    log_panel(frame, &snapshot, app.log_scroll, area);
}

fn log_panel(frame: &mut Frame, snapshot: &Snapshot, scroll: usize, area: Rect) {
    let title = if scroll > 0 {
        format!("Log  (scrolled back {scroll} lines · End for latest)")
    } else {
        "Log".to_string()
    };
    let block = panel(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    let total = snapshot.logs.len();
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = snapshot.logs[start..end]
        .iter()
        .map(|entry| {
            let time = chrono::DateTime::from_timestamp(entry.ts, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();
            let level_style = match entry.level {
                LogLevel::Error => theme::bad(),
                LogLevel::Warn => theme::warn(),
                LogLevel::Share => theme::good(),
                LogLevel::Info => theme::base(),
                LogLevel::Debug => theme::muted(),
            };
            Line::from(vec![
                Span::styled(format!("{time} "), theme::muted()),
                Span::styled(format!("{:<4}", entry.level.label()), level_style),
                Span::styled(
                    format!("{:<12} ", truncate(&entry.source, 12)),
                    theme::label(),
                ),
                Span::styled(entry.message.clone(), level_style),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Left),
        inner,
    );
}

// ---------------------------------------------------------------- shared ---

fn ratio(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// Distinct glyphs, not just distinct colours: state has to survive a
/// monochrome terminal and colour-blind readers.
fn status_dot(ok: Option<bool>) -> &'static str {
    match ok {
        Some(true) => "●",
        Some(false) => "✗",
        None => "○",
    }
}

fn status_style(ok: Option<bool>) -> Style {
    match ok {
        Some(true) => theme::good(),
        Some(false) => theme::bad(),
        None => theme::muted(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

// ----------------------------------------------------------------- nodes ---

/// Every machine in the dashboard: this one plus configured peers.
pub fn nodes(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Min(8)]).areas(area);

    let block = panel("Nodes");
    if snapshot.nodes.len() <= 1 {
        let inner = block.inner(table_area);
        frame.render_widget(block, table_area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("this machine only", theme::muted())),
                Line::from(""),
                Line::from(Span::styled(
                    "Add another machine to see and control all of them here.",
                    theme::base(),
                )),
                Line::from(""),
                Line::from(Span::styled("On the other machine:", theme::label())),
                Line::from(Span::styled("  cryptocli remote enable", theme::base())),
                Line::from(""),
                Line::from(Span::styled("Then here, or press `a`:", theme::label())),
                Line::from(Span::styled(
                    "  cryptocli node add rig2 --address HOST:9944 \\",
                    theme::base(),
                )),
                Line::from(Span::styled(
                    "    --token TOKEN --fingerprint sha256:...",
                    theme::base(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Connections are TLS encrypted with a pinned certificate.",
                    theme::muted(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        frame.render_widget(panel("Node"), detail_area);
        return;
    }

    let rows: Vec<Row> = snapshot
        .nodes
        .iter()
        .map(|node| {
            let state = if node.online { "online" } else { "offline" };
            let style = if node.online {
                theme::good()
            } else {
                theme::bad()
            };
            Row::new(vec![
                Cell::from(Line::from(Span::styled(
                    if node.online { "●" } else { "✗" },
                    style,
                ))),
                Cell::from(Line::from(vec![
                    Span::styled(truncate(&node.name, 16), theme::value()),
                    Span::styled(if node.local { " (this)" } else { "" }, theme::muted()),
                ])),
                Cell::from(Line::from(Span::styled(state, style))),
                Cell::from(Line::from(Span::styled(
                    fmt_hashrate(node.hashrate),
                    theme::value(),
                ))),
                Cell::from(format!("{}/{}", node.rigs_active, node.rigs_total)),
                Cell::from(node.threads.to_string()),
                Cell::from(format!("{:.0}%", node.cpu_usage)),
                Cell::from(
                    node.hottest_c
                        .map(|t| format!("{t:.0}°C"))
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::from(
                    node.latency_ms
                        .map(|v| format!("{v}ms"))
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::from(truncate(&node.address, 28)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        vec![
            Constraint::Length(2),
            Constraint::Length(22),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Min(16),
        ],
    )
    .header(
        Row::new(vec![
            "", "NODE", "STATE", "HASHRATE", "RIGS", "THR", "CPU", "TEMP", "PING", "ADDRESS",
        ])
        .style(theme::label()),
    )
    .block(block)
    .row_highlight_style(theme::selected());
    let mut state = TableState::default().with_selected(Some(app.node_selected));
    frame.render_stateful_widget(table, table_area, &mut state);

    let Some(node) = snapshot.nodes.get(app.node_selected) else {
        frame.render_widget(panel("Node"), detail_area);
        return;
    };
    let block = panel(format!("Node · {}", node.name));
    let inner = block.inner(detail_area);
    frame.render_widget(block, detail_area);

    let node_rigs: Vec<&RigStatus> = snapshot
        .rigs
        .iter()
        .filter(|rig| rig.node == node.name)
        .collect();
    let mut lines = vec![
        kv("address", node.address.clone(), theme::value()),
        kv(
            "state",
            if node.online {
                format!(
                    "online · cryptocli {} · up {}",
                    node.version,
                    fmt_duration(node.uptime_secs)
                )
            } else {
                "offline".to_string()
            },
            if node.online {
                theme::good()
            } else {
                theme::bad()
            },
        ),
        kv(
            "mining",
            format!(
                "{} on {} thread(s) across {} rig(s)",
                fmt_hashrate(node.hashrate),
                node.threads,
                node_rigs.len()
            ),
            theme::value(),
        ),
        kv(
            "shares",
            format!("{} accepted · {} rejected", node.accepted, node.rejected),
            theme::value(),
        ),
        kv(
            "load",
            format!(
                "{:.0}% cpu{}",
                node.cpu_usage,
                node.hottest_c
                    .map(|t| format!(" · {t:.0}°C"))
                    .unwrap_or_default()
            ),
            theme::value(),
        ),
    ];
    if !node_rigs.is_empty() {
        lines.push(Line::from(""));
        for rig in node_rigs {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<20}", truncate(&rig.name, 20)), theme::base()),
                Span::styled(format!("{:<6}", rig.coin), theme::accent()),
                Span::styled(
                    format!("{:<10}", rig.state.label()),
                    Style::default().fg(theme::state_color(rig.state)),
                ),
                Span::styled(fmt_hashrate(rig.hashrate), theme::value()),
            ]));
        }
    }
    if let Some(error) = &node.last_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {}", truncate(error, 80)),
            theme::bad(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
