//! Palette, glyph sets, and styled line assembly.
//!
//! The baseline palette uses the terminal's own ANSI colors, which follow the user's theme.
//! Tuned 256-color and truecolor palettes are used only when the background is known.
//! Styles are written as plain SGR sequences, so a 16-color terminal only ever receives the
//! original `30`–`37` color codes.

use super::caps::{Background, Caps, ColorDepth};
use super::text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Brand,
    Accent,
    Strong,
    Muted,
    Success,
    Warn,
    Error,
    BarFill,
    BarEmpty,
}

#[derive(Clone, Copy, Debug)]
pub struct Painter {
    depth: Option<ColorDepth>,
    background: Option<Background>,
}

impl Painter {
    pub fn new(caps: &Caps) -> Painter {
        Painter {
            depth: caps.color,
            background: caps.background,
        }
    }

    fn color(&self, role: Role, depth: ColorDepth) -> Option<Ink> {
        // (dark rgb, light rgb, dark 256, light 256)
        let tuned = |dark: (u8, u8, u8), light: (u8, u8, u8), d256: u8, l256: u8| {
            let bg = self.background?;
            Some(match (depth, bg) {
                (ColorDepth::TrueColor, Background::Dark) => Ink::Rgb(dark),
                (ColorDepth::TrueColor, Background::Light) => Ink::Rgb(light),
                (_, Background::Dark) => Ink::Indexed(d256),
                (_, Background::Light) => Ink::Indexed(l256),
            })
        };
        let rich = depth >= ColorDepth::Ansi256;
        let pick = |basic: Option<Ink>, tuned_color: Option<Ink>| {
            if rich { tuned_color.or(basic) } else { basic }
        };
        match role {
            Role::Brand | Role::Accent | Role::BarFill => pick(
                Some(Ink::Ansi(CYAN)),
                tuned((95, 215, 255), (0, 125, 160), 81, 31),
            ),
            Role::Success => pick(
                Some(Ink::Ansi(GREEN)),
                tuned((135, 215, 135), (0, 128, 0), 114, 28),
            ),
            Role::Warn => pick(
                Some(Ink::Ansi(YELLOW)),
                tuned((255, 215, 95), (160, 90, 0), 221, 130),
            ),
            Role::Error => pick(
                Some(Ink::Ansi(RED)),
                tuned((255, 105, 105), (200, 0, 0), 203, 160),
            ),
            Role::Muted => pick(None, tuned((150, 150, 150), (100, 100, 100), 246, 242)),
            Role::BarEmpty => pick(None, tuned((88, 88, 88), (200, 200, 200), 240, 252)),
            Role::Strong => None,
        }
    }

    pub fn paint(&self, role: Option<Role>, text: &str) -> String {
        let (Some(role), Some(depth)) = (role, self.depth) else {
            return text.to_string();
        };
        let mut codes = Vec::new();
        if matches!(role, Role::Brand | Role::Strong) {
            codes.push("1".to_string());
        }
        match self.color(role, depth) {
            Some(Ink::Ansi(n)) => codes.push(format!("3{n}")),
            Some(Ink::Indexed(n)) => codes.push(format!("38;5;{n}")),
            Some(Ink::Rgb((r, g, b))) => codes.push(format!("38;2;{r};{g};{b}")),
            None => {}
        }
        if codes.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

/// ANSI color numbers (foreground `30 + n`).
const RED: u8 = 1;
const GREEN: u8 = 2;
const YELLOW: u8 = 3;
const CYAN: u8 = 6;

#[derive(Clone, Copy, Debug)]
enum Ink {
    /// One of the 8 base colors, which follow the terminal's theme.
    Ansi(u8),
    Indexed(u8),
    Rgb((u8, u8, u8)),
}

/// Character set for progress drawing.
#[derive(Clone, Copy, Debug)]
pub struct GlyphSet {
    pub fill: &'static str,
    pub empty: &'static str,
    pub bar_open: &'static str,
    pub bar_close: &'static str,
    pub arrow: &'static str,
    pub ok: &'static str,
    pub fail: &'static str,
    pub warn: &'static str,
    pub dot: &'static str,
    pub sep: &'static str,
    pub dash: &'static str,
    pub ellipsis: &'static str,
    pub spark: &'static [char],
    pub spinner: &'static [&'static str],
}

pub const UNICODE: GlyphSet = GlyphSet {
    fill: "█",
    empty: "░",
    bar_open: "",
    bar_close: "",
    arrow: "↓",
    ok: "✓",
    fail: "✗",
    warn: "!",
    dot: "•",
    sep: "·",
    dash: "—",
    ellipsis: "…",
    spark: &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'],
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
};

pub const ASCII: GlyphSet = GlyphSet {
    fill: "#",
    empty: "-",
    bar_open: "[",
    bar_close: "]",
    arrow: ">",
    ok: "+",
    fail: "x",
    warn: "!",
    dot: "*",
    sep: "-",
    dash: "-",
    ellipsis: "...",
    spark: &['_', '.', ',', '-', '~', '=', '*', '#'],
    spinner: &["|", "/", "-", "\\"],
};

impl GlyphSet {
    pub fn for_caps(caps: &Caps) -> GlyphSet {
        if caps.unicode { UNICODE } else { ASCII }
    }

    pub fn sparkline(&self, history: &[f64], width: usize) -> String {
        let samples = &history[history.len().saturating_sub(width)..];
        let max = samples.iter().copied().fold(0.0f64, f64::max);
        if max <= 0.0 {
            return String::new();
        }
        let levels = self.spark.len() - 1;
        samples
            .iter()
            .map(|&v| {
                let level = ((v / max) * levels as f64).round() as usize;
                self.spark[level.min(levels)]
            })
            .collect()
    }
}

/// A line made of styled segments, measured and clipped by display width.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    segments: Vec<(Option<Role>, String)>,
}

impl Line {
    pub fn new() -> Line {
        Line::default()
    }

    pub fn push(&mut self, role: Role, text: impl Into<String>) -> &mut Line {
        self.segments.push((Some(role), text.into()));
        self
    }

    pub fn plain(&mut self, text: impl Into<String>) -> &mut Line {
        self.segments.push((None, text.into()));
        self
    }

    pub fn append(&mut self, other: &Line) -> &mut Line {
        self.segments.extend(other.segments.iter().cloned());
        self
    }

    pub fn width(&self) -> usize {
        self.segments.iter().map(|(_, t)| text::width(t)).sum()
    }

    /// Add spaces so that the line reaches column `col`.
    pub fn pad_to(&mut self, col: usize) -> &mut Line {
        let w = self.width();
        if w < col {
            self.plain(" ".repeat(col - w));
        }
        self
    }

    /// Text without styling.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.segments.iter().map(|(_, t)| t.as_str()).collect()
    }

    /// Render with styling, clipped to `max` columns.
    pub fn render(&self, painter: &Painter, max: usize) -> String {
        let mut out = String::new();
        let mut used = 0;
        for (role, segment) in &self.segments {
            let w = text::width(segment);
            if used + w <= max {
                out.push_str(&painter.paint(*role, segment));
                used += w;
            } else {
                let clipped = text::truncate_end(segment, max - used, "");
                out.push_str(&painter.paint(*role, &clipped));
                break;
            }
        }
        // Trailing spaces only pad; drop them so lines never wrap.
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(color: Option<ColorDepth>, background: Option<Background>) -> Caps {
        Caps {
            tty: true,
            cursor: true,
            color,
            background,
            unicode: true,
            animation: true,
            ssh: false,
        }
    }

    #[test]
    fn basic_depth_uses_base_color_codes() {
        let p = Painter::new(&caps(Some(ColorDepth::Basic), Some(Background::Dark)));
        assert_eq!(p.paint(Some(Role::Success), "ok"), "\x1b[32mok\x1b[0m");
        assert_eq!(p.paint(Some(Role::Brand), "vayu"), "\x1b[1;36mvayu\x1b[0m");
        assert_eq!(p.paint(Some(Role::Strong), "x"), "\x1b[1mx\x1b[0m");
        assert_eq!(p.paint(Some(Role::Muted), "label"), "label");
    }

    #[test]
    fn plain_when_color_disabled() {
        let p = Painter::new(&caps(None, None));
        assert_eq!(p.paint(Some(Role::Error), "x"), "x");
    }

    #[test]
    fn basic_ansi_without_known_background() {
        let p = Painter::new(&caps(Some(ColorDepth::TrueColor), None));
        let s = p.paint(Some(Role::Success), "ok");
        assert!(s.contains("\x1b[") && s.contains("ok"), "{s:?}");
        assert!(
            !s.contains("38;2;"),
            "no truecolor without a known background: {s:?}"
        );
        assert_eq!(p.paint(Some(Role::Muted), "label"), "label");
    }

    #[test]
    fn tuned_palette_for_known_background() {
        let p = Painter::new(&caps(Some(ColorDepth::TrueColor), Some(Background::Dark)));
        assert!(p.paint(Some(Role::Accent), "x").contains("38;2;95;215;255"));
        let p = Painter::new(&caps(Some(ColorDepth::Ansi256), Some(Background::Light)));
        assert!(p.paint(Some(Role::Accent), "x").contains("38;5;31"));
    }

    #[test]
    fn line_clipping() {
        let p = Painter::new(&caps(Some(ColorDepth::Basic), None));
        let mut line = Line::new();
        line.push(Role::Brand, "vayu")
            .plain("  ")
            .push(Role::Accent, "abcdef")
            .plain("   ");
        assert_eq!(line.width(), 15);
        let rendered = line.render(&Painter::new(&caps(None, None)), 8);
        assert_eq!(rendered, "vayu  ab");
        assert!(line.render(&p, 80).contains("abcdef"));
        assert!(!line.render(&p, 80).ends_with(' '));
    }

    #[test]
    fn sparkline_scales() {
        assert_eq!(UNICODE.sparkline(&[0.0, 50.0, 100.0], 10), "▁▅█");
        assert_eq!(UNICODE.sparkline(&[1.0, 2.0, 3.0, 4.0], 2), "▆█");
        assert_eq!(ASCII.sparkline(&[], 5), "");
        assert_eq!(ASCII.sparkline(&[0.0, 0.0], 5), "");
    }
}
