//! One running llama-server process: its log capture, health polling and
//! shutdown.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Config, ModelConfig};

/// Grace period between SIGTERM and SIGKILL.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Log lines shared between reader threads and the UI, mirrored to a file
/// with timestamps when one is open.
pub struct LogBuf {
    pub lines: VecDeque<String>,
    cap: usize,
    /// Lines ever pushed; lets the UI keep a scrolled view steady.
    pub total: u64,
    file: Option<File>,
}

/// Files larger than this are moved to `<name>.1` when opened.
const LOG_ROTATE_BYTES: u64 = 50 * 1024 * 1024;

impl LogBuf {
    pub fn new(cap: usize, file: Option<File>) -> Self {
        Self {
            lines: VecDeque::new(),
            cap,
            total: 0,
            file,
        }
    }

    /// Write to the log file without showing the line on screen.
    pub fn file_only(&mut self, line: &str) {
        if let Some(f) = self.file.as_mut()
            && writeln!(f, "{} {line}", timestamp("%H:%M:%S")).is_err()
        {
            self.file = None;
        }
    }

    pub fn push(&mut self, line: String) {
        self.file_only(&line);
        if self.lines.len() >= self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
        self.total += 1;
    }
}

pub type Logs = Arc<Mutex<LogBuf>>;

/// Open (append) the log file for a model, rotating it if it got big, and
/// write a header for this run.
pub fn open_log(dir: &Path, model_name: &str) -> std::io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let slug: String = model_name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let path = dir.join(format!("{slug}.log"));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        std::fs::rename(&path, path.with_extension("log.1"))?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(
        f,
        "\n===== {} lamasery: starting {model_name} =====",
        timestamp("%Y-%m-%d %H:%M:%S")
    )?;
    Ok((f, path))
}

/// Local time formatted with strftime.
fn timestamp(fmt: &str) -> String {
    // SAFETY: localtime_r/strftime with valid pointers and a bounded buffer.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        let mut buf = [0u8; 64];
        let cfmt = std::ffi::CString::new(fmt).unwrap_or_default();
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

pub fn log(logs: &Logs, line: impl Into<String>) {
    if let Ok(mut l) = logs.lock() {
        l.push(line.into());
    }
}

const HEALTH_LOADING: u8 = 0;
const HEALTH_READY: u8 = 1;

pub struct Server {
    child: Child,
    pub pid: u32,
    pub started: Instant,
    pub stopping_since: Option<Instant>,
    health: Arc<AtomicU8>,
    alive: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Loading,
    Ready,
    Stopping,
    Exited(Option<i32>),
    Killed(i32),
}

impl Status {
    pub fn label(self) -> String {
        match self {
            Status::Stopped => "stopped".into(),
            Status::Loading => "loading".into(),
            Status::Ready => "ready".into(),
            Status::Stopping => "stopping".into(),
            Status::Exited(Some(0)) => "exited".into(),
            Status::Exited(Some(c)) => format!("exited ({c})"),
            Status::Exited(None) => "exited".into(),
            Status::Killed(s) => format!("killed (signal {s})"),
        }
    }
}

pub fn status_from_exit(st: ExitStatus) -> Status {
    use std::os::unix::process::ExitStatusExt;
    match st.signal() {
        Some(sig) => Status::Killed(sig),
        None => Status::Exited(st.code()),
    }
}

/// Host to connect to for a server bound to `host`.
pub fn connect_host(host: &str) -> &str {
    match host {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" | "[::]" => "::1",
        h => h,
    }
}

/// `http://host:port`, with IPv6 hosts bracketed.
pub fn base_url(host: &str, port: u16) -> String {
    let h = connect_host(host);
    if h.contains(':') && !h.starts_with('[') {
        format!("http://[{h}]:{port}")
    } else {
        format!("http://{h}:{port}")
    }
}

/// Whether something already listens on the port.
pub fn port_in_use(host: &str, port: u16) -> bool {
    let h = connect_host(host);
    let h = h.trim_start_matches('[').trim_end_matches(']');
    std::net::TcpListener::bind((h, port)).is_err()
}

pub fn command_line(cfg: &Config, m: &ModelConfig) -> String {
    let mut parts = vec![cfg.llama_server.to_string_lossy().into_owned()];
    parts.extend(m.argv.iter().cloned());
    shell_words::join(parts)
}

/// Per-slot context size the running server actually uses (after `-fit`
/// may have shrunk it), from `/props`.
pub fn fetch_ctx(base: &str) -> Option<u64> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into();
    let body = agent
        .get(format!("{base}/props"))
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v["default_generation_settings"]["n_ctx"]
        .as_u64()
        .filter(|&n| n > 0)
}

impl Server {
    pub fn spawn(cfg: &Config, m: &ModelConfig, logs: Logs) -> Result<Self> {
        let mut cmd = Command::new(&cfg.llama_server);
        cmd.args(&m.argv)
            .envs(&m.env)
            .current_dir(&m.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group: Ctrl-C in a tool we launch in this terminal
            // must not reach the servers, and stop() can signal the group.
            .process_group(0);
        // SAFETY: prctl is async-signal-safe. If lamasery dies without
        // cleaning up, the kernel sends the server SIGTERM.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }

        log(&logs, format!("$ {}", command_line(cfg, m)));
        let mut child = cmd
            .spawn()
            .with_context(|| format!("cannot start {}", cfg.llama_server.display()))?;
        let pid = child.id();

        let alive = Arc::new(AtomicBool::new(true));
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, logs.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, logs.clone());
        }

        let health = Arc::new(AtomicU8::new(HEALTH_LOADING));
        spawn_health(base_url(&cfg.host, m.port), health.clone(), alive.clone());

        Ok(Self {
            child,
            pid,
            started: Instant::now(),
            stopping_since: None,
            health,
            alive,
        })
    }

    /// Ask the server to stop; `poll` escalates to SIGKILL after a timeout.
    pub fn stop(&mut self) {
        if self.stopping_since.is_none() {
            self.signal(libc::SIGTERM);
            self.stopping_since = Some(Instant::now());
        }
    }

    pub fn kill(&mut self) {
        self.signal(libc::SIGKILL);
    }

    fn signal(&self, sig: libc::c_int) {
        // SAFETY: plain syscall; negative pid targets the process group we
        // created at spawn time.
        unsafe {
            libc::kill(-(self.pid as libc::pid_t), sig);
        }
    }

    /// Current status; `Some(exit)` once the process is gone.
    pub fn poll(&mut self) -> (Status, Option<ExitStatus>) {
        if let Ok(Some(st)) = self.child.try_wait() {
            self.alive.store(false, Ordering::Relaxed);
            return (status_from_exit(st), Some(st));
        }
        if let Some(t) = self.stopping_since {
            if t.elapsed() > STOP_TIMEOUT {
                self.kill();
            }
            return (Status::Stopping, None);
        }
        let status = if self.health.load(Ordering::Relaxed) == HEALTH_READY {
            Status::Ready
        } else {
            Status::Loading
        };
        (status, None)
    }

    /// Block until the process exits (used on shutdown).
    pub fn wait(&mut self) -> Option<ExitStatus> {
        self.alive.store(false, Ordering::Relaxed);
        self.child.wait().ok()
    }

    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if self.child.try_wait().ok().flatten().is_none() {
            self.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_reader(stream: impl Read + Send + 'static, logs: Logs) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf);
                    // Progress output redraws with \r; keep the last state.
                    let text = text.trim_end_matches(['\n', '\r']);
                    let text = text.rsplit('\r').next().unwrap_or("");
                    log(&logs, clean(text));
                }
            }
        }
    });
}

/// Drop ANSI escape sequences and expand tabs so lines render predictably.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn spawn_health(base: String, health: Arc<AtomicU8>, alive: Arc<AtomicBool>) {
    thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{base}/health");
        while alive.load(Ordering::Relaxed) {
            let ready = matches!(agent.get(&url).call(), Ok(r) if r.status().as_u16() == 200);
            health.store(
                if ready { HEALTH_READY } else { HEALTH_LOADING },
                Ordering::Relaxed,
            );
            thread::sleep(Duration::from_millis(if ready { 2000 } else { 500 }));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(clean("\x1b[32mok\x1b[0m\tdone"), "ok    done");
    }

    #[test]
    fn urls() {
        assert_eq!(base_url("0.0.0.0", 8080), "http://127.0.0.1:8080");
        assert_eq!(base_url("::", 1), "http://[::1]:1");
    }
}
