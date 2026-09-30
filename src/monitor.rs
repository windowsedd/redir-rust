//! `--monitor`: a btop-style terminal dashboard. It is a separate process
//! from the redirector; it polls the snapshot files the running service
//! writes once a second (`connections::STATS_FILE` / `STATE_FILE`) and
//! derives throughput from the difference between consecutive samples.
//!
//! The file is split into a pure model (`Monitor`, parsing, rate maths,
//! formatting) and a thin ratatui/crossterm shell (`draw`, `run`), so the
//! former can be unit tested without a terminal.

use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Dataset, GraphType, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use serde_json::Value;

use crate::connections::{STATE_FILE, STATS_FILE};

/// Seconds of throughput history kept per redirect (one sample per second).
pub const HISTORY: usize = 120;
/// A snapshot older than this is reported as stale (the service is down or
/// its snapshot task stopped).
const STALE_AFTER_MS: u64 = 3_500;

#[derive(Debug, Clone, PartialEq)]
pub struct RedirectStat {
    pub name: String,
    pub protocol: String,
    pub listen: String,
    pub up_total: u64,
    pub down_total: u64,
    pub connections: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnStat {
    pub redirect: String,
    pub protocol: String,
    pub client: String,
    pub target: String,
    pub duration_secs: u64,
    pub up_bytes: u64,
    pub down_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub ts_ms: u64,
    pub started_ms: u64,
    pub redirects: Vec<RedirectStat>,
    pub connections: Vec<ConnStat>,
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

fn num(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}

/// Parses the two snapshot documents. Missing or malformed pieces degrade
/// to empty rather than failing: the writer and reader are different
/// processes and may briefly disagree on versions.
pub fn parse_sample(stats: &str, connections: &str) -> Option<Sample> {
    let stats: Value = serde_json::from_str(stats).ok()?;
    let connections: Value = serde_json::from_str(connections).unwrap_or(Value::Null);

    let redirects = stats["redirects"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|r| RedirectStat {
                    name: text(r, "name"),
                    protocol: text(r, "protocol"),
                    listen: text(r, "listen"),
                    up_total: num(r, "up_total"),
                    down_total: num(r, "down_total"),
                    connections: num(r, "connections"),
                })
                .collect()
        })
        .unwrap_or_default();
    let connections = connections
        .as_array()
        .map(|list| {
            list.iter()
                .map(|c| ConnStat {
                    redirect: text(c, "redirect"),
                    protocol: text(c, "protocol"),
                    client: text(c, "client"),
                    target: text(c, "target"),
                    duration_secs: num(c, "duration_secs"),
                    up_bytes: num(c, "up_bytes"),
                    down_bytes: num(c, "down_bytes"),
                })
                .collect()
        })
        .unwrap_or_default();

    Some(Sample {
        ts_ms: num(&stats, "ts_ms"),
        started_ms: num(&stats, "started_ms"),
        redirects,
        connections,
    })
}

/// Bytes per second between two cumulative readings. A reading that went
/// backwards means the service restarted, which counts as no traffic.
pub fn rate(prev: u64, now: u64, dt_secs: f64) -> f64 {
    if now < prev || dt_secs <= 0.0 {
        0.0
    } else {
        (now - prev) as f64 / dt_secs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Duration,
    Traffic,
}

/// Rolling throughput history of one redirect, oldest first.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub up: VecDeque<f64>,
    pub down: VecDeque<f64>,
}

impl History {
    fn push(&mut self, up: f64, down: f64) {
        for (queue, value) in [(&mut self.up, up), (&mut self.down, down)] {
            if queue.len() == HISTORY {
                queue.pop_front();
            }
            queue.push_back(value);
        }
    }
}

pub struct Monitor {
    pub current: Option<Sample>,
    previous: Option<Sample>,
    /// Redirect name -> history, in the order redirects appear.
    pub histories: Vec<(String, History)>,
    pub total: History,
    /// `None` = all redirects, otherwise the selected redirect's name.
    pub filter: Option<String>,
    pub sort: Sort,
    pub selected: usize,
    /// Last time a snapshot was read successfully, for the stale banner.
    pub read_error: Option<String>,
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            current: None,
            previous: None,
            histories: Vec::new(),
            total: History::default(),
            filter: None,
            sort: Sort::Duration,
            selected: 0,
            read_error: None,
        }
    }

    /// Folds a new snapshot in, extending every redirect's history.
    pub fn ingest(&mut self, sample: Sample) {
        self.read_error = None;
        // Identical timestamp = the writer has not ticked yet; keep the
        // existing history instead of recording a fake zero.
        if self.current.as_ref().map(|c| c.ts_ms) == Some(sample.ts_ms) {
            return;
        }
        if let Some(prev) = self.current.take() {
            let dt = sample.ts_ms.saturating_sub(prev.ts_ms) as f64 / 1000.0;
            let (mut total_up, mut total_down) = (0.0, 0.0);
            for r in &sample.redirects {
                let (up, down) = match prev.redirects.iter().find(|p| p.name == r.name) {
                    Some(p) => (
                        rate(p.up_total, r.up_total, dt),
                        rate(p.down_total, r.down_total, dt),
                    ),
                    None => (0.0, 0.0),
                };
                total_up += up;
                total_down += down;
                self.history_mut(&r.name).push(up, down);
            }
            self.total.push(total_up, total_down);
            self.previous = Some(prev);
        }
        self.current = Some(sample);
        self.clamp_selection();
    }

    fn history_mut(&mut self, name: &str) -> &mut History {
        if let Some(i) = self.histories.iter().position(|(n, _)| n == name) {
            return &mut self.histories[i].1;
        }
        self.histories.push((name.to_string(), History::default()));
        &mut self.histories.last_mut().unwrap().1
    }

    pub fn history(&self) -> &History {
        match &self.filter {
            None => &self.total,
            Some(name) => self
                .histories
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, h)| h)
                .unwrap_or(&self.total),
        }
    }

    pub fn is_stale(&self, now_ms: u64) -> bool {
        match &self.current {
            Some(c) => now_ms.saturating_sub(c.ts_ms) > STALE_AFTER_MS,
            None => true,
        }
    }

    /// Connections after applying the redirect filter and sort order.
    pub fn visible_connections(&self) -> Vec<&ConnStat> {
        let Some(sample) = &self.current else {
            return Vec::new();
        };
        let mut list: Vec<&ConnStat> = sample
            .connections
            .iter()
            .filter(|c| self.filter.as_deref().is_none_or(|f| c.redirect == f))
            .collect();
        match self.sort {
            Sort::Duration => list.sort_by(|a, b| a.duration_secs.cmp(&b.duration_secs)),
            Sort::Traffic => {
                list.sort_by(|a, b| (b.up_bytes + b.down_bytes).cmp(&(a.up_bytes + a.down_bytes)))
            }
        }
        list
    }

    /// Cycles All -> redirect 1 -> ... -> All (or backwards).
    pub fn cycle_filter(&mut self, forward: bool) {
        let names: Vec<&str> = self
            .current
            .iter()
            .flat_map(|s| s.redirects.iter().map(|r| r.name.as_str()))
            .collect();
        // Position 0 is "All", positions 1..=n are redirects.
        let pos = match &self.filter {
            None => 0,
            Some(f) => names.iter().position(|n| n == f).map_or(0, |i| i + 1),
        };
        let slots = names.len() + 1;
        let next = if forward {
            (pos + 1) % slots
        } else {
            (pos + slots - 1) % slots
        };
        self.filter = (next > 0).then(|| names[next - 1].to_string());
        self.selected = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.visible_connections().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(len - 1);
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_connections().len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    pub fn toggle_sort(&mut self) {
        self.sort = match self.sort {
            Sort::Duration => Sort::Traffic,
            Sort::Traffic => Sort::Duration,
        };
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

pub fn fmt_bytes(n: u64) -> String {
    fmt_scaled(n as f64, "")
}

pub fn fmt_rate(bytes_per_sec: f64) -> String {
    fmt_scaled(bytes_per_sec, "/s")
}

fn fmt_scaled(value: f64, suffix: &str) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = value;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{v:.0} {}{suffix}", UNITS[unit])
    } else {
        format!("{v:.1} {}{suffix}", UNITS[unit])
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

const UP_COLOR: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
const DOWN_COLOR: Color = Color::Rgb(0x89, 0xb4, 0xfa);
const ACCENT: Color = Color::Rgb(0xa6, 0xe3, 0xa1);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn draw(frame: &mut Frame, monitor: &Monitor, now_ms: u64) {
    let [header, middle, connections, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(45),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [graph, redirects] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(middle);

    draw_header(frame, header, monitor, now_ms);
    draw_graph(frame, graph, monitor);
    draw_redirects(frame, redirects, monitor);
    draw_connections(frame, connections, monitor);
    draw_footer(frame, footer, monitor);
}

fn latest(queue: &VecDeque<f64>) -> f64 {
    queue.back().copied().unwrap_or(0.0)
}

fn draw_header(frame: &mut Frame, area: Rect, monitor: &Monitor, now_ms: u64) {
    let (state, color) = if let Some(err) = &monitor.read_error {
        (format!("no data ({err})"), Color::Red)
    } else if monitor.is_stale(now_ms) {
        ("stale — is the service running?".to_string(), Color::Yellow)
    } else {
        ("live".to_string(), ACCENT)
    };
    let conns: u64 = monitor
        .current
        .as_ref()
        .map_or(0, |s| s.redirects.iter().map(|r| r.connections).sum());
    let line = Line::from(vec![
        Span::styled(
            " redir-rust ",
            Style::new().add_modifier(Modifier::BOLD).fg(ACCENT),
        ),
        Span::styled(format!("● {state}"), Style::new().fg(color)),
        Span::raw(
            monitor
                .current
                .as_ref()
                .filter(|s| s.started_ms > 0)
                .map_or_else(String::new, |s| {
                    format!(
                        "   uptime {}",
                        fmt_duration(now_ms.saturating_sub(s.started_ms) / 1000)
                    )
                }),
        ),
        Span::raw(format!("   connections {conns}   ")),
        Span::styled(
            format!("↑ {}", fmt_rate(latest(&monitor.total.up))),
            Style::new().fg(UP_COLOR),
        ),
        Span::raw("   "),
        Span::styled(
            format!("↓ {}", fmt_rate(latest(&monitor.total.down))),
            Style::new().fg(DOWN_COLOR),
        ),
    ]);
    frame.render_widget(Paragraph::new(line).block(Block::bordered()), area);
}

fn series(queue: &VecDeque<f64>) -> Vec<(f64, f64)> {
    // Right-align so the newest sample always sits at the right edge.
    let offset = HISTORY - queue.len();
    queue
        .iter()
        .enumerate()
        .map(|(i, v)| ((offset + i) as f64, *v))
        .collect()
}

fn draw_graph(frame: &mut Frame, area: Rect, monitor: &Monitor) {
    let history = monitor.history();
    let (up, down) = (series(&history.up), series(&history.down));
    let peak = history
        .up
        .iter()
        .chain(history.down.iter())
        .cloned()
        .fold(0.0_f64, f64::max)
        .max(1024.0);
    let title = format!(" traffic: {} ", monitor.filter.as_deref().unwrap_or("all"));
    let datasets = vec![
        Dataset::default()
            .name("↑ up")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(UP_COLOR))
            .data(&up),
        Dataset::default()
            .name("↓ down")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(DOWN_COLOR))
            .data(&down),
    ];
    let chart = Chart::new(datasets)
        .block(Block::bordered().title(title))
        .x_axis(Axis::default().bounds([0.0, (HISTORY - 1) as f64]))
        .y_axis(
            Axis::default()
                .bounds([0.0, peak * 1.1])
                .labels(vec![Span::raw("0"), Span::raw(fmt_rate(peak))]),
        );
    frame.render_widget(chart, area);
}

fn draw_redirects(frame: &mut Frame, area: Rect, monitor: &Monitor) {
    let header = Row::new([
        "name",
        "proto",
        "listen",
        "↑/s",
        "↓/s",
        "conns",
        "total ↑/↓",
    ])
    .style(Style::new().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = monitor
        .current
        .iter()
        .flat_map(|s| s.redirects.iter())
        .map(|r| {
            let h = monitor
                .histories
                .iter()
                .find(|(n, _)| *n == r.name)
                .map(|(_, h)| h);
            let (up, down) = h.map_or((0.0, 0.0), |h| (latest(&h.up), latest(&h.down)));
            let row = Row::new([
                r.name.clone(),
                r.protocol.clone(),
                r.listen.clone(),
                fmt_rate(up),
                fmt_rate(down),
                r.connections.to_string(),
                format!("{} / {}", fmt_bytes(r.up_total), fmt_bytes(r.down_total)),
            ]);
            if monitor.filter.as_deref() == Some(r.name.as_str()) {
                row.style(Style::new().fg(ACCENT))
            } else {
                row
            }
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Length(5),
            Constraint::Min(14),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(5),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(Block::bordered().title(" redirects "));
    frame.render_widget(table, area);
}

fn draw_connections(frame: &mut Frame, area: Rect, monitor: &Monitor) {
    let list = monitor.visible_connections();
    let header = Row::new(["redirect", "proto", "client", "target", "age", "↑", "↓"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = list
        .iter()
        .map(|c| {
            Row::new([
                c.redirect.clone(),
                c.protocol.clone(),
                c.client.clone(),
                c.target.clone(),
                fmt_duration(c.duration_secs),
                fmt_bytes(c.up_bytes),
                fmt_bytes(c.down_bytes),
            ])
        })
        .collect();
    let sort = match monitor.sort {
        Sort::Duration => "newest first",
        Sort::Traffic => "most traffic first",
    };
    let table = Table::new(
        rows,
        [
            Constraint::Min(8),
            Constraint::Length(5),
            Constraint::Min(21),
            Constraint::Min(21),
            Constraint::Length(8),
            Constraint::Length(11),
            Constraint::Length(11),
        ],
    )
    .header(header)
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title(format!(" connections ({}) — {sort} ", list.len())));
    let mut state =
        TableState::default().with_selected((!list.is_empty()).then_some(monitor.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_footer(frame: &mut Frame, area: Rect, _monitor: &Monitor) {
    let help = " q quit   Tab/Shift-Tab redirect   ↑↓ select   s sort ";
    frame.render_widget(
        Paragraph::new(help)
            .alignment(Alignment::Left)
            .style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

// ---------------------------------------------------------------------
// Terminal shell
// ---------------------------------------------------------------------

fn read_snapshots() -> Result<Sample, String> {
    let stats = std::fs::read_to_string(STATS_FILE).map_err(|e| format!("{STATS_FILE}: {e}"))?;
    let connections = std::fs::read_to_string(STATE_FILE).unwrap_or_default();
    parse_sample(&stats, &connections).ok_or_else(|| format!("{STATS_FILE}: malformed"))
}

pub fn run() -> ExitCode {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("error: --monitor needs an interactive terminal");
        return ExitCode::FAILURE;
    }
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: monitor failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
    let mut monitor = Monitor::new();
    let mut last_read: Option<Instant> = None;
    loop {
        if last_read.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            match read_snapshots() {
                Ok(sample) => monitor.ingest(sample),
                Err(err) => monitor.read_error = Some(err),
            }
            last_read = Some(Instant::now());
        }
        terminal.draw(|frame| draw(frame, &monitor, now_ms()))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Tab => monitor.cycle_filter(true),
            KeyCode::BackTab => monitor.cycle_filter(false),
            KeyCode::Up | KeyCode::Char('k') => monitor.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => monitor.move_selection(1),
            KeyCode::Char('s') => monitor.toggle_sort(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn redirect(name: &str, up: u64, down: u64, conns: u64) -> RedirectStat {
        RedirectStat {
            name: name.into(),
            protocol: "tcp".into(),
            listen: "0.0.0.0:25565".into(),
            up_total: up,
            down_total: down,
            connections: conns,
        }
    }

    fn conn(redirect: &str, age: u64, up: u64) -> ConnStat {
        ConnStat {
            redirect: redirect.into(),
            protocol: "tcp".into(),
            client: "10.0.0.5:5000".into(),
            target: "10.0.0.10:25566".into(),
            duration_secs: age,
            up_bytes: up,
            down_bytes: 0,
        }
    }

    fn sample(ts_ms: u64, redirects: Vec<RedirectStat>, connections: Vec<ConnStat>) -> Sample {
        Sample {
            ts_ms,
            started_ms: 0,
            redirects,
            connections,
        }
    }

    #[test]
    fn rate_handles_normal_growth_reset_and_zero_interval() {
        assert_eq!(rate(1000, 3000, 2.0), 1000.0);
        assert_eq!(rate(5000, 100, 1.0), 0.0, "counter reset after restart");
        assert_eq!(rate(1, 2, 0.0), 0.0);
    }

    #[test]
    fn parses_both_snapshot_files() {
        let stats = r#"{"ts_ms":5,"started_ms":2,"redirects":[{"name":"mc","protocol":"tcp","listen":"0.0.0.0:1","up_total":7,"down_total":9,"connections":2}]}"#;
        let conns = r#"[{"id":1,"redirect":"mc","protocol":"tcp","client":"c","target":"t","duration_secs":4,"up_bytes":1,"down_bytes":2}]"#;
        let s = parse_sample(stats, conns).unwrap();
        assert_eq!(s.ts_ms, 5);
        assert_eq!(s.started_ms, 2);
        assert_eq!(
            s.redirects[0],
            redirect_with_listen("mc", "0.0.0.0:1", 7, 9, 2)
        );
        assert_eq!(s.connections[0].down_bytes, 2);
        assert!(parse_sample("not json", "[]").is_none());
        // Old-format or missing connection file must not lose the stats.
        assert_eq!(parse_sample(stats, "").unwrap().connections.len(), 0);
    }

    fn redirect_with_listen(
        name: &str,
        listen: &str,
        up: u64,
        down: u64,
        conns: u64,
    ) -> RedirectStat {
        RedirectStat {
            listen: listen.into(),
            ..redirect(name, up, down, conns)
        }
    }

    #[test]
    fn ingest_derives_per_redirect_and_total_rates() {
        let mut m = Monitor::new();
        m.ingest(sample(
            1_000,
            vec![redirect("a", 0, 0, 0), redirect("b", 0, 0, 0)],
            vec![],
        ));
        m.ingest(sample(
            3_000,
            vec![redirect("a", 2_000, 4_000, 1), redirect("b", 1_000, 0, 0)],
            vec![],
        ));

        let a = &m.histories.iter().find(|(n, _)| n == "a").unwrap().1;
        assert_eq!(latest(&a.up), 1000.0);
        assert_eq!(latest(&a.down), 2000.0);
        assert_eq!(latest(&m.total.up), 1500.0);
        assert_eq!(latest(&m.total.down), 2000.0);
    }

    #[test]
    fn repeated_timestamp_does_not_record_a_zero_sample() {
        let mut m = Monitor::new();
        m.ingest(sample(1_000, vec![redirect("a", 0, 0, 0)], vec![]));
        m.ingest(sample(2_000, vec![redirect("a", 100, 0, 0)], vec![]));
        m.ingest(sample(2_000, vec![redirect("a", 100, 0, 0)], vec![]));
        assert_eq!(m.total.up.len(), 1);
    }

    #[test]
    fn history_is_capped() {
        let mut m = Monitor::new();
        for i in 0..(HISTORY as u64 + 20) {
            m.ingest(sample(
                1_000 * (i + 1),
                vec![redirect("a", i * 10, 0, 0)],
                vec![],
            ));
        }
        assert_eq!(m.total.up.len(), HISTORY);
    }

    #[test]
    fn filter_cycles_through_all_and_each_redirect_both_ways() {
        let mut m = Monitor::new();
        m.ingest(sample(
            1,
            vec![redirect("a", 0, 0, 0), redirect("b", 0, 0, 0)],
            vec![],
        ));
        m.cycle_filter(true);
        assert_eq!(m.filter.as_deref(), Some("a"));
        m.cycle_filter(true);
        assert_eq!(m.filter.as_deref(), Some("b"));
        m.cycle_filter(true);
        assert_eq!(m.filter, None);
        m.cycle_filter(false);
        assert_eq!(m.filter.as_deref(), Some("b"));
    }

    #[test]
    fn connections_filter_sort_and_selection_stay_in_range() {
        let mut m = Monitor::new();
        m.ingest(sample(
            1,
            vec![redirect("a", 0, 0, 2), redirect("b", 0, 0, 1)],
            vec![conn("a", 30, 5), conn("a", 10, 500), conn("b", 20, 50)],
        ));
        let ages: Vec<u64> = m
            .visible_connections()
            .iter()
            .map(|c| c.duration_secs)
            .collect();
        assert_eq!(ages, [10, 20, 30], "newest first");

        m.toggle_sort();
        let ups: Vec<u64> = m.visible_connections().iter().map(|c| c.up_bytes).collect();
        assert_eq!(ups, [500, 50, 5], "most traffic first");

        m.cycle_filter(true); // "a"
        assert_eq!(m.visible_connections().len(), 2);
        m.move_selection(10);
        assert_eq!(m.selected, 1, "selection is clamped");
        m.move_selection(-10);
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn stale_when_no_sample_or_old_sample() {
        let mut m = Monitor::new();
        assert!(m.is_stale(0));
        m.ingest(sample(10_000, vec![], vec![]));
        assert!(!m.is_stale(11_000));
        assert!(m.is_stale(20_000));
    }

    #[test]
    fn formats_sizes_rates_and_durations() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_rate(3.0 * 1024.0 * 1024.0), "3.0 MiB/s");
        assert_eq!(fmt_duration(9), "9s");
        assert_eq!(fmt_duration(125), "2m05s");
        assert_eq!(fmt_duration(3 * 3600 + 7 * 60), "3h07m");
    }

    fn render(m: &Monitor, now: u64) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, m, now)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_redirects_connections_and_totals() {
        let mut m = Monitor::new();
        m.ingest(sample(1_000, vec![redirect("minecraft", 0, 0, 0)], vec![]));
        m.ingest(sample(
            2_000,
            vec![redirect("minecraft", 2048, 4096, 1)],
            vec![conn("minecraft", 65, 2048)],
        ));
        m.current.as_mut().unwrap().started_ms = 500;
        let screen = render(&m, 2_500);
        for expected in [
            "redir-rust",
            "live",
            "uptime 2s",
            "minecraft",
            "0.0.0.0:25565",
            "10.0.0.5:5000",
            "10.0.0.10:25566",
            "1m05s",
            "2.0 KiB/s",
            "4.0 KiB/s",
        ] {
            assert!(
                screen.contains(expected),
                "missing {expected:?} in:\n{screen}"
            );
        }
    }

    #[test]
    fn renders_error_and_stale_states_without_panicking() {
        let mut m = Monitor::new();
        m.read_error = Some("permission denied".into());
        assert!(render(&m, 0).contains("no data (permission denied)"));

        let mut m = Monitor::new();
        m.ingest(sample(1_000, vec![], vec![]));
        assert!(render(&m, 60_000).contains("stale"));
    }
}
