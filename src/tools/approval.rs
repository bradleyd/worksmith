//! Asking before doing something outward-facing.
//!
//! [`policy`](super::policy) decides *whether* to ask; this decides *who* gets
//! asked and how the answer comes back. They are separate because the answer
//! depends entirely on where worksmith is running: an interactive TUI can put a
//! prompt on screen, a `--print` run in a CI job has nobody to ask, and the eval
//! harness wants no prompts at all.
//!
//! The default for a non-interactive run is to **refuse**, not to allow. A
//! headless agent that silently pushes because there was no one to object is the
//! failure this whole layer exists to prevent.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

/// The user's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Run it this once.
    Once,
    /// Run it, and stop asking about this kind of command for the session.
    AlwaysThisSession,
    /// Don't run it.
    Deny,
}

/// Something that can answer "may I run this?".
#[async_trait]
pub trait Approver: Send + Sync {
    /// `reason` is the human-readable why-we-asked from the policy layer, and
    /// doubles as the category remembered by [`Approval::AlwaysThisSession`].
    async fn ask(&self, command: &str, reason: &str) -> Approval;
}

/// Approves everything. For the eval harness and for an explicit
/// "I know what I'm doing" flag — never a default.
pub struct AutoApprove;

#[async_trait]
impl Approver for AutoApprove {
    async fn ask(&self, _command: &str, _reason: &str) -> Approval {
        Approval::Once
    }
}

/// Refuses everything that needs asking, because there is nobody to ask.
/// The refusal text tells the model what happened so it can route around it
/// rather than retry forever.
pub struct RefuseWhenUnattended;

#[async_trait]
impl Approver for RefuseWhenUnattended {
    async fn ask(&self, _command: &str, _reason: &str) -> Approval {
        Approval::Deny
    }
}

/// Remembers "always" answers for the session, delegating anything new to an
/// inner approver. Wraps whatever front end is in use, so the remembering
/// behaves the same everywhere.
pub struct RememberingApprover {
    inner: Arc<dyn Approver>,
    allowed: Mutex<HashSet<String>>,
}

impl RememberingApprover {
    pub fn new(inner: Arc<dyn Approver>) -> Self {
        Self { inner, allowed: Mutex::new(HashSet::new()) }
    }

    /// Categories the user has blanket-approved this session.
    pub fn remembered(&self) -> Vec<String> {
        let mut v: Vec<String> = self.allowed.lock().unwrap().iter().cloned().collect();
        v.sort();
        v
    }
}

#[async_trait]
impl Approver for RememberingApprover {
    async fn ask(&self, command: &str, reason: &str) -> Approval {
        if self.allowed.lock().unwrap().contains(reason) {
            return Approval::Once;
        }
        let answer = self.inner.ask(command, reason).await;
        if answer == Approval::AlwaysThisSession {
            self.allowed.lock().unwrap().insert(reason.to_string());
        }
        answer
    }
}

/// One question, waiting for an answer.
pub struct ApprovalRequest {
    pub command: String,
    pub reason: String,
    reply: tokio::sync::oneshot::Sender<Approval>,
}

impl ApprovalRequest {
    /// Answer it. Dropping the request instead denies, which is the safe
    /// direction if a front end forgets one.
    pub fn answer(self, approval: Approval) {
        let _ = self.reply.send(approval);
    }
}

/// Hands the question to another task — the TUI's event loop — and waits.
///
/// The agent runs on its own task, so it cannot draw a prompt or read a key. It
/// sends the question down a channel and blocks on a one-shot reply, which is
/// also why a dropped receiver has to mean *deny*: if the UI is gone, nobody
/// approved anything.
pub struct ChannelApprover {
    tx: tokio::sync::mpsc::Sender<ApprovalRequest>,
}

impl ChannelApprover {
    pub fn new() -> (Self, tokio::sync::mpsc::Receiver<ApprovalRequest>) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        (Self { tx }, rx)
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn ask(&self, command: &str, reason: &str) -> Approval {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let req = ApprovalRequest {
            command: command.to_string(),
            reason: reason.to_string(),
            reply,
        };
        if self.tx.send(req).await.is_err() {
            return Approval::Deny; // no UI listening
        }
        answer.await.unwrap_or(Approval::Deny) // dropped without answering
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingApprover {
        answer: Approval,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl Approver for CountingApprover {
        async fn ask(&self, _c: &str, _r: &str) -> Approval {
            *self.calls.lock().unwrap() += 1;
            self.answer
        }
    }

    #[tokio::test]
    async fn unattended_runs_refuse_rather_than_allow() {
        // The whole point: nobody to ask must not mean "go ahead".
        assert_eq!(RefuseWhenUnattended.ask("git push", "pushes").await, Approval::Deny);
    }

    #[tokio::test]
    async fn always_is_remembered_per_category_and_stops_asking() {
        let inner = Arc::new(CountingApprover {
            answer: Approval::AlwaysThisSession,
            calls: Mutex::new(0),
        });
        let a = RememberingApprover::new(inner.clone());

        assert_eq!(a.ask("git push origin main", "pushes commits to a remote").await, Approval::AlwaysThisSession);
        // Same category, different command: no second prompt.
        assert_eq!(a.ask("git push --tags", "pushes commits to a remote").await, Approval::Once);
        assert_eq!(*inner.calls.lock().unwrap(), 1, "asked once, not twice");

        // A different category is still a fresh question.
        assert_eq!(a.ask("sudo ls", "runs as root").await, Approval::AlwaysThisSession);
        assert_eq!(*inner.calls.lock().unwrap(), 2);
        assert_eq!(a.remembered(), vec!["pushes commits to a remote", "runs as root"]);
    }

    #[tokio::test]
    async fn a_question_nobody_answers_is_a_no() {
        // If the UI is gone or drops the request, the command must not run.
        let (approver, rx) = ChannelApprover::new();
        drop(rx);
        assert_eq!(approver.ask("git push", "pushes").await, Approval::Deny);

        let (approver, mut rx) = ChannelApprover::new();
        let h = tokio::spawn(async move { approver.ask("git push", "pushes").await });
        let req = rx.recv().await.unwrap();
        assert_eq!(req.command, "git push");
        drop(req); // answered by nobody
        assert_eq!(h.await.unwrap(), Approval::Deny);
    }

    #[tokio::test]
    async fn an_answer_reaches_the_waiting_agent() {
        let (approver, mut rx) = ChannelApprover::new();
        let h = tokio::spawn(async move { approver.ask("git push", "pushes").await });
        rx.recv().await.unwrap().answer(Approval::AlwaysThisSession);
        assert_eq!(h.await.unwrap(), Approval::AlwaysThisSession);
    }

    #[tokio::test]
    async fn a_one_time_yes_is_not_remembered() {
        let inner = Arc::new(CountingApprover { answer: Approval::Once, calls: Mutex::new(0) });
        let a = RememberingApprover::new(inner.clone());
        a.ask("git push", "pushes commits to a remote").await;
        a.ask("git push", "pushes commits to a remote").await;
        assert_eq!(*inner.calls.lock().unwrap(), 2, "each time is a fresh question");
        assert!(a.remembered().is_empty());
    }
}
