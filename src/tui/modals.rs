use crossterm::event::{KeyCode, KeyEvent};

use crate::tools::approval::{Approval, ApprovalRequest, TextRequest};

#[derive(Default)]
pub(super) struct Modals {
    /// A command waiting on the user's yes/no. While this is set, keys answer
    /// the question instead of editing the composer.
    pending_approval: Option<ApprovalRequest>,
    /// A pairing checkpoint waiting on prose. Unlike an approval it does not
    /// seize typing; Enter and Esc are the only routed keys.
    pending_ask: Option<TextRequest>,
}

pub(super) enum ApprovalKey {
    Answered { note: &'static str, quit: bool },
    WaitingForAnswer,
}

pub(super) enum AskAnswer {
    Answered(String),
    Skipped,
}

impl Modals {
    pub(super) fn approval_pending(&self) -> bool {
        self.pending_approval.is_some()
    }

    pub(super) fn ask_pending(&self) -> bool {
        self.pending_ask.is_some()
    }

    pub(super) fn set_approval(&mut self, req: ApprovalRequest) {
        self.pending_approval = Some(req);
    }

    pub(super) fn set_ask(&mut self, req: TextRequest) {
        self.pending_ask = Some(req);
    }

    pub(super) fn answer_approval_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<ApprovalKey> {
        let req = self.pending_approval.take()?;
        let (answer, note) = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => (Approval::Once, "approved once"),
            KeyCode::Char('a') | KeyCode::Char('A') => (
                Approval::AlwaysThisSession,
                "approved — and not asking again this session for this kind of command",
            ),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => (Approval::Deny, "denied"),
            KeyCode::Char('c') if ctrl => (Approval::Deny, "denied (quitting)"),
            _ => {
                self.pending_approval = Some(req);
                return Some(ApprovalKey::WaitingForAnswer);
            }
        };

        req.answer(answer);
        Some(ApprovalKey::Answered {
            note,
            quit: key.code == KeyCode::Char('c') && ctrl,
        })
    }

    pub(super) fn answer_ask(&mut self, answer: Option<String>) -> Option<AskAnswer> {
        let req = self.pending_ask.take()?;
        let echo = match &answer {
            Some(input) => AskAnswer::Answered(input.clone()),
            None => AskAnswer::Skipped,
        };
        req.answer(answer);
        Some(echo)
    }
}
