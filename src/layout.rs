//! Pure window-placement geometry for the cockpit strip — no ratatui state, no
//! `App`, no I/O, so every rule here is unit-testable in isolation (like the v2
//! `tail_fit` / `split` helpers it sits beside).
//!
//! Both layouts pin the **Spine** left at a fixed width; which one is in force is
//! chosen by the docked-coin count (see [`crate::window::WindowSet::auto_layout`]),
//! not a manual toggle:
//! * **Mosaic** (≤ `MOSAIC_MAX` docked coins) tiles every non-Spine window
//!   near-square via [`split_grid`] in the area right of the Spine — the
//!   full-attention layout where every coin (including the preview, when it
//!   exists) is live at once. No duplicate ever appears, because the preview is
//!   suppressed upstream for a selection that already has a pinned coin.
//! * **Rail** (more docked coins) gives the focused window — or the active window
//!   (the selection's pinned coin, else the preview) when the Spine is focused — a
//!   single **big pane**, and lists every *other docked* window as a compact
//!   **card** down a thin right-hand rail. The preview is **never** a card; it
//!   shows only as the big pane. Only the big pane hosts a live PTY, so the rail
//!   sidesteps tui-term 0.3.4's horizontal-clipping limit (it paints only the
//!   top-left `w×h` subgrid of the vt100 parser, with no horizontal offset) and
//!   preserves the idle-quiet property.

use ratatui::layout::Rect;

/// The Spine's fixed width (matches the v2 left-list width).
pub const SPINE_WIDTH: u16 = 44;
/// The width of the right-hand rail of status cards.
const RAIL_WIDTH: u16 = 32;
/// The big pane never shrinks below this. On a terminal too narrow to fit both a
/// readable big pane and the rail, the rail is dropped — cards aren't drawn, but
/// every window stays focusable (focusing it makes it the big pane).
const MIN_BIG_WIDTH: u16 = 50;

/// A window's placement on screen: its index into the window set and the `Rect`
/// to draw it in. The renderer iterates these, so a window with no placement
/// simply isn't drawn (and its PTY is never resized — the idle-quiet property).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Index into `WindowSet::windows`.
    pub index: usize,
    pub rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailOverflow {
    pub hidden: usize,
    pub rect: Rect,
}

/// Which window is the big (live-PTY) pane in the rail layout: the focused window,
/// or — when the Spine itself is focused — the **active** window (the selection's
/// pinned coin if it has one, else the transient preview). `None` only when there
/// is nothing but the Spine (or `focus`/`active_idx` are out of range).
pub fn rail_big_index(n: usize, focus: usize, active_idx: Option<usize>) -> Option<usize> {
    if n == 0 {
        return None;
    }
    if focus == 0 {
        // The Spine is focused → the active window (pinned coin or preview) is big.
        active_idx.filter(|&i| i < n)
    } else if focus < n {
        Some(focus)
    } else {
        None
    }
}

/// Lay out the rail: the Spine pinned left, one big pane (the focused window, or
/// the active window when the Spine is focused), and a column of cards for every
/// other *docked* window — never the preview (it shows only as the big pane).
/// `active_idx` is the selection's pinned coin or the preview; `preview_idx` is the
/// unpinned coin (excluded from cards). Never panics on a tiny viewport.
pub fn rail(
    area: Rect,
    n: usize,
    focus: usize,
    active_idx: Option<usize>,
    preview_idx: Option<usize>,
) -> (Vec<Placement>, Vec<Placement>, Option<RailOverflow>) {
    if n == 0 || area.area() == 0 {
        return (Vec::new(), Vec::new(), None);
    }
    let mut full = Vec::new();

    let spine_w = SPINE_WIDTH.min(area.width);
    full.push(Placement {
        index: 0,
        rect: Rect::new(area.x, area.y, spine_w, area.height),
    });
    // No room past the Spine → just the Spine (everything else is off-screen).
    if spine_w >= area.width {
        return (full, Vec::new(), None);
    }

    let big = rail_big_index(n, focus, active_idx);
    let rest_x = area.x + spine_w;
    let rest_w = area.width - spine_w;

    // Every non-spine window that isn't the big pane becomes a card, in order —
    // except the preview, which is drawn only when it IS the big pane.
    let card_indices: Vec<usize> = (1..n)
        .filter(|&i| Some(i) != big && Some(i) != preview_idx)
        .collect();

    // Reserve the rail only when there are cards AND the big pane stays readable;
    // otherwise drop it (the cards' windows are still reachable by focusing them).
    let rail_w = if !card_indices.is_empty() && rest_w >= MIN_BIG_WIDTH + RAIL_WIDTH {
        RAIL_WIDTH
    } else {
        0
    };
    let big_w = rest_w - rail_w;

    if let Some(bi) = big
        && big_w > 0
    {
        full.push(Placement {
            index: bi,
            rect: Rect::new(rest_x, area.y, big_w, area.height),
        });
    }

    let mut cards = Vec::new();
    let mut overflow = None;
    if rail_w > 0 {
        let rail_rect = Rect::new(rest_x + big_w, area.y, rail_w, area.height);
        let slots = usize::from(rail_rect.height);
        if slots > 0 {
            let visible = if card_indices.len() > slots {
                slots.saturating_sub(1)
            } else {
                card_indices.len()
            };
            let hidden = card_indices.len().saturating_sub(visible);
            let total_slots = visible + usize::from(hidden > 0);
            let rects = split_1d(rail_rect, total_slots, true);
            for (slot, rect) in rects.iter().copied().take(visible).enumerate() {
                cards.push(Placement {
                    index: card_indices[slot],
                    rect,
                });
            }
            if hidden > 0
                && let Some(rect) = rects.get(visible).copied()
            {
                overflow = Some(RailOverflow { hidden, rect });
            }
        }
    }
    (full, cards, overflow)
}

/// Whether window `idx` renders as a live PTY in the rail right now — i.e. it is
/// the big pane. Allocation-free, so the poll loop and the AgentOutput repaint
/// gate can call it every tick. A carded agent returns `false`, so it never pins
/// the loop at 16 ms (the idle-quiet property). Takes `viewport_width` so it
/// mirrors [`rail`]'s early return — at a width that leaves no room past the
/// Spine, nothing is the big pane, so an off-screen agent never pins the loop.
pub fn rail_visible(
    n: usize,
    focus: usize,
    active_idx: Option<usize>,
    idx: usize,
    viewport_width: u16,
) -> bool {
    if SPINE_WIDTH.min(viewport_width) >= viewport_width {
        return false; // no room past the Spine → rail() draws no big pane
    }
    rail_big_index(n, focus, active_idx) == Some(idx)
}

/// Lay out the mosaic: the Spine pinned left, every non-Spine window tiled
/// near-square in the area to its right (reusing [`split_grid`]) — the
/// full-attention layout where every coin (including the preview, when it exists)
/// is live at once. Row-major over `1..n`. There is never a duplicate, because the
/// preview is suppressed for a selection that already has a pinned coin.
pub fn mosaic(area: Rect, n: usize) -> Vec<Placement> {
    if n == 0 || area.area() == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let spine_w = SPINE_WIDTH.min(area.width);
    out.push(Placement {
        index: 0,
        rect: Rect::new(area.x, area.y, spine_w, area.height),
    });
    if spine_w >= area.width {
        return out; // only the Spine fits
    }
    let rest = Rect::new(area.x + spine_w, area.y, area.width - spine_w, area.height);
    for (slot, rect) in split_grid(rest, n - 1).into_iter().enumerate() {
        out.push(Placement {
            index: slot + 1,
            rect,
        });
    }
    out
}

/// Whether window `idx` is drawn (and so its PTY is live) in the mosaic right now
/// — every non-Spine window is, so this just guards the bounds and the cramped-
/// terminal early return (only the Spine fits at a width ≤ `SPINE_WIDTH`), keeping
/// the poll-cadence gate in lock-step with the renderer.
pub fn mosaic_visible(n: usize, idx: usize, viewport_width: u16) -> bool {
    if idx >= n || viewport_width == 0 {
        return false;
    }
    if idx == 0 {
        return true; // the Spine is always placed (it's never a live PTY anyway)
    }
    SPINE_WIDTH.min(viewport_width) < viewport_width // false → too narrow for coins
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
    fn rail_big_index_is_focus_or_preview_when_spine_focused() {
        // Spine focused (0) → the active window (here index 1) is the big pane.
        assert_eq!(rail_big_index(3, 0, Some(1)), Some(1));
        // A non-spine focus is itself the big pane.
        assert_eq!(rail_big_index(3, 2, Some(1)), Some(2));
        // Spine focused with no active window → nothing to enlarge.
        assert_eq!(rail_big_index(1, 0, None), None);
    }

    #[test]
    fn rail_places_the_spine_then_the_big_pane_then_cards_never_the_preview() {
        // windows: [Spine, Preview(1), Pin(2), Pin(3)], focus on Pin(2); the
        // selection is unpinned so active == preview == 1.
        let (full, cards, overflow) = rail(area(), 4, 2, Some(1), Some(1));
        assert!(overflow.is_none());
        assert_eq!(full[0].index, 0, "the spine leads");
        assert_eq!(full[0].rect.width, SPINE_WIDTH);
        assert_eq!(full[0].rect.x, 0);
        assert_eq!(full[1].index, 2, "the focused window is the big pane");
        assert!(
            full[1].rect.width >= MIN_BIG_WIDTH,
            "the big pane stays readable"
        );
        // Only the OTHER pinned window (3) is a card; the preview (1) is never one.
        let carded: Vec<usize> = cards.iter().map(|p| p.index).collect();
        assert_eq!(carded, vec![3], "the preview is never carded");
        // Cards sit to the right of the big pane, never overlapping it.
        let big = full[1].rect;
        for c in &cards {
            assert!(c.rect.x >= big.x + big.width, "card overlaps the big pane");
            assert!(c.rect.x + c.rect.width <= area().x + area().width);
        }
    }

    #[test]
    fn rail_cards_every_pinned_coin_when_the_spine_is_focused() {
        // Spine focused → the active window (here the preview, 1) is the big pane;
        // both pins (2,3) card.
        let (full, cards, overflow) = rail(area(), 4, 0, Some(1), Some(1));
        assert!(overflow.is_none());
        assert_eq!(full[1].index, 1, "the preview is the big pane");
        let carded: Vec<usize> = cards.iter().map(|p| p.index).collect();
        assert_eq!(carded, vec![2, 3]);
    }

    #[test]
    fn rail_drops_the_card_column_on_a_narrow_terminal() {
        // 80 wide → 36 past the spine: not enough for a readable big pane + rail,
        // so the big pane takes it all and no cards are drawn (still focusable).
        let (full, cards, overflow) = rail(Rect::new(0, 0, 80, 24), 4, 2, Some(1), Some(1));
        assert!(overflow.is_none());
        assert_eq!(full.len(), 2, "spine + big pane");
        assert!(cards.is_empty(), "the rail is dropped when it won't fit");
        assert_eq!(
            full[1].rect.width,
            80 - SPINE_WIDTH,
            "big pane takes the rest"
        );
    }

    #[test]
    fn rail_visible_agrees_with_what_rail_actually_enlarges() {
        // The poll loop / AgentOutput gate keys off rail_visible; it must report
        // exactly the one window rail() draws as the big pane (else an off-screen
        // agent would pin the loop at 16 ms). Sweep widths — including the
        // ≤ SPINE_WIDTH band where rail() drops the big pane entirely.
        for width in [0u16, 1, 30, 44, 45, 80, 200] {
            for n in 1..=5usize {
                for focus in 0..n {
                    let preview = (n > 1).then_some(1usize);
                    let active = preview; // unpinned selection → active == preview
                    let (full, _cards, _overflow) =
                        rail(Rect::new(0, 0, width, 24), n, focus, active, preview);
                    // The big pane is the one full placement past the Spine (idx 0).
                    let big = full.iter().map(|p| p.index).find(|&i| i != 0);
                    for idx in 0..n {
                        assert_eq!(
                            rail_visible(n, focus, active, idx, width),
                            big == Some(idx),
                            "rail disagreement: width={width} n={n} focus={focus} idx={idx}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mosaic_pins_the_spine_left_and_tiles_every_non_spine_window() {
        // [Spine, Coin(1), Coin(2), Coin(3), Coin(4)] → spine left + 4 tiles. There
        // is no focus/preview dependence: a duplicate preview is suppressed upstream
        // (in reaim_preview), so mosaic simply tiles whatever windows exist.
        let p = mosaic(area(), 5);
        assert_eq!(p.len(), 5, "the spine + every non-spine window");
        assert_eq!(p[0].index, 0, "the spine leads");
        assert_eq!(
            p[0].rect.width, SPINE_WIDTH,
            "the spine keeps its fixed width"
        );
        let idxs: Vec<usize> = p.iter().map(|pl| pl.index).collect();
        assert_eq!(idxs, vec![0, 1, 2, 3, 4]);
        // Every tiled coin sits to the right of the spine.
        for pl in &p[1..] {
            assert!(pl.rect.x >= SPINE_WIDTH, "a coin overlaps the spine");
        }
    }

    #[test]
    fn mosaic_visible_agrees_with_what_mosaic_actually_places() {
        // Sweep widths — including the ≤ SPINE_WIDTH band where mosaic() places
        // only the Spine — so the poll-cadence gate never fast-polls an agent the
        // renderer dropped.
        for width in [0u16, 1, 30, 44, 45, 80, 200] {
            for n in 1..=5usize {
                let placed: Vec<usize> = mosaic(Rect::new(0, 0, width, 24), n)
                    .iter()
                    .map(|p| p.index)
                    .collect();
                for idx in 0..n {
                    assert_eq!(
                        mosaic_visible(n, idx, width),
                        placed.contains(&idx),
                        "mosaic disagreement: width={width} n={n} idx={idx}"
                    );
                }
            }
        }
    }

    #[test]
    fn layout_helpers_survive_a_zero_area() {
        let z = Rect::new(0, 0, 0, 0);
        assert_eq!(
            rail(z, 3, 0, Some(1), Some(1)),
            (Vec::new(), Vec::new(), None)
        );
        assert!(mosaic(z, 3).is_empty());
        assert!(split_grid(z, 3).is_empty());
    }

    #[test]
    fn rail_survives_a_width_at_or_below_the_spine() {
        // At width ≤ SPINE_WIDTH the renderer draws only the spine and no cards —
        // so no carded agent is "visible" and the poll loop stays quiet.
        for w in [1u16, 20, 44] {
            let (full, cards, overflow) = rail(Rect::new(0, 0, w, 24), 4, 2, Some(1), Some(1));
            assert_eq!(full.len(), 1, "only the spine fits");
            assert!(cards.is_empty());
            assert!(overflow.is_none());
        }
    }

    #[test]
    fn rail_uses_a_more_marker_instead_of_zero_height_cards() {
        let tiny = Rect::new(0, 0, 200, 3);
        let (full, cards, overflow) = rail(tiny, 8, 0, Some(1), Some(1));
        assert_eq!(full.len(), 2, "spine + big pane");
        assert_eq!(
            cards.len(),
            2,
            "height 3 reserves the last slot for overflow"
        );
        let overflow = overflow.expect("extra cards collapse into a marker");
        assert_eq!(overflow.hidden, 4);
        assert!(cards.iter().all(|p| p.rect.height > 0));
        assert!(overflow.rect.height > 0);
    }

    #[test]
    fn split_grid_tiles_without_gaps_or_overlaps() {
        // Paint each cell into a per-cell coverage grid and assert every cell is
        // covered EXACTLY once — the property the name advertises. An area-sum
        // check can't distinguish a gap from an overlap; this can, and it also
        // exercises ragged final rows (k = 5, 7) where the risk actually lives.
        let a = area(); // 200×40
        let w = a.width as usize;
        for k in [1usize, 2, 3, 4, 5, 7, 9] {
            let cells = split_grid(a, k);
            assert_eq!(cells.len(), k, "k={k}: one rect per pane");
            let mut cover = vec![0u16; w * a.height as usize];
            for r in &cells {
                for y in r.y..r.y + r.height {
                    for x in r.x..r.x + r.width {
                        cover[y as usize * w + x as usize] += 1;
                    }
                }
            }
            assert!(
                cover.iter().all(|&c| c == 1),
                "k={k}: every cell covered exactly once — no gaps, no overlaps"
            );
        }
    }
}
