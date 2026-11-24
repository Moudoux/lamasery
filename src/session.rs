//! A live session with one model: stream its colored output with a status
//! bar underneath, and react to single-key shortcuts.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::cursor::{MoveTo, MoveToColumn};
use crossterm::event::KeyCode;
use crossterm::queue;
use crossterm::style::{Color, Stylize};
use crossterm::terminal::{Clear, ClearType};

use crate::config::{self, Config, ModelConfig, ToolConfig};
use crate::server::{self, LogBuf, Logs, STOP_TIMEOUT, Server, Status};
use crate::term::{self, ACCENT, ACCENT2, BAD, DIM, Item, OK, Pick, WARN};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PINK: Color = Color::Rgb {
    r: 0xf4,
    g: 0x72,
    b: 0xb6,
};
const TAG_COLORS: &[Color] = &[
    Color::Rgb {
        r: 0x60,
        g: 0xa5,
        b: 0xfa,
    },
    Color::Rgb {
        r: 0xc0,
        g: 0x84,
        b: 0xfc,
    },
    Color::Rgb {
        r: 0x5e,
        g: 0xea,
        b: 0xd4,
    },
    Color::Rgb {
        r: 0xfb,
        g: 0x92,
        b: 0x3c,
    },
    Color::Rgb {
        r: 0xa3,
        g: 0xe6,
        b: 0x35,
    },
    Color::Rgb {
        r: 0xf4,
        g: 0x72,
        b: 0xb6,
    },
    Color::Rgb {
        r: 0x93,
        g: 0xc5,
        b: 0xfd,
    },
];

/// What to do once a requested stop completes.
#[derive(Clone, Copy, PartialEq)]
enum AfterStop {
    Leave,
    Restart,
}

struct Session<'a> {
    cfg: &'a Config,
    model: &'a ModelConfig,
    logs: Logs,
    server: Option<Server>,
    status: Status,
    after_stop: Option<AfterStop>,
    leave: bool,
    printed: u64,
    show_logs: bool,
    hidden: u64,
    tps: Option<f64>,
    tick: usize,
}

/// Run `model` until the user stops it; returns to the caller's menu.
pub fn run(cfg: &Config, model: &ModelConfig) -> Result<()> {
    term::line("");
    if !model.model.is_file() {
        term::line(format!(
            "{} model file not found: {}",
            "✘".with(BAD),
            model.model.display()
        ));
        term::line("");
        return Ok(());
    }
    if server::port_in_use(&cfg.host, model.port) {
        term::line(format!(
            "{} port {} is already in use (another llama-server?). Stop it, or change this model's `port`.",
            "✘".with(BAD),
            model.port
        ));
        term::line("");
        return Ok(());
    }
    let (log_file, log_path) = match &cfg.log_dir {
        Some(dir) => match server::open_log(dir, &model.name) {
            Ok((f, p)) => (Some(f), Some(p)),
            Err(e) => {
                term::line(format!(
                    "{} cannot open log file in {}: {e}",
                    "!".with(WARN),
                    dir.display()
                ));
                (None, None)
            }
        },
        None => (None, None),
    };
    let mut s = Session {
        cfg,
        model,
        logs: Arc::new(Mutex::new(LogBuf::new(cfg.log_lines, log_file))),
        server: None,
        status: Status::Stopped,
        after_stop: None,
        leave: false,
        printed: 0,
        show_logs: true,
        hidden: 0,
        tps: None,
        tick: 0,
    };
    term::line(format!(
        "{} {}  {}",
        "▶".with(ACCENT).bold(),
        model.name.clone().bold(),
        "press ? for keys".with(DIM)
    ));
    if let Some(p) = log_path {
        term::line(format!("  {}", format!("log: {}", p.display()).with(DIM)));
    }
    s.start();
    let r = s.event_loop();
    // Never leave a server behind, whatever happened.
    s.shutdown();
    clear_status();
    term::line("");
    r
}

impl Session<'_> {
    fn start(&mut self) {
        self.tps = None;
        match Server::spawn(self.cfg, self.model, self.logs.clone()) {
            Ok(srv) => {
                self.server = Some(srv);
                self.status = Status::Loading;
            }
            Err(e) => {
                self.note(&format!("{e:#}"), BAD);
                self.status = Status::Exited(None);
            }
        }
    }

    fn event_loop(&mut self) -> Result<()> {
        while !self.leave {
            self.poll();
            self.flush_logs();
            self.draw_status();
            let Some(k) = term::key(Duration::from_millis(100))? else {
                continue;
            };
            let running = self.server.is_some();
            match k.code {
                _ if term::is_ctrl_c(&k) => {
                    if !running {
                        self.leave = true;
                    } else if self.status == Status::Stopping {
                        self.note("killing", BAD);
                        if let Some(srv) = self.server.as_mut() {
                            srv.kill();
                        }
                    } else {
                        self.request_stop(AfterStop::Leave);
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('s') => {
                    if running {
                        self.request_stop(AfterStop::Leave);
                    } else {
                        self.leave = true;
                    }
                }
                KeyCode::Char('r') => {
                    if running {
                        self.request_stop(AfterStop::Restart);
                    } else {
                        self.note("starting again", ACCENT2);
                        self.start();
                    }
                }
                KeyCode::Char('o') | KeyCode::Char('w') => self.open_web(),
                KeyCode::Char('t') => self.tools()?,
                KeyCode::Char('l') => self.toggle_logs(),
                KeyCode::Char('c') => {
                    let mut out = io::stdout();
                    let _ = queue!(
                        out,
                        Clear(ClearType::All),
                        Clear(ClearType::Purge),
                        MoveTo(0, 0)
                    );
                    let _ = out.flush();
                }
                KeyCode::Char('?') | KeyCode::Char('h') => self.help(),
                _ => {}
            }
        }
        Ok(())
    }

    /// Reap the server and react to status changes.
    fn poll(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        let Some(srv) = self.server.as_mut() else {
            return;
        };
        let (status, exit) = srv.poll();
        let elapsed = srv.started.elapsed();
        let was = self.status;
        self.status = status;
        if status == Status::Ready && was == Status::Loading {
            self.flush_logs();
            let url = server::base_url(&self.cfg.host, self.model.port);
            clear_status();
            term::line(format!(
                "{} {} in {:.1}s  {}  {}",
                "●".with(OK),
                "ready".with(OK).bold(),
                elapsed.as_secs_f64(),
                url.with(ACCENT2).underlined(),
                format!("model id: {}", self.model.model_id()).with(DIM),
            ));
        }
        if exit.is_none() {
            return;
        }
        self.server = None;
        self.flush_logs();
        match self.after_stop.take() {
            Some(AfterStop::Restart) => {
                self.note("restarting", ACCENT2);
                self.start();
            }
            Some(AfterStop::Leave) => {
                self.status = Status::Stopped;
                self.note("llama-server stopped", DIM);
                self.leave = true;
            }
            None => {
                self.note(
                    &format!(
                        "llama-server {} on its own. r to start again, q for the menu",
                        status.label()
                    ),
                    BAD,
                );
            }
        }
    }

    fn request_stop(&mut self, then: AfterStop) {
        if let Some(srv) = self.server.as_mut() {
            srv.stop();
            self.status = Status::Stopping;
            self.after_stop = Some(then);
            self.note("stopping (ctrl-c again to kill)", WARN);
        }
    }

    /// Stop the server synchronously, still streaming its last words.
    fn shutdown(&mut self) {
        let Some(srv) = self.server.as_mut() else {
            return;
        };
        srv.stop();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while srv.try_wait().is_none() {
            if Instant::now() > deadline {
                srv.kill();
                srv.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.server = None;
        self.flush_logs();
    }

    fn flush_logs(&mut self) {
        let new: Vec<String> = {
            let logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = (logs.total - self.printed) as usize;
            self.printed = logs.total;
            let n = fresh.min(logs.lines.len());
            logs.lines
                .iter()
                .skip(logs.lines.len() - n)
                .cloned()
                .collect()
        };
        if new.is_empty() {
            return;
        }
        for l in &new {
            if let Some(t) = parse_tps(l) {
                self.tps = Some(t);
            }
        }
        if !self.show_logs {
            self.hidden += new.len() as u64;
            return;
        }
        let mut out = io::stdout();
        let _ = queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine));
        for l in &new {
            let _ = write!(out, "{}\r\n", colorize(l));
        }
        let _ = out.flush();
    }

    /// The status-bar items for `status`.
    fn info(&self, status: Status, tick: usize) -> Vec<(String, Color)> {
        let (icon, color) = match status {
            Status::Ready => ("●".to_string(), OK),
            Status::Loading | Status::Stopping => (SPINNER[tick % SPINNER.len()].to_string(), WARN),
            _ => ("○".to_string(), BAD),
        };
        let mut info: Vec<(String, Color)> = vec![
            (format!("{icon} {}", status.label()), color),
            (self.model.name.clone(), Color::White),
            (server::base_url(&self.cfg.host, self.model.port), ACCENT2),
        ];
        info.extend(crate::mem::segments());
        if let Some(srv) = &self.server {
            info.push((
                format!("up {}", fmt_duration(srv.started.elapsed().as_secs())),
                DIM,
            ));
        }
        if let Some(t) = self.tps {
            info.push((format!("{t:.1} tok/s"), PINK));
        }
        info
    }

    /// The status bar shown under a tool running in the pty. Polls the
    /// server quietly; its status changes are reported once the tool exits.
    fn tool_bar(&mut self, w: usize, tool: &str, tick: usize) -> String {
        let status = match self.server.as_mut() {
            Some(srv) => srv.poll().0,
            None => self.status,
        };
        {
            let logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = logs.lines.iter().rev().find_map(|l| parse_tps(l)) {
                self.tps = Some(t);
            }
        }
        let info = self.info(status, tick);
        bar_line(&info, &[], &format!("in {tool}"), w)
    }

    fn draw_status(&self) {
        let mut info = self.info(self.status, self.tick);
        if !self.show_logs {
            info.push((format!("logs off ({} hidden)", self.hidden), WARN));
        }
        let keys: &[(&str, &str)] = if self.server.is_some() {
            &[
                ("o", "web"),
                ("t", "tools"),
                ("r", "restart"),
                ("l", "logs"),
                ("q", "stop"),
                ("?", "help"),
            ]
        } else {
            &[("r", "start"), ("q", "menu")]
        };

        let line = bar_line(&info, keys, "", term::width() - 1);
        let mut out = io::stdout();
        let _ = queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine));
        let _ = write!(out, "{line}");
        let _ = out.flush();
    }

    /// A lamasery message in the log stream.
    fn note(&self, msg: &str, color: Color) {
        if let Ok(mut l) = self.logs.lock() {
            l.file_only(&format!("[lamasery] {msg}"));
        }
        clear_status();
        term::line(format!(
            "{} {}",
            "⋯".with(color),
            msg.to_string().with(color)
        ));
    }

    fn toggle_logs(&mut self) {
        self.show_logs = !self.show_logs;
        if self.show_logs {
            let msg = format!("logs on ({} lines skipped)", self.hidden);
            self.hidden = 0;
            self.note(&msg, DIM);
        } else {
            self.note("logs off: press l to turn them back on", DIM);
        }
    }

    fn help(&self) {
        clear_status();
        let rows = [
            ("o", "open the llama.cpp web UI in your browser"),
            ("t", "launch a tool (Claude Code, ...) using this model"),
            ("r", "restart the server (start it, if it has exited)"),
            ("l", "pause / resume log output"),
            ("c", "clear the screen"),
            ("q", "stop the server and go back to the menu"),
            ("ctrl-c", "same as q; press again while stopping to kill"),
        ];
        for (k, d) in rows {
            term::line(format!(
                "  {}  {}",
                format!("{k:>6}").with(ACCENT).bold(),
                d
            ));
        }
    }

    fn open_web(&mut self) {
        if self.server.is_none() {
            self.note("the server isn't running (r to start it)", BAD);
            return;
        }
        let url = server::base_url(&self.cfg.host, self.model.port);
        let program = self
            .cfg
            .browser
            .clone()
            .unwrap_or_else(|| "xdg-open".into());
        let result = shell_words::split(&program)
            .map_err(anyhow::Error::from)
            .and_then(|argv| spawn_detached(&argv, std::slice::from_ref(&url), &BTreeMap::new()));
        match result {
            Ok(()) => self.note(&format!("opened {url} in your browser"), ACCENT2),
            Err(e) => self.note(&format!("{e:#}"), BAD),
        }
    }

    fn tools(&mut self) -> Result<()> {
        if self.cfg.tools.is_empty() {
            self.note(
                "no tools configured: add some under `tools:` in the config",
                BAD,
            );
            return Ok(());
        }
        if self.server.is_none() {
            self.note("the server isn't running (r to start it)", BAD);
            return Ok(());
        }
        clear_status();
        let items: Vec<Item> = self
            .cfg
            .tools
            .iter()
            .map(|t| {
                let hint = if t.new_window {
                    "opens in a new window"
                } else {
                    ""
                };
                Item::new(&t.name, hint, Color::Reset)
            })
            .collect();
        let title = format!("Launch with {}", self.model.name);
        let Pick::Index(i) = term::select(&title, &items, 0, "", "enter launch · esc cancel")?
        else {
            return Ok(());
        };
        let tool = self.cfg.tools[i].clone();
        if let Err(e) = self.launch(&tool) {
            self.note(&format!("{e:#}"), BAD);
        }
        Ok(())
    }

    fn launch(&mut self, tool: &ToolConfig) -> Result<()> {
        let ctx = server::fetch_ctx(&server::base_url(&self.cfg.host, self.model.port));
        let (argv, env) = tool_command(self.cfg, tool, self.model, ctx)?;
        if self.status == Status::Loading {
            self.note(
                "the model is still loading; requests wait until it's ready",
                WARN,
            );
        }
        if tool.new_window {
            let Some(mut full) = self.cfg.terminal.clone() else {
                bail!(
                    "{} wants a new window: set `terminal` in the config",
                    tool.name
                );
            };
            full.extend(argv);
            spawn_detached(&full, &[], &env)?;
            self.note(&format!("launched {} in a new window", tool.name), ACCENT2);
            return Ok(());
        }

        self.flush_logs();
        self.note(
            &format!(
                "launching {}: the server keeps running; exit it to come back here",
                tool.name
            ),
            ACCENT2,
        );
        let mut tick = 0;
        let status = crate::pty::run(&argv, &env, &mut |w| {
            tick += 1;
            self.tool_bar(w, &tool.name, tick)
        });
        // Skip what the server logged while the tool had the terminal.
        let skipped = {
            let logs = self.logs.lock().unwrap_or_else(|e| e.into_inner());
            let n = logs.total - self.printed;
            self.printed = logs.total;
            n
        };
        let st = status?;
        let mut msg = format!("back from {}", tool.name);
        if !st.success() {
            msg.push_str(&format!(" ({st})"));
        }
        if skipped > 0 {
            msg.push_str(&format!(", {skipped} server log lines skipped"));
        }
        term::line("");
        self.note(&msg, if st.success() { ACCENT2 } else { WARN });
        Ok(())
    }
}

/// One styled status line: `info` items separated by dots, then as many key
/// hints as fit, then `note` if there's room. Never wider than `w`.
fn bar_line(info: &[(String, Color)], keys: &[(&str, &str)], note: &str, w: usize) -> String {
    use std::fmt::Write as _;
    let info_len: usize = info.iter().map(|(s, _)| s.chars().count() + 3).sum();
    // Show as many key hints as fit.
    let mut n_keys = keys.len();
    let keys_len = |n: usize| {
        keys[..n]
            .iter()
            .map(|(k, d)| k.len() + d.len() + 3)
            .sum::<usize>()
    };
    while n_keys > 0 && info_len + 2 + keys_len(n_keys) > w {
        n_keys -= 1;
    }

    let mut out = String::new();
    let mut used = 0;
    for (i, (s, c)) in info.iter().enumerate() {
        let sep = if i == 0 { "" } else { " · " };
        let room = w.saturating_sub(used + sep.chars().count());
        if room == 0 {
            break;
        }
        let s = term::fit(s, room);
        used += sep.chars().count() + s.chars().count();
        let _ = write!(
            out,
            "{}{}",
            sep.with(DIM),
            if i == 0 {
                s.with(*c).bold()
            } else {
                s.with(*c)
            }
        );
    }
    if n_keys > 0 {
        out.push_str("  ");
        used += 2;
        for (k, d) in &keys[..n_keys] {
            used += k.len() + d.len() + 2;
            let _ = write!(
                out,
                " {} {}",
                k.with(ACCENT).bold(),
                d.to_string().with(DIM)
            );
        }
    }
    if !note.is_empty() && used + 2 + note.chars().count() <= w {
        let _ = write!(out, "  {}", note.to_string().with(DIM));
    }
    out
}

fn clear_status() {
    let mut out = io::stdout();
    let _ = queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine));
    let _ = out.flush();
}

/// Fill in a tool's templates for `m`. `ctx` is the server's live context
/// size, if known; otherwise it is estimated from `-c` / `-np`.
pub fn tool_command(
    cfg: &Config,
    tool: &ToolConfig,
    m: &ModelConfig,
    ctx: Option<u64>,
) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let base = server::base_url(&cfg.host, m.port);
    let mut vars = cfg.vars.clone();
    vars.extend([
        ("base_url".to_string(), base.clone()),
        ("api_base".to_string(), format!("{base}/v1")),
        (
            "host".to_string(),
            server::connect_host(&cfg.host).to_string(),
        ),
        ("port".to_string(), m.port.to_string()),
        ("model".to_string(), m.model_id()),
        ("model_name".to_string(), m.name.clone()),
    ]);
    let ctx = ctx.or_else(|| {
        let total: u64 = m.arg_value(&["-c", "--ctx-size"])?.parse().ok()?;
        let slots: u64 = m
            .arg_value(&["-np", "--parallel"])
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        (total > 0).then(|| total / slots.max(1))
    });
    if let Some(c) = ctx {
        vars.insert("ctx".into(), c.to_string());
    }
    if let Some(n) = m
        .arg_value(&["-n", "--predict", "--n-predict"])
        .filter(|n| n.parse::<u64>().is_ok_and(|n| n > 0))
    {
        vars.insert("max_output".into(), n.to_string());
    }
    let mut argv = config::split_args(std::slice::from_ref(&tool.command), &vars)?;
    if argv.is_empty() {
        bail!("tool {} has an empty command", tool.name);
    }
    argv.extend(tool.args.to_argv(&vars)?);
    // Leave out variables whose placeholders couldn't be filled (e.g. no
    // known context size) rather than passing a literal `${ctx}`.
    let env = tool
        .env
        .iter()
        .map(|(k, v)| (k.clone(), config::expand(v, &vars)))
        .filter(|(_, v)| !v.contains("${"))
        .collect();
    Ok((argv, env))
}

pub fn spawn_detached(
    argv: &[String],
    extra: &[String],
    env: &BTreeMap<String, String>,
) -> Result<()> {
    let (prog, rest) = argv.split_first().context("empty command")?;
    let mut child = Command::new(prog)
        .args(rest)
        .args(extra)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot run {prog}"))?;
    // Reap it so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn fmt_duration(s: u64) -> String {
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Generation speed from llama-server's timing lines, e.g.
/// `eval time = 1234.5 ms / 100 tokens (12.3 ms per token, 81.03 tokens per second)`.
fn parse_tps(line: &str) -> Option<f64> {
    if !line.contains("eval time") || line.contains("prompt eval") {
        return None;
    }
    let before = line.split("tokens per second").next()?;
    if before.len() == line.len() {
        return None;
    }
    before.rsplit([',', '(']).next()?.trim().parse().ok()
}

/// Color a llama-server log line: errors red, warnings yellow, milestones
/// green, timings pink, and the leading tag (`srv`, `slot`, `load_tensors:`)
/// in a stable color of its own.
fn colorize(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if line.starts_with("$ ") {
        return line.to_string().with(DIM).to_string();
    }
    if lower.contains("error") || lower.contains("failed") || lower.contains("fatal") {
        return line.to_string().with(BAD).to_string();
    }
    if lower.contains("warn") {
        return line.to_string().with(WARN).to_string();
    }
    if lower.contains("tokens per second") {
        return line.to_string().with(PINK).to_string();
    }
    if lower.contains("listening")
        || lower.contains("model loaded")
        || lower.contains("all slots are idle")
    {
        return line.to_string().with(OK).to_string();
    }
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let (tag, rest) = trimmed.split_at(end);
    if tag.is_empty() || tag.len() > 32 || rest.is_empty() {
        return line.to_string();
    }
    let key = tag.trim_end_matches(':');
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    let color = TAG_COLORS[(h.finish() % TAG_COLORS.len() as u64) as usize];
    format!("{indent}{}{rest}", tag.to_string().with(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tps() {
        let l = "       eval time =    1234.56 ms /   100 tokens (   12.35 ms per token,    81.03 tokens per second)";
        assert_eq!(parse_tps(l), Some(81.03));
        let p = "prompt eval time =  50.00 ms / 10 tokens (5.00 ms per token, 200.00 tokens per second)";
        assert_eq!(parse_tps(p), None);
        assert_eq!(parse_tps("srv  eval time: nothing"), None);
    }

    #[test]
    fn colorize_keeps_text() {
        let c = colorize("srv  load_model: loading");
        assert!(
            c.contains("srv") && c.ends_with("  load_model: loading"),
            "{c:?}"
        );
    }

    #[test]
    fn tool_templates() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/tools");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.yaml");
        std::fs::write(
            &p,
            "llama_server: x\nmodels:\n  - {name: M, model: a/qwen.gguf, port: 9001, args: {-c: 65536, -np: 2, -n: 4096}}\n  - {name: N, model: b.gguf}\ntools:\n  - name: T\n    command: echo\n    args: [\"${api_base}\", \"${model}\"]\n    env: {URL: \"${base_url}\", CTX: \"${ctx}\", OUT: \"${max_output}\"}\n",
        )
        .unwrap();
        let cfg = config::load(&p).unwrap();
        let (argv, env) = tool_command(&cfg, &cfg.tools[0], &cfg.models[0], None).unwrap();
        assert_eq!(argv, ["echo", "http://127.0.0.1:9001/v1", "qwen"]);
        assert_eq!(env["URL"], "http://127.0.0.1:9001");
        // -c is shared between -np slots; -n is the output cap.
        assert_eq!(env["CTX"], "32768");
        assert_eq!(env["OUT"], "4096");
        // The live value from the server wins.
        let (_, env) = tool_command(&cfg, &cfg.tools[0], &cfg.models[0], Some(100000)).unwrap();
        assert_eq!(env["CTX"], "100000");
        // Unknown values leave the variable unset.
        let (_, env) = tool_command(&cfg, &cfg.tools[0], &cfg.models[1], None).unwrap();
        assert!(!env.contains_key("CTX") && !env.contains_key("OUT"));
    }
}
