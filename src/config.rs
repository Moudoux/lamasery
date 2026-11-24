//! YAML configuration: where llama-server lives, which models to serve, and
//! which coding tools can be pointed at a running model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const CONFIG_NAMES: &[&str] = &["lamasery.yaml", "lamasery.yml"];
pub const EXAMPLE_CONFIG: &str = include_str!("../lamasery.example.yaml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    llama_server: String,
    #[serde(default)]
    vars: BTreeMap<String, String>,
    #[serde(default = "default_host")]
    host: String,
    working_dir: Option<String>,
    #[serde(default)]
    common_args: Args,
    browser: Option<String>,
    terminal: Option<Vec<String>>,
    #[serde(default = "default_log_lines")]
    log_lines: usize,
    log_dir: Option<String>,
    tools_dir: Option<String>,
    #[serde(default)]
    models: Vec<RawModel>,
    #[serde(default)]
    tools: Vec<RawTool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    name: String,
    model: String,
    #[serde(default = "default_port")]
    port: u16,
    alias: Option<String>,
    working_dir: Option<String>,
    #[serde(default)]
    args: Args,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTool {
    name: String,
    command: String,
    #[serde(default)]
    args: Args,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    new_window: bool,
    download: Option<RawDownload>,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDownload {
    url: String,
    sha256: String,
}

/// llama-server arguments, written either as a map or as a list:
///
/// ```yaml
/// args:              # map: one flag per key
///   -c: 131072       #   flag with a value
///   --no-warmup: true  # bare flag; `false` or empty leaves it out
///   --lora: [a.gguf, b.gguf]  # list repeats the flag
/// args:              # list: shell-style words, several per entry
///   - -c 131072
///   - --no-warmup
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Args {
    List(Vec<String>),
    Map(serde_norway::Mapping),
}

impl Default for Args {
    fn default() -> Self {
        Args::List(Vec::new())
    }
}

impl Args {
    pub fn to_argv(&self, vars: &BTreeMap<String, String>) -> Result<Vec<String>> {
        let map = match self {
            Args::List(items) => return split_args(items, vars),
            Args::Map(map) => map,
        };
        let mut out = Vec::new();
        for (k, v) in map {
            let flag = scalar(k).with_context(|| format!("argument name {k:?} must be text"))?;
            let flag = expand(&flag, vars);
            let values: Vec<&serde_norway::Value> = match v {
                serde_norway::Value::Sequence(items) => items.iter().collect(),
                v => vec![v],
            };
            for v in values {
                match v {
                    serde_norway::Value::Null | serde_norway::Value::Bool(false) => {}
                    serde_norway::Value::Bool(true) => out.push(flag.clone()),
                    v => {
                        let val = scalar(v)
                            .with_context(|| format!("value of {flag} must be text or a number"))?;
                        out.push(flag.clone());
                        out.push(expand(&val, vars));
                    }
                }
            }
        }
        Ok(out)
    }
}

fn scalar(v: &serde_norway::Value) -> Option<String> {
    match v {
        serde_norway::Value::String(s) => Some(s.clone()),
        serde_norway::Value::Number(n) => Some(n.to_string()),
        serde_norway::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    8080
}
fn default_log_lines() -> usize {
    5000
}

#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub llama_server: PathBuf,
    pub host: String,
    pub vars: BTreeMap<String, String>,
    pub browser: Option<String>,
    pub terminal: Option<Vec<String>>,
    pub log_lines: usize,
    /// Where per-model log files go; `None` turns them off.
    pub log_dir: Option<PathBuf>,
    pub models: Vec<ModelConfig>,
    pub tools: Vec<ToolConfig>,
    /// Where each tool's own folder is (`<tools_dir>/<slug of its name>`).
    pub tools_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub name: String,
    pub model: PathBuf,
    pub port: u16,
    pub alias: Option<String>,
    pub working_dir: PathBuf,
    /// Fully expanded argv for llama-server, excluding the binary itself.
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl ModelConfig {
    /// Value of the last occurrence of any of `flags` in the argv.
    pub fn arg_value(&self, flags: &[&str]) -> Option<&str> {
        let mut found = None;
        for w in self.argv.windows(2) {
            if flags.contains(&w[0].as_str()) {
                found = Some(w[1].as_str());
            }
        }
        found
    }

    /// The model id that clients should request.
    pub fn model_id(&self) -> String {
        self.alias.clone().unwrap_or_else(|| {
            self.model
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.name.clone())
        })
    }
}

/// Tool definitions keep their templates unexpanded: `${base_url}`, `${port}`
/// and friends are only known once a model is picked.
#[derive(Debug, Clone)]
pub struct ToolConfig {
    pub name: String,
    pub command: String,
    pub args: Args,
    pub env: BTreeMap<String, String>,
    pub new_window: bool,
    /// The tool's own folder, `${tool_dir}` in its templates: downloads go in
    /// `dist/` there, and `files` are relative to it.
    pub dir: PathBuf,
    pub download: Option<crate::install::Download>,
    /// Paths relative to `dir` and their templates, written at each launch.
    pub files: BTreeMap<String, String>,
}

/// A tool's folder name: its name in lowercase, other characters as `-`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Find the config: explicit flag first, then next to the executable.
pub fn locate(flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p.to_path_buf());
    }
    let exe = std::env::current_exe().context("cannot determine executable path")?;
    let dir = exe.parent().context("executable has no parent directory")?;
    for name in CONFIG_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "no config found next to the binary ({}/lamasery.yaml).\n\
         Pass one with --config <file>, or create one with `lamasery init`.",
        dir.display()
    )
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let raw: RawConfig = serde_norway::from_str(&text)
        .with_context(|| format!("invalid config {}", path.display()))?;
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    // Vars may reference each other (in any order) and the environment.
    let mut vars = raw.vars.clone();
    for _ in 0..8 {
        let next: BTreeMap<_, _> = vars
            .iter()
            .map(|(k, v)| (k.clone(), expand(v, &vars)))
            .collect();
        if next == vars {
            break;
        }
        vars = next;
    }
    let resolve = |s: &str, base: &Path| -> PathBuf {
        let p = PathBuf::from(expand(s, &vars));
        let p = if p.is_absolute() { p } else { base.join(p) };
        // Drop `.` components so paths display cleanly.
        p.components().collect::<PathBuf>()
    };

    let llama_server = resolve(&raw.llama_server, &base);
    let global_wd = raw
        .working_dir
        .as_deref()
        .map(|w| resolve(w, &base))
        .unwrap_or_else(|| base.clone());
    let common = raw.common_args.to_argv(&vars).context("in common_args")?;

    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in raw.models {
        if !seen.insert(m.name.clone()) {
            bail!("duplicate model name {:?}", m.name);
        }
        let working_dir = m
            .working_dir
            .as_deref()
            .map(|w| resolve(w, &base))
            .unwrap_or_else(|| global_wd.clone());
        let model = resolve(&m.model, &working_dir);
        let mut argv = vec!["-m".to_string(), model.to_string_lossy().into_owned()];
        argv.extend(common.iter().cloned());
        argv.extend(
            m.args
                .to_argv(&vars)
                .with_context(|| format!("in model {:?}", m.name))?,
        );
        if let Some(a) = &m.alias {
            argv.extend(["--alias".into(), a.clone()]);
        }
        argv.extend([
            "--host".into(),
            raw.host.clone(),
            "--port".into(),
            m.port.to_string(),
        ]);
        let env = m
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand(v, &vars)))
            .collect();
        models.push(ModelConfig {
            name: m.name,
            model,
            port: m.port,
            alias: m.alias,
            working_dir,
            argv,
            env,
        });
    }

    let tools_dir = raw
        .tools_dir
        .as_deref()
        .map(|d| resolve(d, &base))
        .unwrap_or_else(|| base.join("tools"));
    let mut tools = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for t in raw.tools {
        let slug = slug(&t.name);
        if slug.is_empty() {
            bail!("tool name {:?} needs a letter or digit", t.name);
        }
        if !seen.insert(slug.clone()) {
            bail!(
                "tool {:?}: another tool already uses the folder {slug}",
                t.name
            );
        }
        for path in t.files.keys() {
            let p = Path::new(path);
            if p.is_absolute()
                || p.components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                bail!(
                    "tool {:?}: file {path:?} must be a plain path inside the tool's folder",
                    t.name
                );
            }
        }
        let download = t.download.map(|d| crate::install::Download {
            url: expand(&d.url, &vars),
            sha256: expand(&d.sha256, &vars),
        });
        if let Some(d) = &download
            && (d.sha256.len() != 64 || !d.sha256.chars().all(|c| c.is_ascii_hexdigit()))
        {
            bail!("tool {:?}: download sha256 must be 64 hex digits", t.name);
        }
        tools.push(ToolConfig {
            dir: tools_dir.join(&slug),
            name: t.name,
            command: t.command,
            args: t.args,
            env: t.env,
            new_window: t.new_window,
            download,
            files: t.files,
        });
    }

    let log_dir = match raw.log_dir.as_deref() {
        Some("") => None,
        Some(d) => Some(resolve(d, &base)),
        None => Some(base.join("logs")),
    };

    Ok(Config {
        path,
        llama_server,
        host: raw.host,
        vars,
        browser: raw.browser,
        terminal: raw.terminal,
        log_lines: raw.log_lines.max(100),
        log_dir,
        models,
        tools,
        tools_dir,
    })
}

/// Each list entry may hold several words (`-c 131072`); quoting works as in
/// a shell. Splitting happens before expansion so paths with spaces survive.
pub fn split_args(items: &[String], vars: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for item in items {
        let words =
            shell_words::split(item).with_context(|| format!("cannot parse argument {item:?}"))?;
        out.extend(words.iter().map(|w| expand(w, vars)));
    }
    Ok(out)
}

/// Expand `~/`, `${name}` and `$name` from `vars`, falling back to the
/// environment. Unknown names are left untouched.
pub fn expand(s: &str, vars: &BTreeMap<String, String>) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let s = if s == "~" {
        home.clone()
    } else if let Some(rest) = s.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        s.to_string()
    };
    let lookup = |name: &str| vars.get(name).cloned().or_else(|| std::env::var(name).ok());

    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let rest = &s[i + 1..];
        if let Some(inner) = rest.strip_prefix('{') {
            if let Some(end) = inner.find('}') {
                let name = &inner[..end];
                match lookup(name) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&s[i..i + 2 + end + 1]),
                }
                for _ in 0..end + 2 {
                    chars.next();
                }
                continue;
            }
        } else {
            let len = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            if len > 0 {
                let name = &rest[..len];
                match lookup(name) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&s[i..i + 1 + len]),
                }
                for _ in 0..len {
                    chars.next();
                }
                continue;
            }
        }
        out.push('$');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("m".into(), "/models".into()),
            ("port".into(), "8080".into()),
        ])
    }

    #[test]
    fn expands_vars() {
        assert_eq!(expand("${m}/a.gguf", &vars()), "/models/a.gguf");
        assert_eq!(expand("http://h:$port/v1", &vars()), "http://h:8080/v1");
        assert_eq!(expand("${nope_xyz}", &vars()), "${nope_xyz}");
        assert_eq!(expand("cost $5", &vars()), "cost $5");
        assert_eq!(expand("a$", &vars()), "a$");
    }

    #[test]
    fn splits_and_keeps_json() {
        let args = vec![
            "-c 131072".to_string(),
            r#"--chat-template-kwargs '{"preserve_thinking": true}'"#.to_string(),
            "--mmproj ${m}/x.gguf".to_string(),
        ];
        let out = split_args(&args, &vars()).unwrap();
        assert_eq!(
            out,
            vec![
                "-c",
                "131072",
                "--chat-template-kwargs",
                r#"{"preserve_thinking": true}"#,
                "--mmproj",
                "/models/x.gguf"
            ]
        );
    }

    #[test]
    fn map_args() {
        let yaml = r#"
-c: 131072
-fa: on
--no-warmup: true
--mmap: false
--chat-template-kwargs: '{"preserve_thinking": true}'
--lora: [a.gguf, "${m}/b.gguf"]
--temp: 0.6
"#;
        let args: Args = serde_norway::from_str(yaml).unwrap();
        assert_eq!(
            args.to_argv(&vars()).unwrap(),
            vec![
                "-c",
                "131072",
                "-fa",
                "on",
                "--no-warmup",
                "--chat-template-kwargs",
                r#"{"preserve_thinking": true}"#,
                "--lora",
                "a.gguf",
                "--lora",
                "/models/b.gguf",
                "--temp",
                "0.6"
            ]
        );
    }

    #[test]
    fn example_config_parses() {
        let raw: RawConfig = serde_norway::from_str(EXAMPLE_CONFIG).unwrap();
        assert!(!raw.models.is_empty());
        assert!(!raw.tools.is_empty());
    }
}
