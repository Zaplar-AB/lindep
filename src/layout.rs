//! Pure window-placement geometry for the cockpit strip — no ratatui state, no
//! `App`, no I/O, so every rule here is unit-testable in isolation (like the v2
//! `tail_fit` / `split` helpers it sits beside).
//!
//! Two layouts share this module:
//! * **Mosaic** tiles every window near-square via [`split_grid`] — the proven
//!   layout that sidesteps tui-term's horizontal-clipping limitation.
//! * **Filmstrip** pins the Spine left and lays non-spine windows out as
//!   fixed-width columns, drawing **only the columns that fully fit** and
//!   dropping any that would be partially clipped. tui-term 0.3.4 paints the
//!   top-left `w×h` subgrid of the vt100 parser with no horizontal offset, so a
//!   partially-scrolled-in 80-col PTY column would show its *left* N cols
//!   sliced, not windowed — and `PseudoTerminal::render` `Clear`s first, so
//!   overlapping Rects erase each other. Snap-to-whole-column avoids both: a
//!   column is either fully placed or not placed at all.

use ratatui::layout::Rect;

/// The Spine's fixed width in the filmstrip (matches the v2 left-list width).
pub const SPINE_WIDTH: u16 = 44;
/// A `claude` PTY below this inner width is unreadable, so non-spine columns
/// never render narrower than this — they letterbox instead.
pub const MIN_COL_WIDTH: u16 = 80;
/// Upper bound on a non-spine column's width, so a single window on a very wide
/// terminal letterboxes (centred) rather than stretching to an unusable width.
pub const MAX_COL_WIDTH: u16 = 120;

/// A window's placement on screen: its index into the window set and the `Rect`
/// to draw it in. Only windows that are actually visible get an entry — the
/// renderer iterates these, so an off-strip column simply isn't drawn (and its
/// PTY is never resized, preserving the idle-quiet property).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Index into `WindowSet::windows`.
    pub index: usize,
    pub rect: Rect,
}

/// The width one non-spine filmstrip column *wants*, given the viewport width:
/// `clamp(viewport, 80..=120)`. Count-independent — it depends only on the
/// viewport, never on how many windows exist — so opening, pinning or scrolling
/// never reflows a live pane. Placement may clamp this down to the strip width
/// on a narrow terminal (where the lone window then letterboxes).
pub fn column_width(viewport_width: u16) -> u16 {
    viewport_width.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH)
}

/// How many whole non-spine columns fit in the strip to the right of the Spine.
/// At least 1 (a lone window letterboxes within it) so focus always has
/// somewhere to land.
pub fn visible_columns(viewport_width: u16) -> usize {
    let strip_w = viewport_width.saturating_sub(SPINE_WIDTH);
    let col = column_width(viewport_width).min(strip_w.max(1));
    ((strip_w / col.max(1)) as usize).max(1)
}

/// Clamp/adjust the horizontal scroll so the focused non-spine column is fully
/// in view. `focus_col` is the focused window's position **among non-spine
/// windows** (0-based); `non_spine` is how many there are. Returns the new
/// scroll offset (count of non-spine columns scrolled off the left).
///
/// Lives here (pure) and is called from focus-move / resize handlers, never from
/// `draw` — `draw` only ever *reads* `scroll_x`.
pub fn scroll_offset(
    current: usize,
    focus_col: Option<usize>,
    non_spine: usize,
    viewport_width: u16,
) -> usize {
    let cols = visible_columns(viewport_width);
    // Never scroll past the point where the last window sits flush right.
    let max_scroll = non_spine.saturating_sub(cols);
    let mut scroll = current.min(max_scroll);
    if let Some(f) = focus_col {
        if f < scroll {
            scroll = f; // focus is left of the window — page left to it
        } else if f >= scroll + cols {
            scroll = f + 1 - cols; // focus is right of the window — page right
        }
    }
    scroll.min(max_scroll)
}

/// Lay out the filmstrip: the Spine pinned left, then the whole columns that fit
/// starting at `scroll_x` (in non-spine-column units). A lone non-spine window
/// letterboxes (centred at its natural width) within the column area. Partially
/// clipped columns are dropped (the snap-to-whole-column rule).
///
/// `n` is the total window count (index 0 = Spine). Returns one [`Placement`]
/// per *visible* window. Never panics on a tiny viewport — it just places
/// fewer windows.
pub fn filmstrip(area: Rect, n: usize, scroll_x: usize) -> Vec<Placement> {
    if n == 0 || area.area() == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();

    // The Spine: fixed width, but never wider than the viewport.
    let spine_w = SPINE_WIDTH.min(area.width);
    out.push(Placement {
        index: 0,
        rect: Rect::new(area.x, area.y, spine_w, area.height),
    });
    if n == 1 || spine_w >= area.width {
        return out;
    }

    let strip_x = area.x + spine_w;
    let strip_w = area.width - spine_w;
    let col_w = column_width(area.width).min(strip_w);
    let cols = (strip_w / col_w.max(1)).max(1) as usize;
    let non_spine = n - 1;

    // A lone non-spine window letterboxes: centre it at its natural width inside
    // the whole strip rather than stretching or left-anchoring it.
    if non_spine == 1 {
        let w = col_w.min(strip_w);
        let x = strip_x + (strip_w - w) / 2;
        out.push(Placement {
            index: 1,
            rect: Rect::new(x, area.y, w, area.height),
        });
        return out;
    }

    let scroll = scroll_x.min(non_spine.saturating_sub(cols));
    for slot in 0..cols {
        let win = 1 + scroll + slot; // window index (skip the Spine)
        if win >= n {
            break;
        }
        let x = strip_x + (slot as u16) * col_w;
        // Snap-to-whole-column: only place a column that fits entirely.
        if x + col_w > strip_x + strip_w {
            break;
        }
        out.push(Placement {
            index: win,
            rect: Rect::new(x, area.y, col_w, area.height),
        });
    }
    out
}

/// Lay out the mosaic: every window tiled near-square to fill `area`, row-major
/// (reusing [`split_grid`]). All `n` windows are placed (none scrolled off).
pub fn mosaic(area: Rect, n: usize) -> Vec<Placement> {
    if n == 0 || area.area() == 0 {
        return Vec::new();
    }
    split_grid(area, n)
        .into_iter()
        .enumerate()
        .map(|(index, rect)| Placement { index, rect })
        .collect()
}

/// Whether window `idx` is on screen in the filmstrip right now — allocation-
/// free, so the render loop can poll it every tick to pick its cadence and gate
/// AgentOutput repaints without materialising the placement list. Keys off the
/// post-scroll visible set: an idle agent scrolled off-screen is *not* visible,
/// so it never pins the loop at 16 ms (the idle-quiet property). Mosaic/zoom
/// visibility is decided by the caller (mosaic shows all; zoom shows focus).
pub fn filmstrip_visible(n: usize, scroll_x: usize, viewport_width: u16, idx: usize) -> bool {
    if idx >= n || viewport_width == 0 {
        return false; // a zero-area strip draws nothing at all
    }
    if idx == 0 {
        return true; // the spine is always on screen (when there's any width)
    }
    // Mirror filmstrip(): when the spine fills the whole width (viewport ≤
    // SPINE_WIDTH) the renderer draws *only* the spine and drops every non-spine
    // column — so none is visible. Without this, the poll loop would fast-poll
    // (and AgentOutput would repaint) for an agent that isn't on screen on a very
    // narrow terminal, defeating the idle-quiet property.
    if SPINE_WIDTH.min(viewport_width) >= viewport_width {
        return false;
    }
    let non_spine = n - 1;
    if non_spine == 1 {
        return true; // the lone window letterboxes — always visible
    }
    let cols = visible_columns(viewport_width);
    let scroll = scroll_x.min(non_spine.saturating_sub(cols));
    let col = idx - 1;
    col >= scroll && col < scroll + cols
}

/// Lay out a single zoomed window filling the whole `area`.
pub fn zoomed(area: Rect, focus: usize) -> Vec<Placement> {
    if area.area() == 0 {
        return Vec::new();
    }
    vec![Placement {
        index: focus,
        rect: area,
    }]
}

/// Split `area` into `k` strips along one axis, giving the remainder to the
/// first strips (so sizes differ by at most one). `vertical` stacks rows;
/// otherwise it lays out columns. (Moved verbatim from the v2 chat wall.)
pub fn split_1d(area: Rect, k: usize, vertical: bool) -> Vec<Rect> {
    if k == 0 {
        return Vec::new();
    }
    let k = k as u16;
    let total = if vertical { area.height } else { area.width };
    let base = total / k;
    let extra = total % k;
    let mut rects = Vec::with_capacity(k as usize);
    let mut pos = if vertical { area.y } else { area.x };
    for i in 0..k {
        let size = base + u16::from(i < extra);
        rects.push(if vertical {
            Rect::new(area.x, pos, area.width, size)
        } else {
            Rect::new(pos, area.y, size, area.height)
        });
        pos = pos.saturating_add(size);
    }
    rects
}

/// Tile `area` into `k` near-square cells, row-major: `ceil(√k)` columns and the
/// rows needed to hold them. A short final row spreads its cells across the full
/// width, so there's no ragged gap. (Moved verbatim from the v2 chat wall.)
pub fn split_grid(area: Rect, k: usize) -> Vec<Rect> {
    if k == 0 || area.area() == 0 {
        return Vec::new();
    }
    // Smallest `c` with `c² ≥ k` — `ceil(√k)`, integer-only (no float casts).
    let cols = (1..=k).find(|c| c * c >= k).unwrap_or(1);
    let rows = k.div_ceil(cols);
    let mut cells = Vec::with_capacity(k);
    for (r, row_rect) in split_1d(area, rows, true).into_iter().enumerate() {
        // The final row holds only the leftover panes.
        let in_row = if r + 1 == rows { k - r * cols } else { cols };
        for cell in split_1d(row_rect, in_row, false) {
            cells.push(cell);
            if cells.len() == k {
                return cells;
            }
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 200, 40)
    }

    #[test]
    fn column_width_clamps_to_the_readable_band() {
        // clamp(viewport, 80..=120): a roomy viewport gives the max column…
        assert_eq!(column_width(200), MAX_COL_WIDTH);
        assert_eq!(column_width(400), MAX_COL_WIDTH);
        // …a mid viewport gives the viewport width…
        assert_eq!(column_width(100), 100);
        // …and a cramped one floors at the readable minimum (placement then
        // clamps it to the strip and the lone window letterboxes).
        assert_eq!(column_width(10), MIN_COL_WIDTH);
    }

    #[test]
    fn filmstrip_always_places_the_spine_first() {
        let p = filmstrip(area(), 3, 0);
        assert_eq!(p[0].index, 0, "the spine leads");
        assert_eq!(p[0].rect.width, SPINE_WIDTH);
        assert_eq!(p[0].rect.x, 0);
    }

    #[test]
    fn filmstrip_drops_partially_clipped_columns() {
        // 200 wide → 156 for the strip → one 120-col column fits whole, a second
        // would be clipped (156 < 240), so only one non-spine column is drawn.
        let p = filmstrip(area(), 4, 0);
        let non_spine = p.iter().filter(|pl| pl.index != 0).count();
        assert_eq!(non_spine, 1, "no partial PTY columns are ever placed");
        // Every placed column is fully inside the area.
        for pl in &p {
            assert!(
                pl.rect.x + pl.rect.width <= area().x + area().width,
                "placement {pl:?} spills past the viewport"
            );
        }
    }

    #[test]
    fn a_lone_window_letterboxes_centred() {
        let p = filmstrip(area(), 2, 0);
        let win = p.iter().find(|pl| pl.index == 1).expect("the lone window");
        // Centred in the strip to the right of the spine, not left-anchored.
        let strip_x = SPINE_WIDTH;
        let strip_w = 200 - SPINE_WIDTH;
        let w = column_width(200); // 120
        assert_eq!(win.rect.width, w);
        assert_eq!(win.rect.x, strip_x + (strip_w - w) / 2);
    }

    #[test]
    fn scroll_keeps_the_focused_column_in_view() {
        // Strip fits one 80-col column. Focus on the 3rd non-spine window must
        // page the scroll so it's the visible column.
        let vw = 200;
        let cols = visible_columns(vw);
        assert_eq!(cols, 1);
        // Focus right of the window → scroll forward to show it.
        let s = scroll_offset(0, Some(2), 5, vw);
        assert_eq!(s, 2, "focus column 2 becomes the (only) visible column");
        // Focus left of the window → page back.
        let s = scroll_offset(4, Some(1), 5, vw);
        assert_eq!(s, 1);
        // Never scrolls past the last window.
        let s = scroll_offset(99, None, 5, vw);
        assert_eq!(s, 5 - cols);
    }

    #[test]
    fn mosaic_places_every_window() {
        let p = mosaic(area(), 5);
        assert_eq!(p.len(), 5, "mosaic never drops a window");
        let idxs: Vec<usize> = p.iter().map(|pl| pl.index).collect();
        assert_eq!(idxs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn layout_helpers_survive_a_zero_area() {
        let z = Rect::new(0, 0, 0, 0);
        assert!(filmstrip(z, 3, 0).is_empty());
        assert!(mosaic(z, 3).is_empty());
        assert!(split_grid(z, 3).is_empty());
        assert!(zoomed(z, 0).is_empty());
    }

    #[test]
    fn filmstrip_visible_agrees_with_what_filmstrip_actually_places() {
        // The poll loop and AgentOutput repaint gate on `filmstrip_visible`; it
        // must report exactly the windows `filmstrip` draws, or an off-screen
        // agent would pin the loop at 16 ms (notably at width ≤ SPINE_WIDTH, where
        // the renderer draws only the spine).
        for width in [0u16, 1, 10, 30, 43, 44, 45, 80, 124, 165, 200, 320] {
            for n in 1..=6usize {
                for scroll in 0..=5usize {
                    let area = Rect::new(0, 0, width, 24);
                    let placed: std::collections::HashSet<usize> =
                        filmstrip(area, n, scroll).iter().map(|p| p.index).collect();
                    for idx in 0..n {
                        assert_eq!(
                            filmstrip_visible(n, scroll, width, idx),
                            placed.contains(&idx),
                            "disagreement: width={width} n={n} scroll={scroll} idx={idx}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn split_grid_tiles_without_gaps_or_overlaps() {
        // Four panes on a 200×40 area tile 2×2, covering it exactly.
        let cells = split_grid(area(), 4);
        assert_eq!(cells.len(), 4);
        let covered: u32 = cells.iter().map(|r| r.area()).sum();
        assert_eq!(covered, area().area(), "the grid tiles the area exactly");
    }
}
