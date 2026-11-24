//! Small terminal toolkit: raw-mode guard, line output and an inline
//! arrow-key menu. Output stays in the normal screen so scrollback works.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Stylize};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

pub const ACCENT: Color = Color::Rgb {
    r: 0xc0,
    g: 0x84,
    b: 0xfc,
};
pub const ACCENT2: Color = Color::Rgb {
    r: 0x5e,
    g: 0xea,
    b: 0xd4,
};
pub const DIM: Color = Color::DarkGrey;
pub const OK: Color = Color::Rgb {
    r: 0x4a,
    g: 0xde,
    b: 0x80,
};
pub const WARN: Color = Color::Rgb {
    r: 0xfa,
    g: 0xcc,
    b: 0x15,
};
pub const BAD: Color = Color::Rgb {
    r: 0xf8,
    g: 0x71,
    b: 0x71,
};

/// Keeps the terminal in raw mode for its lifetime, restoring it on drop
/// and on panic.
pub struct RawGuard;

impl RawGuard {
    pub fn new() -> Result<Self> {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), Hide)?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let _ = execute!(io::stdout(), Show);
    let _ = terminal::disable_raw_mode();
}

/// Run `f` with a normal (cooked) terminal, e.g. for a child process.
pub fn cooked<T>(f: impl FnOnce() -> T) -> T {
    let _ = execute!(
        io::stdout(),
        MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        Show
    );
    let _ = terminal::disable_raw_mode();
    let r = f();
    let _ = terminal::enable_raw_mode();
    let _ = execute!(io::stdout(), Hide);
    r
}

pub fn width() -> usize {
    match terminal::size() {
        Ok((w, _)) if w > 0 => (w as usize).max(20),
        _ => 80,
    }
}

/// Print a line in raw mode (raw mode needs an explicit `\r`).
pub fn line(s: impl std::fmt::Display) {
    let mut out = io::stdout();
    let _ = queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine));
    let _ = write!(out, "{s}\r\n");
    let _ = out.flush();
}

/// Next key press within `timeout`, if any. Resize and release events are
/// swallowed.
pub fn key(timeout: Duration) -> Result<Option<KeyEvent>> {
    if event::poll(timeout)?
        && let Event::Key(k) = event::read()?
        && k.kind != KeyEventKind::Release
    {
        return Ok(Some(k));
    }
    Ok(None)
}

pub fn is_ctrl_c(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c')
}

/// Truncate to `max` characters, adding an ellipsis.
pub fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

pub struct Item {
    pub label: String,
    pub hint: String,
    pub color: Color,
}

impl Item {
    pub fn new(label: impl Into<String>, hint: impl Into<String>, color: Color) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
            color,
        }
    }
}

pub enum Pick {
    Index(usize),
    /// One of the extra hotkeys was pressed.
    Key(char),
    Cancel,
}

/// Inline menu. Arrows/j/k move, Enter or a digit picks, Esc/q cancels,
/// and any char in `hotkeys` returns `Pick::Key`. The menu collapses to a
/// one-line summary when done.
pub fn select(
    title: &str,
    items: &[Item],
    start: usize,
    hotkeys: &str,
    footer: &str,
) -> Result<Pick> {
    let mut sel = start.min(items.len().saturating_sub(1));
    let mut out = io::stdout();
    let label_w = items
        .iter()
        .map(|i| i.label.chars().count())
        .max()
        .unwrap_or(0);
    let lines = items.len() + 2;
    let mut drawn = false;

    let pick = loop {
        if drawn {
            queue!(
                out,
                MoveToColumn(0),
                MoveUp(lines as u16 - 1),
                Clear(ClearType::FromCursorDown)
            )?;
        }
        let w = width();
        write!(out, "{} {}\r\n", "?".with(ACCENT).bold(), title.bold())?;
        for (i, it) in items.iter().enumerate() {
            let num = if i < 9 {
                format!("{}", i + 1)
            } else {
                " ".into()
            };
            let label = format!("{:<label_w$}", it.label);
            let hint = fit(&it.hint, w.saturating_sub(label_w + 9));
            if i == sel {
                write!(
                    out,
                    "{} {} {}  {}\r\n",
                    " ❯".with(ACCENT).bold(),
                    num.with(DIM),
                    label.with(it.color).bold().underlined(),
                    hint.with(DIM)
                )?;
            } else {
                write!(
                    out,
                    "   {} {}  {}\r\n",
                    num.with(DIM),
                    label.with(it.color),
                    hint.with(DIM)
                )?;
            }
        }
        write!(out, "{}", fit(footer, w - 1).with(DIM))?;
        out.flush()?;
        drawn = true;

        let Some(k) = key(Duration::from_secs(1))? else {
            continue;
        };
        match k.code {
            _ if is_ctrl_c(&k) => break Pick::Cancel,
            KeyCode::Up | KeyCode::Char('k') => sel = (sel + items.len() - 1) % items.len(),
            KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1) % items.len(),
            KeyCode::Home => sel = 0,
            KeyCode::End => sel = items.len() - 1,
            KeyCode::Enter => break Pick::Index(sel),
            KeyCode::Esc | KeyCode::Char('q') => break Pick::Cancel,
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap() as usize;
                if (1..=items.len()).contains(&n) {
                    sel = n - 1;
                    break Pick::Index(sel);
                }
            }
            KeyCode::Char(c) if hotkeys.contains(c) => break Pick::Key(c),
            _ => {}
        }
    };

    // Collapse to a summary line.
    queue!(
        out,
        MoveToColumn(0),
        MoveUp(lines as u16 - 1),
        Clear(ClearType::FromCursorDown)
    )?;
    if let Pick::Index(i) = &pick {
        write!(
            out,
            "{} {} {}\r\n",
            "✔".with(OK),
            title.with(DIM),
            items[*i].label.clone().with(ACCENT2)
        )?;
    }
    out.flush()?;
    Ok(pick)
}
