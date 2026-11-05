//! Live in-place dashboard for interactive terminals.
//!
//! Frames are rebuilt from the latest snapshot, diffed line by line against the previous frame,
//! and written in a single batch. Normal scrollback is preserved: no alternate screen, raw mode,
//! or hidden cursor, so an abrupt exit leaves the terminal usable.

use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::queue;
use crossterm::terminal::{Clear, ClearType};

use super::caps::{self, Caps};
use super::style::{GlyphSet, Line, Painter, Role};
use super::text;
use super::{Completion, Outcome};
use crate::cli::UiMode;
use crate::metrics::{Event, Phase, Snapshot};

const NOTICE_DURATION: Duration = Duration::from_secs(6);
const MAX_BAR: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Standard,
    Detailed,
    Compact,
    Minimal,
}

/// Inputs shared by all layouts.
pub struct View<'a> {
    pub glyphs: GlyphSet,
    pub cols: usize,
    pub spinner: Option<&'static str>,
    pub notice: Option<&'a str>,
}

pub fn choose_layout(mode: UiMode, cols: u16, rows: u16) -> Layout {
    let (cols, rows) = (cols as usize, rows as usize);
    if cols < 30 || rows < 3 {
        return Layout::Minimal;
    }
    let roomy = cols >= 50 && rows >= 12;
    match mode {
        UiMode::Compact => Layout::Compact,
        UiMode::Detailed if roomy => Layout::Detailed,
        UiMode::Auto if roomy => Layout::Standard,
        _ => Layout::Compact,
    }
}

pub fn frame(layout: Layout, snap: &Snapshot, view: &View) -> Vec<Line> {
    match layout {
        Layout::Standard => standard(snap, view, false),
        Layout::Detailed => standard(snap, view, true),
        Layout::Compact => compact(snap, view),
        Layout::Minimal => vec![minimal(snap, view)],
    }
}

fn status(snap: &Snapshot, view: &View) -> (Role, String) {
    let g = &view.glyphs;
    match &snap.phase {
        Phase::Recovering => (Role::Accent, "Checking saved progress".into()),
        Phase::Connecting => (
            Role::Accent,
            format!("Connecting to {}", text::sanitize(&snap.host)),
        ),
        Phase::Downloading => match view.notice {
            Some(notice) => (Role::Warn, notice.to_string()),
            None => (Role::Accent, "Downloading".into()),
        },
        Phase::Retrying { reason, until, .. } => {
            let secs = until
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .ceil();
            (
                Role::Warn,
                format!("{} {} retrying in {secs:.0}s", capitalize(reason), g.sep),
            )
        }
        Phase::Verifying => (Role::Accent, "Verifying SHA-256".into()),
        Phase::Finalizing => (Role::Accent, "Finalizing".into()),
        Phase::Stopping => (Role::Warn, "Stopping safely".into()),
    }
}

fn display_name(snap: &Snapshot) -> String {
    text::sanitize(snap.name.as_deref().unwrap_or(&snap.host))
}

fn eta_text(snap: &Snapshot, g: &GlyphSet) -> String {
    snap.eta()
        .map(text::duration)
        .unwrap_or_else(|| g.dash.to_string())
}

fn bar(fraction: f64, width: usize, g: &GlyphSet, line: &mut Line) {
    let inner = width.saturating_sub(text::width(g.bar_open) + text::width(g.bar_close));
    let filled = ((fraction * inner as f64).floor() as usize).min(inner);
    line.plain(g.bar_open)
        .push(Role::BarFill, g.fill.repeat(filled))
        .push(Role::BarEmpty, g.empty.repeat(inner - filled))
        .plain(g.bar_close);
}

fn percent(fraction: f64) -> String {
    // Never round up to 100% before the last byte arrives.
    let tenths = (fraction * 1000.0).floor() / 10.0;
    format!("{tenths:.1}%")
}

fn standard(snap: &Snapshot, view: &View, detailed: bool) -> Vec<Line> {
    let g = &view.glyphs;
    let indent = "  ";
    let bar_width = MAX_BAR.min(view.cols.saturating_sub(12));
    let right_edge = indent.len() + bar_width + 8;
    let mut lines = Vec::with_capacity(9);

    let mut header = Line::new();
    let name_room = view.cols.saturating_sub(12);
    header
        .plain(indent)
        .push(Role::Brand, "vayu")
        .plain("  ")
        .push(Role::Accent, g.arrow)
        .plain("  ")
        .push(
            Role::Strong,
            text::truncate_middle(&display_name(snap), name_room, g.ellipsis),
        );
    lines.push(header);
    lines.push(Line::new());

    let mut progress = Line::new();
    let mut amounts = Line::new();
    progress.plain(indent);
    amounts.plain(indent);
    match snap.fraction() {
        Some(fraction) => {
            bar(fraction, bar_width, g, &mut progress);
            let pct = percent(fraction);
            progress
                .pad_to(right_edge - text::width(&pct))
                .push(Role::Strong, pct);
            amounts.plain(format!(
                "{} / {}",
                text::bytes(snap.received),
                text::bytes(snap.total.unwrap_or(0))
            ));
        }
        None => {
            progress
                .push(Role::Strong, text::bytes(snap.received))
                .plain(" received");
            amounts.push(Role::Muted, "Size not reported by the server");
        }
    }
    let eta = format!("ETA {}", eta_text(snap, g));
    amounts.pad_to(
        right_edge
            .saturating_sub(text::width(&eta))
            .max(amounts.width() + 2),
    );
    amounts.push(Role::Muted, "ETA ").plain(eta_text(snap, g));
    lines.push(progress);
    lines.push(amounts);
    lines.push(Line::new());

    let mut speed = Line::new();
    speed
        .plain(indent)
        .push(Role::Muted, "Speed  ")
        .push(Role::Accent, text::pad(&text::rate(snap.speed), 12));
    let spark = g.sparkline(&snap.history, 12);
    speed.push(Role::Accent, text::pad(&spark, 12)).plain("  ");
    speed
        .push(Role::Muted, "Peak  ")
        .plain(text::rate(snap.peak));
    lines.push(speed);

    let (role, status_text) = status(snap, view);
    let mut state = Line::new();
    state
        .plain(indent)
        .push(Role::Muted, "Connections  ")
        .plain(snap.connections.to_string())
        .plain("    ");
    match view.spinner {
        Some(frame) => state.push(role, frame),
        None => state.push(
            if snap.connections > 0 {
                role
            } else {
                Role::Muted
            },
            g.dot,
        ),
    };
    state.plain("    ").push(role, status_text);
    lines.push(state);

    let mut footer = Line::new();
    footer.plain(indent).push(Role::Muted, "Saved ");
    match snap.saved_at {
        Some(at) if snap.saved > 0 => footer.plain(text::ago(at.elapsed())),
        _ => footer.plain(g.dash),
    };
    footer
        .plain("    ")
        .push(Role::Muted, "Elapsed ")
        .plain(text::duration(snap.elapsed));
    let hint = "Ctrl+C to stop";
    footer
        .pad_to(
            right_edge
                .saturating_sub(hint.len())
                .max(footer.width() + 4),
        )
        .push(Role::Muted, hint);
    lines.push(footer);

    if detailed {
        let mut info = Line::new();
        info.plain(indent)
            .push(Role::Muted, "Protocol ")
            .plain(snap.http_version.unwrap_or(g.dash))
            .plain(format!("  {}  ", g.sep))
            .plain(text::sanitize(&snap.host));
        if let Some(route) = &snap.route {
            info.plain(format!("  {}  ", g.sep))
                .plain(text::sanitize(route));
        }
        info.plain(format!("  {}  ", g.sep))
            .push(Role::Muted, "Retries ")
            .plain(snap.retries.to_string());
        lines.push(info);
        if let Some(segments) = snap.segments.as_ref().filter(|s| !s.rates.is_empty()) {
            let mut activity = Line::new();
            activity
                .plain(indent)
                .push(Role::Muted, "Segments ")
                .plain(segments.active.to_string())
                .plain(format!("  {}  ", g.sep))
                .push(Role::Muted, "Per connection ");
            let rates: Vec<String> = segments.rates.iter().map(|&r| text::rate(r)).collect();
            activity.push(Role::Accent, rates.join("  "));
            lines.push(activity);
        }
    }
    lines
}

fn compact(snap: &Snapshot, view: &View) -> Vec<Line> {
    let g = &view.glyphs;
    let (role, status_text) = status(snap, view);
    let mut first = Line::new();
    first
        .push(Role::Brand, "vayu")
        .plain(" ")
        .push(Role::Accent, g.arrow)
        .plain(" ");
    let status_room = text::width(&status_text) + 3;
    let name_room = view
        .cols
        .saturating_sub(first.width() + status_room + 1)
        .max(8);
    first
        .push(
            Role::Strong,
            text::truncate_middle(&display_name(snap), name_room, g.ellipsis),
        )
        .plain("  ");
    if let Some(frame) = view.spinner {
        first.push(role, frame).plain(" ");
    }
    first.push(role, status_text);

    // Second line: add metrics in priority order while they fit.
    let mut second = Line::new();
    let budget = view.cols.saturating_sub(1);
    let fraction = snap.fraction();
    let mut pieces: Vec<(u8, Line)> = Vec::new();
    if let Some(f) = fraction {
        let mut l = Line::new();
        l.push(Role::Strong, percent(f));
        pieces.push((0, l));
    }
    let mut speed = Line::new();
    speed.push(Role::Accent, text::rate(snap.speed));
    pieces.push((1, speed));
    let mut eta = Line::new();
    eta.push(Role::Muted, "ETA ").plain(eta_text(snap, g));
    pieces.push((2, eta));
    let mut amount = Line::new();
    match snap.total {
        Some(total) => amount.plain(format!(
            "{}/{}",
            text::bytes(snap.received),
            text::bytes(total)
        )),
        None => amount.plain(text::bytes(snap.received)),
    };
    pieces.push((3, amount));

    let mut chosen: Vec<&(u8, Line)> = Vec::new();
    let mut used = 0;
    for piece in &pieces {
        let w = piece.1.width() + 2;
        if used + w <= budget {
            used += w;
            chosen.push(piece);
        }
    }
    let bar_room = budget.saturating_sub(used + 1);
    if let Some(f) = fraction
        && bar_room >= 8
    {
        bar(f, bar_room.min(30), g, &mut second);
        second.plain(" ");
    }
    // Keep display order: percent, amount, speed, ETA.
    chosen.sort_by_key(|(p, _)| match p {
        0 => 0,
        3 => 1,
        1 => 2,
        _ => 3,
    });
    for (i, (_, piece)) in chosen.into_iter().enumerate() {
        if i > 0 {
            second.plain("  ");
        }
        second.append(piece);
    }
    vec![first, second]
}

fn minimal(snap: &Snapshot, view: &View) -> Line {
    let mut line = Line::new();
    if let Some(f) = snap.fraction() {
        line.push(Role::Strong, percent(f)).plain(" ");
    }
    line.push(Role::Accent, text::rate(snap.speed));
    if let Phase::Retrying { .. } = snap.phase {
        line.plain(" ").push(Role::Warn, "retrying");
    }
    let _ = view;
    line
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Summary left in scrollback when the download ends.
pub fn summary(outcome: &Outcome, snap: &Snapshot, g: &GlyphSet, compact: bool) -> Vec<Line> {
    let indent = if compact { "" } else { "  " };
    let gap = if compact { " " } else { "  " };
    let mut lines = Vec::new();
    match outcome {
        Outcome::Complete(done) => {
            let mut head = Line::new();
            head.plain(indent)
                .push(Role::Brand, "vayu")
                .plain(gap)
                .push(Role::Success, g.ok)
                .plain(gap)
                .push(Role::Success, "Complete")
                .plain(format!(" {} saved to ", g.sep))
                .push(
                    Role::Strong,
                    text::sanitize(&done.path.display().to_string()),
                );
            lines.push(head);
            let mut stats = Line::new();
            stats.plain(indent).plain(stats_text(done, g));
            lines.push(stats);
            let mut integrity = Line::new();
            integrity.plain(indent);
            if done.verified {
                integrity
                    .push(Role::Success, "SHA-256 verified")
                    .push(Role::Muted, " against the supplied checksum");
            } else {
                integrity
                    .push(Role::Muted, "SHA-256 ")
                    .plain(crate::integrity::hex(&done.sha256))
                    .push(Role::Muted, " (computed; no checksum supplied)");
            }
            lines.push(integrity);
        }
        Outcome::Failed { failure, partial } => {
            let interrupted = matches!(failure.category, crate::error::Category::Interrupted(_));
            let (role, glyph, word) = if interrupted {
                (Role::Warn, g.warn, "Stopped")
            } else {
                (Role::Error, g.fail, "Failed")
            };
            let mut head = Line::new();
            head.plain(indent)
                .push(Role::Brand, "vayu")
                .plain(gap)
                .push(role, glyph)
                .plain(gap)
                .push(role, word)
                .plain(format!(" {} ", g.sep))
                .plain(text::sanitize(&failure.message));
            lines.push(head);
            if let Some(hint) = &failure.hint {
                let mut l = Line::new();
                l.plain(indent).plain(text::sanitize(hint));
                lines.push(l);
            }
            if let Some(part) = partial {
                let mut l = Line::new();
                l.plain(indent)
                    .push(
                        Role::Muted,
                        format!("Partial data ({}) kept at ", text::bytes(snap.received)),
                    )
                    .plain(text::sanitize(&part.display().to_string()));
                lines.push(l);
            }
        }
    }
    lines
}

pub fn stats_text(done: &Completion, g: &GlyphSet) -> String {
    let secs = done.elapsed.as_secs_f64();
    let average = if secs > 0.0 {
        done.bytes as f64 / secs
    } else {
        0.0
    };
    format!(
        "{} in {} {} average {}",
        text::bytes(done.bytes),
        text::duration(done.elapsed),
        g.sep,
        text::rate(average)
    )
}

/// In-place renderer state.
pub struct Dashboard {
    caps: Caps,
    painter: Painter,
    glyphs: GlyphSet,
    mode: UiMode,
    /// Previously drawn lines and the width they were drawn at.
    drawn: Vec<String>,
    drawn_cols: usize,
    spinner_index: usize,
    last_activity: u64,
    notice: Option<(String, Instant)>,
    dead: bool,
}

impl Dashboard {
    pub fn new(caps: Caps, mode: UiMode) -> Dashboard {
        Dashboard {
            painter: Painter::new(&caps),
            glyphs: GlyphSet::for_caps(&caps),
            caps,
            mode,
            drawn: Vec::new(),
            drawn_cols: 0,
            spinner_index: 0,
            last_activity: 0,
            notice: None,
            dead: false,
        }
    }

    pub fn interval(&self) -> Duration {
        self.caps.frame_interval()
    }

    pub fn event(&mut self, event: &Event) {
        let message = match event {
            Event::Restarted { reason } => Some(capitalize(reason)),
            Event::Resumed { .. } => Some("Reconnected · continuing".replace('·', self.glyphs.sep)),
            Event::Recovered { .. } => Some("Resuming saved progress".to_string()),
            Event::Notice { message } => Some(message.clone()),
            _ => None,
        };
        if let Some(message) = message {
            self.notice = Some((message, Instant::now()));
        }
    }

    pub fn draw(&mut self, snap: &Snapshot) {
        if self.dead {
            return;
        }
        if caps::backgrounded() {
            // A background job must not draw over the shell, and with `stty tostop` the write
            // would stop the download. Back in the foreground, the next frame starts fresh
            // below whatever the shell printed meanwhile.
            self.drawn.clear();
            return;
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > NOTICE_DURATION)
        {
            self.notice = None;
        }
        let (cols, rows) = caps::size();
        let spinner = if self.caps.animation {
            if snap.activity != self.last_activity {
                self.last_activity = snap.activity;
                self.spinner_index = (self.spinner_index + 1) % self.glyphs.spinner.len();
            }
            Some(self.glyphs.spinner[self.spinner_index])
        } else {
            None
        };
        let view = View {
            glyphs: self.glyphs,
            cols: cols as usize,
            spinner,
            notice: self.notice.as_ref().map(|(m, _)| m.as_str()),
        };
        let layout = choose_layout(self.mode, cols, rows);
        let max = (cols as usize).saturating_sub(1);
        let lines: Vec<String> = frame(layout, snap, &view)
            .iter()
            .map(|l| l.render(&self.painter, max))
            .collect();
        self.write_frame(&lines, cols as usize);
    }

    /// Replace the live display with the final summary.
    pub fn finish(&mut self, outcome: &Outcome, snap: &Snapshot) {
        if self.dead {
            return;
        }
        let background = caps::backgrounded();
        if background && matches!(outcome, Outcome::Complete(_)) && caps::background_writes_stop() {
            // The shell reports the finished job. A failure is still written: the job stops
            // with `SIGTTOU` and shows the message once brought back with `fg`.
            return;
        }
        if background {
            self.drawn.clear();
        }
        let (cols, rows) = caps::size();
        let compact = !matches!(
            choose_layout(self.mode, cols, rows),
            Layout::Standard | Layout::Detailed
        );
        // The summary stays in scrollback, so it may wrap: never clip the destination path.
        let mut lines: Vec<String> = summary(outcome, snap, &self.glyphs, compact)
            .iter()
            .map(|l| l.render(&self.painter, usize::MAX))
            .collect();
        if !compact {
            lines.insert(0, String::new());
        }
        let mut out = Vec::new();
        self.erase(&mut out, cols as usize);
        for line in &lines {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        self.flush(&out);
        self.drawn.clear();
    }

    fn write_frame(&mut self, lines: &[String], cols: usize) {
        let mut out = Vec::with_capacity(1024);
        if cols != self.drawn_cols || lines.len() != self.drawn.len() {
            self.erase(&mut out, cols);
            for line in lines {
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
        } else {
            let _ = queue!(out, MoveUp(self.drawn.len() as u16), MoveToColumn(0));
            for (old, new) in self.drawn.iter().zip(lines) {
                if old != new {
                    out.extend_from_slice(new.as_bytes());
                    let _ = queue!(out, Clear(ClearType::UntilNewLine));
                }
                out.push(b'\n');
            }
            if self.drawn.as_slice() == lines {
                return;
            }
        }
        self.flush(&out);
        self.drawn = lines.to_vec();
        self.drawn_cols = cols;
    }

    /// Move to the start of the previous frame and clear it. Rows are counted at the new width
    /// so that lines rewrapped by a narrowing resize are also cleared.
    fn erase(&self, out: &mut Vec<u8>, cols: usize) {
        if self.drawn.is_empty() {
            return;
        }
        let rows: usize = self
            .drawn
            .iter()
            .map(|line| visible_width(line).max(1).div_ceil(cols.max(1)))
            .sum();
        let _ = queue!(
            out,
            MoveUp(rows as u16),
            MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        );
    }

    fn flush(&mut self, bytes: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        if stderr
            .write_all(bytes)
            .and_then(|_| stderr.flush())
            .is_err()
        {
            // The terminal went away; keep downloading without a display.
            self.dead = true;
        }
    }
}

/// Display width of a rendered line, ignoring ANSI escape sequences.
fn visible_width(rendered: &str) -> usize {
    let mut plain = String::with_capacity(rendered.len());
    let mut chars = rendered.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            plain.push(c);
        }
    }
    text::width(&plain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::style::{ASCII, UNICODE};
    use std::path::PathBuf;

    fn snapshot() -> Snapshot {
        let (reporter, rx) = crate::metrics::Reporter::new(
            "download.example.com".into(),
            None,
            std::sync::mpsc::sync_channel(1).0,
        );
        reporter.update(|s| {
            s.phase = Phase::Downloading;
            s.name = Some("linux-image.iso".into());
            s.received = 640 * 1024 * 1024;
            s.total = Some(1024 * 1024 * 1024);
            s.speed = 800.0 * 1024.0;
            s.eta_speed = 800.0 * 1024.0;
            s.peak = 940.0 * 1024.0;
            s.history = vec![1.0, 3.0, 2.0, 5.0, 8.0];
            s.connections = 1;
            s.elapsed = Duration::from_secs(820);
            s.saved = 600 * 1024 * 1024;
            s.saved_at = Instant::now().checked_sub(Duration::from_secs(2));
        });
        rx.borrow().clone()
    }

    fn view(glyphs: GlyphSet, cols: usize) -> View<'static> {
        View {
            glyphs,
            cols,
            spinner: None,
            notice: None,
        }
    }

    #[test]
    fn standard_layout_matches_design() {
        let lines: Vec<String> = frame(Layout::Standard, &snapshot(), &view(UNICODE, 80))
            .iter()
            .map(Line::text)
            .collect();
        assert_eq!(lines[0], "  vayu  ↓  linux-image.iso");
        assert!(
            lines[2].starts_with(&format!("  {}░", "█".repeat(25))),
            "{}",
            lines[2]
        );
        assert!(lines[2].ends_with("62.5%"), "{}", lines[2]);
        assert!(lines[3].starts_with("  640 MiB / 1.00 GiB"));
        assert!(lines[3].ends_with("ETA 08:11"), "{}", lines[3]);
        assert!(lines[5].contains("Speed  800 KiB/s") && lines[5].contains("Peak  940 KiB/s"));
        assert!(lines[6].contains("Connections  1") && lines[6].contains("Downloading"));
        assert!(lines[7].contains("Elapsed 13:40") && lines[7].contains("Ctrl+C to stop"));
        assert!(lines[7].starts_with("  Saved 2s ago"), "{}", lines[7]);
        assert!(lines.len() <= 8);
        // The bar and the right-aligned text share an edge.
        assert_eq!(text::width(&lines[2]), text::width(&lines[3]));
    }

    #[test]
    fn detailed_view_shows_segment_activity() {
        let mut snap = snapshot();
        snap.segments = Some(crate::metrics::SegmentView {
            active: 2,
            rates: vec![512.0 * 1024.0, 300.0 * 1024.0],
        });
        let lines: Vec<String> = frame(Layout::Detailed, &snap, &view(UNICODE, 100))
            .iter()
            .map(Line::text)
            .collect();
        let last = lines.last().unwrap();
        assert!(last.contains("Segments 2"), "{last}");
        assert!(last.contains("512 KiB/s  300 KiB/s"), "{last}");
    }

    #[test]
    fn ascii_and_unknown_size() {
        let mut snap = snapshot();
        snap.total = None;
        let lines: Vec<String> = frame(Layout::Standard, &snap, &view(ASCII, 80))
            .iter()
            .map(Line::text)
            .collect();
        assert_eq!(lines[0], "  vayu  >  linux-image.iso");
        assert_eq!(lines[2], "  640 MiB received");
        assert!(
            lines[3].ends_with("ETA -"),
            "no fabricated ETA: {}",
            lines[3]
        );
        assert!(lines.iter().all(|l| l.is_ascii()));
        assert!(
            !lines.iter().any(|l| l.contains('%')),
            "no fabricated percentage"
        );
    }

    #[test]
    fn layouts_fit_narrow_terminals() {
        for cols in [20usize, 30, 40, 49, 50, 60, 80, 120] {
            for mode in [UiMode::Auto, UiMode::Compact, UiMode::Detailed] {
                let layout = choose_layout(mode, cols as u16, 24);
                let painter = Painter::new(&Caps {
                    tty: true,
                    cursor: true,
                    color: None,
                    background: None,
                    unicode: true,
                    animation: false,
                    ssh: false,
                });
                for line in frame(layout, &snapshot(), &view(UNICODE, cols)) {
                    let rendered = line.render(&painter, cols - 1);
                    assert!(
                        text::width(&rendered) < cols,
                        "{cols} {layout:?}: {rendered}"
                    );
                }
            }
        }
        assert_eq!(choose_layout(UiMode::Auto, 80, 24), Layout::Standard);
        assert_eq!(choose_layout(UiMode::Auto, 45, 24), Layout::Compact);
        assert_eq!(choose_layout(UiMode::Detailed, 100, 30), Layout::Detailed);
        assert_eq!(choose_layout(UiMode::Auto, 25, 24), Layout::Minimal);
    }

    #[test]
    fn retry_status_and_percent_rounding() {
        let mut snap = snapshot();
        snap.phase = Phase::Retrying {
            reason: "connection lost".into(),
            attempt: 2,
            until: Instant::now() + Duration::from_millis(3500),
        };
        let (role, status) = status(&snap, &view(UNICODE, 80));
        assert_eq!(role, Role::Warn);
        assert_eq!(status, "Connection lost · retrying in 4s");
        assert_eq!(snap.eta(), None);
        assert_eq!(percent(0.99999), "99.9%");
        assert_eq!(percent(1.0), "100.0%");
    }

    #[test]
    fn summaries() {
        let done = Outcome::Complete(Completion {
            path: PathBuf::from("/tmp/file.zip"),
            bytes: 1024 * 1024,
            elapsed: Duration::from_secs(4),
            sha256: [0xab; 32],
            verified: false,
        });
        let text: Vec<String> = summary(&done, &snapshot(), &UNICODE, false)
            .iter()
            .map(Line::text)
            .collect();
        assert_eq!(text[0], "  vayu  ✓  Complete · saved to /tmp/file.zip");
        assert_eq!(text[1], "  1.00 MiB in 00:04 · average 256 KiB/s");
        assert!(text[2].contains("computed; no checksum supplied"));

        let failed = Outcome::Failed {
            failure: crate::error::Failure::remote("access denied (HTTP 403 Forbidden)")
                .with_hint("get a fresh link"),
            partial: Some(PathBuf::from("/tmp/file.zip.part")),
        };
        let text: Vec<String> = summary(&failed, &snapshot(), &ASCII, true)
            .iter()
            .map(Line::text)
            .collect();
        assert_eq!(
            text[0],
            "vayu x Failed - access denied (HTTP 403 Forbidden)"
        );
        assert_eq!(text[1], "get a fresh link");
        assert!(text[2].ends_with("kept at /tmp/file.zip.part"));
    }

    #[test]
    fn measures_rendered_width() {
        assert_eq!(visible_width("\x1b[36mabc\x1b[0m"), 3);
        assert_eq!(visible_width("日本"), 4);
    }
}
