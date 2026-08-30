//! Modal add-forms, so wallets, rigs, coins and endpoints can be created from
//! inside the dashboard rather than from a shell.
//!
//! The form only collects text; validation lives in the daemon, which is the
//! single place that knows whether a config is coherent. Whatever it rejects
//! comes straight back into the form so the user can fix it in place without
//! retyping everything.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::theme;
use super::widgets::centered;
use crate::ipc::Request;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    Wallet,
    Rig,
    /// Add another coin to an existing rig, mined at the same time.
    RigCoin,
    Endpoint,
    /// Another machine to show and control from this dashboard.
    Node,
}

pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub hint: &'static str,
    pub required: bool,
    /// Rendered as dots. Nothing here is a private key, but shoulder-surfing a
    /// pool password is still rude.
    pub secret: bool,
}

impl Field {
    fn new(label: &'static str, hint: &'static str, required: bool) -> Self {
        Self {
            label,
            value: String::new(),
            hint,
            required,
            secret: false,
        }
    }

    fn secret(label: &'static str, hint: &'static str, required: bool) -> Self {
        Self {
            label,
            value: String::new(),
            hint,
            required,
            secret: true,
        }
    }

    fn with(label: &'static str, hint: &'static str, default: &str) -> Self {
        Self {
            label,
            value: default.to_string(),
            hint,
            required: false,
            secret: false,
        }
    }
}

pub struct Form {
    pub kind: FormKind,
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    /// Rig the new coin attaches to, for `FormKind::RigCoin`.
    pub subject: Option<String>,
}

impl Form {
    pub fn wallet() -> Self {
        Self {
            kind: FormKind::Wallet,
            title: "Add wallet".into(),
            fields: vec![
                Field::new("name", "short label, e.g. main", true),
                Field::new("coin", "BTC, LTC, DOGE...", true),
                Field::new("address", "payout address — never a private key", true),
                Field::new("note", "optional", false),
            ],
            focus: 0,
            error: None,
            subject: None,
        }
    }

    pub fn rig() -> Self {
        Self {
            kind: FormKind::Rig,
            title: "Add rig".into(),
            fields: vec![
                Field::new("name", "e.g. btc-main", true),
                Field::new("pool", "an endpoint name, or stratum+tcp://host:port", true),
                Field::new("coin", "BTC", true),
                Field::with("algo", "sha256d or sha256", "sha256d"),
                Field::new("wallet", "name of a configured wallet", false),
                Field::new("worker", "optional worker suffix", false),
                Field::new("user", "full stratum user (instead of wallet)", false),
                Field::with("pass", "pool password", "x"),
                Field::with("threads", "0 = share the global budget", "0"),
                Field::with("weight", "share vs other rigs", "1"),
            ],
            focus: 0,
            error: None,
            subject: None,
        }
    }

    pub fn rig_coin(rig: &str) -> Self {
        Self {
            kind: FormKind::RigCoin,
            title: format!("Mine another coin on `{rig}`"),
            fields: vec![
                Field::new("coin", "LTC", true),
                Field::new("pool", "an endpoint name, or stratum+tcp://host:port", true),
                Field::new("wallet", "name of a configured wallet", false),
                Field::new("worker", "optional worker suffix", false),
                Field::with("weight", "share of this rig's threads", "1"),
            ],
            focus: 0,
            error: None,
            subject: Some(rig.to_string()),
        }
    }

    pub fn node() -> Self {
        Self {
            kind: FormKind::Node,
            title: "Add machine".into(),
            fields: vec![
                Field::new("name", "how to label it here, e.g. rig2", true),
                Field::new("address", "host:port from `cryptocli remote enable`", true),
                Field::secret("token", "shared secret from that machine", true),
                Field::new(
                    "fingerprint",
                    "sha256:... — blank trusts the cert on first use",
                    false,
                ),
            ],
            focus: 0,
            error: None,
            subject: None,
        }
    }

    pub fn endpoint() -> Self {
        Self {
            kind: FormKind::Endpoint,
            title: "Add check endpoint".into(),
            fields: vec![
                Field::new("name", "e.g. pool-check", true),
                Field::new("url", "https://host/api  or  stratum+tcp://host:3333", true),
                Field::new("user", "pool worker, or basic-auth user", false),
                Field::secret("password", "pool password (often just x)", false),
                Field::with("interval", "seconds between checks", "60"),
                Field::with("timeout", "seconds", "10"),
                Field::with("method", "http only: GET or POST", "GET"),
                Field::with("expect", "http only: expected status", "200"),
                Field::new("headers", "http only: Name: value, Name: value", false),
                Field::new("fields", "http only: label=json.path", false),
            ],
            focus: 0,
            error: None,
            subject: None,
        }
    }

    fn value(&self, index: usize) -> String {
        self.fields
            .get(index)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default()
    }

    fn optional(&self, index: usize) -> Option<String> {
        let value = self.value(index);
        if value.is_empty() { None } else { Some(value) }
    }

    // ------------------------------------------------------------- input ---

    pub fn insert(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.push(c);
            self.error = None;
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.pop();
            self.error = None;
        }
    }

    pub fn clear_field(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.clear();
            self.error = None;
        }
    }

    pub fn next(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
    }

    pub fn previous(&mut self) {
        self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
    }

    /// Turn the filled-in form into a request, or explain what is missing.
    pub fn build(&self) -> Result<Request, String> {
        for field in &self.fields {
            if field.required && field.value.trim().is_empty() {
                return Err(format!("`{}` is required", field.label));
            }
        }
        match self.kind {
            FormKind::Wallet => Ok(Request::AddWallet {
                name: self.value(0),
                coin: self.value(1),
                address: self.value(2),
                label: self.optional(3),
            }),
            FormKind::Rig => {
                if self.optional(4).is_none() && self.optional(6).is_none() {
                    return Err("set `wallet` or `user` so the pool knows who to pay".into());
                }
                Ok(Request::AddRig {
                    name: self.value(0),
                    url: self.value(1),
                    coin: self.value(2),
                    algo: self.value(3),
                    wallet: self.optional(4),
                    worker: self.optional(5),
                    user: self.optional(6),
                    pass: self.value(7),
                    threads: parse(&self.value(8), "threads")?,
                    weight: parse(&self.value(9), "weight")?,
                })
            }
            FormKind::RigCoin => Ok(Request::AddRigCoin {
                rig: self.subject.clone().unwrap_or_default(),
                coin: self.value(0),
                url: self.value(1),
                wallet: self.optional(2),
                worker: self.optional(3),
                weight: parse(&self.value(4), "weight")?,
            }),
            FormKind::Node => Ok(Request::AddNode {
                name: self.value(0),
                address: self.value(1),
                token: self.value(2),
                fingerprint: self.value(3),
            }),
            FormKind::Endpoint => Ok(Request::AddEndpoint {
                name: self.value(0),
                url: self.value(1),
                user: self.optional(2),
                password: self.optional(3),
                interval_secs: parse(&self.value(4), "interval")?,
                timeout_secs: parse(&self.value(5), "timeout")?,
                method: self.value(6),
                expect_status: parse(&self.value(7), "expect")?,
                headers: pairs(&self.value(8), ':')?,
                fields: pairs(&self.value(9), '=')?,
            }),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let height = self.fields.len() as u16 + 7;
        let target = centered(area, 74, height);
        frame.render_widget(Clear, target);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ACCENT))
            .title(Span::styled(format!(" {} ", self.title), theme::title()));
        let inner = block.inner(target);
        frame.render_widget(block, target);

        let [rows, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);

        let lines: Vec<Line> = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let focused = i == self.focus;
                let marker = if focused { "▸" } else { " " };
                let label = format!("{marker} {:<12}", field.label);
                let mut spans = vec![Span::styled(
                    label,
                    if focused {
                        theme::accent()
                    } else {
                        theme::label()
                    },
                )];
                if field.value.is_empty() {
                    spans.push(Span::styled(field.hint, theme::muted()));
                } else {
                    spans.push(Span::styled(
                        if field.secret {
                            "•".repeat(field.value.chars().count())
                        } else {
                            field.value.clone()
                        },
                        Style::default()
                            .fg(theme::TEXT)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                if focused {
                    // A block cursor, so it is obvious which field takes input.
                    spans.push(Span::styled("█", theme::accent()));
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows);

        let mut footer_lines = Vec::new();
        if let Some(error) = &self.error {
            footer_lines.push(Line::from(Span::styled(format!("✗ {error}"), theme::bad())));
        } else {
            footer_lines.push(Line::from(Span::styled(
                self.fields
                    .get(self.focus)
                    .map(|f| {
                        if f.required {
                            format!("{} — required", f.hint)
                        } else {
                            format!("{} — optional", f.hint)
                        }
                    })
                    .unwrap_or_default(),
                theme::muted(),
            )));
        }
        footer_lines.push(Line::from(vec![
            Span::styled("Tab/↑↓", theme::accent()),
            Span::styled(" move   ", theme::muted()),
            Span::styled("Enter", theme::accent()),
            Span::styled(" save   ", theme::muted()),
            Span::styled("Ctrl-U", theme::accent()),
            Span::styled(" clear field   ", theme::muted()),
            Span::styled("Esc", theme::accent()),
            Span::styled(" cancel", theme::muted()),
        ]));
        frame.render_widget(
            Paragraph::new(footer_lines).wrap(Wrap { trim: false }),
            footer,
        );
    }
}

fn parse<T: std::str::FromStr>(value: &str, label: &str) -> Result<T, String> {
    value
        .trim()
        .parse::<T>()
        .map_err(|_| format!("`{label}` should be a number, got `{value}`"))
}

/// Parse `a: b, c: d` (headers) or `a=b, c=d` (fields) into pairs.
fn pairs(value: &str, separator: char) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, val) = entry
            .split_once(separator)
            .ok_or_else(|| format!("`{entry}` should look like `key{separator} value`"))?;
        out.push((key.trim().to_string(), val.trim().to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_fields_are_enforced() {
        let form = Form::wallet();
        assert!(form.build().is_err());
    }

    #[test]
    fn a_filled_wallet_form_builds_a_request() {
        let mut form = Form::wallet();
        form.fields[0].value = "main".into();
        form.fields[1].value = "BTC".into();
        form.fields[2].value = "bc1qexample".into();
        match form.build() {
            Ok(Request::AddWallet {
                name, coin, label, ..
            }) => {
                assert_eq!(name, "main");
                assert_eq!(coin, "BTC");
                assert_eq!(label, None, "blank optional fields stay unset");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_rig_needs_a_wallet_or_a_user() {
        let mut form = Form::rig();
        form.fields[0].value = "btc".into();
        form.fields[1].value = "stratum+tcp://pool:3333".into();
        form.fields[2].value = "BTC".into();
        assert!(form.build().unwrap_err().contains("wallet"));
        form.fields[4].value = "main".into();
        assert!(form.build().is_ok());
    }

    #[test]
    fn numeric_fields_report_bad_input() {
        let mut form = Form::rig();
        form.fields[0].value = "btc".into();
        form.fields[1].value = "stratum+tcp://pool:3333".into();
        form.fields[2].value = "BTC".into();
        form.fields[4].value = "main".into();
        form.fields[8].value = "lots".into();
        assert!(form.build().unwrap_err().contains("threads"));
    }

    /// The endpoint form has a lot of fields and they are read positionally,
    /// so pin the mapping down.
    #[test]
    fn endpoint_form_maps_every_field_to_the_right_place() {
        let mut form = Form::endpoint();
        form.fields[0].value = "unmineable".into();
        form.fields[1].value = "stratum+tcp://sha256.unmineable.com:3333".into();
        form.fields[2].value = "BTC:addr.worker".into();
        form.fields[3].value = "x".into();
        form.fields[4].value = "45".into();
        form.fields[5].value = "8".into();
        match form.build() {
            Ok(Request::AddEndpoint {
                name,
                url,
                user,
                password,
                interval_secs,
                timeout_secs,
                method,
                expect_status,
                ..
            }) => {
                assert_eq!(name, "unmineable");
                assert_eq!(url, "stratum+tcp://sha256.unmineable.com:3333");
                assert_eq!(user.as_deref(), Some("BTC:addr.worker"));
                assert_eq!(password.as_deref(), Some("x"));
                assert_eq!(interval_secs, 45);
                assert_eq!(timeout_secs, 8);
                assert_eq!(method, "GET");
                assert_eq!(expect_status, 200);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_password_field_is_masked() {
        let form = Form::endpoint();
        assert!(form.fields[3].secret, "password must render as dots");
        assert!(!form.fields[2].secret, "user is not a secret");
    }

    #[test]
    fn a_node_form_requires_a_token() {
        let mut form = Form::node();
        form.fields[0].value = "rig2".into();
        form.fields[1].value = "192.168.1.50:9944".into();
        assert!(form.build().unwrap_err().contains("token"));
        form.fields[2].value = "sekret".into();
        match form.build() {
            Ok(Request::AddNode {
                name,
                address,
                token,
                fingerprint,
            }) => {
                assert_eq!(name, "rig2");
                assert_eq!(address, "192.168.1.50:9944");
                assert_eq!(token, "sekret");
                assert!(fingerprint.is_empty(), "blank means trust on first use");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(form.fields[2].secret, "the token must be masked");
    }

    #[test]
    fn header_and_field_pairs_parse() {
        assert_eq!(
            pairs("Authorization: Bearer x, X-Key: 1", ':').unwrap(),
            vec![
                ("Authorization".to_string(), "Bearer x".to_string()),
                ("X-Key".to_string(), "1".to_string())
            ]
        );
        assert_eq!(
            pairs("balance=data.balance", '=').unwrap(),
            vec![("balance".to_string(), "data.balance".to_string())]
        );
        assert_eq!(pairs("", '=').unwrap(), vec![]);
        assert!(pairs("nonsense", '=').is_err());
    }
}
