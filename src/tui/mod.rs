//! The dashboard.
//!
//! The TUI is a thin client: it holds no mining state of its own, it polls the
//! daemon for a snapshot and renders it. Quitting — `q`, `Esc` or Ctrl-C — only
//! ends this process; the daemon and every rig carry on. That is the whole
//! reason for the split.

mod form;
mod theme;
mod views;
mod widgets;

use anyhow::Result;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::{DefaultTerminal, Frame};

use crate::ipc::{Client, Request};
use crate::model::{Snapshot, fmt_duration, fmt_hashrate};
use form::Form;

/// How often to ask the daemon for a fresh snapshot.
const REFRESH: Duration = Duration::from_millis(500);
/// Input poll timeout; also bounds the animation tick.
const POLL: Duration = Duration::from_millis(100);
const TOAST_TTL: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Rigs,
    Nodes,
    Hardware,
    Endpoints,
    Wallets,
    Logs,
}

impl Tab {
    const ALL: [Tab; 7] = [
        Tab::Dashboard,
        Tab::Rigs,
        Tab::Nodes,
        Tab::Hardware,
        Tab::Endpoints,
        Tab::Wallets,
        Tab::Logs,
    ];

    fn title(&self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Rigs => "Rigs",
            Tab::Nodes => "Nodes",
            Tab::Hardware => "Hardware",
            Tab::Endpoints => "Endpoints",
            Tab::Wallets => "Wallets",
            Tab::Logs => "Logs",
        }
    }

    fn index(&self) -> usize {
        Tab::ALL.iter().position(|t| t == self).unwrap_or(0)
    }
}

struct Confirm {
    prompt: String,
    request: Request,
}

/// Wrap a request so the hub forwards it to the machine that owns the rig.
/// The local node is left unwrapped, keeping single-machine traffic identical.
fn route(node: String, request: Request) -> Request {
    if node.is_empty() {
        request
    } else {
        Request::OnNode {
            node,
            request: Box::new(request),
        }
    }
}

pub struct App {
    client: Option<Client>,
    snapshot: Option<Snapshot>,
    connection_error: Option<String>,
    tab: Tab,
    pub rig_selected: usize,
    pub endpoint_selected: usize,
    pub wallet_selected: usize,
    pub node_selected: usize,
    /// Lines scrolled up from the bottom of the log.
    pub log_scroll: usize,
    toast: Option<(String, bool, Instant)>,
    confirm: Option<Confirm>,
    /// Open add-form, if any. Swallows all input while present.
    form: Option<Form>,
    help: bool,
    frozen: bool,
    tick: usize,
    last_refresh: Instant,
    quit: bool,
    /// True when this process started the daemon just to show the dashboard.
    we_started_daemon: bool,
}

impl App {
    fn new() -> Self {
        Self {
            client: None,
            snapshot: None,
            connection_error: None,
            tab: Tab::Dashboard,
            rig_selected: 0,
            endpoint_selected: 0,
            wallet_selected: 0,
            node_selected: 0,
            log_scroll: 0,
            toast: None,
            confirm: None,
            form: None,
            help: false,
            frozen: false,
            tick: 0,
            last_refresh: Instant::now() - REFRESH,
            quit: false,
            we_started_daemon: false,
        }
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        if self.client.is_none() {
            match Client::connect() {
                Ok(client) => {
                    self.client = Some(client);
                    self.connection_error = None;
                }
                Err(err) => {
                    self.connection_error = Some(format!("{err:#}"));
                    return;
                }
            }
        }
        let Some(client) = self.client.as_mut() else {
            return;
        };
        match client.snapshot() {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.connection_error = None;
                self.clamp_selection();
            }
            Err(err) => {
                self.connection_error = Some(format!("{err:#}"));
                self.client = None;
            }
        }
    }

    fn clamp_selection(&mut self) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let clamp = |index: &mut usize, len: usize| {
            if len == 0 {
                *index = 0;
            } else if *index >= len {
                *index = len - 1;
            }
        };
        clamp(&mut self.rig_selected, snapshot.rigs.len());
        clamp(&mut self.endpoint_selected, snapshot.endpoints.len());
        clamp(&mut self.wallet_selected, snapshot.wallets.len());
        clamp(&mut self.node_selected, snapshot.nodes.len());
    }

    /// Run one request against the daemon and hand back what it said.
    fn request(&mut self, request: Request) -> Result<String, String> {
        if self.client.is_none() {
            match Client::connect() {
                Ok(client) => self.client = Some(client),
                Err(err) => return Err(format!("{err:#}")),
            }
        }
        let Some(client) = self.client.as_mut() else {
            return Err("no connection to the daemon".into());
        };
        let result = client.command(&request).map_err(|err| format!("{err:#}"));
        if result.is_err() {
            // A failed request may have left an unread reply in the pipe,
            // which would be mistaken for the answer to the next command.
            // Reconnecting is cheap; a silently wrong answer is not.
            self.client = None;
        }
        // Reflect the change immediately rather than waiting for the next tick.
        self.last_refresh = Instant::now() - REFRESH;
        result
    }

    fn send(&mut self, request: Request) {
        match self.request(request) {
            Ok(message) => self.toast(message, false),
            Err(err) => self.toast(err, true),
        }
    }

    /// On quit, stop a daemon we started if it never ended up mining. A daemon
    /// that was already running, or one that is mining, is always left alone.
    fn tidy_up(&mut self) {
        if !self.we_started_daemon {
            return;
        }
        let idle = self
            .snapshot
            .as_ref()
            .map(|s| s.totals.rigs_active == 0)
            .unwrap_or(false);
        if idle && let Some(client) = self.client.as_mut() {
            let _ = client.command(&Request::Shutdown);
        }
    }

    fn toast(&mut self, message: String, is_error: bool) {
        self.toast = Some((message, is_error, Instant::now()));
    }

    fn selected_rig(&self) -> Option<&crate::model::RigStatus> {
        self.snapshot
            .as_ref()
            .and_then(|s| s.rigs.get(self.rig_selected))
    }

    /// The selected rig as `(node, rig)`, so actions reach the right machine.
    fn selected_rig_ref(&self) -> Option<(String, String)> {
        self.selected_rig()
            .map(|rig| (rig.node.clone(), rig.name.clone()))
    }

    fn selected_endpoint(&self) -> Option<&crate::model::EndpointStatus> {
        self.snapshot
            .as_ref()
            .and_then(|s| s.endpoints.get(self.endpoint_selected))
    }

    fn list_len(&self) -> usize {
        let Some(snapshot) = &self.snapshot else {
            return 0;
        };
        match self.tab {
            Tab::Rigs | Tab::Dashboard => snapshot.rigs.len(),
            Tab::Endpoints => snapshot.endpoints.len(),
            Tab::Wallets => snapshot.wallets.len(),
            Tab::Logs => snapshot.logs.len(),
            Tab::Nodes => snapshot.nodes.len(),
            Tab::Hardware => 0,
        }
    }

    fn cursor(&mut self) -> &mut usize {
        match self.tab {
            Tab::Endpoints => &mut self.endpoint_selected,
            Tab::Wallets => &mut self.wallet_selected,
            Tab::Nodes => &mut self.node_selected,
            _ => &mut self.rig_selected,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.tab == Tab::Logs {
            let len = self.list_len();
            // Scrolling up in the log means moving back through history.
            let scroll = self.log_scroll as isize - delta;
            self.log_scroll = scroll.clamp(0, len.saturating_sub(1) as isize) as usize;
            return;
        }
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let cursor = self.cursor();
        let next = *cursor as isize + delta;
        *cursor = next.clamp(0, len as isize - 1) as usize;
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // An open form takes all input, so typing a pool url can contain any
        // character without triggering a shortcut.
        if self.form.is_some() {
            self.form_key(key);
            return;
        }
        // A pending confirmation swallows everything else.
        if let Some(confirm) = &self.confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let request = confirm.request.clone();
                    self.confirm = None;
                    self.send(request);
                }
                _ => {
                    self.confirm = None;
                    self.toast("cancelled".into(), false);
                }
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Leaving the dashboard never stops mining.
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.help = true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()];
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()];
            }
            KeyCode::Char(c @ '1'..='7') => {
                self.tab = Tab::ALL[c as usize - '1' as usize];
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => {
                if self.tab == Tab::Logs {
                    self.log_scroll = self.list_len().saturating_sub(1);
                } else {
                    *self.cursor() = 0;
                }
            }
            KeyCode::End => {
                if self.tab == Tab::Logs {
                    self.log_scroll = 0;
                } else {
                    let len = self.list_len().saturating_sub(1);
                    *self.cursor() = len;
                }
            }
            KeyCode::Char('s') => {
                if let Some((node, name)) = self.selected_rig_ref() {
                    self.send(route(node, Request::StartRig { name }));
                }
            }
            KeyCode::Char('x') => {
                if let Some((node, name)) = self.selected_rig_ref() {
                    self.send(route(node, Request::StopRig { name }));
                }
            }
            KeyCode::Char('S') => self.send(Request::StartAll),
            KeyCode::Char('X') => {
                self.confirm = Some(Confirm {
                    prompt: "Stop every running rig?".into(),
                    request: Request::StopAll,
                });
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_threads(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_threads(-1),
            KeyCode::Char('p') => {
                if let Some(endpoint) = self.selected_endpoint() {
                    let (node, name) = (endpoint.node.clone(), endpoint.name.clone());
                    self.send(route(node, Request::CheckEndpoint { name }));
                }
            }
            // Reconnect to a machine now rather than waiting out its backoff.
            KeyCode::Char('t') if self.tab == Tab::Nodes => {
                let selected = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.nodes.get(self.node_selected))
                    .filter(|node| !node.local)
                    .map(|node| node.name.clone());
                match selected {
                    // The daemon dials the peer while this blocks, so the
                    // result lands in one toast rather than a "connecting..."
                    // that could never be drawn before the answer arrives.
                    Some(name) => self.send(Request::CheckNode { name }),
                    None => self.toast("select another machine to test".into(), true),
                }
            }
            KeyCode::Char('a') => self.open_add_form(),
            KeyCode::Char('c') => {
                // Add another coin to the selected rig, mined at the same time.
                if matches!(self.tab, Tab::Rigs | Tab::Dashboard)
                    && let Some(rig) = self.selected_rig()
                {
                    let group = rig.group.clone();
                    self.form = Some(Form::rig_coin(&group));
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => self.delete_selected(),
            KeyCode::Char('e') => {
                if let Some(rig) = self.selected_rig() {
                    let (node, name, enabled) = (rig.node.clone(), rig.group.clone(), !rig.enabled);
                    self.send(route(node, Request::SetRigEnabled { name, enabled }));
                }
            }
            KeyCode::Char('r') => self.send(Request::Reload),
            KeyCode::Char('f') => {
                self.frozen = !self.frozen;
                let message = if self.frozen {
                    "display frozen (mining continues)"
                } else {
                    "display live"
                };
                self.toast(message.into(), false);
            }
            KeyCode::Char('Q') => {
                self.confirm = Some(Confirm {
                    prompt: "Shut the daemon down and stop all mining?".into(),
                    request: Request::Shutdown,
                });
            }
            _ => {}
        }
    }

    fn form_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.form = None;
                self.toast("cancelled".into(), false);
            }
            KeyCode::Char('u') if ctrl => form.clear_field(),
            KeyCode::Char('c') if ctrl => {
                self.form = None;
            }
            KeyCode::Tab | KeyCode::Down => form.next(),
            KeyCode::BackTab | KeyCode::Up => form.previous(),
            KeyCode::Left => form.move_left(),
            KeyCode::Right => form.move_right(),
            KeyCode::Home => form.move_to_start(),
            KeyCode::End => form.move_to_end(),
            KeyCode::Char('a') if ctrl => form.move_to_start(),
            KeyCode::Char('e') if ctrl => form.move_to_end(),
            KeyCode::Backspace => form.backspace(),
            KeyCode::Delete => form.delete(),
            KeyCode::Char(c) => form.insert(c),
            KeyCode::Enter => match form.build() {
                Ok(request) => match self.request(request) {
                    Ok(message) => {
                        self.form = None;
                        self.toast(message, false);
                    }
                    // Keep the form up when the daemon says no. Its footer
                    // wraps, so a long "cannot connect to ..." is readable in
                    // full, and nothing typed has to be typed again.
                    Err(message) => {
                        if let Some(form) = self.form.as_mut() {
                            form.error = Some(message);
                        }
                    }
                },
                Err(message) => form.error = Some(message),
            },
            _ => {}
        }
    }

    /// `a` adds whatever the current view is about.
    fn open_add_form(&mut self) {
        self.form = Some(match self.tab {
            Tab::Wallets => Form::wallet(),
            Tab::Endpoints => Form::endpoint(),
            Tab::Nodes => Form::node(),
            _ => Form::rig(),
        });
    }

    /// Delete the selected item, after confirming.
    fn delete_selected(&mut self) {
        let (prompt, request) = match self.tab {
            Tab::Wallets => {
                let Some(wallet) = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.wallets.get(self.wallet_selected))
                else {
                    return;
                };
                (
                    format!("Remove wallet `{}` from the config?", wallet.name),
                    Request::RemoveWallet {
                        name: wallet.name.clone(),
                    },
                )
            }
            Tab::Nodes => {
                let Some(node) = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.nodes.get(self.node_selected))
                else {
                    return;
                };
                if node.local {
                    self.toast("that is this machine".into(), true);
                    return;
                }
                (
                    format!("Remove node `{}` from this dashboard?", node.name),
                    Request::RemoveNode {
                        name: node.name.clone(),
                    },
                )
            }
            Tab::Endpoints => {
                let Some(endpoint) = self.selected_endpoint() else {
                    return;
                };
                (
                    format!("Remove endpoint `{}`?", endpoint.name),
                    Request::RemoveEndpoint {
                        name: endpoint.name.clone(),
                    },
                )
            }
            _ => {
                let Some(rig) = self.selected_rig() else {
                    return;
                };
                // Deleting removes the whole rig, including its other coins.
                (
                    format!("Stop and remove rig `{}` on {}?", rig.group, rig.node),
                    route(
                        rig.node.clone(),
                        Request::RemoveRig {
                            name: rig.group.clone(),
                        },
                    ),
                )
            }
        };
        self.confirm = Some(Confirm { prompt, request });
    }

    fn nudge_threads(&mut self, delta: isize) {
        let Some(rig) = self.selected_rig() else {
            return;
        };
        if !rig.state.is_live() {
            self.toast("rig is not running".into(), true);
            return;
        }
        let threads = (rig.threads as isize + delta).max(1) as usize;
        let (node, name) = (rig.node.clone(), rig.name.clone());
        self.send(route(node, Request::SetThreads { name, threads }));
    }
}

/// `we_started_daemon` is true when opening the dashboard is what brought the
/// daemon up. In that case, if nothing is mining when the user quits, the
/// daemon is stopped again — looking at the dashboard should not leave a
/// process behind.
pub fn run(we_started_daemon: bool) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, we_started_daemon);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, we_started_daemon: bool) -> Result<()> {
    let mut app = App::new();
    app.we_started_daemon = we_started_daemon;
    app.refresh();
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;

        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if last_tick.elapsed() >= Duration::from_millis(250) {
            app.tick = app.tick.wrapping_add(1);
            last_tick = Instant::now();
        }
        if !app.frozen && app.last_refresh.elapsed() >= REFRESH {
            app.refresh();
        }
        if let Some((_, _, at)) = &app.toast
            && at.elapsed() > TOAST_TTL
        {
            app.toast = None;
        }
        if app.quit {
            app.tidy_up();
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);

    match app.snapshot.as_ref() {
        Some(_) => match app.tab {
            Tab::Dashboard => views::dashboard(frame, app, body),
            Tab::Rigs => views::rigs(frame, app, body),
            Tab::Nodes => views::nodes(frame, app, body),
            Tab::Hardware => views::hardware(frame, app, body),
            Tab::Endpoints => views::endpoints(frame, app, body),
            Tab::Wallets => views::wallets(frame, app, body),
            Tab::Logs => views::logs(frame, app, body),
        },
        None => draw_disconnected(frame, app, body),
    }

    draw_footer(frame, app, footer);

    if app.help {
        widgets::help_overlay(frame, frame.area());
    }
    if let Some(confirm) = &app.confirm {
        widgets::confirm_overlay(frame, frame.area(), &confirm.prompt);
    }
    if let Some(form) = &app.form {
        form.render(frame, frame.area());
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [tabs_area, status_area] =
        Layout::horizontal([Constraint::Min(30), Constraint::Length(58)]).areas(inner);

    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            Line::from(vec![
                Span::styled(format!("{} ", i + 1), theme::muted()),
                Span::raw(t.title()),
            ])
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .style(theme::base())
        .highlight_style(theme::accent())
        .divider(Span::styled("│", theme::border()));
    frame.render_widget(tabs, tabs_area);

    let status = match (&app.snapshot, &app.connection_error) {
        (Some(snapshot), _) => {
            let totals = &snapshot.totals;
            let live = totals.rigs_active > 0;
            let spinner = if live && !app.frozen {
                widgets::SPINNER[app.tick % widgets::SPINNER.len()]
            } else {
                "•"
            };
            Line::from(vec![
                Span::styled(
                    format!("{spinner} "),
                    Style::default().fg(if live { theme::GOOD } else { theme::MUTED }),
                ),
                Span::styled(fmt_hashrate(totals.hashrate), theme::accent()),
                Span::styled("  rigs ", theme::label()),
                Span::styled(
                    format!("{}/{}", totals.rigs_active, totals.rigs_total),
                    theme::value(),
                ),
                Span::styled("  thr ", theme::label()),
                Span::styled(
                    format!("{}/{}", totals.threads_active, totals.threads_budget),
                    theme::value(),
                ),
                Span::styled("  A/R ", theme::label()),
                Span::styled(format!("{}", totals.accepted), theme::good()),
                Span::styled("/", theme::muted()),
                Span::styled(
                    format!("{}", totals.rejected + totals.stale),
                    if totals.rejected + totals.stale > 0 {
                        theme::bad()
                    } else {
                        theme::muted()
                    },
                ),
                Span::styled("  up ", theme::label()),
                Span::styled(fmt_duration(snapshot.daemon.uptime_secs), theme::value()),
            ])
        }
        (None, Some(_)) => Line::from(Span::styled("daemon unreachable", theme::bad())),
        (None, None) => Line::from(Span::styled("connecting...", theme::muted())),
    };
    frame.render_widget(Paragraph::new(status).right_aligned(), status_area);
}

fn draw_disconnected(frame: &mut Frame, app: &App, area: Rect) {
    let message = app
        .connection_error
        .clone()
        .unwrap_or_else(|| "connecting to the daemon...".into());
    let text = vec![
        Line::from(""),
        Line::from(Span::styled("  no daemon", theme::title())),
        Line::from(""),
        Line::from(Span::styled(format!("  {message}"), theme::bad())),
        Line::from(""),
        Line::from(Span::styled(
            "  start one with `cryptocli daemon start`, then press r",
            theme::muted(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border()),
        ),
        area,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some((message, is_error, _)) = &app.toast {
        let style = if *is_error {
            theme::bad()
        } else {
            theme::good()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if *is_error { " ✗ " } else { " ✓ " }, style),
                Span::styled(message.clone(), style),
            ])),
            area,
        );
        return;
    }

    let keys: &[(&str, &str)] = match app.tab {
        Tab::Endpoints => &[
            ("↑↓", "select"),
            ("a", "add"),
            ("d", "remove"),
            ("p", "check now"),
            ("?", "help"),
            ("q", "quit (keeps mining)"),
        ],
        Tab::Nodes => &[
            ("↑↓", "select"),
            ("a", "add machine"),
            ("d", "remove"),
            ("t", "reconnect now"),
            ("r", "reload"),
            ("?", "help"),
            ("q", "quit (keeps mining)"),
        ],
        Tab::Wallets => &[
            ("↑↓", "select"),
            ("a", "add wallet"),
            ("d", "remove"),
            ("?", "help"),
            ("q", "quit (keeps mining)"),
        ],
        Tab::Logs => &[
            ("↑↓", "scroll"),
            ("End", "latest"),
            ("f", "freeze"),
            ("?", "help"),
            ("q", "quit (keeps mining)"),
        ],
        _ => &[
            ("↑↓", "select"),
            ("s/x", "start/stop"),
            ("a", "add rig"),
            ("c", "add coin"),
            ("d", "remove"),
            ("±", "threads"),
            ("?", "help"),
            ("q", "quit (keeps mining)"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, description) in keys {
        spans.push(Span::styled(*key, theme::accent()));
        spans.push(Span::styled(format!(" {description}   "), theme::muted()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
