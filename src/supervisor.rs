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
    /// Cumulative completion tokens before escalating. `None` = unlimited.
    pub token_budget: Option<u32>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Rules,
            idle_timeout: Duration::from_secs(120),
            max_nudges: 3,
            repeat_threshold: 4,
            token_budget: None,
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
    /// Call signatures already flagged (nudge each one only once).
    flagged: HashSet<String>,
    nudges: usize,
    completion_tokens: u32,
    blocked_flagged: bool,
}

impl Supervisor {
    pub fn new(cfg: SupervisorConfig) -> Self {
        Self {
            cfg,
            calls: HashMap::new(),
            flagged: HashSet::new(),
            nudges: 0,
            completion_tokens: 0,
            blocked_flagged: false,
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
