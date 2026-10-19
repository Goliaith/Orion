//! The full-screen chat. The conversation runs on its own thread
//! ([`agent`]); this thread draws, reads keys, and answers approvals, so
//! the screen stays live while the AI works.

mod agent;
mod input;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use self::agent::{Answer, FromAgent, Status, ToAgent};
use self::input::Input;
use super::store::Turn;
use super::terminal::{clip, provider_label, Saver};
use super::{Mode, Session};
use crate::voice::{Recording, VoiceConfig, VoiceIo};
use crate::{interrupt, Orion};

const LABEL: usize = 8;
const PANEL_WIDTH: u16 = 36;
const CLAUDE: Color = Color::Rgb(217, 119, 87);
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

const KEYS: &[(&str, &str)] = &[
    ("Enter", "send (while the AI works, it waits its turn)"),
    ("Alt-Enter / Ctrl-J", "new line"),
    ("Ctrl-C", "stop the AI · clear the line · twice to quit"),
    ("Shift-Tab", "cycle routing: auto → claude → local"),
    ("Ctrl-T", "talk: listen until you pause (Ctrl-T again finishes, Esc cancels)"),
    ("Ctrl-O", "show or hide reasoning"),
    ("Ctrl-B", "show or hide the machine panel"),
    ("PgUp / PgDn, wheel", "scroll (End follows the conversation again)"),
    ("↑ ↓", "earlier messages"),
    ("Ctrl-L", "redraw"),
    ("Ctrl-D", "quit"),
];

#[derive(Debug, Clone, PartialEq)]
enum ToolState {
    Running,
    Ok(Duration),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Entry {
    User(String),
    Route(String),
    Thinking(String),
    Reply { who: &'static str, text: String },
    Tool { name: String, args: String, state: ToolState },
    /// A tool call from a resumed conversation: its saved summary line.
    PastTool(String),
    Approval { tool: String, preview: String, answer: Option<Answer> },
    Confirm { question: String, answer: Option<bool> },
    Notice(String),
    Error(String),
    Output(String),
    Divider(String),
}

/// What a pending prompt answers to.
enum Prompt {
    Approve(Sender<Answer>),
    Confirm(Sender<bool>),
}

#[derive(Default, Clone)]
struct Gpu {
    name: String,
    util: Option<u64>,
    mem_used: Option<u64>,
    mem_total: Option<u64>,
    temp: Option<u64>,
}

#[derive(Default, Clone)]
struct Machine {
    host: String,
    os: String,
    cpu: Option<f64>,
    cores: Option<u64>,
    mem_total_kb: Option<u64>,
    mem_avail_kb: Option<u64>,
    gpus: Vec<Gpu>,
    /// Display/compute devices without nvidia-smi readings.
    other_gpus: Vec<String>,
}

struct App {
    to_agent: Sender<ToAgent>,
    entries: Vec<Entry>,
    input: Input,
    prompt: Option<Prompt>,
    busy: bool,
    stopping: bool,
    queued: Option<String>,
    status: Status,
    machine: Machine,
    /// Who is answering, for labeling replies.
    who: &'static str,
    show_reasoning: bool,
    show_panel: bool,
    voice: VoiceIo,
    listening: Option<Recording>,
    /// Recent input levels, for the meter.
    levels: Vec<f32>,
    /// A transcript being produced off-thread.
    transcribing: Option<Receiver<Result<String>>>,
    /// First visible line while scrolled up; `None` follows the bottom.
    top: Option<usize>,
    /// Lines of the conversation and its visible height at the last draw.
    last_total: usize,
    last_height: usize,
    flash: Option<(String, Instant)>,
    last_ctrl_c: Option<Instant>,
    started: Instant,
    quit: bool,
}

impl App {
    fn new(to_agent: Sender<ToAgent>, show_reasoning: bool, voice: &VoiceConfig) -> Self {
        Self {
            to_agent,
            entries: Vec::new(),
            input: Input::default(),
            prompt: None,
            busy: true, // until the agent reports in
            stopping: false,
            queued: None,
            status: Status::default(),
            machine: Machine::default(),
            who: "claude",
            show_reasoning,
            show_panel: true,
            voice: VoiceIo::new(voice),
            listening: None,
            levels: Vec::new(),
            transcribing: None,
            top: None,
            last_total: 0,
            last_height: 0,
            flash: None,
            last_ctrl_c: None,
            started: Instant::now(),
            quit: false,
        }
    }

    fn send(&self, msg: ToAgent) {
        let _ = self.to_agent.send(msg);
    }

    fn flash(&mut self, text: &str) {
        self.flash = Some((text.to_string(), Instant::now()));
    }

    fn answer_approval(&mut self, answer: Answer) {
        if let Some(Prompt::Approve(reply)) = self.prompt.take() {
            let _ = reply.send(answer);
        }
        if let Some(Entry::Approval { answer: a @ None, .. }) =
            self.entries.iter_mut().rev().find(|e| matches!(e, Entry::Approval { answer: None, .. }))
        {
            *a = Some(answer);
        }
    }

    fn answer_confirm(&mut self, yes: bool) {
        if let Some(Prompt::Confirm(reply)) = self.prompt.take() {
            let _ = reply.send(yes);
        }
        if let Some(Entry::Confirm { answer: a @ None, .. }) =
            self.entries.iter_mut().rev().find(|e| matches!(e, Entry::Confirm { answer: None, .. }))
        {
            *a = Some(yes);
        }
    }

    fn apply(&mut self, ev: FromAgent) {
        match ev {
            FromAgent::Text(t) => {
                self.voice.reply_text(&t);
                match self.entries.last_mut() {
                    Some(Entry::Reply { text, .. }) => text.push_str(&t),
                    _ => {
                        let t = t.trim_start().to_string();
                        if !t.is_empty() {
                            self.entries.push(Entry::Reply { who: self.who, text: t });
                        }
                    }
                }
            }
            FromAgent::Thinking(t) => match self.entries.last_mut() {
                Some(Entry::Thinking(text)) => text.push_str(&t),
                _ => self.entries.push(Entry::Thinking(t)),
            },
            FromAgent::Route { to, model, reason } => {
                self.who = if to == "Claude" { "claude" } else { "local" };
                let why = if reason.is_empty() { String::new() } else { format!(" · {reason}") };
                self.entries.push(Entry::Route(format!("→ {to} ({model}){why}")));
            }
            FromAgent::ToolStart { name, args } => {
                self.entries.push(Entry::Tool { name, args, state: ToolState::Running })
            }
            FromAgent::ToolEnd { ok, detail, took } => {
                if let Some(Entry::Tool { state, .. }) =
                    self.entries.iter_mut().rev().find(|e| matches!(e, Entry::Tool { state: ToolState::Running, .. }))
                {
                    *state = if ok { ToolState::Ok(took) } else { ToolState::Failed(detail) };
                }
            }
            FromAgent::Notice(t) => self.entries.push(Entry::Notice(t)),
            FromAgent::Error(t) => self.entries.push(Entry::Error(t)),
            FromAgent::Output(t) => self.entries.push(Entry::Output(t)),
            FromAgent::Approve { tool, preview, reply } => {
                self.entries.push(Entry::Approval { tool, preview, answer: None });
                self.prompt = Some(Prompt::Approve(reply));
                self.top = None;
            }
            FromAgent::Confirm { question, reply } => {
                self.entries.push(Entry::Confirm { question, answer: None });
                self.prompt = Some(Prompt::Confirm(reply));
                self.top = None;
            }
            FromAgent::Transcript(turns) => {
                self.entries = transcript_entries(&turns);
                self.top = None;
            }
            FromAgent::Status(s) => {
                match s.mode {
                    Mode::Claude => self.who = "claude",
                    Mode::Local => self.who = "local",
                    Mode::Auto => {}
                }
                self.status = s;
            }
            FromAgent::Done => {
                self.voice.reply_done();
                self.busy = false;
                self.stopping = false;
                if let Some(text) = self.queued.take() {
                    self.submit(text);
                }
            }
            FromAgent::Quit => self.quit = true,
        }
    }

    /// Send a message or command; `/keys`, `/exit` and friends are handled here.
    fn submit(&mut self, text: String) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(cmd) = trimmed.strip_prefix('/') {
            match cmd.split_whitespace().next().unwrap_or_default() {
                "exit" | "quit" | "q" => {
                    self.quit = true;
                    return;
                }
                "voice" => {
                    let arg = cmd.split_once(' ').map(|(_, a)| a.trim()).unwrap_or("");
                    let out = self.voice.command(arg);
                    self.entries.push(Entry::Output(format!("/{cmd}\n{out}")));
                    self.top = None;
                    return;
                }
                "keys" => {
                    let keys = KEYS.iter().map(|(k, v)| format!("  {k:<20} {v}")).collect::<Vec<_>>().join("\n");
                    self.entries.push(Entry::Output(keys));
                    return;
                }
                _ => {}
            }
            self.entries.push(Entry::Output(format!("/{cmd}")));
            self.busy = true;
            self.send(ToAgent::Command(cmd.to_string()));
        } else {
            self.entries.push(Entry::User(trimmed.to_string()));
            self.voice.hush();
            self.busy = true;
            self.send(ToAgent::Turn(trimmed.to_string()));
        }
        self.top = None;
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // An open prompt takes the keys it needs.
        match &self.prompt {
            Some(Prompt::Approve(_)) => {
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        self.answer_approval(Answer::No);
                        self.stop();
                    }
                    KeyCode::Char('y' | 'Y') => self.answer_approval(Answer::Yes),
                    KeyCode::Char('a' | 'A') => self.answer_approval(Answer::Always),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => self.answer_approval(Answer::No),
                    _ => self.flash("y to approve, n to decline, a to always allow"),
                }
                return;
            }
            Some(Prompt::Confirm(_)) => {
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        self.answer_confirm(false);
                        self.stop();
                    }
                    KeyCode::Char('y' | 'Y') => self.answer_confirm(true),
                    KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Enter => self.answer_confirm(false),
                    _ => self.flash("y or n"),
                }
                return;
            }
            None => {}
        }

        // While listening, keys either finish or cancel.
        if self.listening.is_some() {
            match key.code {
                KeyCode::Char('t') if ctrl => self.finish_listening(),
                KeyCode::Enter => self.finish_listening(),
                KeyCode::Esc => self.cancel_listening("cancelled"),
                KeyCode::Char('c') if ctrl => self.cancel_listening("cancelled"),
                _ => self.flash("listening: pause or Ctrl-T to finish, Esc to cancel"),
            }
            return;
        }

        match key.code {
            KeyCode::Char('t') if ctrl => self.start_listening(),
            KeyCode::Char('c') if ctrl => {
                if self.voice.hush() {
                    self.flash("stopped speaking");
                    if !self.busy {
                        return;
                    }
                }
                if self.busy {
                    self.stop();
                } else if !self.input.is_empty() {
                    self.input.clear();
                } else if self.last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    self.quit = true;
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.flash("Ctrl-C again to quit");
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() && !self.busy => self.quit = true,
            KeyCode::Char('o') if ctrl => {
                self.show_reasoning = !self.show_reasoning;
                self.flash(if self.show_reasoning { "reasoning shown" } else { "reasoning hidden" });
            }
            KeyCode::Char('b') if ctrl => self.show_panel = !self.show_panel,
            KeyCode::BackTab => {
                if self.busy {
                    self.flash("routing can change once the AI is done");
                } else {
                    self.busy = true;
                    self.send(ToAgent::CycleMode);
                }
            }
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => self.input.insert('\n'),
            KeyCode::Char('j') if ctrl => self.input.insert('\n'),
            KeyCode::Enter => {
                if self.busy {
                    if !self.input.is_empty() {
                        self.queued = Some(self.input.submit());
                        self.flash("queued: sends when the AI is done");
                    }
                } else {
                    let text = self.input.submit();
                    self.submit(text);
                }
            }
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Char('k') if ctrl => self.input.kill_to_end(),
            KeyCode::Char(c) if !ctrl => self.input.insert(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End if self.input.is_empty() || ctrl => self.top = None,
            KeyCode::End => self.input.end(),
            KeyCode::Up if !self.input.is_multiline() => self.input.history_prev(),
            KeyCode::Down if !self.input.is_multiline() => self.input.history_next(),
            KeyCode::PageUp => self.scroll_up(self.last_height.saturating_sub(2).max(1)),
            KeyCode::PageDown => self.scroll_down(self.last_height.saturating_sub(2).max(1)),
            _ => {}
        }
    }

    fn start_listening(&mut self) {
        if self.transcribing.is_some() {
            return self.flash("still transcribing the last recording");
        }
        self.voice.hush();
        let recorder = match &self.voice.recorder {
            Ok(r) => r,
            Err(e) => {
                let e = e.clone();
                self.entries.push(Entry::Notice(format!("voice: {e}")));
                return;
            }
        };
        if let Err(e) = &self.voice.transcriber {
            let e = e.clone();
            self.entries.push(Entry::Notice(format!("voice: can't transcribe: {e}")));
            return;
        }
        match recorder.start() {
            Ok(rec) => {
                self.levels.clear();
                self.listening = Some(rec);
            }
            Err(e) => self.entries.push(Entry::Notice(format!("voice: {e:#}"))),
        }
    }

    /// Stop recording and transcribe on another thread.
    fn finish_listening(&mut self) {
        let Some(rec) = self.listening.take() else { return };
        let Ok(transcriber) = self.voice.transcriber.clone() else { return };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(rec.finish().and_then(|wav| transcriber.transcribe(&wav)));
        });
        self.transcribing = Some(rx);
    }

    fn cancel_listening(&mut self, why: &str) {
        if let Some(rec) = self.listening.take() {
            rec.cancel();
            self.flash(&format!("listening {why}"));
        }
    }

    /// Called every frame: the meter, the pause detector, the transcript.
    fn tick_voice(&mut self) -> bool {
        let mut changed = false;
        if let Some(rec) = &self.listening {
            self.levels.push(rec.level());
            if self.levels.len() > 24 {
                self.levels.remove(0);
            }
            changed = true;
            if rec.should_stop() {
                self.finish_listening();
            }
        }
        if let Some(rx) = &self.transcribing {
            if let Ok(result) = rx.try_recv() {
                self.transcribing = None;
                changed = true;
                match result {
                    Ok(text) => {
                        if !self.input.is_empty() {
                            self.input.insert(' ');
                        }
                        self.input.insert_str(&text);
                        if self.voice.config.send_after_transcribe && !self.busy {
                            let text = self.input.submit();
                            self.submit(text);
                        }
                    }
                    Err(e) => self.entries.push(Entry::Notice(format!("voice: {e:#}"))),
                }
            }
        }
        changed
    }

    fn stop(&mut self) {
        if self.busy && !self.stopping {
            interrupt::request();
            self.stopping = true;
            self.queued = None;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let bottom = self.last_total.saturating_sub(self.last_height);
        self.top = Some(self.top.unwrap_or(bottom).saturating_sub(n));
    }

    fn scroll_down(&mut self, n: usize) {
        let bottom = self.last_total.saturating_sub(self.last_height);
        self.top = match self.top {
            Some(t) if t + n < bottom => Some(t + n),
            _ => None,
        };
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let input_width = area.width.saturating_sub(4) as usize;
        let (rows, cursor) = self.input.layout(input_width);
        let input_height = (rows.len().clamp(1, 8) + 2) as u16;
        let [main, input_area, status_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(input_height), Constraint::Length(1)]).areas(area);
        let panel = self.show_panel && main.width >= 90;
        let [convo_area, panel_area] = if panel {
            Layout::horizontal([Constraint::Min(30), Constraint::Length(PANEL_WIDTH)]).areas(main)
        } else {
            [main, Rect::default()]
        };

        self.render_conversation(f, convo_area);
        if panel {
            let lines = panel_lines(&self.machine, &self.status, PANEL_WIDTH.saturating_sub(4) as usize);
            let block = Block::bordered().title(" Machine ").border_style(Style::new().fg(Color::DarkGray));
            f.render_widget(Paragraph::new(lines).block(block), panel_area);
        }
        self.render_input(f, input_area, &rows, cursor);
        f.render_widget(Paragraph::new(self.status_line()), status_area);
    }

    fn render_conversation(&mut self, f: &mut Frame, area: Rect) {
        let title = if self.status.title.is_empty() {
            " Orion ".to_string()
        } else {
            format!(" Orion · {} ", clip(&self.status.title, area.width.saturating_sub(14) as usize))
        };
        let block = Block::bordered().title(title).border_style(Style::new().fg(Color::DarkGray));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let mut lines = conversation_lines(&self.entries, inner.width as usize, self.show_reasoning);
        if self.entries.is_empty() {
            lines = welcome_lines();
        }
        let height = inner.height as usize;
        let total = lines.len();
        self.last_total = total;
        self.last_height = height;
        let bottom = total.saturating_sub(height);
        let top = self.top.map_or(bottom, |t| t.min(bottom));
        if self.top.is_some_and(|t| t >= bottom) {
            self.top = None;
        }
        let visible: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
        f.render_widget(Paragraph::new(visible), inner);
        if top < bottom {
            let more = format!(" ↓ {} more · End ", bottom - top);
            let w = more.width() as u16;
            let r = Rect { x: inner.right().saturating_sub(w), y: inner.bottom().saturating_sub(1), width: w, height: 1 };
            f.render_widget(Paragraph::new(more).style(Style::new().fg(Color::Black).bg(Color::Yellow)), r);
        }
    }

    fn render_input(&self, f: &mut Frame, area: Rect, rows: &[String], cursor: (usize, usize)) {
        let meter: String = self
            .levels
            .iter()
            .map(|l| ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][((l * 7.0).round() as usize).min(7)])
            .collect();
        let (title, color) = match &self.prompt {
            None if self.listening.is_some() => {
                let secs = self.listening.as_ref().map_or(0, |r| r.elapsed().as_secs());
                (format!(" ● listening {meter} {secs}s · pause or Ctrl-T to finish · Esc cancels "), Color::Red)
            }
            None if self.transcribing.is_some() => (" transcribing… ".to_string(), Color::Red),
            Some(Prompt::Approve(_)) => (" Approve?  y yes · n no · a always this session ".to_string(), Color::Yellow),
            Some(Prompt::Confirm(_)) => (" y yes · n no ".to_string(), Color::Yellow),
            None if self.stopping => (" stopping… ".to_string(), Color::Yellow),
            None if self.busy && self.queued.is_some() => (" queued · sends when the AI is done ".to_string(), Color::Magenta),
            None if self.busy => (" working · Ctrl-C stops · you can type ahead ".to_string(), Color::Magenta),
            None => (" message · Enter sends · Alt-Enter new line ".to_string(), Color::Magenta),
        };
        let block = Block::bordered().title(title).border_style(Style::new().fg(color));
        let inner = block.inner(area);
        let visible = inner.height as usize;
        let skip = (cursor.0 + 1).saturating_sub(visible);
        let text: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .map(|(i, r)| {
                let prefix = if i == 0 { "› " } else { "  " };
                Line::from(vec![Span::styled(prefix, Style::new().fg(Color::Magenta)), Span::raw(r.clone())])
            })
            .collect();
        f.render_widget(Paragraph::new(text).block(block), area);
        if self.prompt.is_none() {
            f.set_cursor_position((inner.x + 2 + cursor.1 as u16, inner.y + (cursor.0 - skip) as u16));
        }
    }

    fn status_line(&self) -> Line<'static> {
        let (label, color) = match self.status.mode {
            Mode::Auto => (" AUTO ", Color::Magenta),
            Mode::Claude => (" CLAUDE ", CLAUDE),
            Mode::Local => (" LOCAL ", Color::Green),
        };
        let mut spans = vec![Span::styled(label, Style::new().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD))];
        if self.voice.speaker.as_ref().is_ok_and(|s| s.is_speaking()) {
            spans.push(Span::styled(" ♪ speaking · Ctrl-C hushes ", Style::new().fg(Color::Cyan)));
        }
        if self.busy {
            let frame = SPINNER[(self.started.elapsed().as_millis() / 100) as usize % SPINNER.len()];
            spans.push(Span::styled(format!(" {frame} working "), Style::new().fg(Color::Magenta)));
        }
        if let Some((text, at)) = &self.flash {
            if at.elapsed() < Duration::from_secs(3) {
                spans.push(Span::styled(format!(" {text} "), Style::new().fg(Color::Yellow)));
            }
        }
        spans.push(Span::styled(
            "  Ctrl-T talk · Shift-Tab routing · Ctrl-O reasoning · Ctrl-B panel · PgUp/PgDn · /keys",
            Style::new().fg(Color::DarkGray),
        ));
        Line::from(spans)
    }
}

fn welcome_lines() -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    vec![
        Line::raw(""),
        Line::styled("  Ask Orion anything about this machine, or to do something on it.", Style::new()),
        Line::raw(""),
        Line::styled("  Risky actions stop and ask you first. /help lists commands, /keys the hotkeys,", dim),
        Line::styled("  and /history your saved conversations.", dim),
    ]
}

fn transcript_entries(turns: &[Turn]) -> Vec<Entry> {
    let mut out = Vec::new();
    if turns.is_empty() {
        return out;
    }
    out.push(Entry::Divider(format!("resumed · {} earlier message{}", turns.len(), if turns.len() == 1 { "" } else { "s" })));
    for t in turns {
        out.push(Entry::User(t.user.clone()));
        out.extend(t.tools.iter().map(|tool| Entry::PastTool(tool.clone())));
        if !t.reply.trim().is_empty() {
            out.push(Entry::Reply { who: provider_label(t.provider), text: t.reply.trim().to_string() });
        }
    }
    out.push(Entry::Divider("now".into()));
    out
}

/// Word-wrap `text` to `width` columns; long words are broken.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut w = 0;
        for word in line.split_inclusive(' ') {
            let ww = word.trim_end().width();
            if w > 0 && w + ww > width {
                rows.push(row.trim_end().to_string());
                row.clear();
                w = 0;
            }
            if ww > width {
                for c in word.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if w + cw > width {
                        rows.push(std::mem::take(&mut row));
                        w = 0;
                    }
                    row.push(c);
                    w += cw;
                }
            } else {
                row.push_str(word);
                w += word.width();
            }
        }
        rows.push(row.trim_end().to_string());
    }
    rows
}

/// Push `text` wrapped under a left label column.
fn push_block(lines: &mut Vec<Line<'static>>, label: Option<Span<'static>>, text: &str, style: Style, width: usize) {
    let body = width.saturating_sub(LABEL).max(10);
    for (i, row) in wrap(text, body).into_iter().enumerate() {
        let lead = match (&label, i) {
            (Some(l), 0) => {
                let pad = LABEL.saturating_sub(l.content.width());
                vec![l.clone(), Span::raw(" ".repeat(pad))]
            }
            _ => vec![Span::raw(" ".repeat(LABEL))],
        };
        let mut spans = lead;
        spans.push(Span::styled(row, style));
        lines.push(Line::from(spans));
    }
}

fn conversation_lines(entries: &[Entry], width: usize, show_reasoning: bool) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let mut lines = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        match e {
            Entry::User(t) => {
                if i > 0 {
                    lines.push(Line::raw(""));
                }
                let label = Span::styled("you", Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD));
                push_block(&mut lines, Some(label), t, Style::new().add_modifier(Modifier::BOLD), width);
            }
            Entry::Route(t) => push_block(&mut lines, None, t, dim, width),
            Entry::Thinking(t) if show_reasoning => {
                push_block(&mut lines, None, t.trim(), dim.add_modifier(Modifier::ITALIC), width)
            }
            Entry::Thinking(_) => {
                if !matches!(entries.get(i.wrapping_sub(1)), Some(Entry::Thinking(_))) {
                    push_block(&mut lines, None, "… reasoning hidden (Ctrl-O shows it)", dim, width);
                }
            }
            Entry::Reply { who, text } => {
                let color = if *who == "claude" { CLAUDE } else { Color::Green };
                let mut label = Some(Span::styled(*who, Style::new().fg(color).add_modifier(Modifier::BOLD)));
                let mut code = false;
                for src in text.trim_end().split('\n') {
                    let fence = src.trim_start().starts_with("```");
                    let style = if fence {
                        dim
                    } else if code {
                        Style::new().fg(Color::Cyan)
                    } else {
                        Style::new()
                    };
                    push_block(&mut lines, label.take(), src, style, width);
                    if fence {
                        code = !code;
                    }
                }
            }
            Entry::Tool { name, args, state } => {
                let mut spans = vec![
                    Span::raw(" ".repeat(LABEL)),
                    Span::styled("⚙ ", Style::new().fg(Color::Cyan)),
                    Span::raw(name.clone()),
                    Span::styled(format!(" {}", clip(args, width.saturating_sub(LABEL + name.width() + 16).max(10))), dim),
                ];
                match state {
                    ToolState::Running => spans.push(Span::styled("  …", Style::new().fg(Color::Yellow))),
                    ToolState::Ok(took) => {
                        spans.push(Span::styled(format!("  ✓ {}ms", took.as_millis()), Style::new().fg(Color::Green)))
                    }
                    ToolState::Failed(_) => spans.push(Span::styled("  ✗", Style::new().fg(Color::Red))),
                }
                lines.push(Line::from(spans));
                if let ToolState::Failed(why) = state {
                    push_block(&mut lines, None, &format!("  {why}"), Style::new().fg(Color::Red), width);
                }
            }
            Entry::PastTool(t) => push_block(&mut lines, None, &format!("⚙ {}", clip(t, 200)), dim, width),
            Entry::Approval { tool, preview, answer } => {
                let y = Style::new().fg(Color::Yellow);
                push_block(&mut lines, None, &format!("┌─ Approve {tool}?"), y.add_modifier(Modifier::BOLD), width);
                for l in preview.lines().take(14) {
                    push_block(&mut lines, None, &format!("│ {l}"), dim, width);
                }
                let end = match answer {
                    None => "└─ y yes · n no · a always (this session)".to_string(),
                    Some(Answer::Yes) => "└─ approved".into(),
                    Some(Answer::Always) => format!("└─ approved, and {tool} won't ask again this session"),
                    Some(Answer::No) => "└─ declined".into(),
                };
                push_block(&mut lines, None, &end, y, width);
            }
            Entry::Confirm { question, answer } => {
                let y = Style::new().fg(Color::Yellow);
                push_block(&mut lines, None, &format!("? {question}"), y.add_modifier(Modifier::BOLD), width);
                let end = match answer {
                    None => "  y yes · n no",
                    Some(true) => "  yes",
                    Some(false) => "  no",
                };
                push_block(&mut lines, None, end, y, width);
            }
            Entry::Notice(t) => push_block(&mut lines, None, t, Style::new().fg(Color::Yellow), width),
            Entry::Error(t) => push_block(&mut lines, None, &format!("error: {t}"), Style::new().fg(Color::Red), width),
            Entry::Output(t) => {
                for row in t.split('\n').flat_map(|l| wrap(l, width.max(1))) {
                    lines.push(Line::raw(row));
                }
            }
            Entry::Divider(t) => {
                let side = width.saturating_sub(t.width() + 2) / 2;
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("{} {t} {}", "─".repeat(side), "─".repeat(side)), dim));
            }
        }
    }
    lines
}

/// Cut `s` to `width` columns, keeping its spacing (unlike `clip`).
fn fit(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 1 > width {
            break;
        }
        out.push(c);
    }
    out + "…"
}

fn bar(frac: f64, width: usize) -> String {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn gauge_color(frac: f64) -> Color {
    if frac > 0.9 {
        Color::Red
    } else if frac > 0.7 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn human_k(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1000..=999_999 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn panel_lines(m: &Machine, s: &Status, width: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let head = Style::new().add_modifier(Modifier::BOLD);
    let gauge = |label: &str, frac: f64, text: String| {
        Line::from(vec![
            Span::styled(format!(" {label:<5}"), dim),
            Span::styled(bar(frac, 10), Style::new().fg(gauge_color(frac))),
            Span::raw(format!(" {text}")),
        ])
    };
    let mut l = vec![Line::styled(format!(" {}", clip(&m.host, width)), head)];
    if !m.os.is_empty() {
        l.push(Line::styled(format!(" {}", clip(&m.os, width)), dim));
    }
    match m.cpu {
        Some(c) => l.push(gauge("CPU", c, format!("{:>3.0}%{}", c * 100.0, m.cores.map(|n| format!(" ·{n}c")).unwrap_or_default()))),
        None => l.push(Line::styled(" CPU  measuring…", dim)),
    }
    if let (Some(total), Some(avail)) = (m.mem_total_kb, m.mem_avail_kb) {
        let used = total.saturating_sub(avail);
        let gb = |kb: u64| kb as f64 / 1_048_576.0;
        l.push(gauge("RAM", used as f64 / total.max(1) as f64, format!("{:.1}/{:.0}G", gb(used), gb(total))));
    }
    for (i, g) in m.gpus.iter().enumerate() {
        l.push(Line::styled(format!(" GPU{i} {}", clip(&g.name.replace("NVIDIA ", "").replace("GeForce ", ""), width.saturating_sub(5))), Style::new()));
        if let Some(u) = g.util {
            l.push(gauge(" use", u as f64 / 100.0, format!("{u:>3}%{}", g.temp.map(|t| format!(" {t}°C")).unwrap_or_default())));
        }
        if let (Some(used), Some(total)) = (g.mem_used, g.mem_total) {
            l.push(gauge(" mem", used as f64 / total.max(1) as f64, format!("{:.1}/{:.0}G", used as f64 / 1024.0, total as f64 / 1024.0)));
        }
    }
    for g in &m.other_gpus {
        l.push(Line::styled(format!(" GPU  {g}"), dim));
    }
    if m.gpus.is_empty() && m.other_gpus.is_empty() {
        l.push(Line::styled(" GPU  none found", dim));
    }

    l.push(Line::raw(""));
    l.push(Line::styled(" Routing", head));
    for row in wrap(&s.routing.replace("routing: ", ""), width) {
        l.push(Line::styled(format!(" {row}"), dim));
    }
    for p in &s.providers {
        let color = if p.starts_with("claude") { CLAUDE } else { Color::Green };
        let (name, rest) = p.split_once(char::is_whitespace).unwrap_or((p, ""));
        let rest = clip(rest.trim(), width.saturating_sub(8));
        let style = if rest.starts_with("unavailable") { Style::new().fg(Color::Red) } else { Style::new() };
        l.push(Line::from(vec![Span::styled(format!(" {name:<7}"), Style::new().fg(color)), Span::styled(rest, style)]));
    }

    l.push(Line::raw(""));
    l.push(Line::styled(" Session", head));
    let turns = format!(" {} message{}", s.turns, if s.turns == 1 { "" } else { "s" });
    l.push(Line::styled(turns, dim));
    for (label, u) in &s.usage {
        let mut t = format!(" {label:<7}in {} · out {}", human_k(u.input_tokens), human_k(u.output_tokens));
        if u.cache_read_input_tokens > 0 {
            t += &format!(" · cached {}", human_k(u.cache_read_input_tokens));
        }
        l.push(Line::styled(fit(&t, width + 1), dim));
    }
    l.push(Line::styled(format!(" tools  {} automatic · {} ask", s.tools_auto, s.tools_ask), dim));
    l
}

/// CPU busy fraction between two /proc/stat samples.
fn cpu_sample() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let nums: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    let idle = nums.get(3)? + nums.get(4).copied().unwrap_or(0);
    Some((nums.iter().sum(), idle))
}

fn read_machine(prev_cpu: &mut Option<(u64, u64)>, with_gpu: bool, last: &Machine) -> Machine {
    let info = crate::tools::machine_info();
    let now = cpu_sample();
    let cpu = match (*prev_cpu, now) {
        (Some((t0, i0)), Some((t1, i1))) if t1 > t0 => Some(1.0 - (i1 - i0) as f64 / (t1 - t0) as f64),
        _ => None,
    };
    *prev_cpu = now;
    let mut m = Machine {
        host: info["hostname"].as_str().unwrap_or("this machine").to_string(),
        os: info["os"].as_str().unwrap_or_default().to_string(),
        cpu,
        cores: info["cpu"]["logical_cores"].as_u64(),
        mem_total_kb: info["memory_kb"]["total"].as_u64(),
        mem_avail_kb: info["memory_kb"]["available"].as_u64(),
        gpus: last.gpus.clone(),
        other_gpus: last.other_gpus.clone(),
    };
    if with_gpu {
        let g = crate::tools::gpu_status();
        m.gpus = g["nvidia"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| Gpu {
                        name: x["name"].as_str().unwrap_or("GPU").to_string(),
                        util: x["utilization_pct"].as_u64(),
                        mem_used: x["memory_used_mib"].as_u64(),
                        mem_total: x["memory_total_mib"].as_u64(),
                        temp: x["temperature_c"].as_u64(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        m.other_gpus = if m.gpus.is_empty() {
            g["devices"]
                .as_array()
                .map(|a| a.iter().filter_map(|d| d["vendor"].as_str()).filter(|v| *v != "unknown").map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    }
    m
}

/// Sample the machine every two seconds (GPUs every four) until `stop`.
fn watch_machine(tx: Sender<Machine>, stop: &AtomicBool) {
    let mut prev = cpu_sample();
    let mut last = Machine::default();
    let mut tick = 0u64;
    while !stop.load(Ordering::Relaxed) {
        last = read_machine(&mut prev, tick.is_multiple_of(2), &last);
        if tx.send(last.clone()).is_err() {
            return;
        }
        tick += 1;
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Run the full-screen chat until the user quits.
pub fn run(orion: &Orion, session: Session, saver: Saver) -> Result<()> {
    let (to_agent, agent_rx) = mpsc::channel();
    let (agent_tx, from_agent) = mpsc::channel();
    let (machine_tx, machine_rx) = mpsc::channel();
    let stop = AtomicBool::new(false);
    let show_reasoning = orion.policy().chat.show_reasoning;
    std::thread::scope(|scope| {
        scope.spawn(move || agent::run(orion, session, saver, agent_rx, agent_tx));
        scope.spawn(|| watch_machine(machine_tx, &stop));
        let app = App::new(to_agent, show_reasoning, &orion.policy().voice);
        let result = screen(app, from_agent, machine_rx);
        stop.store(true, Ordering::Relaxed);
        // A turn still running stops promptly; dropping the App hung up on the agent.
        interrupt::request();
        result
    })
}

fn screen(mut app: App, from_agent: Receiver<FromAgent>, machine: Receiver<Machine>) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    let result = (|| -> Result<()> {
        let mut dirty = true;
        let mut last_tick = Instant::now();
        while !app.quit {
            if dirty {
                terminal.draw(|f| app.render(f))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(30))? {
                match event::read()? {
                    Event::Key(k) if k.code == KeyCode::Char('l') && k.modifiers.contains(KeyModifiers::CONTROL) => {
                        terminal.clear()?
                    }
                    Event::Key(k) => app.on_key(k),
                    Event::Paste(text) if app.prompt.is_none() => app.input.insert_str(&text),
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => app.scroll_up(3),
                        MouseEventKind::ScrollDown => app.scroll_down(3),
                        _ => continue,
                    },
                    _ => {}
                }
                dirty = true;
            }
            while let Ok(ev) = from_agent.try_recv() {
                app.apply(ev);
                dirty = true;
            }
            while let Ok(m) = machine.try_recv() {
                app.machine = m;
                dirty = true;
            }
            // Every 100 ms: the meter and pause detector, the spinner, flashes.
            if last_tick.elapsed() >= Duration::from_millis(100) {
                last_tick = Instant::now();
                let speaking = app.voice.speaker.as_ref().is_ok_and(|s| s.is_speaking());
                if app.tick_voice() || app.busy || app.flash.is_some() || speaking {
                    dirty = true;
                }
            }
        }
        Ok(())
    })();
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::super::Provider;
    use ratatui::Terminal;

    use super::*;

    fn app() -> (App, Receiver<ToAgent>) {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(tx, true, &VoiceConfig::default());
        app.apply(FromAgent::Done);
        (app, rx)
    }

    fn screen_text(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.render(f)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn wraps_words_and_long_runs() {
        assert_eq!(wrap("the quick brown fox", 9), ["the quick", "brown fox"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
    }

    #[test]
    fn a_turn_on_screen() {
        let (mut app, rx) = app();
        for c in "how full is my disk?".chars() {
            app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(rx.try_recv().unwrap(), ToAgent::Turn(t) if t == "how full is my disk?"));
        assert!(app.busy);

        app.apply(FromAgent::Route { to: "the local model".into(), model: "qwen3:8b".into(), reason: "short request".into() });
        app.apply(FromAgent::Thinking("Run df.".into()));
        app.apply(FromAgent::ToolStart { name: "proc_run".into(), args: r#"{"command":"df"}"#.into() });
        let (reply, answers) = mpsc::channel();
        app.apply(FromAgent::Approve { tool: "proc_run".into(), preview: "{\n  \"command\": \"df\"\n}".into(), reply });
        let shown = screen_text(&mut app, 110, 30);
        assert!(shown.contains("Approve proc_run?"), "{shown}");
        assert!(shown.contains("y yes · n no"), "{shown}");

        app.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(answers.try_recv().unwrap(), Answer::Yes);
        app.apply(FromAgent::ToolEnd { ok: true, detail: String::new(), took: Duration::from_millis(11) });
        app.apply(FromAgent::Text("Your disk is ".into()));
        app.apply(FromAgent::Text("28% full.".into()));
        app.apply(FromAgent::Done);

        let shown = screen_text(&mut app, 110, 30);
        for want in ["you", "how full is my disk?", "→ the local model (qwen3:8b) · short request", "Run df.", "⚙ proc_run", "✓ 11ms", "└─ approved", "local", "Your disk is 28% full.", "Machine", " AUTO "] {
            assert!(shown.contains(want), "missing {want:?} in:\n{shown}");
        }

        // Ctrl-O hides reasoning.
        app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let shown = screen_text(&mut app, 110, 30);
        assert!(!shown.contains("Run df.") && shown.contains("reasoning hidden"), "{shown}");
    }

    #[test]
    fn typing_ahead_queues_and_ctrl_c_stops() {
        let (mut app, rx) = app();
        app.submit("first".into());
        let _ = rx.try_recv();
        app.input.insert_str("second");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rx.try_recv().is_err(), "held while busy");
        app.apply(FromAgent::Done);
        assert!(matches!(rx.try_recv().unwrap(), ToAgent::Turn(t) if t == "second"));

        let flag = interrupt::test_flag::install();
        let _ = flag; // the TUI raises the process-wide flag; check its effect on state
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.stopping);
        interrupt::clear();
        app.apply(FromAgent::Done);
        assert!(!app.busy && !app.stopping);

        // Idle: Ctrl-C twice quits.
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn resumed_transcripts_and_scrolling() {
        let (mut app, _rx) = app();
        let turns: Vec<Turn> = (0..30)
            .map(|i| Turn {
                user: format!("question {i}"),
                provider: if i % 2 == 0 { Provider::Claude } else { Provider::Local },
                private: false,
                reply: format!("answer {i}"),
                tools: vec![],
            })
            .collect();
        app.apply(FromAgent::Transcript(turns));
        let bottom = screen_text(&mut app, 80, 20);
        assert!(bottom.contains("answer 29") && !bottom.contains("question 0 "), "{bottom}");
        app.on_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        let up = screen_text(&mut app, 80, 20);
        assert!(!up.contains("answer 29") && up.contains("more · End"), "{up}");
        app.on_key(key(KeyCode::End, KeyModifiers::NONE));
        assert!(screen_text(&mut app, 80, 20).contains("answer 29"));
        assert!(app.entries.first() == Some(&Entry::Divider("resumed · 30 earlier messages".into())));
    }

    #[test]
    fn panel_shows_machine_and_routing() {
        let m = Machine {
            host: "box".into(),
            cpu: Some(0.31),
            mem_total_kb: Some(16 * 1_048_576),
            mem_avail_kb: Some(7 * 1_048_576),
            gpus: vec![Gpu { name: "NVIDIA GeForce RTX 4070".into(), util: Some(42), mem_used: Some(3174), mem_total: Some(12282), temp: Some(61) }],
            ..Default::default()
        };
        let s = Status {
            routing: "routing: auto (private → local; qwen3:8b judges the rest)".into(),
            providers: vec!["claude  claude-opus-5-5 · effort high".into(), "local   unavailable: cannot connect".into()],
            ..Default::default()
        };
        let text: String = panel_lines(&m, &s, 32).iter().map(|l| l.to_string() + "\n").collect();
        for want in ["box", "31%", "9.0/16G", "GPU0 RTX 4070", "42% 61°C", "3.1/12G", "judges the rest", "unavailable"] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
    }
}
