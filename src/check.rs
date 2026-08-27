//! `worksmith config check` — what the effective config is, where every key
//! came from, and what is wrong with it.
//!
//! The source table is the whole point: a key whose value came from `default`
//! while a loaded file sets it is a merge bug (three have shipped — see
//! `Config::merge` and the `every_config_field_survives_the_merge` test), and
//! only this view shows it. The flags make the command CI-usable: any flag is
//! a non-zero exit, and the model call, the TUI, and the session machinery are
//! all out of the picture. The one exception is the optional `GET /v1/models`
//! for the context check, which is why it is the only network this subcommand
//! makes.
//!
//! The noun is `config`, the same one the planned `config schema --json` uses
//! (DOCS_PLAN.md §0): `check` reports on the running config, `schema` will
//! describe the static one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::config::{Config, global_config_path, read_toml_value};
use crate::trust::{Decision, TrustStore};

/// One line of the source table: the effective value and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// `default`, or the path of the file the value came from.
    pub from: String,
    /// The effective value, or `None` when unset and defaulting.
    pub value: Option<String>,
}

/// Everything `config check` needs: the effective config, the per-key
/// sources, one entry per config file, and the flags.
pub struct Check {
    pub config: Config,
    /// Effective value and source, keyed by the dotted key (`model`,
    /// `providers.omlx.base-url`, `agent.validate`, …).
    pub sources: BTreeMap<String, Source>,
    /// The last writer for each key, as its raw `toml::Value`, keyed the same
    /// way. The merge flag compares this against the merged config's
    /// `toml::Value` (the same comparison the `every_config_field_survives_the_merge`
    /// test makes); a string comparison would be wrong (see `flag_merge`).
    pub writers: BTreeMap<String, (String, toml::Value)>,
    pub files: Vec<FileStatus>,
    pub flags: Vec<String>,
}

/// One config file and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub path: PathBuf,
    /// `missing`, `ok`, or `error: …`.
    pub state: String,
    /// The project file only: the standing trust decision, and whether the
    /// file still has the content it was decided about.
    pub trusted: Option<bool>,
    pub changed_since_trusted: bool,
}

impl Check {
    /// Load the effective config (honouring the project trust decision, so the
    /// report describes what a run would actually use) and check it. `probe`
    /// opts in to the one network call (the `/v1/models` context check); a
    /// CI job that runs `config check` unattended should not be blocked on a
    /// server that may be a slow tunnel.
    pub async fn run(cwd: &Path, probe: bool) -> anyhow::Result<Check> {
        let mut check = Check::load(cwd)?;
        check.flag_model();
        check.flag_api_keys();
        check.flag_context(probe).await;
        check.flag_decisions_dir(cwd);
        Ok(check)
    }

    fn load(cwd: &Path) -> anyhow::Result<Check> {
        let config = Config::load(cwd)?;

        // The file lines. The global path is the same one `Config::load`
        // reads; the project one is what the trust store decides about. A
        // project config is applied only when the store says `Trust` for its
        // current content — the same test `Config::load` makes — so the
        // "applied" flag below is the one that decides whether the file is a
        // writer in the source table.
        let mut files = Vec::new();
        if let Some(global) = global_config_path() {
            let state = file_state(&global);
            files.push(FileStatus {
                path: global,
                state,
                trusted: None,
                changed_since_trusted: false,
            });
        }
        let project = cwd.join(".worksmith").join("config.toml");
        let (applied, trusted, changed) = {
            let store = TrustStore::load();
            if let Some(prompt) = crate::trust::prompt_for(cwd, &store) {
                let trusted =
                    matches!(store.decision_for(cwd, &prompt.fingerprint), Some(Decision::Trust));
                (trusted, Some(trusted), prompt.changed_since_trusted)
            } else {
                // No project config to decide about.
                (false, None, false)
            }
        };
        files.push(FileStatus {
            path: project.clone(),
            state: file_state(&project),
            trusted,
            changed_since_trusted: changed,
        });

        // The source table. Source attribution comes from the raw TOML of each
        // file, because a deserialized `Config` has already filled in defaults
        // and cannot tell "the file set this" from "it is the default". A file
        // is a writer only where it is actually applied — an untrusted project
        // config sets nothing. The last writer for a key is the merge's own
        // rule (project over global over default), and the effective value is
        // whatever that last writer set it to.
        let mut writers: BTreeMap<String, (String, toml::Value)> = BTreeMap::new();
        if let Some(global) = global_config_path()
            && global.exists()
            && let Ok(v) = read_toml_value(&global)
        {
            collect_writers(&v, &global.display().to_string(), &mut writers);
        }
        if applied
            && project.exists()
            && let Ok(v) = read_toml_value(&project)
        {
            collect_writers(&v, &project.display().to_string(), &mut writers);
        }
        // The source table: the last writer per key, as a string. The default
        // is the value when no file set the key.
        let mut sources = BTreeMap::new();
        for (key, (from, value)) in &writers {
            sources.insert(
                key.clone(),
                Source {
                    from: from.clone(),
                    value: Some(scalar_to_string(value)),
                },
            );
        }
        // A key no file set: the effective value is the default's, and the
        // source is `default`. We only need the keys the merged config has, so
        // we walk the merged config's set values and skip the ones a file set.
        let merged = toml::Value::try_from(&config).unwrap();
        fill_defaults(&merged, "", &writers, &mut sources);

        let mut check = Check {
            config,
            sources,
            writers,
            files,
            flags: Vec::new(),
        };
        check.flag_merge();
        Ok(check)
    }

    /// A key a loaded file set, but whose effective value is not that file's:
    /// the merge dropped it on the way in. The merge is field-level and
    /// hand-written, so the failure mode is a field added to the structs and
    /// forgotten in `Config::merge` — the value parses, survives validation,
    /// and is silently not applied.
    ///
    /// The comparison is the same one the `every_config_field_survives_the_merge`
    /// test makes: serialize the merged `Config` to `toml::Value` and compare
    /// it against the last writer's raw value, key by key. Comparing the raw
    /// TOML string against the stringified effective value would be wrong — a
    /// file's `thinking = 2000` is a TOML integer that deserializes to
    /// `ThinkingSetting::Budget(2000)`, and the two do not string-compare equal
    /// even though the value survived the merge.
    fn flag_merge(&mut self) {
        let merged = toml::Value::try_from(&self.config).unwrap();
        for (key, (file, value)) in &self.writers {
            let eff = lookup_path(&merged, key);
            match eff {
                // The merge carried the last writer's value: not a bug. A key a
                // later file overrode is attributed to that later file (the
                // merge's own rule), so the earlier file's value is not checked
                // and not flagged.
                Some(e) if e == *value => {}
                Some(e) => self.flags.push(format!(
                    "`{key}` is set in {file} to {value:?}, but the effective value is \
                     {e:?} — the merge dropped it"
                )),
                None => self.flags.push(format!(
                    "`{key}` is set in {file} to {value:?}, but it is absent from the \
                     effective config — the merge dropped it"
                )),
            }
        }
    }

    /// `model` names a provider no `[providers.*]` block defines.
    fn flag_model(&mut self) {
        let Some(spec) = &self.config.model else {
            return;
        };
        let name = match spec.split_once('/') {
            Some((p, _)) => p.to_string(),
            // A bare model name is legal with exactly one provider; with more
            // it cannot resolve, but `resolve_model` says that better.
            None if self.config.providers.len() == 1 => {
                self.config.providers.keys().next().unwrap().clone()
            }
            None => return,
        };
        if !self.config.providers.contains_key(&name) {
            let known: Vec<&str> = self.config.providers.keys().map(String::as_str).collect();
            let known = if known.is_empty() { "none" } else { &known.join(", ") };
            self.flags.push(format!(
                "`model` is `{spec}`, but provider `{name}` is not configured (configured: {known})"
            ));
        }
    }

    /// `api-key-env` names a variable that is not exported. Checked for every
    /// provider and for web, so a key that is set in one file but read from a
    /// variable the shell does not have is caught before the first 401.
    fn flag_api_keys(&mut self) {
        for (name, provider) in &self.config.providers {
            if let Some(var) = &provider.api_key_env
                && std::env::var(var).is_err()
            {
                self.flags.push(format!(
                    "provider `{name}`: `api-key-env` names `{var}`, which is not exported"
                ));
            }
        }
        if let Some(var) = &self.config.web.api_key_env
            && std::env::var(var).is_err()
        {
            self.flags.push(format!(
                "web: `api-key-env` names `{var}`, which is not exported"
            ));
        }
    }

    /// `context` disagrees with the server's `max_model_len`. Reuses
    /// `llm::warn_on_context_mismatch` for the comparison so the two cannot
    /// drift. The probe is the one network this subcommand makes, and it is
    /// opt-in: a CI job that runs `config check` unattended should not be
    /// blocked on a server that may be a slow tunnel.
    async fn flag_context(&mut self, probe: bool) {
        if !probe {
            return;
        }
        let resolved = match self.config.resolve_model(None) {
            Ok(r) => r,
            // An unresolvable model is flagged by `flag_model`; there is no
            // provider to probe.
            Err(_) => return,
        };
        let configured = resolved
            .settings
            .context
            .unwrap_or_else(|| self.config.context_limit());
        // Building a `reqwest::Client` is synchronous and reads the macOS
        // keychain (8 s cold), so this is the one place `config check` may be
        // slow; `--probe` opts in to that.
        let http = match reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                self.flags.push(format!(
                    "could not build the HTTP client for the /v1/models probe: {e}"
                ));
                return;
            }
        };
        let base = resolved.provider.base_url.trim_end_matches('/').to_string();
        let model = resolved.model.clone();
        let mismatch =
            crate::llm::warn_on_context_mismatch(&http, &base, &model, configured).await;
        if let Some(msg) = mismatch {
            self.flags.push(msg);
        }
    }

    /// `decisions-dir` points at a path git ignores: the decisions would be
    /// written and then invisible to version control, which is the point of
    /// filing them there.
    fn flag_decisions_dir(&mut self, cwd: &Path) {
        let dir = self.config.decisions_dir();
        let shown = dir.display().to_string();
        let abs = if dir.is_absolute() {
            dir
        } else {
            cwd.join(&dir)
        };
        if git_ignores(cwd, &abs) {
            self.flags.push(format!(
                "`decisions-dir` is `{shown}`, which git ignores — decisions filed there will not \
                 be committed"
            ));
        }
    }
}

fn file_state(path: &Path) -> String {
    if !path.exists() {
        return "missing".to_string();
    }
    match read_toml_value(path) {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("error: {e:#}"),
    }
}

/// Does git ignore `path`, as seen from `cwd`? Uses `git check-ignore`, which
/// applies the same rules (and the same `.gitignore` files) the repo uses.
/// A path outside the repo, or a missing git, is not ignored.
fn git_ignores(cwd: &Path, path: &Path) -> bool {
    use std::process::Command;
    let out = Command::new("git")
        .args(["check-ignore", "--quiet", "--", &path.display().to_string()])
        .current_dir(cwd)
        .output();
    match out {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// Collect a file's set values, keyed by path, for the source table and the
/// merge-bug flag. The path joins segments with `>`, not `.`: a `.` would
/// collide with the dots in a model name (`openrouter/qwen/qwen3.8-27b`), and
/// the merge flag must unflatten the path the same way it was flattened. The
/// value is the raw `toml::Value` (not its string), because the flag compares
/// it against the merged config's `toml::Value` and a string comparison would
/// be wrong (see `flag_merge`).
fn collect_writers(
    value: &toml::Value,
    from: &str,
    writers: &mut BTreeMap<String, (String, toml::Value)>,
) {
    collect_writers_walk(value, "", from, writers);
}

fn collect_writers_walk(
    value: &toml::Value,
    prefix: &str,
    from: &str,
    writers: &mut BTreeMap<String, (String, toml::Value)>,
) {
    let toml::Value::Table(table) = value else {
        return;
    };
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}>{k}")
        };
        match v {
            toml::Value::Table(_) => collect_writers_walk(v, &key, from, writers),
            scalar => {
                writers.insert(key, (from.to_string(), scalar.clone()));
            }
        }
    }
}

/// For each key the merged config has that no file set, record the default's
/// value and `from = "default"` in the source table. This is what makes the
/// report show every setting, not only the ones a file wrote.
fn fill_defaults(
    value: &toml::Value,
    prefix: &str,
    writers: &BTreeMap<String, (String, toml::Value)>,
    sources: &mut BTreeMap<String, Source>,
) {
    let toml::Value::Table(table) = value else {
        return;
    };
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}>{k}")
        };
        match v {
            toml::Value::Table(_) => fill_defaults(v, &key, writers, sources),
            scalar => {
                if !writers.contains_key(&key) {
                    sources.insert(
                        key,
                        Source {
                            from: "default".to_string(),
                            value: Some(scalar_to_string(scalar)),
                        },
                    );
                }
            }
        }
    }
}

/// The value at a `>`-joined path in a `toml::Value` (toml 1 has no
/// `get_path`). A key is a `>`-joined path because `collect_writers` flattens
/// nested tables that way; the lookup must unflatten it the same way. `>` is
/// the separator (not `.`) precisely so a dot in a model name does not split
/// the path.
fn lookup_path(value: &toml::Value, key: &str) -> Option<toml::Value> {
    let mut cur = value;
    for seg in key.split('>') {
        let toml::Value::Table(table) = cur else {
            return None;
        };
        cur = table.get(seg)?;
    }
    Some(cur.clone())
}

fn scalar_to_string(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        // An array or an empty table is not a scalar; the dotted walk recurses
        // on tables, so an array here is the only shape left. Render it as its
        // TOML so the source table does not lose it.
        other => other.to_string(),
    }
}

/// A key's display form: the internal path joins segments with `>` (so a dot in
/// a model name does not split it), but a report reads better with the usual
/// `.` between them. `>` cannot appear in a field name or model name, so this
/// is unambiguous.
fn display_key(key: &str) -> String {
    key.replace('>', ".")
}

/// Render the check as the command prints it: the file lines, the source
/// table, then the flags.
pub fn render(check: &Check) -> String {
    let mut out = String::new();
    for f in &check.files {
        let line = match (&f.trusted, f.changed_since_trusted) {
            (Some(true), true) => format!(
                "{}  loaded  trusted (but the file has changed since it was trusted)",
                f.path.display()
            ),
            (Some(true), false) => format!("{}  loaded  trusted", f.path.display()),
            (Some(false), _) => format!("{}  loaded  not trusted", f.path.display()),
            (None, _) => match f.state.as_str() {
                "ok" => format!("{}  loaded", f.path.display()),
                "missing" => format!("{}  not present", f.path.display()),
                _ => format!("{}  {}", f.path.display(), f.state),
            },
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');

    // Column widths from the data, so the table does not need a fixed one.
    let max_key = check
        .sources
        .keys()
        .map(|k| display_key(k).len())
        .max()
        .unwrap_or(0);
    let max_val = check
        .sources
        .values()
        .map(|s| s.value.as_deref().unwrap_or("(default)").len())
        .max()
        .unwrap_or(0);
    for (key, s) in &check.sources {
        let key = display_key(key);
        let val = s.value.as_deref().unwrap_or("(default)");
        out.push_str(&format!(
            "{key:<keyw$}  {val:<valw$}  from {}\n",
            s.from,
            keyw = max_key,
            valw = max_val
        ));
    }

    if check.flags.is_empty() {
        out.push_str("\nno problems found\n");
    } else {
        out.push_str("\nproblems:\n");
        for f in &check.flags {
            out.push_str(&format!("  ! {f}\n"));
        }
    }
    out
}

/// The check as JSON, for scripts. The shape mirrors the text output: the
/// files, the sources, and the flags.
pub fn render_json(check: &Check) -> String {
    let files: Vec<_> = check
        .files
        .iter()
        .map(|f| {
            let mut v = json!({
                "path": f.path.display().to_string(),
                "state": f.state,
            });
            if let Some(t) = f.trusted {
                v["trusted"] = json!(t);
                v["changed_since_trusted"] = json!(f.changed_since_trusted);
            }
            v
        })
        .collect();
    let sources: Vec<_> = check
        .sources
        .iter()
        .map(|(k, s)| {
            json!({
                "key": display_key(k),
                "from": s.from,
                "value": s.value,
            })
        })
        .collect();
    let v = json!({
        "files": files,
        "sources": sources,
        "flags": check.flags,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `collect_writers` flattens nested tables with `>` (not `.`) so a dot in a
    /// model name does not split the path, and records each scalar with its file.
    #[test]
    fn collect_writers_flattens_with_the_path_separator() {
        let v: toml::Value = toml::from_str(
            r#"
            model = "p/m"
            [providers.omlx]
            base-url = "http://x/v1"
            [models."openrouter/qwen/qwen3.8-27b"]
            temperature = 0.6
            "#,
        )
        .unwrap();
        let mut writers = BTreeMap::new();
        collect_writers(&v, "/global", &mut writers);

        assert_eq!(writers["model"].0, "/global");
        assert_eq!(
            writers["providers>omlx>base-url"].1,
            toml::Value::from("http://x/v1")
        );
        // A dot in the model name is part of the key, not a separator.
        assert_eq!(
            writers["models>openrouter/qwen/qwen3.8-27b>temperature"].1,
            toml::Value::from(0.6)
        );
    }

    /// `lookup_path` unflattens the `>`-joined path the same way
    /// `collect_writers` flattened it.
    #[test]
    fn lookup_path_unflattens_the_same_way() {
        let v: toml::Value =
            toml::from_str("[providers.omlx]\nbase-url = \"http://x/v1\"\n").unwrap();
        assert_eq!(
            lookup_path(&v, "providers>omlx>base-url"),
            Some(toml::Value::from("http://x/v1"))
        );
        assert_eq!(lookup_path(&v, "providers>omlx>nope"), None);
        // A path that walks past a scalar into nothing is not a table.
        assert_eq!(lookup_path(&v, "providers>omlx>base-url>deep"), None);
    }

    /// A key a file set, but whose effective value is not that file's: the merge
    /// dropped it. A key the file set and the effective config carries is not
    /// flagged. The comparison is on the `toml::Value`, not the string.
    #[test]
    fn flag_merge_flags_a_dropped_value() {
        // The file set `temperature` to 0.5, but the effective config has 0.7:
        // the merge lost the file's value. `model` the file set and the config
        // carries, so it must not be flagged.
        let config: Config = toml::from_str("model = \"p/m\"\ntemperature = 0.7\n").unwrap();
        let mut check = Check {
            config,
            sources: BTreeMap::new(),
            writers: BTreeMap::from([
                ("temperature".to_string(), ("/g".to_string(), toml::Value::from(0.5))),
                ("model".to_string(), ("/g".to_string(), toml::Value::from("p/m"))),
            ]),
            files: Vec::new(),
            flags: Vec::new(),
        };
        check.flag_merge();

        assert_eq!(check.flags.len(), 1, "only the dropped key is flagged: {:?}", check.flags);
        assert!(check.flags[0].contains("temperature"));
        assert!(!check.flags[0].contains("model"));
    }

    /// `fill_defaults` records the default's value and `from = "default"` for a
    /// key no file set, and leaves a key a file set alone (the source table
    /// already has the file's value for it).
    #[test]
    fn fill_defaults_records_the_default_for_unset_keys_only() {
        let merged: toml::Value = toml::from_str("model = \"p/m\"\ntemperature = 0.7\n").unwrap();
        let writers = BTreeMap::from([(
            "model".to_string(),
            ("/g".to_string(), toml::Value::from("p/m")),
        )]);
        let mut sources = BTreeMap::new();
        sources.insert(
            "model".to_string(),
            Source {
                from: "/g".into(),
                value: Some("p/m".into()),
            },
        );
        fill_defaults(&merged, "", &writers, &mut sources);

        // `temperature` no file set: it is the default's value.
        let t = &sources["temperature"];
        assert_eq!(t.from, "default");
        assert_eq!(t.value.as_deref(), Some("0.7"));
        // `model` a file set: `fill_defaults` did not clobber the source.
        assert_eq!(sources["model"].from, "/g");
    }

    /// A key a file set that is absent from the effective config is flagged as
    /// dropped, not silently skipped.
    #[test]
    fn flag_merge_flags_a_key_absent_from_the_effective_config() {
        let config: Config = Config::default(); // no `model` set
        let mut check = Check {
            config,
            sources: BTreeMap::new(),
            writers: BTreeMap::from([(
                "model".to_string(),
                ("/g".to_string(), toml::Value::from("p/m")),
            )]),
            files: Vec::new(),
            flags: Vec::new(),
        };
        check.flag_merge();

        assert_eq!(check.flags.len(), 1, "{:?}", check.flags);
        assert!(check.flags[0].contains("absent from the effective config"));
    }

    /// `display_key` turns the internal `>`-joined path into the usual `.` form
    /// for the report.
    #[test]
    fn display_key_uses_dots_for_the_report() {
        assert_eq!(display_key("providers>omlx>base-url"), "providers.omlx.base-url");
        // A dot already in a model name is left alone.
        assert_eq!(
            display_key("models>openrouter/qwen/qwen3.8-27b>temperature"),
            "models.openrouter/qwen/qwen3.8-27b.temperature"
        );
    }
}
