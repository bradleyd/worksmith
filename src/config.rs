//! Configuration: TOML files with project-over-global field-level merge, plus
//! discovery of `AGENTS.md` / `CLAUDE.md` project instructions walked up the
//! tree. Ported and re-generalized from rustopedia's `config.rs` (env prefix
//! and Rust/RAG fields dropped; source of truth is now TOML).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Merged Worksmith configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    /// Default model as `provider/model` (or bare `model` if one provider).
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub providers: HashMap<String, ProviderConfig>,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProviderConfig {
    /// `openai-compat` (default) or `anthropic` (later).
    #[serde(rename = "type", default = "default_provider_kind")]
    pub kind: String,
    pub base_url: String,
    /// Env var holding the API key. Optional (vLLM needs none).
    #[serde(default)]
    pub api_key_env: Option<String>,
}

fn default_provider_kind() -> String {
    "openai-compat".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct AgentConfig {
    pub max_steps: Option<usize>,
    /// Validation retries before giving up (re-plan attempts).
    pub max_retries: Option<usize>,
    /// Identical-call count that triggers stuck detection.
    pub stuck_threshold: Option<u32>,
    /// Default validation command (`--until` overrides per-run).
    pub validate: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct ToolsConfig {
    pub bash_timeout_secs: Option<u64>,
}

/// A resolved provider + model, ready to build a client.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider: ProviderConfig,
    pub model: String,
    pub api_key: Option<String>,
}

impl Config {
    /// Load `~/.worksmith/config.toml`, then overlay `<project>/.worksmith/config.toml`.
    pub fn load(project_dir: &Path) -> Result<Config> {
        let mut cfg = Config::default();
        if let Some(global) = global_config_path()
            && global.exists() {
                let g: Config = read_toml(&global)?;
                cfg.merge(g);
            }
        let proj = project_dir.join(".worksmith").join("config.toml");
        if proj.exists() {
            let p: Config = read_toml(&proj)?;
            cfg.merge(p);
        }
        Ok(cfg)
    }

    /// Field-level merge; `other` (higher priority) wins where set.
    fn merge(&mut self, other: Config) {
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.temperature.is_some() {
            self.temperature = other.temperature;
        }
        if other.max_tokens.is_some() {
            self.max_tokens = other.max_tokens;
        }
        for (k, v) in other.providers {
            self.providers.insert(k, v);
        }
        if other.agent.max_steps.is_some() {
            self.agent.max_steps = other.agent.max_steps;
        }
        if other.tools.bash_timeout_secs.is_some() {
            self.tools.bash_timeout_secs = other.tools.bash_timeout_secs;
        }
    }

    pub fn max_steps(&self) -> usize {
        self.agent.max_steps.unwrap_or(50)
    }

    pub fn max_retries(&self) -> usize {
        self.agent.max_retries.unwrap_or(3)
    }

    pub fn stuck_threshold(&self) -> u32 {
        self.agent.stuck_threshold.unwrap_or(3)
    }

    pub fn validate_command(&self) -> Option<&str> {
        self.agent.validate.as_deref()
    }

    pub fn bash_timeout_secs(&self) -> u64 {
        self.tools.bash_timeout_secs.unwrap_or(120)
    }

    /// Resolve `cli_override` (or the configured default) into a provider +
    /// model + API key. Accepts `provider/model`, or a bare model when exactly
    /// one provider is configured.
    pub fn resolve_model(&self, cli_override: Option<&str>) -> Result<ResolvedModel> {
        let spec = cli_override
            .map(str::to_string)
            .or_else(|| self.model.clone())
            .context("no model configured (set `model` in config.toml or pass --model)")?;

        let (provider_name, model) = match spec.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => {
                if self.providers.len() == 1 {
                    let name = self.providers.keys().next().unwrap().clone();
                    (name, spec)
                } else {
                    bail!(
                        "model `{spec}` has no provider prefix and {} providers are configured; use `provider/model`",
                        self.providers.len()
                    );
                }
            }
        };

        let provider = self
            .providers
            .get(&provider_name)
            .with_context(|| format!("provider `{provider_name}` not found in config"))?
            .clone();

        let api_key = provider
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok());

        Ok(ResolvedModel { provider, model, api_key })
    }
}

fn read_toml(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
    Ok(cfg)
}

/// `~/.worksmith/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".worksmith").join("config.toml"))
}

/// `~/.worksmith` — the global state directory.
pub fn global_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".worksmith"))
}

/// Collect `AGENTS.md` / `CLAUDE.md` from `start` up to the filesystem root,
/// nearest-last so more-specific instructions land later in the prompt.
pub fn load_project_instructions(start: &Path) -> String {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    let mut dir = Some(start);
    let mut chain: Vec<&Path> = Vec::new();
    while let Some(d) = dir {
        chain.push(d);
        dir = d.parent();
    }
    // Walk root-first so nearer files (more specific) appear later.
    for d in chain.into_iter().rev() {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let p = d.join(name);
            if p.exists()
                && let Ok(text) = std::fs::read_to_string(&p) {
                    found.push((p, text));
                }
        }
    }

    if found.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (path, text) in found {
        out.push_str(&format!("\n# Project instructions ({})\n\n", path.display()));
        out.push_str(text.trim());
        out.push('\n');
    }
    out
}
