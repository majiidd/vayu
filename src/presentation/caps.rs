//! Terminal capability detection for the presentation stream (stderr).
//!
//! Layout, color, glyphs, and animation are decided independently. Detection only reads the
//! environment and asks the kernel for the window size; it never sends terminal queries that
//! could block startup or consume input.

use std::io::IsTerminal;
use std::time::Duration;

use crate::cli::{ColorChoice, Display, Glyphs, Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorDepth {
    Basic,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    /// stderr is a terminal.
    pub tty: bool,
    /// Cursor movement can be used for a live, in-place display.
    pub cursor: bool,
    /// Color depth when color is enabled, `None` otherwise.
    pub color: Option<ColorDepth>,
    /// Known background, when explicitly configured or advertised by `COLORFGBG`.
    pub background: Option<Background>,
    pub unicode: bool,
    pub animation: bool,
    pub ssh: bool,
}

impl Caps {
    /// Upper bound on live refreshes: conservative over SSH, smoother locally.
    pub fn frame_interval(&self) -> Duration {
        if self.ssh {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(125)
        }
    }
}

pub fn detect(display: &Display) -> Caps {
    let env = |name: &str| std::env::var(name).ok();
    detect_with(display, std::io::stderr().is_terminal(), &env)
}

pub fn detect_with(display: &Display, tty: bool, env: &dyn Fn(&str) -> Option<String>) -> Caps {
    let nonempty = |name: &str| env(name).filter(|v| !v.is_empty());
    let term = nonempty("TERM");
    let dumb = term.as_deref().is_none_or(|t| t == "dumb");
    let linux_console = term.as_deref() == Some("linux");
    let cursor = tty && !dumb;

    let enabled = match display.color {
        Some(ColorChoice::Always) => true,
        Some(ColorChoice::Never) => false,
        Some(ColorChoice::Auto) | None => nonempty("NO_COLOR").is_none() && cursor,
    };
    let color = enabled.then(|| {
        let colorterm = nonempty("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let term = term.as_deref().unwrap_or("");
        if colorterm == "truecolor" || colorterm == "24bit" || term.ends_with("-direct") {
            ColorDepth::TrueColor
        } else if term.contains("256color")
            || nonempty("TERM_PROGRAM").as_deref() == Some("Apple_Terminal")
        {
            ColorDepth::Ansi256
        } else {
            ColorDepth::Basic
        }
    });

    let background = match display.theme {
        Theme::Dark => Some(Background::Dark),
        Theme::Light => Some(Background::Light),
        Theme::Auto => nonempty("COLORFGBG").and_then(|v| parse_colorfgbg(&v)),
    };

    let unicode = match display.glyphs {
        Glyphs::Unicode => true,
        Glyphs::Ascii => false,
        Glyphs::Auto => {
            let locale = nonempty("LC_ALL")
                .or_else(|| nonempty("LC_CTYPE"))
                .or_else(|| nonempty("LANG"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            (locale.contains("utf-8") || locale.contains("utf8")) && !linux_console
        }
    };

    let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| nonempty(name).is_some());

    Caps {
        tty,
        cursor,
        color,
        background,
        unicode,
        animation: display.animation && cursor,
        ssh,
    }
}

/// `COLORFGBG` is `fg;bg` (sometimes `fg;default;bg`) with ANSI color indexes.
fn parse_colorfgbg(value: &str) -> Option<Background> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    Some(match bg {
        0..=6 | 8 => Background::Dark,
        _ => Background::Light,
    })
}

/// stderr's terminal belongs to another process group: Vayu was started with `&` or moved to
/// the background with Ctrl+Z and `bg`. Without a controlling terminal this is false.
pub fn backgrounded() -> bool {
    // SAFETY: both calls only query the process and terminal state.
    let foreground = unsafe { libc::tcgetpgrp(libc::STDERR_FILENO) };
    foreground > 0 && foreground != unsafe { libc::getpgrp() }
}

/// A background write to stderr's terminal would stop the whole process with `SIGTTOU`
/// (`stty tostop`).
pub fn background_writes_stop() -> bool {
    // SAFETY: tcgetattr fills a `termios` struct for a terminal descriptor; on failure the
    // result is ignored.
    let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::tcgetattr(libc::STDERR_FILENO, &mut attrs) } == 0;
    ok && attrs.c_lflag & libc::TOSTOP != 0
}

/// Current width and height of the terminal on stderr.
pub fn size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ fills a `winsize` struct for a terminal descriptor; on failure the
    // zeroed struct is left untouched and we fall back to defaults.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        return (ws.ws_col, ws.ws_row);
    }
    let env = |name: &str, default: u16| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default)
    };
    (env("COLUMNS", 80), env("LINES", 24))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn display() -> Display {
        Display {
            ui: crate::cli::UiMode::Auto,
            color: None,
            theme: Theme::Auto,
            glyphs: Glyphs::Auto,
            animation: true,
            quiet: false,
            json: false,
        }
    }

    fn caps(display: &Display, tty: bool, vars: &[(&str, &str)]) -> Caps {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        detect_with(display, tty, &|name| map.get(name).cloned())
    }

    #[test]
    fn capable_local_terminal() {
        let c = caps(
            &display(),
            true,
            &[
                ("TERM", "xterm-256color"),
                ("LANG", "en_US.UTF-8"),
                ("COLORTERM", "truecolor"),
            ],
        );
        assert!(c.cursor && c.unicode && c.animation && !c.ssh);
        assert_eq!(c.color, Some(ColorDepth::TrueColor));
    }

    #[test]
    fn apple_terminal_over_ssh() {
        let c = caps(
            &display(),
            true,
            &[
                ("TERM", "xterm-256color"),
                ("LANG", "en_US.UTF-8"),
                ("SSH_CONNECTION", "1 2 3 4"),
            ],
        );
        assert!(c.ssh && c.cursor);
        assert_eq!(c.color, Some(ColorDepth::Ansi256));
        assert_eq!(c.frame_interval(), Duration::from_millis(500));
    }

    #[test]
    fn redirected_output_is_plain() {
        let c = caps(
            &display(),
            false,
            &[("TERM", "xterm-256color"), ("LANG", "C.UTF-8")],
        );
        assert!(!c.cursor && !c.animation);
        assert_eq!(c.color, None);
        let mut always = display();
        always.color = Some(ColorChoice::Always);
        let c = caps(&always, false, &[("TERM", "xterm-256color")]);
        assert_eq!(c.color, Some(ColorDepth::Ansi256), "explicit color wins");
        assert!(!c.cursor, "color does not imply cursor control");
    }

    #[test]
    fn dumb_and_unknown_terminals() {
        let c = caps(&display(), true, &[("TERM", "dumb")]);
        assert!(!c.cursor && c.color.is_none());
        let c = caps(&display(), true, &[]);
        assert!(!c.cursor && c.color.is_none());
    }

    #[test]
    fn no_color_precedence() {
        let vars = [
            ("TERM", "xterm"),
            ("NO_COLOR", "1"),
            ("LANG", "en_US.UTF-8"),
        ];
        assert_eq!(caps(&display(), true, &vars).color, None);
        let mut auto = display();
        auto.color = Some(ColorChoice::Auto);
        assert_eq!(
            caps(&auto, true, &vars).color,
            None,
            "auto respects NO_COLOR"
        );
        let mut always = display();
        always.color = Some(ColorChoice::Always);
        assert_eq!(caps(&always, true, &vars).color, Some(ColorDepth::Basic));
        let empty = [("TERM", "xterm"), ("NO_COLOR", "")];
        assert!(
            caps(&display(), true, &empty).color.is_some(),
            "empty NO_COLOR is ignored"
        );
    }

    #[test]
    fn linux_console_keeps_color_with_ascii_glyphs() {
        let c = caps(
            &display(),
            true,
            &[("TERM", "linux"), ("LANG", "en_US.UTF-8")],
        );
        assert_eq!(c.color, Some(ColorDepth::Basic));
        assert!(!c.unicode);
        let c = caps(&display(), true, &[("TERM", "xterm"), ("LANG", "C")]);
        assert!(!c.unicode && c.color.is_some());
    }

    #[test]
    fn terminal_matrix() {
        use ColorDepth::*;
        // (TERM, COLORTERM, expected color depth, Unicode glyphs with a UTF-8 locale)
        let matrix = [
            ("xterm", "", Basic, true),
            ("xterm-256color", "", Ansi256, true),
            ("xterm-256color", "truecolor", TrueColor, true),
            ("xterm-direct", "", TrueColor, true),
            ("xterm-kitty", "truecolor", TrueColor, true),
            ("alacritty", "truecolor", TrueColor, true),
            ("tmux-256color", "", Ansi256, true),
            ("tmux-256color", "24bit", TrueColor, true),
            ("screen", "", Basic, true),
            ("screen-256color", "", Ansi256, true),
            ("vt100", "", Basic, true),
            ("linux", "", Basic, false),
        ];
        for (term, colorterm, depth, unicode) in matrix {
            let mut vars = vec![("TERM", term), ("LANG", "en_US.UTF-8")];
            if !colorterm.is_empty() {
                vars.push(("COLORTERM", colorterm));
            }
            let c = caps(&display(), true, &vars);
            assert!(c.cursor && c.animation, "{term}: live display");
            assert_eq!(c.color, Some(depth), "{term} {colorterm}");
            assert_eq!(c.unicode, unicode, "{term}");
            let piped = caps(&display(), false, &vars);
            assert!(!piped.cursor && piped.color.is_none(), "{term} piped");
        }
    }

    #[test]
    fn background_detection() {
        assert_eq!(parse_colorfgbg("15;0"), Some(Background::Dark));
        assert_eq!(parse_colorfgbg("0;15"), Some(Background::Light));
        assert_eq!(parse_colorfgbg("12;default;7"), Some(Background::Light));
        assert_eq!(parse_colorfgbg("x"), None);
    }
}
