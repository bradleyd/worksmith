//! Configuration: TOML files with project-over-global field-level merge, plus
//! discovery of `AGENTS.md` / `CLAUDE.md` project instructions walked up the
//! tree. Ported and re-generalized from rustopedia's `config.rs` (env prefix
//! and Rust/RAG fields dropped; source of truth is now TOML).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::supervisor::{Mode, SupervisorConfig};

/// Merged Worksmith configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    /// Default model as `provider/model` (or bare `model` if one provider).
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub providers: HashMap<String, ProviderConfig>,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub agents: AgentsConfig,
    pub web: WebConfig,
}

/// Web search provider. Fetching a URL needs none of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebProvider {
    Brave,
    Tavily,
    Searxng,
}

impl WebProvider {
    pub fn parse(s: &str) -> Option<WebProvider> {
        match s.trim().to_ascii_lowercase().as_str() {
            "brave" => Some(WebProvider::Brave),
            "tavily" => Some(WebProvider::Tavily),
            "searxng" | "searx" => Some(WebProvider::Searxng),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct WebConfig {
    /// `brave` | `tavily` | `searxng`. Unset = web search is unavailable.
    pub provider: Option<String>,
    /// Env var holding the provider's API key (searxng needs none).
    pub api_key_env: Option<String>,
    /// Override the endpoint; required for a self-hosted searxng.
    pub base_url: Option<String>,
}

/// A resolved web-search setup.
pub struct ResolvedWeb {
    pub provider: Option<WebProvider>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct AgentsConfig {
    /// Max concurrently-running spawned workers.
    pub max: Option<usize>,
    /// `off` | `rules` | `model` — how closely workers are watched.
    pub supervisor: Option<String>,
    /// Seconds without a worker event before the supervisor nudges.
    pub stuck_timeout: Option<u64>,
    /// Nudges allowed before the supervisor stops the worker.
    pub max_nudges: Option<usize>,
    /// Identical tool calls (across the run) before the supervisor nudges.
    pub repeat_threshold: Option<u32>,
    /// Completion tokens a worker may spend before it's stopped.
    pub token_budget: Option<u32>,
    /// `auto` | `off` — may a bare `/spawn` fan out into several workers?
    pub fanout: Option<String>,
    /// After a fan-out group finishes, run a turn combining their results.
    pub synthesize: Option<bool>,
    /// Model spawned workers run on (`provider/model`). Unset = the session's
    /// model. This is the cheap half of a cheap-workers/smart-parent split.
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProviderConfig {
    /// `openai-compat` (default) or `anthropic` (later).
    #[serde(rename = "type", default = "default_provider_kind")]
    pub kind: String,
    pub base_url: String,
    /// Env var holding the API key. Optional (vLLM needs none).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// How this provider spells "don't think": `reasoning` (OpenRouter/OpenAI)
    /// or `chat-template` (vLLM/oMLX/llama.cpp). Guessed from the URL if unset.
    #[serde(default)]
    pub thinking_param: Option<String>,
}

fn default_provider_kind() -> String {
    "openai-compat".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct AgentConfig {
    pub max_steps: Option<usize>,
    /// Validation retries before giving up (re-plan attempts).
    pub max_retries: Option<usize>,
    /// Identical-call count that triggers stuck detection.
    pub stuck_threshold: Option<u32>,
    /// Default validation command (`--until` overrides per-run).
    pub validate: Option<String>,
    /// Approximate context window (tokens); compaction triggers at 75% of it.
    pub context_limit: Option<usize>,
    /// How many recent user turns to keep verbatim when compacting.
    pub keep_recent_turns: Option<usize>,
    /// `on` | `off` | a token budget — how much the model reasons before
    /// answering. Unset leaves the provider's default. `off` is fast mode:
    /// cheaper and quicker, with the validation loop expected to catch what
    /// deliberation would have. A number is the middle setting: reasoning gets
    /// its own cap so it cannot consume all of `max-tokens` and leave nothing
    /// for the answer.
    pub thinking: Option<ThinkingSetting>,
}

/// `thinking = "off"`, `thinking = "on"`, or `thinking = 2000`. TOML gives us
/// either a string or an integer, so accept both rather than making the budget
/// a quoted number.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ThinkingSetting {
    Budget(u32),
    Mode(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
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
        // `take` keeps `other`'s value when set, otherwise leaves ours alone.
        fn take<T>(mine: &mut Option<T>, theirs: Option<T>) {
            if theirs.is_some() {
                *mine = theirs;
            }
        }
        take(&mut self.agents.max, other.agents.max);
        take(&mut self.agents.supervisor, other.agents.supervisor);
        take(&mut self.agents.stuck_timeout, other.agents.stuck_timeout);
        take(&mut self.agents.max_nudges, other.agents.max_nudges);
        take(&mut self.agents.repeat_threshold, other.agents.repeat_threshold);
        take(&mut self.agents.token_budget, other.agents.token_budget);
        take(&mut self.agents.fanout, other.agents.fanout);
        take(&mut self.agents.synthesize, other.agents.synthesize);
        take(&mut self.agents.model, other.agents.model);
        take(&mut self.agent.max_steps, other.agent.max_steps);
        take(&mut self.agent.max_retries, other.agent.max_retries);
        take(&mut self.agent.stuck_threshold, other.agent.stuck_threshold);
        take(&mut self.agent.validate, other.agent.validate);
        take(&mut self.agent.context_limit, other.agent.context_limit);
        take(&mut self.agent.keep_recent_turns, other.agent.keep_recent_turns);
        take(&mut self.agent.thinking, other.agent.thinking);
        take(&mut self.tools.bash_timeout_secs, other.tools.bash_timeout_secs);
        take(&mut self.web.provider, other.web.provider);
        take(&mut self.web.api_key_env, other.web.api_key_env);
        take(&mut self.web.base_url, other.web.base_url);
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

    pub fn context_limit(&self) -> usize {
        self.agent.context_limit.unwrap_or(128_000)
    }

    pub fn keep_recent_turns(&self) -> usize {
        self.agent.keep_recent_turns.unwrap_or(6)
    }

    /// `None` = leave the provider alone (send no thinking field at all).
    pub fn thinking(&self) -> Option<crate::llm::Thinking> {
        use crate::llm::Thinking;
        match self.agent.thinking.as_ref()? {
            ThinkingSetting::Budget(n) if *n > 0 => Some(Thinking::Budget(*n)),
            ThinkingSetting::Budget(_) => Some(Thinking::Off),
            ThinkingSetting::Mode(v) => {
                let v = v.trim();
                if v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false") {
                    Some(Thinking::Off)
                } else if v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("true") {
                    Some(Thinking::On)
                } else {
                    // A bare number in quotes is still a budget.
                    v.parse::<u32>().ok().filter(|n| *n > 0).map(Thinking::Budget)
                }
            }
        }
    }

    pub fn agents_max(&self) -> usize {
        self.agents.max.unwrap_or(4)
    }

    /// The worker-supervision policy (PLAN.md §7).
    pub fn supervisor(&self) -> SupervisorConfig {
        let d = SupervisorConfig::default();
        SupervisorConfig {
            mode: self.agents.supervisor.as_deref().map(Mode::parse).unwrap_or(d.mode),
            idle_timeout: self
                .agents
                .stuck_timeout
                .map(Duration::from_secs)
                .unwrap_or(d.idle_timeout),
            max_nudges: self.agents.max_nudges.unwrap_or(d.max_nudges),
            repeat_threshold: self.agents.repeat_threshold.unwrap_or(d.repeat_threshold),
            token_budget: self.agents.token_budget,
        }
    }

    /// Whether a bare `/spawn` may be split across workers by the planner.
    pub fn fanout_auto(&self) -> bool {
        !matches!(
            self.agents.fanout.as_deref().map(str::trim).map(str::to_ascii_lowercase).as_deref(),
            Some("off") | Some("none") | Some("false")
        )
    }

    /// The model spawned workers run on, if it differs from the session's.
    pub fn agents_model(&self) -> Option<&str> {
        self.agents.model.as_deref()
    }

    /// Whether a finished fan-out group triggers a combining turn.
    pub fn synthesize(&self) -> bool {
        self.agents.synthesize.unwrap_or(true)
    }

    /// The configured web-search provider, if any.
    pub fn web(&self) -> ResolvedWeb {
        ResolvedWeb {
            provider: self.web.provider.as_deref().and_then(WebProvider::parse),
            api_key_env: self.web.api_key_env.clone(),
            base_url: self.web.base_url.clone(),
        }
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

/// `<global dir>/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    global_dir().map(|d| d.join("config.toml"))
}

/// `~/.worksmith` — the global state directory holding config, sessions, and
/// global memory. `WORKSMITH_HOME` overrides it, which is how the test suite
/// keeps its scratch sessions out of your real one.
pub fn global_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(GLOBAL_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|h| h.join(".worksmith"))
}

/// Env var that relocates the whole global state directory.
pub const GLOBAL_DIR_ENV: &str = "WORKSMITH_HOME";

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
