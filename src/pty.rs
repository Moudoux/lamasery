//! Run a program in a pseudo-terminal one row shorter than ours, emulate its
//! screen with `vt100`, and redraw it with lamasery's status bar on the
//! bottom row. The program runs in the alternate screen, so the session's
//! scrollback is untouched when it exits.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::terminal;

/// How often the bar is refreshed when nothing else changes.
const BAR_EVERY: Duration = Duration::from_millis(500);
/// Longest we hold back drawing while the program is mid-frame
/// (synchronized output).
const SYNC_TIMEOUT: Duration = Duration::from_millis(100);

/// Collects what the emulator can't show itself: replies to the program's
/// terminal queries, and sequences meant for the real terminal.
#[derive(Default)]
struct Cb {
    replies: Vec<u8>,
    passthrough: Vec<u8>,
    synced_since: Option<Instant>,
}

fn first_param(params: &[&[u16]]) -> u16 {
    params.first().and_then(|p| p.first()).copied().unwrap_or(0)
}

impl vt100::Callbacks for Cb {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.passthrough.push(0x07);
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.passthrough.extend_from_slice(b"\x1b]2;");
        self.passthrough.extend_from_slice(title);
        self.passthrough.push(0x07);
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        self.passthrough.extend_from_slice(b"\x1b]52;");
        self.passthrough.extend_from_slice(ty);
        self.passthrough.push(b';');
        self.passthrough.extend_from_slice(data);
        self.passthrough.push(0x07);
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // Color queries go to the real terminal; its answer arrives on our
        // stdin, which is forwarded to the program.
        if let [code @ (b"10" | b"11" | b"12"), b"?"] = params {
            self.passthrough.extend_from_slice(b"\x1b]");
            self.passthrough.extend_from_slice(code);
            self.passthrough.extend_from_slice(b";?\x07");
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, i2, c) {
            // Primary device attributes: a VT220 with ANSI color.
            (None, None, 'c') => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            (Some(b'>'), None, 'c') => self.replies.extend_from_slice(b"\x1b[>1;10;0c"),
            (None, None, 'n') => match first_param(params) {
                5 => self.replies.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let (r, c) = screen.cursor_position();
                    self.replies
                        .extend_from_slice(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                }
                _ => {}
            },
            // Synchronized output: hold drawing until the frame is complete.
            (Some(b'?'), None, 'h') if params.iter().any(|p| p.first() == Some(&2026)) => {
                self.synced_since = Some(Instant::now());
            }
            (Some(b'?'), None, 'l') if params.iter().any(|p| p.first() == Some(&2026)) => {
                self.synced_since = None;
            }
            // DECRQM: report synchronized output as supported, the rest unknown.
            (Some(b'?'), Some(b'$'), 'p') => {
                let mode = first_param(params);
                let state = if mode == 2026 { 2 } else { 0 };
                self.replies
                    .extend_from_slice(format!("\x1b[?{mode};{state}$y").as_bytes());
            }
            _ => {}
        }
    }
}

type Parser = vt100::Parser<Cb>;

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n).unwrap_or(std::num::NonZeroU16::MIN)
}

fn term_size() -> (u16, u16) {
    match terminal::size() {
        Ok((c, r)) if c > 0 && r > 1 => (c, r),
        _ => (80, 24),
    }
}

/// Wait until `fd` is readable or `timeout` passes.
fn readable(fd: i32, timeout: Duration) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, timeout.as_millis() as i32) };
    n > 0 && p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

/// Run `argv` in a pty until it exits. `bar(width)` returns the (already
/// styled) status line to draw on the bottom row; it must fit `width`.
pub fn run(
    argv: &[String],
    env: &BTreeMap<String, String>,
    bar: &mut dyn FnMut(usize) -> String,
) -> Result<ExitStatus> {
    let (prog, args) = argv.split_first().context("empty command")?;
    let (mut cols, mut rows) = term_size();

    let (mut master_fd, mut slave_fd) = (0, 0);
    let ws = winsize(rows - 1, cols);
    if unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("cannot open a pseudo-terminal");
    }
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
    unsafe { libc::fcntl(master_fd, libc::F_SETFD, libc::FD_CLOEXEC) };

    let mut child = unsafe {
        Command::new(prog)
            .args(args)
            .envs(env)
            // We emulate an xterm, whatever the real terminal is.
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
    }
    .with_context(|| format!("cannot run {prog}"))?;

    let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
        nz(rows - 1),
        nz(cols),
        0,
        Cb::default(),
    )));
    let dirty = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));

    // Program output → emulator.
    let reader = {
        let mut rd = File::from(master.try_clone()?);
        let mut replies = File::from(master.try_clone()?);
        let (parser, dirty, stop) = (parser.clone(), dirty.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            while !stop.load(Ordering::Relaxed) {
                if !readable(rd.as_raw_fd(), Duration::from_millis(50)) {
                    continue;
                }
                let n = match rd.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let r = {
                    let mut p = parser.lock().unwrap_or_else(|e| e.into_inner());
                    p.process(&buf[..n]);
                    std::mem::take(&mut p.callbacks_mut().replies)
                };
                if !r.is_empty() {
                    let _ = replies.write_all(&r);
                }
                dirty.store(true, Ordering::Relaxed);
            }
        })
    };

    // Our keyboard → program.
    let input = {
        let mut wr = File::from(master.try_clone()?);
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop.load(Ordering::Relaxed) {
                if !readable(0, Duration::from_millis(50)) {
                    continue;
                }
                let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
                if wr.write_all(&buf[..n as usize]).is_err() {
                    break;
                }
            }
        })
    };

    let mut out = io::stdout();
    // Alternate screen, cleared.
    let _ = out.write_all(b"\x1b[?1049h\x1b[H\x1b[2J");
    let mut prev: Option<vt100::Screen> = None;
    let mut last_bar = String::new();
    let mut bar_at = Instant::now() - BAR_EVERY;

    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        let (c, r) = term_size();
        if (c, r) != (cols, rows) {
            (cols, rows) = (c, r);
            let ws = winsize(rows - 1, cols);
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
            parser
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .screen_mut()
                .set_size(nz(rows - 1), nz(cols));
            prev = None;
        }
        let bar_due = bar_at.elapsed() >= BAR_EVERY;
        if bar_due {
            bar_at = Instant::now();
            let b = bar(cols as usize - 1);
            if b != last_bar {
                last_bar = b;
                dirty.store(true, Ordering::Relaxed);
            }
        }
        if dirty.load(Ordering::Relaxed) || prev.is_none() {
            let (screen, pass) = {
                let mut p = parser.lock().unwrap_or_else(|e| e.into_inner());
                let synced = p
                    .callbacks()
                    .synced_since
                    .is_some_and(|t| t.elapsed() < SYNC_TIMEOUT);
                if synced && prev.is_some() {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                dirty.store(false, Ordering::Relaxed);
                let pass = std::mem::take(&mut p.callbacks_mut().passthrough);
                (p.screen().clone(), pass)
            };
            let mut frame = Vec::with_capacity(8192);
            frame.extend_from_slice(b"\x1b[?2026h");
            frame.extend_from_slice(&pass);
            match &prev {
                Some(old) => frame.extend_from_slice(screen.state_diff(old).as_bytes()),
                None => {
                    frame.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
                    frame.extend_from_slice(screen.state_formatted().as_bytes());
                }
            }
            // The bar, then put the cursor and colors back where the program
            // left them.
            frame.extend_from_slice(format!("\x1b[0m\x1b[{rows};1H\x1b[2K").as_bytes());
            frame.extend_from_slice(last_bar.as_bytes());
            frame.extend_from_slice(b"\x1b[0m");
            frame.extend_from_slice(screen.cursor_state_formatted().as_bytes());
            frame.extend_from_slice(screen.attributes_formatted().as_bytes());
            frame.extend_from_slice(b"\x1b[?2026l");
            let _ = out.write_all(&frame);
            let _ = out.flush();
            prev = Some(screen);
        }
        std::thread::sleep(Duration::from_millis(8));
    };

    stop.store(true, Ordering::Relaxed);
    let _ = input.join();
    let _ = reader.join();
    // Undo any input modes the program turned on, leave the alternate
    // screen, and hide the cursor again for lamasery.
    let _ = out.write_all(
        b"\x1b[0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b>\x1b[0 q\x1b[?1049l\x1b[?25l",
    );
    let _ = out.flush();
    Ok(status)
}
