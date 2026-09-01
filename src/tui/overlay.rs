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
    /// reference — like the footer legend — has nothing to pick: Enter just
    /// closes, and the footer bar says so.
    pub(super) picking: bool,
}

#[derive(Clone)]
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
            picking: true,
        }
    }

    /// A read-only list: rows can be scrolled and filtered, but there is
    /// nothing to select. Enter closes rather than putting a row in the
    /// composer, which would be nonsense for a legend.
    pub(super) fn reference(title: impl Into<String>, items: Vec<OverlayItem>) -> Self {
        let matched = (0..items.len()).collect();
        Self {
            title: title.into(),
            items,
            filter: String::new(),
            matched,
            selected: 0,
            picking: false,
        }
    }

    /// Items matching the filter, as `(original index, item)`.
    pub(super) fn matches(&self) -> Vec<(usize, &OverlayItem)> {
        self.matched.iter().map(|&i| (i, &self.items[i])).collect()
    }

    #[cfg(test)]
    pub(super) fn set_filter(&mut self, filter: impl Into<String>) {
        self.filter = filter.into();
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

    pub(super) fn move_by(&mut self, delta: isize) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).rem_euclid(n as isize) as usize;
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
