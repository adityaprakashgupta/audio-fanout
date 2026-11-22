//! Terminal UI (ratatui): live device list, per-output volume and delay, add/remove
//! outputs from a dropdown that follows devices appearing and disappearing.
//!
//! All state transitions live in [`App::on_key`] and all drawing in [`draw`], both of
//! which are tested without a terminal (fake engine + ratatui's TestBackend).

use crate::devices::{loops_back_to, name_matches};
use crate::engine::Engine;
use crate::log::{self, log};
use crate::slave::Settings;
use crate::watch::{Snapshot, Watcher};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// Volume given to outputs added from the UI: a freshly added pair of earbuds should
/// not start at full blast.
pub const NEW_OUTPUT_VOLUME_PCT: u32 = 50;

// ---- what the UI needs to know about the engine --------------------------------

/// What the receiver reports about a network output.
#[derive(Clone, Debug, PartialEq)]
pub struct NetView {
    pub rtt_ms: f32,
    pub loss_pct: f32,
    /// The receiver has a working output device.
    pub playing: bool,
}

#[derive(Clone)]
pub struct OutputView {
    pub id: u64,
    /// Key: a device name, or `net:IP:PORT` for a network receiver.
    pub name: String,
    /// What the table shows (filled in from the live device list).
    pub label: String,
    pub net: Option<NetView>,
    pub connected: bool,
    pub volume_pct: u32,
    pub muted: bool,
    pub delay_ms: u32,
    pub fill_ms: u32,
    pub target_ms: u32,
    pub ratio_ppm: i32,
    pub reconnects: u32,
    pub settings: Arc<Settings>,
}

#[derive(Clone, Default)]
pub struct SourceView {
    pub name: String,
    pub desc: String,
    pub connected: bool,
}

pub trait EngineLike {
    fn outputs(&self) -> Vec<OutputView>;
    fn source(&self) -> SourceView;
    fn master_gain(&self) -> f32;
    fn follow_volume(&self) -> bool;
    fn set_follow_volume(&mut self, on: bool);
    fn add_output(&mut self, device: &str) -> Result<()>;
    fn remove_output(&mut self, id: u64);
    fn tick(&mut self) {}
}

impl EngineLike for Engine {
    fn outputs(&self) -> Vec<OutputView> {
        let rate = self.src_rate.max(1);
        self.handles()
            .iter()
            .map(|o| {
                // A network output's buffer lives on the receiver: show what it reports.
                let (fill_ms, target_ms, net) = match &o.net {
                    Some(n) => (
                        n.remote_fill_ms.load(Ordering::Relaxed),
                        n.remote_target_ms.load(Ordering::Relaxed),
                        Some(NetView {
                            rtt_ms: n.rtt_us.load(Ordering::Relaxed) as f32 / 1000.0,
                            loss_pct: n.loss_pct(),
                            playing: n.playing.load(Ordering::Relaxed),
                        }),
                    ),
                    None => (
                        o.stats.fill_frames.load(Ordering::Relaxed) * 1000 / rate,
                        o.stats.target_frames.load(Ordering::Relaxed) * 1000 / rate,
                        None,
                    ),
                };
                OutputView {
                    id: o.id,
                    name: o.name.clone(),
                    label: o.name.clone(),
                    net,
                    connected: o.stats.connected.load(Ordering::Relaxed),
                    volume_pct: o.settings.volume_pct.load(Ordering::Relaxed),
                    muted: o.settings.muted.load(Ordering::Relaxed),
                    delay_ms: o.settings.delay_ms.load(Ordering::Relaxed),
                    fill_ms,
                    target_ms,
                    ratio_ppm: o.stats.ratio_ppm.load(Ordering::Relaxed),
                    reconnects: o.stats.reconnects.load(Ordering::Relaxed),
                    settings: o.settings.clone(),
                }
            })
            .collect()
    }
    fn source(&self) -> SourceView {
        SourceView {
            name: self.source_name.clone(),
            desc: format!("{} Hz, {} ch", self.src_rate, self.src_ch),
            connected: self.source_connected(),
        }
    }
    fn master_gain(&self) -> f32 {
        Engine::master_gain(self)
    }
    fn follow_volume(&self) -> bool {
        Engine::follow_volume(self)
    }
    fn set_follow_volume(&mut self, on: bool) {
        Engine::set_follow_volume(self, on)
    }
    fn add_output(&mut self, device: &str) -> Result<()> {
        Engine::add_output(self, device, 0, NEW_OUTPUT_VOLUME_PCT).map(|_| ())
    }
    fn remove_output(&mut self, id: u64) {
        Engine::remove_output(self, id)
    }
    fn tick(&mut self) {
        Engine::tick(self)
    }
}

// ---- dropdown ------------------------------------------------------------------

/// A dropdown selection that follows its device by NAME, so when the live list
/// changes underneath (a device appears or disappears) the highlight stays on the
/// same device instead of jumping to whatever now sits at that index.
#[derive(Default, Clone, Debug)]
pub struct Picker {
    chosen: Option<String>,
    idx: usize,
}

impl Picker {
    pub fn index(&self, items: &[String]) -> usize {
        if items.is_empty() {
            return 0;
        }
        let last = items.len() - 1;
        match &self.chosen {
            Some(n) => items.iter().position(|i| i == n).unwrap_or(self.idx.min(last)),
            None => self.idx.min(last),
        }
    }
    pub fn move_by(&mut self, delta: i32, items: &[String]) {
        if items.is_empty() {
            return;
        }
        let cur = self.index(items) as i32;
        let new = (cur + delta).clamp(0, items.len() as i32 - 1) as usize;
        self.idx = new;
        self.chosen = Some(items[new].clone());
    }
    pub fn selected<'a>(&self, items: &'a [String]) -> Option<&'a String> {
        items.get(self.index(items))
    }
}

/// Output devices that can still be added: present now, not already in the group,
/// and not a device that would feed back into the source.
pub fn available_outputs(snap: &Snapshot, source: &str, existing: &[OutputView]) -> Vec<String> {
    let mut v: Vec<String> = snap
        .outputs
        .iter()
        .filter(|d| !existing.iter().any(|o| name_matches(&o.name, d)) && !loops_back_to(source, d))
        .cloned()
        .collect();
    // network receivers announcing themselves on the LAN, by key `net:IP:PORT`
    for r in &snap.receivers {
        let key = format!("net:{}", r.addr);
        if !existing.iter().any(|o| o.name == key) {
            v.push(key);
        }
    }
    v
}

/// Human-readable text for a device key (device names are their own label).
pub fn item_label(key: &str, snap: &Snapshot) -> String {
    match key.strip_prefix("net:") {
        Some(addr) => match snap.receivers.iter().find(|r| r.addr.to_string() == addr) {
            Some(r) => format!("[net] {} ({}){}", r.name, r.addr.ip(), if r.busy { " - busy" } else { "" }),
            None => format!("[net] {addr}"),
        },
        None => key.to_string(),
    }
}

/// Devices that can be the source: inputs, plus (Windows) outputs via loopback.
pub fn source_candidates(snap: &Snapshot) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> =
        snap.inputs.iter().map(|n| (n.clone(), format!("{n}  [input]"))).collect();
    if cfg!(windows) {
        for n in &snap.outputs {
            if !snap.inputs.contains(n) {
                v.push((n.clone(), format!("{n}  [output, captured by loopback]")));
            }
        }
    }
    v
}

// ---- app state and key handling ---------------------------------------------------

pub enum Mode {
    Normal,
    Add(Picker),
    /// Typing the address of a network receiver (`host[:port]`).
    Input(String),
}

pub struct App {
    pub selected: usize,
    pub mode: Mode,
    pub quit: bool,
}

impl Default for App {
    fn default() -> Self {
        App { selected: 0, mode: Mode::Normal, quit: false }
    }
}

impl App {
    pub fn on_key<E: EngineLike>(&mut self, key: KeyEvent, eng: &mut E, snap: &Snapshot) {
        // Windows reports key releases (and repeats) too: act on presses only.
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        let outs = eng.outputs();
        self.selected = self.selected.min(outs.len().saturating_sub(1));

        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Add(mut picker) => {
                let items = available_outputs(snap, &eng.source().name, &outs);
                let mut keep_open = true;
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => picker.move_by(-1, &items),
                    KeyCode::Down | KeyCode::Char('j') => picker.move_by(1, &items),
                    KeyCode::Enter => {
                        if let Some(name) = picker.selected(&items).cloned() {
                            match eng.add_output(&name) {
                                Ok(()) => self.selected = outs.len(), // the new last row
                                Err(e) => log(format!("could not add {name}: {e}")),
                            }
                        }
                        keep_open = false;
                    }
                    KeyCode::Esc | KeyCode::Char('a') | KeyCode::Char('q') => keep_open = false,
                    _ => {}
                }
                if keep_open {
                    self.mode = Mode::Add(picker);
                }
            }
            Mode::Input(mut text) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let t = text.trim().to_string();
                    if !t.is_empty() {
                        match eng.add_output(&format!("net:{t}")) {
                            Ok(()) => self.selected = outs.len(),
                            Err(e) => log(format!("could not add {t}: {e}")),
                        }
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                    self.mode = Mode::Input(text);
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) && text.len() < 64 => {
                    text.push(c);
                    self.mode = Mode::Input(text);
                }
                _ => self.mode = Mode::Input(text),
            },
            Mode::Normal => self.normal_key(key.code, eng, &outs),
        }
    }

    fn normal_key<E: EngineLike>(&mut self, code: KeyCode, eng: &mut E, outs: &[OutputView]) {
        let sel = outs.get(self.selected);
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(outs.len().saturating_sub(1))
            }
            KeyCode::Left | KeyCode::Char('h') => {
                sel.map(|o| o.settings.adjust_volume(-5));
            }
            KeyCode::Right | KeyCode::Char('l') => {
                sel.map(|o| o.settings.adjust_volume(5));
            }
            KeyCode::Char('-') => {
                sel.map(|o| o.settings.adjust_volume(-1));
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                sel.map(|o| o.settings.adjust_volume(1));
            }
            KeyCode::Char('[') => {
                sel.map(|o| o.settings.adjust_delay(-10));
            }
            KeyCode::Char(']') => {
                sel.map(|o| o.settings.adjust_delay(10));
            }
            KeyCode::Char('{') => {
                sel.map(|o| o.settings.adjust_delay(-50));
            }
            KeyCode::Char('}') => {
                sel.map(|o| o.settings.adjust_delay(50));
            }
            KeyCode::Char('m') => {
                sel.map(|o| o.settings.toggle_mute());
            }
            KeyCode::Char('f') => eng.set_follow_volume(!eng.follow_volume()),
            KeyCode::Char('a') => self.mode = Mode::Add(Picker::default()),
            KeyCode::Char('n') => self.mode = Mode::Input(String::new()),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(o) = sel {
                    eng.remove_output(o.id);
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

// ---- drawing --------------------------------------------------------------------

pub struct ViewModel {
    pub source: SourceView,
    pub outputs: Vec<OutputView>,
    pub master_gain: f32,
    pub follow: bool,
    pub logs: Vec<String>,
    pub add_items: Vec<String>,
    pub add_labels: Vec<String>,
}

impl ViewModel {
    pub fn build<E: EngineLike>(eng: &E, snap: &Snapshot) -> ViewModel {
        let mut outputs = eng.outputs();
        for o in &mut outputs {
            o.label = item_label(&o.name, snap);
        }
        let source = eng.source();
        let add_items = available_outputs(snap, &source.name, &outputs);
        let add_labels = add_items.iter().map(|k| item_label(k, snap)).collect();
        ViewModel {
            source,
            outputs,
            master_gain: eng.master_gain(),
            follow: eng.follow_volume(),
            logs: log::recent(6),
            add_items,
            add_labels,
        }
    }
}

fn bar(pct: u32, width: usize) -> String {
    let filled = ((pct as usize * width) + 50) / 100;
    format!("{}{}", "█".repeat(filled.min(width)), "░".repeat(width - filled.min(width)))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

pub fn draw(f: &mut Frame, app: &App, vm: &ViewModel) {
    let area = f.area();
    let [header, table, logs, help] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(8),
        Constraint::Length(4),
    ])
    .areas(area);

    // header
    let (dot, color) = if vm.source.connected { ("●", Color::Green) } else { ("○", Color::Red) };
    let sys = if vm.follow {
        format!("system volume: following (x{:.0}%)", vm.master_gain * 100.0)
    } else {
        "system volume: not following".to_string()
    };
    let head = Line::from(vec![
        Span::raw(" Source: "),
        Span::styled(vm.source.name.clone(), Style::new().bold()),
        Span::raw(format!(" ({}) ", vm.source.desc)),
        Span::styled(dot, Style::new().fg(color)),
        Span::raw(format!("    {sys}")),
    ]);
    f.render_widget(Paragraph::new(head).block(Block::bordered().title(" audio-fanout ")), header);

    // outputs
    let block = Block::bordered().title(" Outputs ");
    if vm.outputs.is_empty() {
        f.render_widget(
            Paragraph::new("\n  No outputs yet. Press 'a' to add one.").block(block),
            table,
        );
    } else {
        let rows: Vec<Row> = vm
            .outputs
            .iter()
            .map(|o| {
                let (status, color) = if o.connected {
                    if o.net.as_ref().map_or(true, |n| n.playing) {
                        ("● live", Color::Green)
                    } else {
                        ("◐ no output", Color::Yellow) // linked, but the receiver has no working device
                    }
                } else if o.reconnects > 0 {
                    ("○ lost", Color::Red)
                } else {
                    ("◌ waiting", Color::Yellow)
                };
                let vol = if o.muted {
                    format!("{} MUTED", "░".repeat(10))
                } else {
                    format!("{} {:>3}%", bar(o.volume_pct, 10), o.volume_pct)
                };
                let (buf, drift) = if o.connected {
                    let link = match &o.net {
                        Some(n) => format!("rtt {:.1} ms, {:.1}% loss", n.rtt_ms, n.loss_pct),
                        None => format!("{:+} ppm", o.ratio_ppm),
                    };
                    (format!("{}/{} ms", o.fill_ms, o.target_ms), link)
                } else {
                    ("-".into(), "-".into())
                };
                Row::new(vec![
                    Cell::from(status).style(Style::new().fg(color)),
                    Cell::from(o.label.clone()),
                    Cell::from(vol),
                    Cell::from(format!("+{} ms", o.delay_ms)),
                    Cell::from(buf),
                    Cell::from(drift),
                    Cell::from(o.reconnects.to_string()),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(12), // "◐ no output"
            Constraint::Min(18),
            Constraint::Length(16),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Length(22), // "rtt 2.3 ms, 0.5% loss"
            Constraint::Length(4),
        ];
        let t = Table::new(rows, widths)
            .header(
                Row::new(["Status", "Output", "Volume", "Delay", "Buffer", "Drift / link", "Lost"])
                    .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
            )
            .block(block)
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default().with_selected(Some(app.selected.min(vm.outputs.len() - 1)));
        f.render_stateful_widget(t, table, &mut state);
    }

    // log + help
    let lines: Vec<Line> = vm.logs.iter().map(|l| Line::from(l.as_str())).collect();
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Log ")), logs);
    let help_text = "↑↓ select   ←→ volume ±5   - + volume ±1   [ ] delay ±10 ms   { } delay ±50 ms\n\
                     m mute   a add output   n add network receiver   d remove   f follow system volume   q quit";
    f.render_widget(
        Paragraph::new(help_text).wrap(Wrap { trim: true }).block(Block::bordered().title(" Keys ")),
        help,
    );

    // dropdown
    match &app.mode {
        Mode::Add(p) => draw_picker(
            f,
            area,
            " Add output: ↑↓ choose · Enter add · Esc cancel (list is live) ",
            &vm.add_items,
            &vm.add_labels,
            p,
        ),
        Mode::Input(text) => {
            let rect = centered(area, area.width.saturating_sub(6).min(70), 5);
            f.render_widget(Clear, rect);
            let body = Paragraph::new(format!("\n  {text}_")).block(
                Block::bordered()
                    .title(" Add network receiver: host[:port] · Enter add · Esc cancel ")
                    .border_style(Style::new().fg(Color::Cyan)),
            );
            f.render_widget(body, rect);
        }
        Mode::Normal => {}
    }
}

/// `items` are the device names (used for selection), `labels` what is shown.
pub fn draw_picker(f: &mut Frame, area: Rect, title: &str, items: &[String], labels: &[String], p: &Picker) {
    let height = (labels.len().max(1) as u16) + 2;
    let rect = centered(area, area.width.saturating_sub(6).min(86), height.min(16));
    f.render_widget(Clear, rect);
    let list_items: Vec<ListItem> = if labels.is_empty() {
        vec![ListItem::new("(no other output devices found; plug one in and it will show up here)")]
    } else {
        labels.iter().map(|l| ListItem::new(l.as_str())).collect()
    };
    let list = List::new(list_items)
        .block(Block::bordered().title(title.to_string()).border_style(Style::new().fg(Color::Cyan)))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(if items.is_empty() { None } else { Some(p.index(items)) });
    f.render_stateful_widget(list, rect, &mut state);
}

// ---- run loops ------------------------------------------------------------------

/// Startup screen: choose the source when `--source` was not given.
pub fn pick_source(terminal: &mut DefaultTerminal, watcher: &Watcher) -> Result<Option<String>> {
    let mut picker = Picker::default();
    loop {
        let cands = source_candidates(&watcher.snapshot());
        let names: Vec<String> = cands.iter().map(|c| c.0.clone()).collect();
        let labels: Vec<String> = cands.iter().map(|c| c.1.clone()).collect();
        terminal.draw(|f| {
            let area = f.area();
            let intro = Paragraph::new(
                "Choose the SOURCE: the virtual device your apps play to (e.g. CABLE Output, BlackHole).\n\
                 Set your system's default output to its playback side, then pick it here.",
            )
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(" audio-fanout "));
            f.render_widget(intro, Rect { height: 4.min(area.height), ..area });
            draw_picker(f, area, " Source: ↑↓ choose · Enter select · q quit (list is live) ", &names, &labels, &picker);
        })?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => picker.move_by(-1, &names),
                    KeyCode::Down | KeyCode::Char('j') => picker.move_by(1, &names),
                    KeyCode::Enter => {
                        if let Some(n) = picker.selected(&names) {
                            return Ok(Some(n.clone()));
                        }
                    }
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}

/// Main screen loop. Returns when the user quits.
pub fn run<E: EngineLike>(terminal: &mut DefaultTerminal, eng: &mut E, watcher: &Watcher) -> Result<()> {
    let mut app = App::default();
    while !app.quit {
        let snap = watcher.snapshot();
        let vm = ViewModel::build(eng, &snap);
        terminal.draw(|f| draw(f, &app, &vm))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                app.on_key(k, eng, &snap);
            }
        }
        eng.tick();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    struct Fake {
        outs: Vec<OutputView>,
        next: u64,
        follow: bool,
        removed: Vec<u64>,
        fail_add: bool,
    }

    fn view(id: u64, name: &str) -> OutputView {
        OutputView {
            id,
            name: name.into(),
            label: name.into(),
            net: None,
            connected: true,
            volume_pct: 80,
            muted: false,
            delay_ms: 0,
            fill_ms: 38,
            target_ms: 40,
            ratio_ppm: 12,
            reconnects: 0,
            settings: Arc::new(Settings::new(0, 80)),
        }
    }

    impl Fake {
        fn new(names: &[&str]) -> Fake {
            let outs: Vec<_> = names.iter().enumerate().map(|(i, n)| view(i as u64 + 1, n)).collect();
            Fake { next: outs.len() as u64 + 1, outs, follow: true, removed: vec![], fail_add: false }
        }
        fn sync(&mut self) {
            for o in &mut self.outs {
                o.volume_pct = o.settings.volume_pct.load(Ordering::Relaxed);
                o.delay_ms = o.settings.delay_ms.load(Ordering::Relaxed);
                o.muted = o.settings.muted.load(Ordering::Relaxed);
            }
        }
    }

    impl EngineLike for Fake {
        fn outputs(&self) -> Vec<OutputView> {
            let mut c = self.outs.clone();
            for o in &mut c {
                o.volume_pct = o.settings.volume_pct.load(Ordering::Relaxed);
                o.delay_ms = o.settings.delay_ms.load(Ordering::Relaxed);
                o.muted = o.settings.muted.load(Ordering::Relaxed);
            }
            c
        }
        fn source(&self) -> SourceView {
            SourceView { name: "CABLE Output (VB-Audio Virtual Cable)".into(), desc: "48000 Hz, 2 ch".into(), connected: true }
        }
        fn master_gain(&self) -> f32 {
            0.5
        }
        fn follow_volume(&self) -> bool {
            self.follow
        }
        fn set_follow_volume(&mut self, on: bool) {
            self.follow = on;
        }
        fn add_output(&mut self, device: &str) -> Result<()> {
            if self.fail_add {
                anyhow::bail!("nope");
            }
            self.outs.push(view(self.next, device));
            self.next += 1;
            Ok(())
        }
        fn remove_output(&mut self, id: u64) {
            self.removed.push(id);
            self.outs.retain(|o| o.id != id);
        }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn snap(outputs: &[&str]) -> Snapshot {
        Snapshot {
            inputs: vec!["CABLE Output (VB-Audio Virtual Cable)".into()],
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn screen(vm: &ViewModel, app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app, vm)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn volume_delay_and_mute_keys_hit_the_selected_output() {
        let mut eng = Fake::new(&["Speakers", "Earbuds"]);
        let mut app = App::default();
        let s = snap(&[]);
        app.on_key(key(KeyCode::Down), &mut eng, &s); // select Earbuds
        app.on_key(key(KeyCode::Left), &mut eng, &s); // -5
        app.on_key(key(KeyCode::Char('-')), &mut eng, &s); // -1
        app.on_key(key(KeyCode::Char(']')), &mut eng, &s); // +10 ms
        app.on_key(key(KeyCode::Char('}')), &mut eng, &s); // +50 ms
        app.on_key(key(KeyCode::Char('m')), &mut eng, &s);
        eng.sync();
        assert_eq!((eng.outs[0].volume_pct, eng.outs[0].delay_ms, eng.outs[0].muted), (80, 0, false));
        assert_eq!((eng.outs[1].volume_pct, eng.outs[1].delay_ms, eng.outs[1].muted), (74, 60, true));
        app.on_key(key(KeyCode::Char('[')), &mut eng, &s);
        app.on_key(key(KeyCode::Char('{')), &mut eng, &s);
        eng.sync();
        assert_eq!(eng.outs[1].delay_ms, 0, "delay floors at 0");
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut eng = Fake::new(&["Speakers"]);
        let mut app = App::default();
        let mut k = key(KeyCode::Char('m'));
        k.kind = KeyEventKind::Release;
        app.on_key(k, &mut eng, &snap(&[]));
        eng.sync();
        assert!(!eng.outs[0].muted);
    }

    #[test]
    fn dropdown_adds_the_chosen_device_and_hides_unsafe_or_duplicate_ones() {
        let mut eng = Fake::new(&["Speakers (Realtek(R) Audio)"]);
        let mut app = App::default();
        let s = snap(&[
            "Speakers (Realtek(R) Audio)",                 // already added
            "CABLE Input (VB-Audio Virtual Cable)",        // would feed back into the source
            "Headphones (OnePlus Bullets Wireless Z2)",
            "Headphones (Airdopes Alpha)",
        ]);
        let items = available_outputs(&s, &eng.source().name, &eng.outputs());
        assert_eq!(items, vec!["Headphones (OnePlus Bullets Wireless Z2)", "Headphones (Airdopes Alpha)"]);

        app.on_key(key(KeyCode::Char('a')), &mut eng, &s);
        assert!(matches!(app.mode, Mode::Add(_)));
        app.on_key(key(KeyCode::Down), &mut eng, &s);
        app.on_key(key(KeyCode::Enter), &mut eng, &s);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(eng.outs.len(), 2);
        assert_eq!(eng.outs[1].name, "Headphones (Airdopes Alpha)");
        assert_eq!(app.selected, 1, "selection jumps to the new output");
    }

    #[test]
    fn escape_closes_dropdown_without_adding() {
        let mut eng = Fake::new(&[]);
        let mut app = App::default();
        let s = snap(&["Speakers"]);
        app.on_key(key(KeyCode::Char('a')), &mut eng, &s);
        app.on_key(key(KeyCode::Esc), &mut eng, &s);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(eng.outs.is_empty());
    }

    #[test]
    fn failed_add_leaves_the_ui_usable() {
        let mut eng = Fake::new(&[]);
        eng.fail_add = true;
        let mut app = App::default();
        let s = snap(&["Speakers"]);
        app.on_key(key(KeyCode::Char('a')), &mut eng, &s);
        app.on_key(key(KeyCode::Enter), &mut eng, &s);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(eng.outs.is_empty());
    }

    /// The dropdown is live: when a device vanishes or appears, the highlight must stay
    /// on the same device, not on whatever now occupies that index.
    #[test]
    fn dropdown_selection_follows_device_by_name_when_list_changes() {
        let mut p = Picker::default();
        let a = vec!["A".to_string(), "B".into(), "C".into()];
        p.move_by(1, &a); // B
        assert_eq!(p.selected(&a).unwrap(), "B");
        let after_a_removed = vec!["B".to_string(), "C".into()];
        assert_eq!(p.selected(&after_a_removed).unwrap(), "B");
        let after_new_first = vec!["Z".to_string(), "A".into(), "B".into(), "C".into()];
        assert_eq!(p.selected(&after_new_first).unwrap(), "B");
        // chosen device itself disappears: fall back to a valid neighbour, no panic
        let gone = vec!["A".to_string(), "C".into()];
        assert!(p.selected(&gone).is_some());
        assert!(p.selected(&[]).is_none());
    }

    #[test]
    fn remove_and_follow_toggle() {
        let mut eng = Fake::new(&["One", "Two"]);
        let mut app = App::default();
        let s = snap(&[]);
        app.on_key(key(KeyCode::Down), &mut eng, &s);
        app.on_key(key(KeyCode::Char('d')), &mut eng, &s);
        assert_eq!(eng.removed, vec![2]);
        assert_eq!(app.selected, 0);
        app.on_key(key(KeyCode::Char('f')), &mut eng, &s);
        assert!(!eng.follow);
        app.on_key(key(KeyCode::Char('q')), &mut eng, &s);
        assert!(app.quit);
        // removing from an empty list must not panic
        app.on_key(key(KeyCode::Char('d')), &mut eng, &s);
        app.on_key(key(KeyCode::Char('d')), &mut eng, &s);
    }

    #[test]
    fn draws_outputs_with_live_values() {
        let eng = Fake::new(&["Speakers (Realtek(R) Audio)", "Earbuds"]);
        eng.outs[1].settings.adjust_delay(180);
        eng.outs[0].settings.toggle_mute();
        let s = snap(&["Headphones (New)"]);
        let vm = ViewModel::build(&eng, &s);
        let text = screen(&vm, &App::default(), 110, 28);
        for needle in ["audio-fanout", "CABLE Output", "48000 Hz, 2 ch", "Speakers (Realtek(R) Audio)", "Earbuds",
                       "MUTED", "+180 ms", "38/40 ms", "+12 ppm", "live", "follow system volume"] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn draws_states_and_empty_list_hint() {
        let mut eng = Fake::new(&["Gone", "Waiting"]);
        eng.outs[0].connected = false;
        eng.outs[0].reconnects = 2;
        eng.outs[1].connected = false;
        let vm = ViewModel::build(&eng, &snap(&[]));
        let text = screen(&vm, &App::default(), 100, 28);
        assert!(text.contains("lost") && text.contains("waiting"), "{text}");

        let empty = Fake::new(&[]);
        let vm = ViewModel::build(&empty, &snap(&[]));
        assert!(screen(&vm, &App::default(), 100, 28).contains("Press 'a' to add one"));
    }

    #[test]
    fn draws_dropdown_overlay_and_survives_tiny_terminals() {
        let eng = Fake::new(&["Speakers"]);
        let s = snap(&["Headphones (OnePlus)", "Headphones (Airdopes)"]);
        let vm = ViewModel::build(&eng, &s);
        let app = App { selected: 0, mode: Mode::Add(Picker::default()), quit: false };
        let text = screen(&vm, &app, 100, 28);
        assert!(text.contains("Add output") && text.contains("Headphones (OnePlus)") && text.contains("Headphones (Airdopes)"), "{text}");
        // empty dropdown shows a hint
        let vm2 = ViewModel::build(&eng, &snap(&[]));
        assert!(screen(&vm2, &app, 100, 28).contains("no other output devices"));
        // tiny and zero-ish sizes must not panic
        for (w, h) in [(1, 1), (10, 3), (30, 8), (60, 12)] {
            let _ = screen(&vm, &app, w, h);
            let _ = screen(&vm, &App::default(), w, h);
        }
    }

    fn receiver(name: &str, addr: &str, busy: bool) -> crate::discovery::Receiver {
        crate::discovery::Receiver { name: name.into(), addr: addr.parse().unwrap(), busy }
    }

    #[test]
    fn discovered_receivers_join_the_live_dropdown_and_leave_it() {
        let mut eng = Fake::new(&["Speakers"]);
        let mut s = snap(&["Speakers"]);
        s.receivers = vec![receiver("Kitchen", "192.168.1.5:47800", false), receiver("Den", "192.168.1.9:47800", true)];
        let items = available_outputs(&s, &eng.source().name, &eng.outputs());
        assert_eq!(items, vec!["net:192.168.1.5:47800", "net:192.168.1.9:47800"]);
        assert_eq!(item_label(&items[0], &s), "[net] Kitchen (192.168.1.5)");
        assert_eq!(item_label(&items[1], &s), "[net] Den (192.168.1.9) - busy");

        // choose Kitchen from the dropdown: it is added by its net: key
        let mut app = App::default();
        app.on_key(key(KeyCode::Char('a')), &mut eng, &s);
        app.on_key(key(KeyCode::Enter), &mut eng, &s);
        assert_eq!(eng.outs.last().unwrap().name, "net:192.168.1.5:47800");

        // already added: hidden from the list. And a receiver that stopped announcing is gone.
        let items = available_outputs(&s, &eng.source().name, &eng.outputs());
        assert_eq!(items, vec!["net:192.168.1.9:47800"]);
        s.receivers.clear();
        assert!(available_outputs(&s, &eng.source().name, &eng.outputs()).is_empty());
        // a label for a receiver we no longer hear still renders
        assert_eq!(item_label("net:10.0.0.2:47800", &s), "[net] 10.0.0.2:47800");
    }

    #[test]
    fn n_prompts_for_an_address_and_adds_a_network_output() {
        let mut eng = Fake::new(&[]);
        let mut app = App::default();
        let s = snap(&[]);
        app.on_key(key(KeyCode::Char('n')), &mut eng, &s);
        assert!(matches!(app.mode, Mode::Input(_)));
        for c in "192.168.1.50x".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut eng, &s);
        }
        app.on_key(key(KeyCode::Backspace), &mut eng, &s); // fix the typo
        // hotkeys are text while typing: 'q' must not quit, 'd' must not delete
        app.on_key(key(KeyCode::Char('q')), &mut eng, &s);
        app.on_key(key(KeyCode::Backspace), &mut eng, &s);
        assert!(!app.quit);
        app.on_key(key(KeyCode::Enter), &mut eng, &s);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(eng.outs.len(), 1);
        assert_eq!(eng.outs[0].name, "net:192.168.1.50");

        // Esc cancels; empty input adds nothing
        app.on_key(key(KeyCode::Char('n')), &mut eng, &s);
        app.on_key(key(KeyCode::Char('1')), &mut eng, &s);
        app.on_key(key(KeyCode::Esc), &mut eng, &s);
        app.on_key(key(KeyCode::Char('n')), &mut eng, &s);
        app.on_key(key(KeyCode::Enter), &mut eng, &s);
        assert_eq!(eng.outs.len(), 1);
        // a failing add leaves the UI usable
        eng.fail_add = true;
        app.on_key(key(KeyCode::Char('n')), &mut eng, &s);
        app.on_key(key(KeyCode::Char('x')), &mut eng, &s);
        app.on_key(key(KeyCode::Enter), &mut eng, &s);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn draws_network_rows_with_link_quality() {
        let mut eng = Fake::new(&["Speakers", "net:192.168.1.5:47800", "net:192.168.1.9:47800"]);
        eng.outs[1].net = Some(NetView { rtt_ms: 2.3, loss_pct: 0.5, playing: true });
        eng.outs[2].net = Some(NetView { rtt_ms: 4.0, loss_pct: 0.0, playing: false });
        let mut s = snap(&[]);
        s.receivers = vec![receiver("Kitchen", "192.168.1.5:47800", false)];
        let vm = ViewModel::build(&eng, &s);
        let text = screen(&vm, &App::default(), 130, 30);
        for needle in ["[net] Kitchen (192.168.1.5)", "rtt 2.3 ms, 0.5% loss", "no output", "[net] 192.168.1.9:47800", "Drift / link", "add network receiver"] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn draws_the_address_prompt() {
        let eng = Fake::new(&["Speakers"]);
        let vm = ViewModel::build(&eng, &snap(&[]));
        let app = App { selected: 0, mode: Mode::Input("192.168.1.5".into()), quit: false };
        let text = screen(&vm, &app, 100, 28);
        assert!(text.contains("Add network receiver") && text.contains("192.168.1.5_"), "{text}");
        for (w, h) in [(1, 1), (12, 4), (40, 9)] {
            let _ = screen(&vm, &app, w, h); // tiny terminals must not panic
        }
    }
}
