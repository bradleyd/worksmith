use std::collections::HashSet;

pub(super) enum OverlayKind {
    Picker,
    Mcp {
        active: HashSet<String>,
    },
    Reference,
    Skills { names: HashSet<String>, loaded: HashSet<String> },
}

/// Reference navigation is separate from typing so queries may contain j/k/q.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReferenceInput {
    Navigate,
    AwaitingTop,
    Filter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkillFocus {
    List,
    Preview,
}

pub(super) struct SkillPreview {
    pub(super) name: String,
    pub(super) loaded: bool,
    pub(super) text: String,
}

/// A floating list: a filter line and a scrollable set of choices. One
/// component, because everything awkward in this UI is picking an opaque thing
/// — a command, a model, a session, a worker id.
pub(super) struct Overlay {
    pub(super) title: String,
    pub(super) items: Vec<OverlayItem>,
    pub(super) filter: String,
    matched: Vec<usize>,
    pub(super) selected: usize,
    /// A picker lets you select a row (Enter puts it in the composer). A
    /// reference has no action; a skill catalog loads the selected instructions.
    kind: OverlayKind,
    pub(super) reference_input: ReferenceInput,
    pub(super) skill_focus: SkillFocus,
    pub(super) preview: Option<SkillPreview>,
    pub(super) preview_scroll: usize,
    pub(super) preview_max_scroll: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct OverlayItem {
    pub(super) label: String,
    pub(super) description: String,
}

impl Overlay {
    pub(super) fn new(title: impl Into<String>, items: Vec<OverlayItem>) -> Self {
        let matched = (0..items.len()).collect();
        Self {
            title: title.into(),
            items,
            filter: String::new(),
            matched,
            selected: 0,
            kind: OverlayKind::Picker,
            reference_input: ReferenceInput::Navigate,
            skill_focus: SkillFocus::List,
            preview: None,
            preview_scroll: 0,
            preview_max_scroll: 0,
        }
    }

    /// A read-only list: rows can be scrolled and filtered, but there is
    /// nothing to select. Esc is the explicit close action.
    pub(super) fn reference(title: impl Into<String>, items: Vec<OverlayItem>) -> Self {
        let matched = (0..items.len()).collect();
        Self {
            title: title.into(),
            items,
            filter: String::new(),
            matched,
            selected: 0,
            kind: OverlayKind::Reference,
            reference_input: ReferenceInput::Navigate,
            skill_focus: SkillFocus::List,
            preview: None,
            preview_scroll: 0,
            preview_max_scroll: 0,
        }
    }

    pub(super) fn skills(items: Vec<OverlayItem>, names: HashSet<String>) -> Self {
        let mut overlay = Self::reference("skills", items);
        overlay.kind = OverlayKind::Skills { names, loaded: HashSet::new() };
        overlay
    }

    pub(super) fn mcp(items: Vec<OverlayItem>, active: HashSet<String>) -> Self {
        let mut overlay = Self::reference("MCP servers and tools", items);
        overlay.kind = OverlayKind::Mcp { active };
        overlay
    }

    pub(super) fn update_mcp(&mut self, items: Vec<OverlayItem>, active: HashSet<String>) {
        let chosen = self.chosen();
        if self.items != items {
            self.items = items;
            self.set_filter(self.filter.clone());
            if let Some(name) = chosen
                && let Some(index) = self
                    .matches()
                    .iter()
                    .position(|(_, item)| item.label == name)
            {
                self.selected = index;
            }
        }
        self.kind = OverlayKind::Mcp { active };
    }

    pub(super) fn is_mcp_catalog(&self) -> bool {
        matches!(self.kind, OverlayKind::Mcp { .. })
    }
    pub(super) fn is_two_pane(&self) -> bool {
        self.is_skill_catalog() || self.is_mcp_catalog()
    }

    pub(super) fn is_picker(&self) -> bool {
        matches!(self.kind, OverlayKind::Picker)
    }

    pub(super) fn is_skill_catalog(&self) -> bool {
        matches!(self.kind, OverlayKind::Skills { .. })
    }

    /// None identifies informational rows, which must not trigger loading.
    pub(super) fn skill_loaded(&self, name: &str) -> Option<bool> {
        match &self.kind {
            OverlayKind::Mcp { active } if !name.starts_with("server:") => {
                Some(active.contains(name))
            }
            OverlayKind::Skills { names, loaded } if names.contains(name) => {
                Some(loaded.contains(name))
            }
            _ => None,
        }
    }

    pub(super) fn set_loaded_skills(&mut self, current: HashSet<String>) {
        if let OverlayKind::Skills { names, loaded } = &mut self.kind {
            self.title = format!(
                "skills · {} active / {} discovered",
                names.intersection(&current).count(),
                names.len()
            );
            *loaded = current;
        }
    }

    /// Items matching the filter, as `(original index, item)`.
    pub(super) fn matches(&self) -> Vec<(usize, &OverlayItem)> {
        self.matched.iter().map(|&i| (i, &self.items[i])).collect()
    }

    pub(super) fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
        self.selected = 0;
        self.rebuild_matches();
    }

    pub(super) fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
        self.rebuild_matches();
    }

    pub(super) fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
        self.rebuild_matches();
    }

    fn rebuild_matches(&mut self) {
        let filter = self.filter.trim().to_ascii_lowercase();
        self.matched = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                filter.is_empty()
                    || i.label.to_ascii_lowercase().contains(&filter)
                    || i.description.to_ascii_lowercase().contains(&filter)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub(super) fn scroll_by(&mut self, delta: isize) {
        if self.is_two_pane() && self.skill_focus == SkillFocus::Preview {
            self.preview_scroll =
                self.preview_scroll.saturating_add_signed(delta).min(self.preview_max_scroll);
        } else {
            self.move_by(delta);
        }
    }

    pub(super) fn jump(&mut self, bottom: bool) {
        if self.is_two_pane() && self.skill_focus == SkillFocus::Preview {
            self.preview_scroll = if bottom { self.preview_max_scroll } else { 0 };
        } else {
            self.selected = if bottom { self.matched.len().saturating_sub(1) } else { 0 };
        }
    }

    pub(super) fn move_by(&mut self, delta: isize) {
        let n = self.matched.len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = if self.is_picker() {
            (cur + delta).rem_euclid(n as isize) as usize
        } else {
            cur.saturating_add(delta).clamp(0, n as isize - 1) as usize
        };
    }

    /// Which row is highlighted, clamped to the current matches. Typing narrows
    /// the list under the cursor, so the stored index can point past the end;
    /// clamping in one place keeps what is drawn and what Enter picks in
    /// agreement, instead of highlighting a row that selects nothing.
    pub(super) fn sel_index(&self, matches: usize) -> usize {
        self.selected.min(matches.saturating_sub(1))
    }

    /// The label of the highlighted row, if the filter matched anything.
    pub(super) fn chosen(&self) -> Option<String> {
        let m = self.matches();
        m.get(self.sel_index(m.len())).map(|(_, i)| i.label.clone())
    }
}
