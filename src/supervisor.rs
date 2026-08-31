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
    /// Call signatures already flagged (nudge each one only once).
    flagged: HashSet<String>,
    nudges: usize,
    completion_tokens: u32,
    blocked_flagged: bool,
    /// Is a model call in flight? While one is, silence means "waiting", not
    /// "stuck".
    in_flight: bool,
    /// The tool call currently running, if any. A nudge cannot interrupt one
    /// any more than it can interrupt a model call, and a worker waiting on a
    /// six-minute `cargo test` emits nothing the whole time — which is
    /// indistinguishable from being stuck unless this is tracked.
    tool_in_flight: Option<String>,
    tool_idles: usize,
    /// Consecutive idle deadlines that passed while a call was in flight.
    in_flight_idles: usize,
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
            in_flight: false,
            tool_in_flight: None,
            tool_idles: 0,
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
            Event::ToolResult { .. } => {
                self.tool_in_flight = None;
                self.tool_idles = 0;
                None
            }
            Event::ToolCall { name, arguments, .. } => {
                self.tool_in_flight = Some(name.clone());
                self.tool_idles = 0;
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
        // Same argument as `in_flight` above, for the other thing a worker can
        // legitimately disappear into. A tool call emits nothing until it
        // returns, and a nudge aimed at one lands after it finishes — so all it
        // does is spend the budget. Measured on this repo: `cargo test` after an
        // edit takes 6m42s, while the supervisor's patience is
        // `idle_timeout` x (`max_nudges` + 1) = 360s. It killed a worker forty
        // seconds before its own check would have proved it had succeeded, and
        // well inside the 600s `bash-timeout-secs` that was already bounding it.
        //
        // The tool layer owns this timeout, so the supervisor defers to it and
        // only steps in if a tool outlives even a hung model call — which means
        // a tool with no timeout of its own, not a slow one.
        if let Some(name) = self.tool_in_flight.clone() {
            self.tool_idles += 1;
            let waited = self.cfg.idle_timeout * self.tool_idles as u32;
            if waited < self.cfg.request_timeout {
                return None;
            }
            return Some(Action::Escalate(format!(
                "`{name}` has been running for {}s with no result",
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
    fn a_long_running_tool_is_not_mistaken_for_a_stuck_worker() {
        // Measured on this repo: `cargo test` after an edit takes 6m42s, while
        // the supervisor's patience is idle_timeout x (max_nudges + 1) = 360s.
        // It stopped a worker forty seconds before that worker's own check
        // would have proved it succeeded — and well inside the 600s
        // bash-timeout-secs that was already bounding the command.
        //
        // The same reasoning the model path already carries: a nudge lands at
        // the top of the *next* step, so one aimed at a running tool arrives
        // after the tool finishes. All it can do is spend the budget.
        let mut s = sup(SupervisorConfig {
            idle_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(600),
            max_nudges: 2,
            ..Default::default()
        });
        assert_eq!(
            s.observe(&Event::ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: r#"{"command":"cargo test"}"#.into(),
            }),
            None
        );

        // Four idle ticks — past what used to be the whole budget — and it has
        // neither nudged nor stopped, because a tool is running.
        for tick in 1..=4 {
            assert_eq!(s.on_idle(), None, "intervened on tick {tick} while a tool was running");
        }
        assert_eq!(s.nudges(), 0, "a running tool must not cost a nudge");

        // The result arrives and ordinary supervision resumes.
        s.observe(&Event::ToolResult {
            id: "c".into(),
            name: "bash".into(),
            ok: true,
            output: "test result: ok".into(),
        });
        assert!(matches!(s.on_idle(), Some(Action::Nudge(_))), "silence after a tool is still idle");
    }

    #[test]
    fn a_tool_with_no_timeout_of_its_own_is_still_caught() {
        // Deferring to the tool layer is right for a slow tool and wrong for a
        // hung one, so the escape hatch stays — just far enough out that it
        // cannot fire on anything legitimate.
        let mut s = sup(SupervisorConfig {
            idle_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(600),
            ..Default::default()
        });
        s.observe(&Event::ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        for _ in 1..5 {
            assert_eq!(s.on_idle(), None);
        }
        match s.on_idle() {
            Some(Action::Escalate(why)) => assert!(why.contains("bash"), "{why}"),
            other => panic!("a tool running past the request timeout must escalate: {other:?}"),
        }
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
