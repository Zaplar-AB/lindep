//! Interactive application state and input handling. Holds the graph, the
//! currently focused issue ("root" of the lens), the flattened upstream and
//! downstream trees, and all view state (search, filter, sort, mode). Rendering
//! lives in [`crate::ui`]; this module never touches the terminal.

use std::cmp::Ordering;
use std::collections::HashSet;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::widgets::ListState;

use crate::model::{Direction, Graph, Issue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    List,
    Upstream,
    Downstream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lens,
    Overview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Blocked,
    HasDeps,
}

impl Filter {
    const fn next(self) -> Self {
        match self {
            Filter::All => Filter::Blocked,
            Filter::Blocked => Filter::HasDeps,
            Filter::HasDeps => Filter::All,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Blocked => "blocked",
            Filter::HasDeps => "has-deps",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Ready-to-start work first: unblocked issues on top, highest downstream
    /// impact within each group.
    Ready,
    Blocked,
    Status,
    Priority,
    Key,
}

impl Sort {
    const fn next(self) -> Self {
        match self {
            Sort::Ready => Sort::Blocked,
            Sort::Blocked => Sort::Status,
            Sort::Status => Sort::Priority,
            Sort::Priority => Sort::Key,
            Sort::Key => Sort::Ready,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Sort::Ready => "ready",
            Sort::Blocked => "blocked",
            Sort::Status => "status",
            Sort::Priority => "priority",
            Sort::Key => "id",
        }
    }
}

/// How a node renders inside a dependency tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Normal,
    Cycle,    // back-edge to an ancestor on the current path
    Ref,      // already drawn elsewhere in this tree (a join, not a cycle)
    External, // lives outside the project — a terminal leaf
}

/// One rendered line of a dependency tree.
pub struct TreeRow {
    pub key: String,
    pub prefix: String,
    pub kind: NodeKind,
    pub has_children: bool,
    pub collapsed: bool,
}

pub struct App {
    pub graph: Graph,
    pub order: Vec<String>,
    pub list_state: ListState,

    pub root: String,
    history: Vec<String>,

    pub up_rows: Vec<TreeRow>,
    pub down_rows: Vec<TreeRow>,
    pub up_state: ListState,
    pub down_state: ListState,
    collapsed: HashSet<String>, // "U:KEY" / "D:KEY"
    collapsed_for: String,      // the root `collapsed` belongs to; reset on re-root

    pub focus: Pane,
    pub mode: Mode,
    pub filter: Filter,
    pub sort: Sort,
    pub search_query: String,
    pub search_active: bool,
    pub show_help: bool,
    pub status_msg: Option<String>,

    pub should_quit: bool,
}

impl App {
    pub fn new(graph: Graph) -> Self {
        // Default focus: the most-connected real issue — usually the spine of
        // the dependency web — so the lens opens somewhere interesting.
        let root = graph
            .keys()
            .iter()
            .filter(|k| graph.get(k).is_some_and(|i| !i.external))
            .max_by_key(|k| {
                graph.direct_count(k, Direction::Upstream)
                    + graph.direct_count(k, Direction::Downstream)
            })
            .cloned()
            .unwrap_or_default();

        let mut app = App {
            graph,
            order: Vec::new(),
            list_state: ListState::default(),
            root,
            history: Vec::new(),
            up_rows: Vec::new(),
            down_rows: Vec::new(),
            up_state: ListState::default(),
            down_state: ListState::default(),
            collapsed: HashSet::new(),
            collapsed_for: String::new(),
            focus: Pane::List,
            mode: Mode::Lens,
            filter: Filter::All,
            sort: Sort::Ready,
            search_query: String::new(),
            search_active: false,
            show_help: false,
            status_msg: None,
            should_quit: false,
        };
        app.rebuild_order();
        app.rebuild_trees();
        app
    }

    pub fn focused_issue(&self) -> Option<&Issue> {
        self.graph.get(&self.root)
    }

    // ── Derived list ordering ────────────────────────────────────────────────

    fn rebuild_order(&mut self) {
        let needle = self.search_query.to_lowercase();
        let g = &self.graph;
        let (filter, sort) = (self.filter, self.sort);

        // Decorate each surviving key with its sort key once (so transitive()
        // isn't recomputed O(log n) times inside the comparator), then sort.
        let mut decorated: Vec<((u8, u64), String)> = g
            .keys()
            .iter()
            .filter_map(|k| {
                let issue = g.get(k)?;
                if issue.external {
                    return None; // externals show in trees, not the project list
                }
                let pass_filter = match filter {
                    Filter::All => true,
                    Filter::Blocked => g.is_blocked(k),
                    Filter::HasDeps => {
                        g.direct_count(k, Direction::Upstream) > 0
                            || g.direct_count(k, Direction::Downstream) > 0
                    }
                };
                let pass_search = needle.is_empty()
                    || issue.key.to_lowercase().contains(&needle)
                    || issue.title.to_lowercase().contains(&needle);
                (pass_filter && pass_search).then(|| (sort_key(g, k, sort), k.clone()))
            })
            .collect();

        decorated.sort_by(|(ka, a), (kb, b)| ka.cmp(kb).then_with(|| natural_key_cmp(a, b)));
        self.order = decorated.into_iter().map(|(_, k)| k).collect();
        // If the active filter/search hid the current root, re-aim the lens at the
        // first visible issue. Otherwise the left list would highlight order[0]
        // while the right-hand lens kept describing the now-invisible root, and
        // the first Down would skip order[0]. An empty list keeps the last root.
        if !self.order.is_empty() && !self.order.contains(&self.root) {
            self.root = self.order[0].clone();
            self.rebuild_trees();
        }
        self.sync_list_selection();
    }

    fn sync_list_selection(&mut self) {
        if self.order.is_empty() {
            self.list_state.select(None);
        } else if let Some(i) = self.order.iter().position(|k| *k == self.root) {
            self.list_state.select(Some(i));
        } else {
            self.list_state.select(Some(0));
        }
    }

    // ── Tree (lens) construction ───────────────────────────────────────────

    fn rebuild_trees(&mut self) {
        // Collapse state is per-root, so each issue opens fully expanded. Re-rooting
        // changes `root` and clears it here; a collapse toggle leaves `root`
        // unchanged (so its state survives its own rebuild).
        if self.collapsed_for != self.root {
            self.collapsed.clear();
            self.collapsed_for = self.root.clone();
        }
        self.up_rows = self.build_forest(Direction::Upstream);
        self.down_rows = self.build_forest(Direction::Downstream);
        clamp_selection(&mut self.up_state, self.up_rows.len());
        clamp_selection(&mut self.down_state, self.down_rows.len());
    }

    fn build_forest(&self, dir: Direction) -> Vec<TreeRow> {
        let tag = match dir {
            Direction::Upstream => "U",
            Direction::Downstream => "D",
        };
        let mut rows = Vec::new();
        let mut drawn = HashSet::new();
        let mut path = HashSet::new();
        path.insert(self.root.clone());

        let children = self.graph.neighbours(&self.root, dir);
        for (i, child) in children.iter().enumerate() {
            self.walk(
                child,
                dir,
                tag,
                &mut Vec::new(),
                i + 1 == children.len(),
                &mut path,
                &mut drawn,
                &mut rows,
            );
        }
        rows
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        key: &str,
        dir: Direction,
        tag: &str,
        ancestors: &mut Vec<bool>,
        is_last: bool,
        path: &mut HashSet<String>,
        drawn: &mut HashSet<String>,
        rows: &mut Vec<TreeRow>,
    ) {
        let mut prefix = String::new();
        for &last in ancestors.iter() {
            prefix.push_str(if last { "   " } else { "│  " });
        }
        prefix.push_str(if is_last { "└─ " } else { "├─ " });

        // Classify before deciding whether to recurse.
        let kind = if path.contains(key) {
            NodeKind::Cycle
        } else if drawn.contains(key) {
            NodeKind::Ref
        } else if self.graph.get(key).is_some_and(|i| i.external) {
            NodeKind::External
        } else {
            NodeKind::Normal
        };

        let children = self.graph.neighbours(key, dir);
        let has_children = kind == NodeKind::Normal && !children.is_empty();
        let collapsed = has_children && self.collapsed.contains(&format!("{tag}:{key}"));

        rows.push(TreeRow {
            key: key.to_string(),
            prefix,
            kind,
            has_children,
            collapsed,
        });

        if kind != NodeKind::Normal {
            return; // Cycle / Ref / External are terminal
        }
        drawn.insert(key.to_string());
        if !has_children || collapsed {
            return;
        }

        path.insert(key.to_string());
        ancestors.push(is_last);
        for (i, child) in children.iter().enumerate() {
            self.walk(
                child,
                dir,
                tag,
                ancestors,
                i + 1 == children.len(),
                path,
                drawn,
                rows,
            );
        }
        ancestors.pop();
        path.remove(key);
    }

    // ── Focus / navigation ───────────────────────────────────────────────────

    fn set_root(&mut self, key: String, push_history: bool) {
        if key.is_empty() || key == self.root {
            return;
        }
        if push_history {
            self.history.push(self.root.clone());
        }
        self.root = key;
        self.sync_list_selection();
        self.rebuild_trees();
    }

    fn move_selection(&mut self, delta: i32) {
        // In the overview the only visible cursor is the highlighted root chip,
        // so arrows always drive the list there regardless of the last pane.
        let pane = if self.mode == Mode::Overview {
            Pane::List
        } else {
            self.focus
        };
        match pane {
            Pane::List => {
                move_state(&mut self.list_state, self.order.len(), delta);
                if let Some(i) = self.list_state.selected()
                    && let Some(k) = self.order.get(i).cloned()
                {
                    // List navigation re-aims the lens without touching history.
                    self.root = k;
                    self.rebuild_trees();
                }
            }
            Pane::Upstream => move_state(&mut self.up_state, self.up_rows.len(), delta),
            Pane::Downstream => move_state(&mut self.down_state, self.down_rows.len(), delta),
        }
    }

    fn selected_tree_row(&self) -> Option<&TreeRow> {
        let (rows, state) = match self.focus {
            Pane::Upstream => (&self.up_rows, &self.up_state),
            Pane::Downstream => (&self.down_rows, &self.down_state),
            Pane::List => return None,
        };
        state.selected().and_then(|i| rows.get(i))
    }

    fn enter(&mut self) {
        match self.focus {
            Pane::List => {
                if !self.up_rows.is_empty() {
                    self.focus = Pane::Upstream;
                } else if !self.down_rows.is_empty() {
                    self.focus = Pane::Downstream;
                }
            }
            Pane::Upstream | Pane::Downstream => {
                let Some(row) = self.selected_tree_row() else {
                    return;
                };
                if row.kind == NodeKind::External {
                    self.status_msg = Some(format!(
                        "{} is external (team {}) — open it in Linear to follow its chain",
                        row.key,
                        self.graph.get(&row.key).map(|i| i.team()).unwrap_or("?")
                    ));
                    return;
                }
                let key = row.key.clone();
                self.set_root(key, true);
            }
        }
    }

    fn toggle_collapse(&mut self) {
        let tag = match self.focus {
            Pane::Upstream => "U",
            Pane::Downstream => "D",
            Pane::List => return,
        };
        if let Some(row) = self.selected_tree_row()
            && row.has_children
        {
            let id = format!("{tag}:{}", row.key);
            if !self.collapsed.remove(&id) {
                self.collapsed.insert(id);
            }
            self.rebuild_trees();
        }
    }

    fn go_back(&mut self) {
        if let Some(prev) = self.history.pop() {
            self.root = prev;
            self.sync_list_selection();
            self.rebuild_trees();
        } else {
            self.status_msg = Some("nothing to go back to".into());
        }
    }

    fn jump_to_cycle(&mut self) {
        let members = self.graph.cycle_members();
        if members.is_empty() {
            self.status_msg = Some("no dependency cycles 🎉".into());
            return;
        }
        // Advance to the next cycle member after wherever we're standing, derived
        // from the current root rather than a counter that drifts when you re-root
        // by other means. Cyclic SCCs always have ≥2 members (self-blocks are
        // rejected), so this always moves — the status line never claims a jump
        // that didn't happen.
        let next = match members.iter().position(|k| *k == self.root) {
            Some(i) => (i + 1) % members.len(),
            None => 0,
        };
        let key = members[next].clone();
        self.status_msg = Some(format!("cycle {}/{} — {}", next + 1, members.len(), key));
        self.set_root(key, true);
    }

    // ── Key handling ─────────────────────────────────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        self.status_msg = None;

        if self.search_active {
            self.on_search_key(key.code);
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.on_escape(),
            _ if self.show_help => self.show_help = false,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Left | KeyCode::Char('h') => self.focus = Pane::List,
            KeyCode::Right | KeyCode::Char('l') => {
                self.focus = match self.focus {
                    Pane::List | Pane::Downstream => Pane::Upstream,
                    Pane::Upstream => Pane::Downstream,
                }
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Pane::List => Pane::Upstream,
                    Pane::Upstream => Pane::Downstream,
                    Pane::Downstream => Pane::List,
                }
            }
            KeyCode::Enter => self.enter(),
            KeyCode::Char(' ') => self.toggle_collapse(),
            KeyCode::Backspace | KeyCode::Char('b') => self.go_back(),
            KeyCode::Char('c') => self.jump_to_cycle(),
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.rebuild_order();
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild_order();
            }
            KeyCode::Char('g') => {
                self.mode = match self.mode {
                    Mode::Lens => Mode::Overview,
                    Mode::Overview => Mode::Lens,
                }
            }
            KeyCode::Char('/') => {
                self.search_active = true;
                self.focus = Pane::List;
            }
            KeyCode::Char('?') => self.show_help = !self.show_help,
            _ => {}
        }
    }

    fn on_escape(&mut self) {
        if self.show_help {
            self.show_help = false;
        } else if self.search_active {
            self.search_active = false;
            self.search_query.clear();
            self.rebuild_order();
        } else if self.mode == Mode::Overview {
            self.mode = Mode::Lens;
        } else {
            self.should_quit = true;
        }
    }

    fn on_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.search_active = false;
                self.search_query.clear();
                self.rebuild_order();
            }
            KeyCode::Enter => self.search_active = false,
            KeyCode::Backspace => {
                self.search_query.pop();
                self.rebuild_order();
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.rebuild_order();
            }
            _ => {}
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────────

fn move_state(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    state.select(Some(next));
}

fn clamp_selection(state: &mut ListState, len: usize) {
    if len == 0 {
        state.select(None);
    } else {
        let i = state.selected().unwrap_or(0).min(len - 1);
        state.select(Some(i));
    }
}

/// Compare identifiers naturally: same prefix sorts by numeric suffix.
fn natural_key_cmp(a: &str, b: &str) -> Ordering {
    fn split(k: &str) -> (&str, u64) {
        match k.split_once('-') {
            Some((p, n)) => (p, n.parse().unwrap_or(0)),
            None => (k, 0),
        }
    }
    let (pa, na) = split(a);
    let (pb, nb) = split(b);
    pa.cmp(pb).then(na.cmp(&nb))
}

/// The once-per-node sort key for the active sort mode. Lower sorts first;
/// `natural_key_cmp` breaks ties. `Ready` and `Blocked` are mirror images:
/// both group by blocked-vs-unblocked, then by higher downstream impact (hence
/// the inversion) — `Ready` surfaces unblocked work, `Blocked` surfaces blocked.
fn sort_key(graph: &Graph, key: &str, sort: Sort) -> (u8, u64) {
    let by_impact = || u64::MAX - graph.transitive(key, Direction::Downstream) as u64;
    match sort {
        Sort::Ready => (u8::from(graph.is_blocked(key)), by_impact()),
        Sort::Blocked => (u8::from(!graph.is_blocked(key)), by_impact()),
        Sort::Status => (status_rank(graph, key), 0),
        Sort::Priority => (graph.get(key).map_or(u8::MAX, |i| i.priority.rank()), 0),
        Sort::Key => (0, 0),
    }
}

/// Sort rank that surfaces live work first: in-progress → unstarted → backlog →
/// done.
fn status_rank(graph: &Graph, key: &str) -> u8 {
    use crate::model::Status::*;
    match graph.get(key).map(|i| i.status) {
        Some(Started) => 0,
        Some(Triage) => 1,
        Some(Unstarted) => 2,
        Some(Backlog) => 3,
        Some(Completed) => 4,
        Some(Duplicate) => 5,
        Some(Canceled) => 6,
        _ => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        App::new(demo::graph())
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn default_focus_is_the_most_connected_issue() {
        // ZAP-204 has the most direct edges in the demo graph.
        assert_eq!(app().root, "ZAP-204");
    }

    #[test]
    fn enter_on_a_blocker_re_roots_and_back_returns() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l')); // focus upstream tree
        assert_eq!(app.focus, Pane::Upstream);

        let target = app.up_rows[0].key.clone(); // first blocker of ZAP-204
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.root, target);
        assert_ne!(app.root, "ZAP-204");

        press(&mut app, KeyCode::Char('b')); // back
        assert_eq!(app.root, "ZAP-204");
    }

    #[test]
    fn list_navigation_re_aims_the_lens() {
        let mut app = app();
        let idx0 = app.list_state.selected().unwrap();
        assert_eq!(app.order[idx0], app.root); // selection tracks the root
        press(&mut app, KeyCode::Down);
        let idx1 = app.list_state.selected().unwrap();
        assert_ne!(idx1, idx0);
        assert_eq!(app.root, app.order[idx1]); // root re-aimed to new selection
    }

    #[test]
    fn collapsing_a_subtree_hides_its_children() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l')); // upstream; row 0 = ZAP-188 (has child ZAP-150)
        let before = app.up_rows.len();
        press(&mut app, KeyCode::Char(' '));
        assert!(app.up_rows.len() < before);
        press(&mut app, KeyCode::Char(' ')); // expand again
        assert_eq!(app.up_rows.len(), before);
    }

    #[test]
    fn collapse_state_resets_when_the_lens_re_roots() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l')); // upstream
        let before = app.up_rows.len();
        press(&mut app, KeyCode::Char(' ')); // collapse a subtree
        assert!(app.up_rows.len() < before);
        press(&mut app, KeyCode::Enter); // re-root onto the collapsed node
        press(&mut app, KeyCode::Char('b')); // back to the original root
        assert_eq!(
            app.up_rows.len(),
            before,
            "the lens re-opens fully expanded after re-rooting"
        );
    }

    #[test]
    fn cycle_jump_moves_to_a_distinct_member_each_press() {
        let mut app = app();
        assert!(app.graph.cycle_members().len() >= 2);
        press(&mut app, KeyCode::Char('c'));
        let first = app.root.clone();
        assert!(app.graph.in_cycle(&first));
        press(&mut app, KeyCode::Char('c'));
        assert!(app.graph.in_cycle(&app.root));
        assert_ne!(
            first, app.root,
            "each press lands on a different cycle member"
        );
        assert!(app.status_msg.as_deref().unwrap().contains(&app.root));
    }

    #[test]
    fn ready_sort_puts_unblocked_issues_first() {
        let app = app(); // Sort::Ready is the default
        assert_eq!(app.sort, Sort::Ready);
        // The list is partitioned: every unblocked issue precedes every blocked one.
        let mut seen_blocked = false;
        for k in &app.order {
            if app.graph.is_blocked(k) {
                seen_blocked = true;
            } else {
                assert!(!seen_blocked, "unblocked {k} appears after a blocked issue");
            }
        }
    }

    #[test]
    fn search_filters_then_clears() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        assert!(app.search_active);
        for c in "210".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.order, vec!["ZAP-210".to_string()]);
        press(&mut app, KeyCode::Esc);
        assert!(!app.search_active);
        assert!(app.search_query.is_empty());
        assert!(app.order.len() > 1);
    }

    #[test]
    fn natural_key_cmp_orders_by_numeric_suffix() {
        assert_eq!(natural_key_cmp("ZAP-9", "ZAP-188"), Ordering::Less);
        assert_eq!(natural_key_cmp("ZAP-188", "ZAP-9"), Ordering::Greater);
        // A differing prefix orders lexically by prefix, ignoring the number.
        assert_eq!(natural_key_cmp("ABC-500", "ZAP-1"), Ordering::Less);
    }

    #[test]
    fn filtering_out_the_root_re_aims_the_lens_to_the_visible_selection() {
        // Searching for an issue other than the default root (ZAP-204) must keep
        // the list highlight and the lens describing the SAME issue.
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        for c in "210".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(app.order, vec!["ZAP-210".to_string()]);
        let sel = app.list_state.selected().expect("a row stays selected");
        assert_eq!(app.order[sel], app.root, "highlight and root agree");
        assert_eq!(app.focused_issue().unwrap().key, "ZAP-210");
    }

    #[test]
    fn external_node_cannot_be_re_rooted() {
        let mut app = app();
        press(&mut app, KeyCode::Char('l'));
        // Find the external INFRA-77 row and select it.
        let idx = app
            .up_rows
            .iter()
            .position(|r| r.kind == NodeKind::External)
            .expect("demo has an external blocker");
        app.up_state.select(Some(idx));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.root, "ZAP-204"); // unchanged
        assert!(app.status_msg.is_some()); // explained instead
    }

    #[test]
    fn cycle_jump_and_mode_and_quit_never_panic() {
        let mut app = app();
        press(&mut app, KeyCode::Char('c')); // jump to a cycle member
        assert!(app.graph.in_cycle(&app.root));
        for k in ['f', 's', 'g', 'g', '?', '?'] {
            press(&mut app, KeyCode::Char(k));
        }
        press(&mut app, KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn empty_list_navigation_keeps_root_and_never_panics() {
        // A search that matches nothing empties the list; navigation must be a
        // safe no-op and the lens must keep its last valid root + trees.
        let mut app = app();
        let root = app.root.clone();
        press(&mut app, KeyCode::Char('/'));
        for c in "zzzznomatch".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.order.is_empty());
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Tab,
        ] {
            press(&mut app, code);
        }
        assert_eq!(app.root, root);
        assert!(!app.up_rows.is_empty());
    }
}
