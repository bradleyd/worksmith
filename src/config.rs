//! Configuration: TOML files with project-over-global field-level merge, plus
//! discovery of `AGENTS.md` / `CLAUDE.md` project instructions walked up the
//! tree. Ported and re-generalized from rustopedia's `config.rs` (env prefix
//! and Rust/RAG fields dropped; source of truth is now TOML).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::supervisor::{Mode, SupervisorConfig};

/// Merged Worksmith configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    /// Default model as `provider/model` (or bare `model` if one provider).
    pub model: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub providers: HashMap<String, ProviderConfig>,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub agents: AgentsConfig,
    pub web: WebConfig,
    pub tui: TuiConfig,
    /// Per-model settings, keyed by the same `provider/model` spec you put in
    /// `model`. One table because three things want the same key: what a model
    /// costs, how it should be sampled, and (later) which models `/model`
    /// offers. Splitting them would mean writing the same list three times.
    pub models: HashMap<String, ModelSettings>,
    /// Where pairing checkpoints file decisions, relative to the project root.
    /// Defaults inside `.worksmith/` because that is worksmith's own namespace
    /// in someone else's repo — `docs/` is this project's convention, not every
    /// project's. Set it to wherever a project already keeps its ADRs.
    pub decisions_dir: Option<PathBuf>,
    /// Set when this project has a config that has not been decided about. The
    /// caller asks and reloads; nothing was applied from it.
    #[serde(skip)]
    pub pending_trust: Option<crate::trust::TrustPrompt>,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    /// Consecutive failures of the *same* check before the supervisor steps in.
    pub stuck_check_threshold: Option<u32>,
    /// Completion tokens a worker may spend before it's stopped.
    pub token_budget: Option<u32>,
    /// Seconds a single model call may take before the worker is stopped as
    /// hung. Separate from `stuck-timeout`, which is about the loop spinning
    /// between steps.
    pub request_timeout: Option<u64>,
    /// `auto` | `off` — may a bare `/spawn` fan out into several workers?
    pub fanout: Option<String>,
    /// After a fan-out group finishes, run a turn combining their results.
    pub synthesize: Option<bool>,
    /// A success check every spawned worker must pass. Workers otherwise stop
    /// when the model says it is done, which is the failure the harness exists
    /// to prevent. Off by default: a fan-out validating concurrently in one
    /// working tree is the collision M11 fixes, so this is a deliberate choice
    /// rather than an inherited one.
    pub validate: Option<String>,
    /// Model spawned workers run on (`provider/model`). Unset = the session's
    /// model. This is the cheap half of a cheap-workers/smart-parent split.
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// The request field this provider uses for a *reasoning token budget*,
    /// when it has one. vLLM calls it `thinking_token_budget` and enforces it
    /// server-side. Opt-in per provider rather than inferred, because the other
    /// chat-template servers (llama.cpp, LM Studio, Ollama) have no such field
    /// and a strict one answers 400 for an unknown key.
    #[serde(default)]
    pub reasoning_budget_param: Option<String>,
    /// Seconds of silence *between stream chunks* before giving up on a
    /// request, per provider because the right number is the endpoint's.
    ///
    /// Not a total timeout: a legitimate generation can run for many minutes
    /// and capping the whole request would kill it. This bounds only the gap,
    /// so a server that accepts a request and then goes quiet cannot hang the
    /// session forever — which it otherwise can, since the supervisor's
    /// `request-timeout` watches spawned workers and never the main loop.
    ///
    /// The default is deliberately generous. Time-to-first-token is the long
    /// gap, and three workers queued on one local server routinely take
    /// minutes to produce it.
    #[serde(default)]
    pub stream_idle_timeout: Option<u64>,
    /// OpenRouter provider routing: `throughput` (fastest tokens/sec),
    /// `latency` (fastest to first token), or `price`. Sent as
    /// `provider: {"sort": …}`. Ignored by servers that don't route.
    #[serde(default)]
    pub sort: Option<String>,
}

fn default_provider_kind() -> String {
    "openai-compat".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    /// Offer the pairing checkpoint — the loop stopping to put a decision to
    /// you, tell you why it did something, or hand you the hard part. Off by
    /// default: it is an interrupt, and an interrupt nobody asked for is a
    /// nuisance. `/pair` toggles it for a session.
    pub pair: Option<bool>,
}

/// `thinking = "off"`, `thinking = "on"`, or `thinking = 2000`. TOML gives us
/// either a string or an integer, so accept both rather than making the budget
/// a quoted number.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ThinkingSetting {
    Budget(u32),
    Mode(String),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub bash_timeout_secs: Option<u64>,
}

/// What one model costs and how it wants to be sampled.
///
/// Sampling lives here rather than as a global default because the right
/// numbers are the model's, not the harness's: Qwen asks for 0.6 with thinking
/// on and 0.7 with it off, and those are Qwen's numbers, not universal ones.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct ModelSettings {
    /// USD per million input (prompt) tokens.
    pub input: Option<f64>,
    /// USD per million output (completion) tokens.
    pub output: Option<f64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    /// This model's context window in tokens. A global `agent.context-limit`
    /// cannot be right for a 32k local model and a 256k one at the same time,
    /// and being wrong here means compaction never fires: a 128k limit against
    /// a 32k model waits for a trigger the server rejects the request long
    /// before reaching.
    pub context: Option<usize>,
}

impl ModelSettings {
    /// Cost in USD for a request, or `None` when this model has no prices (a
    /// local model is free, and guessing a number would be worse than saying
    /// nothing).
    pub fn cost(&self, prompt_tokens: u64, completion_tokens: u64) -> Option<f64> {
        let (i, o) = (self.input?, self.output?);
        Some((prompt_tokens as f64 * i + completion_tokens as f64 * o) / 1_000_000.0)
    }

    /// Field-level merge; `other` (the project's) wins where set. Every field
    /// is optional, so a project block that names only `input` must not reset
    /// the global's `temperature`/`top-p`/`top-k` for the same model.
    fn merge(&mut self, other: ModelSettings) {
        if other.input.is_some() {
            self.input = other.input;
        }
        if other.output.is_some() {
            self.output = other.output;
        }
        if other.temperature.is_some() {
            self.temperature = other.temperature;
        }
        if other.top_p.is_some() {
            self.top_p = other.top_p;
        }
        if other.top_k.is_some() {
            self.top_k = other.top_k;
        }
        if other.context.is_some() {
            self.context = other.context;
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct TuiConfig {
    /// Two characters that, typed in quick succession, leave the composer for
    /// normal mode — the `inoremap jj <Esc>` habit. Empty string disables it.
    pub insert_escape: Option<String>,
    /// How quickly the two must follow each other, in milliseconds.
    pub insert_escape_ms: Option<u64>,
}

/// A resolved provider + model, ready to build a client.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider: ProviderConfig,
    pub model: String,
    pub api_key: Option<String>,
    /// Prices and sampling for this model, from `[models."provider/model"]`.
    pub settings: ModelSettings,
    /// Set when `api-key-env` names a variable that is not exported. The request
    /// will go out unauthenticated; the caller should warn.
    pub missing_key_env: Option<String>,
}

impl ProviderConfig {
    /// Field-level merge; `other` (the project's) wins where set.
    ///
    /// `base_url` and `kind` are not optional, so the project's block always
    /// carries them and they always win. Everything else only wins when the
    /// project actually said something — which is the whole point: naming a
    /// provider to change its URL must not silently drop how it spells
    /// "don't think".
    fn merge(&mut self, other: ProviderConfig) {
        self.kind = other.kind;
        self.base_url = other.base_url;
        if other.api_key_env.is_some() {
            self.api_key_env = other.api_key_env;
        }
        if other.thinking_param.is_some() {
            self.thinking_param = other.thinking_param;
        }
        if other.reasoning_budget_param.is_some() {
            self.reasoning_budget_param = other.reasoning_budget_param;
        }
        if other.stream_idle_timeout.is_some() {
            self.stream_idle_timeout = other.stream_idle_timeout;
        }
        if other.sort.is_some() {
            self.sort = other.sort;
        }
    }
}

impl Config {
    /// Load `~/.worksmith/config.toml`, then overlay `<project>/.worksmith/config.toml`.
    pub fn load(project_dir: &Path) -> Result<Config> {
        // First run: there is no ~/.worksmith yet, and every error downstream
        // ("set `model` in config.toml") names a file in a directory that does
        // not exist and whose path is never printed. Create it and leave an
        // annotated reference next to it, so "which config.toml, where?" has an
        // answer on disk.
        ensure_global_dir();

        let mut cfg = Config::default();
        if let Some(global) = global_config_path()
            && global.exists() {
                let g: Config = read_toml(&global)?;
                cfg.merge(g);
            }
        // A project config is code: it can set `agent.validate` (a shell command
        // the harness runs unattended) and point a provider's base-url anywhere.
        // Applying it because you happened to `cd` into a repo is the hole; it
        // is applied only once the user has said so, for this exact content.
        let store = crate::trust::TrustStore::load();
        let pending = crate::trust::prompt_for(project_dir, &store);
        if let Some(p) = &pending {
            match store.decision_for(project_dir, &p.fingerprint) {
                Some(crate::trust::Decision::Trust) => {
                    let proj: Config = read_toml(&p.config_path)?;
                    cfg.merge(proj);
                }
                // Undecided is treated as "not yet" rather than "yes": the
                // caller prompts and reloads. Nothing here can ask — `load` runs
                // before there is a UI, and is called from tests too.
                Some(crate::trust::Decision::Ignore) | None => {}
            }
        }
        cfg.pending_trust = match pending {
            Some(p) if store.decision_for(project_dir, &p.fingerprint).is_none() => Some(p),
            _ => None,
        };
        Ok(cfg)
    }

    /// Load with the project config applied unconditionally. For `--trust-project`
    /// and for tests: an explicit "I already decided" that cannot be reached by
    /// accident, unlike a default that trusts whatever is on disk.
    pub fn load_trusted(project_dir: &Path) -> Result<Config> {
        ensure_global_dir();
        let mut cfg = Config::default();
        if let Some(global) = global_config_path()
            && global.exists()
        {
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
    ///
    /// **Every `Option` field must appear here.** A field added to the structs
    /// and forgotten here parses, validates, and is then silently dropped — the
    /// project sets it, the effective config does not have it, and nothing
    /// complains. `deny_unknown_fields` cannot catch it, because the key is
    /// valid; only the merge loses it.
    ///
    /// That is not hypothetical: `agent.pair` and `decisions-dir` were both
    /// added and both missed, so `pair = true` in a project config did nothing
    /// for two days and every pairing experiment in that time ran against a
    /// feature that was switched off. `every_config_field_survives_the_merge`
    /// exists so the next one fails a test instead of a dogfooding session.
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
        // Field by field, like everything else — and like this file's own header
        // promises. Whole-entry replacement meant a project block that named
        // only `base-url` silently deleted the global's `thinking-param` and
        // `reasoning-budget-param`, so the dialect fell back to a guess and the
        // reasoning budget was dropped. The warning that surfaced said the
        // provider "has no reasoning budget", which was true of the effective
        // config and not of what anyone had written.
        for (k, v) in other.providers {
            match self.providers.get_mut(&k) {
                Some(mine) => mine.merge(v),
                None => {
                    self.providers.insert(k, v);
                }
            }
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
        take(&mut self.agents.stuck_check_threshold, other.agents.stuck_check_threshold);
        take(&mut self.agents.token_budget, other.agents.token_budget);
        take(&mut self.agents.request_timeout, other.agents.request_timeout);
        take(&mut self.agents.fanout, other.agents.fanout);
        take(&mut self.agents.synthesize, other.agents.synthesize);
        take(&mut self.agents.validate, other.agents.validate);
        take(&mut self.agents.model, other.agents.model);
        take(&mut self.agent.max_steps, other.agent.max_steps);
        take(&mut self.agent.max_retries, other.agent.max_retries);
        take(&mut self.agent.stuck_threshold, other.agent.stuck_threshold);
        take(&mut self.agent.validate, other.agent.validate);
        take(&mut self.agent.context_limit, other.agent.context_limit);
        take(&mut self.agent.keep_recent_turns, other.agent.keep_recent_turns);
        take(&mut self.agent.thinking, other.agent.thinking);
        take(&mut self.agent.pair, other.agent.pair);
        take(&mut self.decisions_dir, other.decisions_dir);
        take(&mut self.tools.bash_timeout_secs, other.tools.bash_timeout_secs);
        take(&mut self.web.provider, other.web.provider);
        take(&mut self.web.api_key_env, other.web.api_key_env);
        take(&mut self.web.base_url, other.web.base_url);
        for (k, v) in other.models {
            // Field by field, like `providers` above: a project block that names
            // only `input` must not delete the global's `temperature`, `top-p`,
            // and `top-k` for the same model. Whole-entry replacement meant a
            // project that tuned one sampling number silently reset the rest to
            // the model's defaults.
            match self.models.get_mut(&k) {
                Some(mine) => mine.merge(v),
                None => {
                    self.models.insert(k, v);
                }
            }
        }
        take(&mut self.tui.insert_escape, other.tui.insert_escape);
        take(&mut self.tui.insert_escape_ms, other.tui.insert_escape_ms);
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

    /// The two-key sequence that leaves the composer, and how fast it must be
    /// typed. `None` when disabled.
    pub fn insert_escape(&self) -> Option<(char, char, std::time::Duration)> {
        let seq = self.tui.insert_escape.clone().unwrap_or_else(|| "jj".to_string());
        let mut chars = seq.chars();
        let (a, b) = (chars.next()?, chars.next()?);
        if chars.next().is_some() {
            return None; // only a two-key sequence is supported
        }
        let ms = self.tui.insert_escape_ms.unwrap_or(300);
        Some((a, b, std::time::Duration::from_millis(ms)))
    }

    /// The check spawned workers must pass, if the config sets one.
    pub fn agents_validate(&self) -> Option<&str> {
        self.agents.validate.as_deref()
    }

    pub fn validate_command(&self) -> Option<&str> {
        self.agent.validate.as_deref()
    }

    pub fn context_limit(&self) -> usize {
        self.agent.context_limit.unwrap_or(128_000)
    }

    /// Where decisions are filed. Relative paths resolve against the project.
    pub fn decisions_dir(&self) -> PathBuf {
        self.decisions_dir.clone().unwrap_or_else(|| PathBuf::from(".worksmith/decisions"))
    }

    /// Whether pairing checkpoints start switched on.
    pub fn pair(&self) -> bool {
        self.agent.pair.unwrap_or(false)
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
                } else if let Some(e) = crate::llm::Effort::parse(v) {
                    Some(Thinking::Effort(e))
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
            stuck_check_threshold: self
                .agents
                .stuck_check_threshold
                .unwrap_or(d.stuck_check_threshold),
            token_budget: self.agents.token_budget,
            request_timeout: self
                .agents
                .request_timeout
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(600)),
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
            .with_context(|| {
                let path = global_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.worksmith/config.toml".to_string());
                let example = global_dir()
                    .map(|d| d.join(EXAMPLE_CONFIG).display().to_string())
                    .unwrap_or_default();
                format!(
                    "no model configured — set `model` in {path}, or pass --model\n\
                     an annotated starter config is at {example}: copy it to config.toml \
                     and set `model` plus the matching [providers.*] section"
                )
            })?;

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
            .with_context(|| {
                let known: Vec<&str> = self.providers.keys().map(String::as_str).collect();
                let path = global_config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.worksmith/config.toml".to_string());
                if known.is_empty() {
                    // The first-run case: naming the section to write beats
                    // reporting the absence of something they never wrote.
                    format!(
                        "provider `{provider_name}` not found — no providers are configured. \
                         Add a [providers.{provider_name}] section to {path} (see \
                         {} for a worked example)",
                        global_dir()
                            .map(|d| d.join(EXAMPLE_CONFIG).display().to_string())
                            .unwrap_or_default()
                    )
                } else {
                    format!(
                        "provider `{provider_name}` not found in {path} (configured: {})",
                        known.join(", ")
                    )
                }
            })?
            .clone();

        // A named-but-unset variable is almost always a mistake, and swallowing
        // it sends the request with no Authorization header at all. The server
        // answers 401 and the cause looks like anything but "you forgot to
        // export it". Carry the fact so the caller can say so.
        if let Some(sort) = &provider.sort
            && !matches!(sort.as_str(), "throughput" | "latency" | "price")
        {
            bail!(
                "provider `{provider_name}`: sort = \"{sort}\" is not one of \
                 throughput, latency, price"
            );
        }

        let missing_key_env = provider
            .api_key_env
            .as_ref()
            .filter(|env| std::env::var(env).is_err())
            .cloned();
        let api_key = provider
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok());

        let settings = self.models.get(&format!("{provider_name}/{model}")).cloned().unwrap_or_default();
        Ok(ResolvedModel { provider, model, api_key, missing_key_env, settings })
    }
}

/// The raw TOML of a config file, as a `toml::Value`. `read_toml` returns a
/// deserialized `Config`, which has already filled in defaults and so cannot
/// tell "the file set this key" from "it is the default" — the source table
/// in `check.rs` needs the raw value for exactly that distinction.
pub fn read_toml_value(path: &Path) -> Result<toml::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    // `from_str`, not `text.parse()`: the latter parses a single TOML *value*,
    // so a config file (a *document* of top-level keys) fails to parse. `Value`
    // deserializes a whole document into a table.
    let value: toml::Value = toml::from_str(&text).with_context(|| {
        format!("parsing config {} (is it valid TOML?)", path.display())
    })?;
    Ok(value)
}

fn read_toml(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| {
        // `deny_unknown_fields` catches typos, which is the point. But it reads
        // identically when the config is *newer than the binary* — a field this
        // version has never heard of. That happens on every upgrade where the
        // config moves first (a brew tap lagging main, a stale copy earlier on
        // PATH), and "unknown field" sends people to edit a config that is
        // correct. Name the version so the other explanation is visible.
        let hint = if e.to_string().contains("unknown field") {
            format!(
                "\n\nIf that setting is newer than this build, the config is ahead of the \
                 binary. This is worksmith {}; check `which -a worksmith` for an older copy \
                 earlier on your PATH.",
                env!("CARGO_PKG_VERSION")
            )
        } else {
            String::new()
        };
        anyhow::anyhow!("parsing config {}: {e}{hint}", path.display())
    })?;
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

/// The annotated project-config sample, dropped into a project's `.worksmith/`
/// the first time that directory is created. Short on purpose: a project config
/// overrides the global one field by field, so it only needs the handful of
/// settings that are genuinely per-project.
const PROJECT_EXAMPLE: &str = r#"# Worksmith, this project only.
#
# Everything here overrides ~/.worksmith/config.toml field by field, and all of
# it is optional. Copy this file to config.toml and uncomment what you need.
#
# Worksmith asks before using a project config the first time it sees one, and
# again whenever the file changes: it can run shell commands and redirect where
# your prompts are sent. `/trust` shows the current decision.

# The model for this project's sessions.
# model = "openrouter/qwen/qwen3.8-27b"

# [agent]
# validate = "cargo test"      # a turn is not done until this passes
# thinking = 2000              # cap reasoning, leaving room for an answer
# max-steps = 50

# [agents]
# model = "local/Qwen3.5-9B"   # workers on a cheaper or local model
# validate = "cargo check"     # per-worker check (read-only is safest today)
# max = 4

# A provider only this project uses. Local servers (oMLX, llama.cpp, vLLM,
# LM Studio) all speak the OpenAI-compatible API; point base-url at yours.
# [providers.local]
# type = "openai-compat"
# base-url = "http://127.0.0.1:8000/v1"
# thinking-param = "chat-template"

# [tools]
# bash-timeout-secs = 300
"#;

/// Create a project's `.worksmith/` directory, seeding the annotated sample the
/// first time. Centralised so memory and knowledge cannot each create the
/// directory in their own way and disagree about what lives in it.
pub fn ensure_project_dir(dir: &Path) -> std::io::Result<()> {
    let existed = dir.exists();
    std::fs::create_dir_all(dir)?;
    // Only on creation, and never over a real config: a project that has made
    // its choices should not find a sample appearing next to them.
    if !existed && dir.file_name().is_some_and(|n| n == ".worksmith") {
        let example = dir.join(EXAMPLE_CONFIG);
        if !example.exists() && !dir.join("config.toml").exists() {
            let _ = std::fs::write(&example, PROJECT_EXAMPLE);
        }
    }
    Ok(())
}

/// Env var that relocates the whole global state directory.
pub const GLOBAL_DIR_ENV: &str = "WORKSMITH_HOME";

/// The annotated reference config dropped into the global directory on first
/// run. Not `config.toml`: writing that would pick a model and a provider on the
/// user's behalf, and a wrong guess there is worse than an empty directory.
pub const EXAMPLE_CONFIG: &str = "config.example.toml";

/// The shipped example, compiled in so it cannot drift from the real one.
const EXAMPLE_CONFIG_BODY: &str = include_str!("../config.example.toml");

/// Create the global directory if it is missing, and seed the annotated example
/// beside it. Best-effort: a read-only or unwritable home should not stop a run
/// that passes `--model` and never needs the directory.
pub fn ensure_global_dir() {
    let Some(dir) = global_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let example = dir.join(EXAMPLE_CONFIG);
    if !example.exists() {
        let _ = std::fs::write(&example, EXAMPLE_CONFIG_BODY);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key a project config sets must survive the merge.
    ///
    /// Generic on purpose. The obvious version of this test is a hand-written
    /// list of accessors, which is the same list `merge` already has and fails
    /// the same way: add a field, forget both, learn nothing. This one
    /// serializes the project config and the merged result and compares them
    /// key by key, so a field added to the structs is covered without anyone
    /// remembering to cover it.
    ///
    /// It exists because `agent.pair` and `decisions-dir` were both added to
    /// the structs and both missed in `merge`. They parsed, validated, and were
    /// silently dropped: `pair = true` did nothing for two days, and every
    /// pairing experiment in that window ran against a switched-off feature.
    /// `deny_unknown_fields` cannot catch that — the key is valid; only the
    /// merge loses it.
    #[test]
    fn every_config_field_survives_the_merge() {
        // One value per key, all different from the defaults so a dropped field
        // cannot pass by coincidence.
        let project: Config = toml::from_str(
            r#"
            model = "p/m"
            temperature = 0.11
            max-tokens = 111
            decisions-dir = "docs/decisions"

            [providers.p]
            base-url = "http://p/v1"
            api-key-env = "P_KEY"
            thinking-param = "reasoning"
            reasoning-budget-param = "thinking_budget"
            stream-idle-timeout = 111
            sort = "latency"

            [agent]
            max-steps = 11
            max-retries = 11
            stuck-threshold = 11
            validate = "check me"
            context-limit = 11111
            keep-recent-turns = 11
            thinking = 111
            pair = true

            [agents]
            max = 11
            supervisor = "off"
            stuck-timeout = 11
            max-nudges = 11
            repeat-threshold = 11
            token-budget = 11111
            request-timeout = 11
            fanout = "off"
            synthesize = false
            validate = "worker check"
            model = "p/worker"

            [tools]
            bash-timeout-secs = 11

            [web]
            provider = "tavily"
            api-key-env = "W_KEY"
            base-url = "http://w"

            [tui]
            insert-escape = "jj"
            insert-escape-ms = 11

            [models."p/m"]
            input = 0.11
            output = 0.11
            temperature = 0.11
            top-p = 0.11
            top-k = 11
            context = 11111
            "#,
        )
        .expect("the fixture itself must parse");

        let mut merged = Config::default();
        merged.merge(project.clone());

        let want: toml::Value = toml::Value::try_from(&project).unwrap();
        let got: toml::Value = toml::Value::try_from(&merged).unwrap();
        let mut lost = Vec::new();
        walk(&want, &got, String::new(), &mut lost);
        assert!(lost.is_empty(), "these keys did not survive `merge`: {lost:?}");
    }

    /// Compare `want` against `got`, collecting the paths of anything missing or
    /// changed. Recurses so a table added later is covered too.
    fn walk(want: &toml::Value, got: &toml::Value, path: String, lost: &mut Vec<String>) {
        match want {
            toml::Value::Table(t) => {
                for (k, v) in t {
                    let here = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    match got.get(k) {
                        Some(g) => walk(v, g, here, lost),
                        None => lost.push(here),
                    }
                }
            }
            other => {
                if got != other {
                    lost.push(format!("{path} (got {got}, wanted {other})"));
                }
            }
        }
    }

    /// Naming a provider in a project config to change its URL must not delete
    /// the rest of it.
    ///
    /// Observed: a project block with only type/base-url/api-key-env wiped the
    /// global's `thinking-param` and `reasoning-budget-param`. The dialect then
    /// fell back to a guess off the hostname, the reasoning budget was dropped,
    /// and worksmith reported that the provider "has no reasoning budget" —
    /// true of the effective config, and not of anything anyone had written.
    #[test]
    fn a_project_provider_block_does_not_delete_the_globals_fields() {
        let mut global: Config = toml::from_str(
            r#"
            [providers.omlx]
            base-url = "http://127.0.0.1:8000/v1"
            api-key-env = "OMLX_API_KEY"
            thinking-param = "chat-template"
            reasoning-budget-param = "thinking_budget"
            sort = "latency"
            "#,
        )
        .unwrap();
        let project: Config = toml::from_str(
            r#"
            [providers.omlx]
            base-url = "http://127.0.0.1:8100/v1"
            "#,
        )
        .unwrap();
        global.merge(project);

        let p = &global.providers["omlx"];
        assert_eq!(p.base_url, "http://127.0.0.1:8100/v1", "the project's URL wins");
        assert_eq!(
            p.reasoning_budget_param.as_deref(),
            Some("thinking_budget"),
            "the budget field survives a block that did not mention it"
        );
        assert_eq!(p.thinking_param.as_deref(), Some("chat-template"));
        assert_eq!(p.api_key_env.as_deref(), Some("OMLX_API_KEY"));
        assert_eq!(p.sort.as_deref(), Some("latency"));
    }

    #[test]
    fn a_project_provider_still_overrides_what_it_does_set() {
        let mut global: Config = toml::from_str(
            r#"
            [providers.p]
            base-url = "http://a/v1"
            thinking-param = "chat-template"
            "#,
        )
        .unwrap();
        let project: Config = toml::from_str(
            r#"
            [providers.p]
            base-url = "http://b/v1"
            thinking-param = "reasoning"
            "#,
        )
        .unwrap();
        global.merge(project);
        let p = &global.providers["p"];
        assert_eq!(p.base_url, "http://b/v1");
        assert_eq!(p.thinking_param.as_deref(), Some("reasoning"));
    }

    /// The one the generic test cannot express: the *direction* of the merge.
    #[test]
    fn the_project_wins_and_unset_keys_leave_the_global_alone() {
        let mut global: Config =
            toml::from_str("model = \"g/m\"\n[agent]\nmax-steps = 99\npair = false\n").unwrap();
        let project: Config = toml::from_str("[agent]\npair = true\n").unwrap();
        global.merge(project);

        assert!(global.pair(), "the project's `pair` must win");
        assert_eq!(global.max_steps(), 99, "a key the project omits keeps the global value");
        assert_eq!(global.model.as_deref(), Some("g/m"));
    }

    /// Naming a model in a project config to set its price must not delete the
    /// rest of it. The `models` map was the one left on whole-entry replacement
    /// after the `providers` merge went field by field: a project block that set
    /// only `input`/`output` silently reset the global's `temperature`, `top-p`,
    /// and `top-k` for the same model back to the model's defaults.
    #[test]
    fn a_project_model_block_does_not_delete_the_globals_fields() {
        let mut global: Config = toml::from_str(
            r#"
            [models."p/m"]
            input = 0.5
            output = 1.5
            temperature = 0.6
            top-p = 0.95
            top-k = 20
            context = 32000
            "#,
        )
        .unwrap();
        let project: Config = toml::from_str(
            r#"
            [models."p/m"]
            input = 0.3
            output = 1.2
            "#,
        )
        .unwrap();
        global.merge(project);

        let m = &global.models["p/m"];
        assert_eq!(m.input, Some(0.3), "the project's price wins");
        assert_eq!(m.output, Some(1.2), "the project's price wins");
        assert_eq!(m.temperature, Some(0.6), "a field the block omits keeps the global value");
        assert_eq!(m.top_p, Some(0.95));
        assert_eq!(m.top_k, Some(20));
        assert_eq!(m.context, Some(32000));
    }

    /// A model block only in the project (no global entry) must still apply in
    /// full, not be dropped by the merge.
    #[test]
    fn a_project_only_model_block_applies() {
        let mut global: Config = Config::default();
        let project: Config =
            toml::from_str("[models.\"p/m\"]\ninput = 0.3\noutput = 1.2\ntemperature = 0.6\n")
                .unwrap();
        global.merge(project);

        let m = &global.models["p/m"];
        assert_eq!(m.input, Some(0.3));
        assert_eq!(m.output, Some(1.2));
        assert_eq!(m.temperature, Some(0.6));
    }
}
