//! lamasery: pick a llama.cpp model, run it, and point your tools at it.

mod config;
mod server;
mod session;
mod term;

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crossterm::style::{Color, Stylize};

use config::Config;
use term::{ACCENT, ACCENT2, BAD, DIM, Item, Pick};

#[derive(Parser)]
#[command(
    version,
    about = "Pick a llama.cpp model, run it, and point your tools at it"
)]
struct Cli {
    /// Config file. Defaults to lamasery.yaml next to the binary.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Start this model right away (name or alias, any unique part of it).
    model: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write an example config (next to the binary unless --config is given).
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Validate the config and print the llama-server command for each model.
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Init { force }) => init(cli.config, force),
        Some(Cmd::Check) => check(cli.config),
        None => run(cli.config, cli.model),
    }
}

fn init(path: Option<PathBuf>, force: bool) -> Result<()> {
    let path = match path {
        Some(p) => p,
        None => std::env::current_exe()?
            .parent()
            .context("executable has no parent directory")?
            .join(config::CONFIG_NAMES[0]),
    };
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    std::fs::write(&path, config::EXAMPLE_CONFIG)
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("{} wrote {}", "✔".with(term::OK), path.display());
    Ok(())
}

fn check(path: Option<PathBuf>) -> Result<()> {
    let path = config::locate(path.as_deref())?;
    let cfg = config::load(&path)?;
    println!("{} {}", "config".with(DIM), cfg.path.display());
    let mut problems = 0;
    if !cfg.llama_server.is_file() {
        println!(
            "{} llama_server not found: {}",
            "✘".with(BAD),
            cfg.llama_server.display()
        );
        problems += 1;
    }
    for m in &cfg.models {
        println!(
            "\n{} {}",
            m.name.clone().with(ACCENT).bold(),
            format!(":{}", m.port).with(DIM)
        );
        if !m.model.is_file() {
            println!("  {} model not found: {}", "✘".with(BAD), m.model.display());
            problems += 1;
        }
        println!(
            "  {}",
            format!(
                "cd {}",
                shell_words::quote(&m.working_dir.to_string_lossy())
            )
            .with(DIM)
        );
        println!("  {}", server::command_line(&cfg, m));
    }
    let tools: Vec<&str> = cfg.tools.iter().map(|t| t.name.as_str()).collect();
    println!("\n{} {}", "tools".with(DIM), tools.join(", "));
    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    println!("{} looks good", "✔".with(term::OK));
    Ok(())
}

fn find_model(cfg: &Config, query: &str) -> Result<usize> {
    let q = query.to_lowercase();
    let exact = cfg.models.iter().position(|m| {
        m.name.to_lowercase() == q || m.alias.as_deref().is_some_and(|a| a.to_lowercase() == q)
    });
    if let Some(i) = exact {
        return Ok(i);
    }
    let hits: Vec<usize> = (0..cfg.models.len())
        .filter(|&i| {
            let m = &cfg.models[i];
            m.name.to_lowercase().contains(&q) || m.model_id().to_lowercase().contains(&q)
        })
        .collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => bail!("no model matches {query:?}"),
        _ => bail!(
            "{query:?} matches several models: {}",
            hits.iter()
                .map(|&i| cfg.models[i].name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn banner(cfg: &Config) {
    term::line("");
    term::line(format!(
        "  {}{}  {}",
        "lama".with(ACCENT).bold(),
        "sery".with(ACCENT2).bold(),
        "· llama.cpp launcher".with(DIM)
    ));
    term::line(format!("  {}", cfg.path.display().to_string().with(DIM)));
    if !cfg.llama_server.is_file() {
        term::line(format!(
            "  {} llama-server not found at {}",
            "✘".with(BAD),
            cfg.llama_server.display()
        ));
    }
    term::line("");
}

fn run(path: Option<PathBuf>, model: Option<String>) -> Result<()> {
    let path = config::locate(path.as_deref())?;
    let mut cfg = config::load(&path)?;
    let first = model.map(|q| find_model(&cfg, &q)).transpose()?;

    let _raw = term::RawGuard::new()?;
    banner(&cfg);
    let mut selected = first.unwrap_or(0);
    if let Some(i) = first {
        session::run(&cfg, &cfg.models[i])?;
    }

    loop {
        let mut items: Vec<Item> = cfg
            .models
            .iter()
            .map(|m| {
                let mut hint = format!(":{}  {}", m.port, m.model_id());
                let color = if m.model.is_file() {
                    Color::Reset
                } else {
                    hint.push_str("  (model file missing)");
                    BAD
                };
                Item::new(&m.name, hint, color)
            })
            .collect();
        let n = items.len();
        items.push(Item::new(
            "Edit config",
            "opens $EDITOR, reloads on save",
            DIM,
        ));
        items.push(Item::new("Quit", "", DIM));

        let pick = term::select(
            "Pick a model to run",
            &items,
            selected,
            "e",
            "  ↑↓ move · enter start · e edit config · q quit",
        )?;
        match pick {
            Pick::Index(i) if i < n => {
                selected = i;
                session::run(&cfg, &cfg.models[i])?;
            }
            Pick::Index(i) if i == n => selected = edit_config(&mut cfg, selected),
            Pick::Key('e') => selected = edit_config(&mut cfg, selected),
            _ => break,
        }
    }
    term::line(format!("{}", "bye 🦙".with(DIM)));
    Ok(())
}

/// Open the config in $VISUAL/$EDITOR and reload it. Returns the selection
/// to show next.
fn edit_config(cfg: &mut Config, selected: usize) -> usize {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let argv = shell_words::split(&editor).unwrap_or_default();
    let Some((prog, args)) = argv.split_first() else {
        term::line(format!("{} cannot parse editor {editor:?}", "✘".with(BAD)));
        return selected;
    };
    let status = term::cooked(|| Command::new(prog).args(args).arg(&cfg.path).status());
    if let Err(e) = status {
        term::line(format!("{} cannot run {prog}: {e}", "✘".with(BAD)));
        return selected;
    }
    match config::load(&cfg.path) {
        Ok(new) => {
            *cfg = new;
            term::line(format!("{} config reloaded", "✔".with(term::OK)));
            selected.min(cfg.models.len().saturating_sub(1))
        }
        Err(e) => {
            term::line(format!(
                "{} config has errors, keeping the old one:",
                "✘".with(BAD)
            ));
            for l in format!("{e:#}").lines() {
                term::line(format!("  {}", l.to_string().with(BAD)));
            }
            selected
        }
    }
}
