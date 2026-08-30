//! The supervisor — the factory foreman (PLAN.md §7, M7).
//!
//! It doesn't do the work: it watches the event stream a worker already emits
//! and decides whether to nudge the worker back on track or pull it off the
//! floor. Detection is deterministic and free (no model in the loop of every
//! worker); the optional cheap-model observer is a later mode.
//!
//! [`Supervisor`] is a pure state machine — feed it events and idle ticks, get
//! back [`Action`]s — so it's testable without a runtime and reusable for the
//! single-agent loop later.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::event::Event;

/// What the supervisor decided to do about a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Inject this directive into the worker's input (steering).
    Nudge(String),
    /// Stop the worker; the string is the reason reported to the parent.
    Escalate(String),
}

/// `agents.supervisor` — how closely workers are watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// No watching at all.
    Off,
    /// Deterministic rules (idle, repeated calls, runaway spend, "I'm blocked").
    #[default]
    Rules,
}

impl Mode {
    /// Parse `off` / `rules` / `model`. `model` (the cheap-model observer) isn't
    /// built yet and behaves as `rules`; unknown values also fall back to rules.
    pub fn parse(s: &str) -> Mode {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => Mode::Off,
            _ => Mode::Rules,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub mode: Mode,
    /// No event from the worker for this long → nudge.
    pub idle_timeout: Duration,
    /// Nudges allowed before escalating instead.
    pub max_nudges: usize,
    /// Identical tool calls (across the whole worker run) that trigger a nudge.
    pub repeat_threshold: u32,
    /// Consecutive failures of the *same* check before nudging. Lower than
    /// `repeat_threshold`: a repeated tool call might still be gathering
    /// information, while a check that fails identically has already told the
    /// model everything it is going to.
    pub stuck_check_threshold: u32,
    /// Cumulative completion tokens before escalating. `None` = unlimited.
    pub token_budget: Option<u32>,
    /// How long a *single model call* may take before it is treated as hung.
    /// Deliberately not derived from `idle_timeout`: that one is about the loop
    /// spinning between steps, while this is about a server that never
    /// answered. Tying them together killed three local workers whose only
    /// crime was queueing behind each other on one machine.
    pub request_timeout: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Rules,
            idle_timeout: Duration::from_secs(120),
            max_nudges: 3,
            repeat_threshold: 4,
            stuck_check_threshold: 3,
            token_budget: None,
            // Generous: a cold pod, a queued local server, or a long prefill
            // are all normal and none of them are a hang.
            request_timeout: Duration::from_secs(600),
        }
    }
}

impl SupervisorConfig {
    pub fn is_on(&self) -> bool {
        self.mode != Mode::Off
    }
}

/// Per-worker watcher state.
pub struct Supervisor {
    cfg: SupervisorConfig,
    /// Identical `name::arguments` call counts, spanning re-plan attempts.
    calls: HashMap<String, u32>,
    /// The last failing check, normalised. `agent.rs` already counts
    /// *consecutive* validation failures, but counts any of them: three
    /// different errors is a model working through a problem, three identical
    /// ones is a model going nowhere, and it treats them alike. Measured on
    /// qwen3.5-4B: one task failed the same assertion nine times across valid
    /// edits, and what finally stopped it was the `bash` rule above noticing
    /// the same command five times, several minutes in.
    last_check: Option<String>,
    /// How many times in a row that same check has failed.
    same_check: u32,
    /// Call signatures already flagged (nudge each one only once).
    flagged: HashSet<String>,
    nudges: usize,
    completion_tokens: u32,
    blocked_flagged: bool,
    /// Is a model call in flight? While one is, silence means "waiting", not
    /// "stuck".
    in_flight: bool,
    /// Consecutive idle deadlines that passed while a call was in flight.
    in_flight_idles: usize,
}

impl Supervisor {
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self {
            cfg,
            calls: HashMap::new(),
            flagged: HashSet::new(),
            last_check: None,
            same_check: 0,
            nudges: 0,
            completion_tokens: 0,
            blocked_flagged: false,
            in_flight: false,
            in_flight_idles: 0,
        }
    }

    pub fn idle_timeout(&self) -> Duration {
        self.cfg.idle_timeout
    }

    pub fn is_on(&self) -> bool {
        self.cfg.is_on()
    }

    pub fn nudges(&self) -> usize {
        self.nudges
    }

    /// Observe one worker event.
    pub fn observe(&mut self, event: &Event) -> Option<Action> {
        if !self.cfg.is_on() {
            return None;
        }
        match event {
            Event::ModelCallStarted => {
                self.in_flight = true;
                self.in_flight_idles = 0;
                None
            }
            Event::ModelCallFinished => {
                self.in_flight = false;
                self.in_flight_idles = 0;
                None
            }
            Event::Usage { completion_tokens, .. } => {
                self.completion_tokens += completion_tokens;
                match self.cfg.token_budget {
                    Some(budget) if self.completion_tokens > budget => Some(Action::Escalate(
                        format!(
                            "token budget exceeded ({} > {budget} completion tokens)",
                            self.completion_tokens
                        ),
                    )),
                    _ => None,
                }
            }
            Event::ToolCall { name, arguments, .. } => {
                let sig = format!("{name}::{arguments}");
                let count = self.calls.entry(sig.clone()).or_insert(0);
                *count += 1;
                if *count < self.cfg.repeat_threshold || self.flagged.contains(&sig) {
                    return None;
                }
                self.flagged.insert(sig);
                let count = *count;
                self.act(format!(
                    "You have called `{name}` {count} times with identical arguments and learned \
                     nothing new. Stop repeating it: state what you actually know, then take a \
                     different approach to finish the task."
                ))
            }
            Event::Validation { ok, detail } => {
                if *ok {
                    self.last_check = None;
                    self.same_check = 0;
                    return None;
                }
                let sig = normalise_check(detail);
                if self.last_check.as_deref() != Some(sig.as_str()) {
                    self.last_check = Some(sig);
                    self.same_check = 1;
                    return None;
                }
                self.same_check += 1;
                if self.same_check < self.cfg.stuck_check_threshold {
                    return None;
                }
                // Reset rather than latch: if the model changes approach and
                // gets a *different* failure, that is progress and it should
                // get the same number of attempts again.
                let n = self.same_check;
                self.same_check = 0;
                self.act(format!(
                    "The check has now failed {n} times in a row with the same output, so \
                     nothing you have changed since is reaching it. Stop adjusting the same \
                     lines. Read the failure again, say what it actually proves about the \
                     current code, and change something else."
                ))
            }
            Event::AssistantMessage { text } => {
                if self.blocked_flagged || !looks_blocked(text) {
                    return None;
                }
                self.blocked_flagged = true;
                self.act(
                    "You said you are blocked. You are a background worker — nobody will answer \
                     you. Make the most reasonable assumption, say what you assumed, and finish \
                     the task."
                        .to_string(),
                )
            }
            _ => None,
        }
    }

    /// Called when the worker has emitted nothing for `idle_timeout`.
    pub fn on_idle(&mut self) -> Option<Action> {
        if !self.cfg.is_on() {
            return None;
        }
        // Waiting on the model is not the same as being stuck. A slow or queued
        // request emits nothing for its whole duration, and a nudge cannot help
        // it: steering is drained at the top of the *next* step, so the message
        // arrives after the call it was meant to interrupt. All a nudge does
        // here is spend one of `max_nudges`, which is how three workers sharing
        // one local server died on a 20s timeout during prefill.
        //
        // A request can still hang, and stopping it is the one action that
        // helps, so escalate after a long multiple of the timeout.
        if self.in_flight {
            self.in_flight_idles += 1;
            let waited = self.cfg.idle_timeout * self.in_flight_idles as u32;
            if waited < self.cfg.request_timeout {
                return None;
            }
            return Some(Action::Escalate(format!(
                "no response from the model for {}s",
                waited.as_secs()
            )));
        }
        let secs = self.cfg.idle_timeout.as_secs();
        self.act(format!(
            "No progress for {secs}s. Briefly state where you are, then take a concrete next \
             step — or finish with what you have."
        ))
    }

    /// Turn an intervention into a nudge, or escalate once nudges run out.
    fn act(&mut self, directive: String) -> Option<Action> {
        if self.nudges >= self.cfg.max_nudges {
            return Some(Action::Escalate(format!(
                "still off track after {} nudges",
                self.nudges
            )));
        }
        self.nudges += 1;
        Some(Action::Nudge(directive))
    }
}

/// Strip the parts of a check's output that move on their own, so "the same
/// failure twice" can be recognised.
///
/// Two of them matter here and both come from real output. A traceback carries
/// the scratch directory it ran in, which differs per run, and a `line 32` that
/// **moves every time the model edits anything above it** — so a raw comparison
/// stops matching exactly when the model is editing, which is when it needs to
/// match. Everything else is left alone: the assertion text is the signal.
pub(crate) fn normalise_check(detail: &str) -> String {
    static RES: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> =
        std::sync::OnceLock::new();
    let res = RES.get_or_init(|| {
        vec![
            // Scratch and temp paths, which differ per run.
            (regex::Regex::new(r#"(/private)?/var/folders/[^\s"']+"#).unwrap(), "<tmp>"),
            (regex::Regex::new(r#"/tmp/[^\s"']+"#).unwrap(), "<tmp>"),
            // Line numbers, which move as the file is edited.
            (regex::Regex::new(r"\bline \d+").unwrap(), "line <n>"),
            // Pointer-ish values that vary between processes.
            (regex::Regex::new(r"0x[0-9a-fA-F]+").unwrap(), "<addr>"),
        ]
    });
    let mut out = detail.to_string();
    for (re, with) in res {
        out = re.replace_all(&out, *with).into_owned();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does this message read as "I can't continue without you"? Workers run
/// unattended, so an explicit block is a stall, not a question.
fn looks_blocked(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "i'm blocked",
        "i am blocked",
        "i am stuck",
        "i'm stuck",
        "cannot proceed",
        "can't proceed",
        "unable to proceed",
        "need more information",
        "need clarification",
        "please clarify",
    ]
    .iter()
    .any(|needle| t.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: &str) -> Event {
        Event::ToolCall {
            id: "c".into(),
            name: "grep".into(),
            arguments: args.into(),
        }
    }

    fn sup(cfg: SupervisorConfig) -> Supervisor {
        Supervisor::new(cfg)
    }

    fn failed(detail: &str) -> Event {
        Event::Validation { ok: false, detail: detail.to_string() }
    }

    /// The failure this exists for, taken from a real run: qwen3.5-4B failed
    /// the same assertion nine times across valid edits, and only the `bash`
    /// repeat rule eventually stopped it, minutes in.
    #[test]
    fn the_same_check_failing_three_times_gets_a_nudge() {
        let mut s = sup(SupervisorConfig { stuck_check_threshold: 3, ..Default::default() });
        assert_eq!(s.observe(&failed("AssertionError: parse_amount")), None);
        assert_eq!(s.observe(&failed("AssertionError: parse_amount")), None);
        let Some(Action::Nudge(d)) = s.observe(&failed("AssertionError: parse_amount")) else {
            panic!("third identical failure should nudge");
        };
        assert!(d.contains("3 times in a row"), "{d}");
    }

    /// Three *different* failures is a model working through a problem. The
    /// existing counter in `agent.rs` cannot tell these apart; this one must.
    #[test]
    fn different_failures_are_progress_and_are_left_alone() {
        let mut s = sup(SupervisorConfig { stuck_check_threshold: 3, ..Default::default() });
        assert_eq!(s.observe(&failed("AssertionError: one")), None);
        assert_eq!(s.observe(&failed("AssertionError: two")), None);
        assert_eq!(s.observe(&failed("AssertionError: three")), None);
    }

    /// The whole reason normalisation exists: the scratch directory differs per
    /// run and the line number moves whenever the model edits above it, so a
    /// raw comparison stops matching exactly when the model is editing.
    #[test]
    fn a_moving_line_number_is_still_the_same_failure() {
        let mut s = sup(SupervisorConfig { stuck_check_threshold: 3, ..Default::default() });
        let at = |dir: &str, line: u32| {
            format!(
                "Traceback:\n  File \"/private/var/folders/{dir}/T/tmpab12/money.py\", \
                 line {line}, in parse_amount\n    raise ValueError(\"bad\")"
            )
        };
        assert_eq!(s.observe(&failed(&at("d3", 29))), None);
        assert_eq!(s.observe(&failed(&at("d3", 32))), None);
        assert!(matches!(s.observe(&failed(&at("d3", 41))), Some(Action::Nudge(_))));
    }

    /// A passing check clears the streak: the next failure starts over.
    #[test]
    fn a_passing_check_resets_the_streak() {
        let mut s = sup(SupervisorConfig { stuck_check_threshold: 3, ..Default::default() });
        assert_eq!(s.observe(&failed("same")), None);
        assert_eq!(s.observe(&failed("same")), None);
        assert_eq!(
            s.observe(&Event::Validation { ok: true, detail: "cargo test".into() }),
            None
        );
        assert_eq!(s.observe(&failed("same")), None);
        assert_eq!(s.observe(&failed("same")), None);
    }

    #[test]
    fn repeated_identical_calls_nudge_once() {
        let mut s = sup(SupervisorConfig { repeat_threshold: 3, ..Default::default() });
        assert_eq!(s.observe(&call("{}")), None);
        assert_eq!(s.observe(&call("{}")), None);
        assert!(matches!(s.observe(&call("{}")), Some(Action::Nudge(_))));
        // Same signature again: already flagged, no second nudge.
        assert_eq!(s.observe(&call("{}")), None);
        assert_eq!(s.nudges(), 1);
        // A different signature is tracked separately.
        assert_eq!(s.observe(&call(r#"{"q":"x"}"#)), None);
    }

    #[test]
    fn nudges_are_bounded_then_escalate() {
        let mut s = sup(SupervisorConfig { max_nudges: 2, ..Default::default() });
        assert!(matches!(s.on_idle(), Some(Action::Nudge(_))));
        assert!(matches!(s.on_idle(), Some(Action::Nudge(_))));
        assert!(matches!(s.on_idle(), Some(Action::Escalate(_))));
        assert_eq!(s.nudges(), 2, "escalation is not a nudge");
    }

    #[test]
    fn token_budget_escalates() {
        let mut s = sup(SupervisorConfig { token_budget: Some(100), ..Default::default() });
        let usage = |n| Event::Usage {
            reasoning_tokens: 0,
            finish_reason: None,
            prompt_tokens: 0,
            completion_tokens: n,
            total_tokens: n,
        };
        assert_eq!(s.observe(&usage(60)), None);
        match s.observe(&usage(60)) {
            Some(Action::Escalate(r)) => assert!(r.contains("120"), "reason should show spend: {r}"),
            other => panic!("expected escalation, got {other:?}"),
        }
    }

    #[test]
    fn explicit_block_nudges_once() {
        let mut s = sup(SupervisorConfig::default());
        let msg = |t: &str| Event::AssistantMessage { text: t.into() };
        assert_eq!(s.observe(&msg("working on it")), None);
        assert!(matches!(s.observe(&msg("I'm blocked: which file?")), Some(Action::Nudge(_))));
        assert_eq!(s.observe(&msg("I am blocked again")), None, "flagged only once");
    }

    #[test]
    fn off_mode_never_acts() {
        let mut s = sup(SupervisorConfig { mode: Mode::Off, ..Default::default() });
        for _ in 0..10 {
            assert_eq!(s.observe(&call("{}")), None);
        }
        assert_eq!(s.on_idle(), None);
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("off"), Mode::Off);
        assert_eq!(Mode::parse("Rules"), Mode::Rules);
        // The model observer isn't built yet — it degrades to rules.
        assert_eq!(Mode::parse("model"), Mode::Rules);
    }
}
