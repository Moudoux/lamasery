//! RAM and VRAM usage, sampled in the background so drawing never waits.
//!
//! RAM comes from /proc/meminfo (used = MemTotal - MemAvailable). VRAM comes
//! from the amdgpu sysfs counters if present, otherwise from `nvidia-smi`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crossterm::style::Color;

use crate::term::{BAD, OK, WARN};

const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Default)]
struct Usage {
    used: u64,
    total: u64,
}

#[derive(Clone, Default)]
struct Snapshot {
    ram: Option<Usage>,
    /// One entry per GPU.
    vram: Vec<Usage>,
}

static SNAP: OnceLock<Mutex<Snapshot>> = OnceLock::new();

/// Start the sampler thread (once). The first sample is taken right away.
pub fn start() {
    if SNAP.get().is_some() {
        return;
    }
    let mut src = VramSource::detect();
    let _ = SNAP.set(Mutex::new(sample(&mut src)));
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(INTERVAL);
            let s = sample(&mut src);
            if let Some(m) = SNAP.get() {
                *m.lock().unwrap_or_else(|e| e.into_inner()) = s;
            }
        }
    });
}

/// Status-bar segments such as `vram 18.2/24.0G` and `ram 21.4/62.7G`,
/// colored by how full they are. Empty until `start` has run.
pub fn segments() -> Vec<(String, Color)> {
    let Some(m) = SNAP.get() else {
        return Vec::new();
    };
    let s = m.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut out = Vec::new();
    for (i, v) in s.vram.iter().enumerate() {
        let name = if s.vram.len() > 1 {
            format!("vram{i}")
        } else {
            "vram".into()
        };
        out.push(segment(&name, *v));
    }
    if let Some(r) = s.ram {
        out.push(segment("ram", r));
    }
    out
}

fn segment(name: &str, u: Usage) -> (String, Color) {
    const GIB: f64 = (1u64 << 30) as f64;
    let frac = u.used as f64 / u.total.max(1) as f64;
    let color = if frac >= 0.9 {
        BAD
    } else if frac >= 0.75 {
        WARN
    } else {
        OK
    };
    (
        format!(
            "{name} {:.1}/{:.1}G",
            u.used as f64 / GIB,
            u.total as f64 / GIB
        ),
        color,
    )
}

fn sample(src: &mut VramSource) -> Snapshot {
    Snapshot {
        ram: std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| parse_meminfo(&s)),
        vram: src.read(),
    }
}

fn parse_meminfo(s: &str) -> Option<Usage> {
    let field = |name: &str| -> Option<u64> {
        let line = s.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line[name.len()..].split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    };
    let total = field("MemTotal:")?;
    let avail = field("MemAvailable:")?;
    Some(Usage {
        used: total.saturating_sub(avail),
        total,
    })
}

enum VramSource {
    /// (used, total) file pairs from amdgpu.
    Sysfs(Vec<(PathBuf, PathBuf)>),
    NvidiaSmi,
    None,
}

impl VramSource {
    fn detect() -> Self {
        let mut pairs = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
            let mut cards: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
                })
                .collect();
            cards.sort();
            for c in cards {
                let used = c.join("device/mem_info_vram_used");
                let total = c.join("device/mem_info_vram_total");
                if used.is_file() && total.is_file() {
                    pairs.push((used, total));
                }
            }
        }
        if !pairs.is_empty() {
            return VramSource::Sysfs(pairs);
        }
        let mut s = VramSource::NvidiaSmi;
        if s.read().is_empty() {
            s = VramSource::None;
        }
        s
    }

    fn read(&mut self) -> Vec<Usage> {
        match self {
            VramSource::Sysfs(pairs) => pairs
                .iter()
                .filter_map(|(u, t)| {
                    let rd = |p: &PathBuf| std::fs::read_to_string(p).ok()?.trim().parse().ok();
                    Some(Usage {
                        used: rd(u)?,
                        total: rd(t)?,
                    })
                })
                .collect(),
            VramSource::NvidiaSmi => Command::new("nvidia-smi")
                .args([
                    "--query-gpu=memory.used,memory.total",
                    "--format=csv,noheader,nounits",
                ])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| parse_nvidia(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_default(),
            VramSource::None => Vec::new(),
        }
    }
}

/// `nvidia-smi` CSV lines of `used, total` in MiB.
fn parse_nvidia(s: &str) -> Vec<Usage> {
    s.lines()
        .filter_map(|l| {
            let (u, t) = l.split_once(',')?;
            let mib = |v: &str| v.trim().parse::<u64>().ok().map(|n| n << 20);
            Some(Usage {
                used: mib(u)?,
                total: mib(t)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo() {
        let s = "MemTotal:       65536000 kB\nMemFree:  1000 kB\nMemAvailable:   16384000 kB\n";
        let u = parse_meminfo(s).unwrap();
        assert_eq!(u.total, 65536000 * 1024);
        assert_eq!(u.used, (65536000 - 16384000) * 1024);
    }

    #[test]
    fn nvidia() {
        let v = parse_nvidia("18432, 24576\n512, 8192\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].used, 18432 << 20);
        assert_eq!(v[1].total, 8192 << 20);
        assert!(parse_nvidia("[N/A], [N/A]\n").is_empty());
    }
}
