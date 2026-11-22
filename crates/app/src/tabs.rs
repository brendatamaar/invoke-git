//! The tab list as a pure model: which repo tabs are open and which screen
//! is showing. It holds no repo state, only the order and the selection, so
//! every rule here is testable without a window.

/// The screen that is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Active {
    /// The open/init screen (no tab is selected).
    Welcome,
    /// The repo tab at this index.
    Repo(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tabs<T> {
    items: Vec<T>,
    active: Active,
}

impl<T> Default for Tabs<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            active: Active::Welcome,
        }
    }
}

impl<T> Tabs<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn active(&self) -> Active {
        self.active
    }

    /// The selected tab's index, if a repo tab is showing.
    pub fn active_index(&self) -> Option<usize> {
        match self.active {
            Active::Repo(index) => Some(index),
            Active::Welcome => None,
        }
    }

    pub fn active_item(&self) -> Option<&T> {
        self.active_index().and_then(|index| self.items.get(index))
    }

    pub fn position(&self, is: impl FnMut(&T) -> bool) -> Option<usize> {
        self.items.iter().position(is)
    }

    /// Shows the open/init screen. The tabs stay open.
    pub fn show_welcome(&mut self) {
        self.active = Active::Welcome;
    }

    /// Selects a tab; an index that is not open is ignored.
    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.active = Active::Repo(index);
        }
    }

    /// Adds a tab at the end and selects it. Returns its index.
    pub fn push(&mut self, item: T) -> usize {
        self.items.push(item);
        let index = self.items.len() - 1;
        self.active = Active::Repo(index);
        index
    }

    /// Selects the tab at `existing` (from [`Tabs::position`]), or adds the
    /// item `make` builds. Callers look for an existing tab first, so the
    /// same thing is never opened twice.
    pub fn open(&mut self, existing: Option<usize>, make: impl FnOnce() -> T) -> usize {
        match existing.filter(|index| *index < self.items.len()) {
            Some(index) => {
                self.active = Active::Repo(index);
                index
            }
            None => self.push(make()),
        }
    }

    /// Closes a tab and returns it. If it was the selected one, the tab that
    /// moves into its place is selected (its right neighbour, else the left
    /// one), and the welcome screen when it was the last. Closing another
    /// tab keeps the same tab selected.
    pub fn close(&mut self, index: usize) -> Option<T> {
        if index >= self.items.len() {
            return None;
        }
        let item = self.items.remove(index);
        self.active = match self.active {
            Active::Welcome => Active::Welcome,
            Active::Repo(_) if self.items.is_empty() => Active::Welcome,
            Active::Repo(active) if active == index => {
                Active::Repo(index.min(self.items.len() - 1))
            }
            Active::Repo(active) if active > index => Active::Repo(active - 1),
            Active::Repo(active) => Active::Repo(active),
        };
        Some(item)
    }

    /// The next tab, wrapping. From the welcome screen: the first tab.
    pub fn select_next(&mut self) {
        let count = self.items.len();
        if count == 0 {
            return;
        }
        self.active = Active::Repo(match self.active {
            Active::Welcome => 0,
            Active::Repo(index) => (index + 1) % count,
        });
    }

    /// The previous tab, wrapping. From the welcome screen: the last tab.
    pub fn select_previous(&mut self) {
        let count = self.items.len();
        if count == 0 {
            return;
        }
        self.active = Active::Repo(match self.active {
            Active::Welcome => count - 1,
            Active::Repo(index) => (index + count - 1) % count,
        });
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[derive(Clone, Debug)]
    enum Op {
        Open(u8),
        Close(usize),
        Select(usize),
        Welcome,
        Next,
        Previous,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u8..6).prop_map(Op::Open),
            (0usize..8).prop_map(Op::Close),
            (0usize..8).prop_map(Op::Select),
            Just(Op::Welcome),
            Just(Op::Next),
            Just(Op::Previous),
        ]
    }

    fn apply(tabs: &mut Tabs<u8>, op: &Op) {
        match op {
            Op::Open(key) => {
                let existing = tabs.position(|item| item == key);
                tabs.open(existing, || *key);
            }
            Op::Close(index) => {
                tabs.close(*index);
            }
            Op::Select(index) => tabs.select(*index),
            Op::Welcome => tabs.show_welcome(),
            Op::Next => tabs.select_next(),
            Op::Previous => tabs.select_previous(),
        }
    }

    #[test]
    fn closing_the_selected_tab_selects_its_right_neighbour() {
        let mut tabs = Tabs::new();
        for key in [1u8, 2, 3] {
            tabs.push(key);
        }
        tabs.select(1);
        assert_eq!(tabs.close(1), Some(2));
        assert_eq!(tabs.active_item(), Some(&3));
    }

    #[test]
    fn closing_the_last_tab_selects_the_one_before() {
        let mut tabs = Tabs::new();
        for key in [1u8, 2, 3] {
            tabs.push(key);
        }
        tabs.close(2);
        assert_eq!(tabs.active_item(), Some(&2));
    }

    #[test]
    fn closing_the_only_tab_shows_the_welcome_screen() {
        let mut tabs = Tabs::new();
        tabs.push(1u8);
        tabs.close(0);
        assert_eq!(tabs.active(), Active::Welcome);
    }

    #[test]
    fn closing_a_tab_before_the_selected_one_keeps_the_selection() {
        let mut tabs = Tabs::new();
        for key in [1u8, 2, 3] {
            tabs.push(key);
        }
        tabs.select(2);
        tabs.close(0);
        assert_eq!(tabs.active_item(), Some(&3));
    }

    proptest! {
        /// Whatever the user does, the selection points at a tab that
        /// exists, and no two tabs are the same repo.
        #[test]
        fn the_model_stays_consistent(ops in proptest::collection::vec(op(), 0..60)) {
            let mut tabs = Tabs::new();
            for op in &ops {
                apply(&mut tabs, op);
                if let Active::Repo(index) = tabs.active() {
                    prop_assert!(index < tabs.items().len());
                }
                let mut seen = std::collections::BTreeSet::new();
                for item in tabs.items() {
                    prop_assert!(seen.insert(*item), "{item} opened twice");
                }
            }
        }

        /// Opening selects the tab it names, new or not.
        #[test]
        fn open_selects_what_it_opened(
            ops in proptest::collection::vec(op(), 0..30),
            key in 0u8..6,
        ) {
            let mut tabs = Tabs::new();
            for op in &ops {
                apply(&mut tabs, op);
            }
            let existing = tabs.position(|item| *item == key);
            tabs.open(existing, || key);
            prop_assert_eq!(tabs.active_item(), Some(&key));
        }

        /// Closing a tab other than the selected one never changes which
        /// tab is selected.
        #[test]
        fn closing_another_tab_keeps_the_selected_one(
            ops in proptest::collection::vec(op(), 0..30),
            index in 0usize..8,
        ) {
            let mut tabs = Tabs::new();
            for op in &ops {
                apply(&mut tabs, op);
            }
            let before = tabs.active_item().copied();
            if tabs.active_index() != Some(index) {
                tabs.close(index);
                prop_assert_eq!(tabs.active_item().copied(), before);
            }
        }

        /// Next then previous comes back.
        #[test]
        fn next_and_previous_are_inverses(
            ops in proptest::collection::vec(op(), 0..30),
        ) {
            let mut tabs = Tabs::new();
            for op in &ops {
                apply(&mut tabs, op);
            }
            if tabs.active_index().is_some() {
                let before = tabs.active();
                tabs.select_next();
                tabs.select_previous();
                prop_assert_eq!(tabs.active(), before);
            }
        }
    }
}
