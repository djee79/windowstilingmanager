//! Layout math. Pure functions, no Win32 — this is the easiest part of the
//! codebase to unit test.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn shrink(&self, by: i32) -> Rect {
        Rect {
            x: self.x + by,
            y: self.y + by,
            w: (self.w - 2 * by).max(1),
            h: (self.h - 2 * by).max(1),
        }
    }
}

/// Hyprland-style "dwindle" spiral: each window splits the remaining space,
/// alternating orientation based on which side of the remainder is longer.
/// `ratios[i]` is the share window i keeps at its split, so each divider can
/// be resized independently; missing entries fall back to an even split.
pub fn dwindle(area: Rect, count: usize, ratios: &[f32], gap: i32) -> Vec<Rect> {
    let mut out = Vec::with_capacity(count);
    if count == 0 {
        return out;
    }
    let mut cur = area;
    for i in 0..count {
        if i == count - 1 {
            out.push(cur);
            break;
        }
        let ratio = ratios.get(i).copied().unwrap_or(0.5).clamp(0.1, 0.9);
        if cur.w >= cur.h {
            // Split vertically: left part | gap | right part
            let left = (((cur.w - gap).max(2)) as f32 * ratio) as i32;
            out.push(Rect { x: cur.x, y: cur.y, w: left.max(1), h: cur.h });
            cur = Rect {
                x: cur.x + left + gap,
                y: cur.y,
                w: (cur.w - left - gap).max(1),
                h: cur.h,
            };
        } else {
            // Split horizontally: top part / gap / bottom part
            let top = (((cur.h - gap).max(2)) as f32 * ratio) as i32;
            out.push(Rect { x: cur.x, y: cur.y, w: cur.w, h: top.max(1) });
            cur = Rect {
                x: cur.x,
                y: cur.y + top + gap,
                w: cur.w,
                h: (cur.h - top - gap).max(1),
            };
        }
    }
    out
}

/// Translate a mouse-resize of tiled window `idx` into ratio adjustments:
/// the window's rect changed from its computed slot to `actual`, so find
/// which split dividers its moved edges sit on and re-derive those ratios.
/// A corner drag moves two edges and adjusts two dividers.
pub fn resize_ratios(
    area: Rect,
    count: usize,
    ratios: &mut Vec<f32>,
    gap: i32,
    idx: usize,
    actual: Rect,
) {
    const TOL: i32 = 4; // ignore sub-pixel/rounding jitter
    if count < 2 || idx >= count {
        return;
    }
    if ratios.len() < count - 1 {
        ratios.resize(count - 1, 0.5);
    }
    let exp = dwindle(area, count, ratios, gap)[idx];
    let mut cur = area;
    for j in 0..count - 1 {
        let ratio = ratios[j].clamp(0.1, 0.9);
        if cur.w >= cur.h {
            let span = (cur.w - gap).max(2);
            let left = (span as f32 * ratio) as i32;
            let divider = cur.x + left; // right edge of window j
            // Window j's own right edge, or a later window's left edge that
            // sits on this divider — either way the user dragged this split.
            let new_div = if j == idx {
                let (oe, ne) = (exp.x + exp.w, actual.x + actual.w);
                ((ne - oe).abs() > TOL).then_some(ne)
            } else if (exp.x - (divider + gap)).abs() <= 2 && (actual.x - exp.x).abs() > TOL {
                Some(actual.x - gap)
            } else {
                None
            };
            if let Some(d) = new_div {
                ratios[j] = ((d - cur.x) as f32 / span as f32).clamp(0.15, 0.85);
            }
            cur = Rect { x: cur.x + left + gap, y: cur.y, w: (cur.w - left - gap).max(1), h: cur.h };
        } else {
            let span = (cur.h - gap).max(2);
            let top = (span as f32 * ratio) as i32;
            let divider = cur.y + top; // bottom edge of window j
            let new_div = if j == idx {
                let (oe, ne) = (exp.y + exp.h, actual.y + actual.h);
                ((ne - oe).abs() > TOL).then_some(ne)
            } else if (exp.y - (divider + gap)).abs() <= 2 && (actual.y - exp.y).abs() > TOL {
                Some(actual.y - gap)
            } else {
                None
            };
            if let Some(d) = new_div {
                ratios[j] = ((d - cur.y) as f32 / span as f32).clamp(0.15, 0.85);
            }
            cur = Rect { x: cur.x, y: cur.y + top + gap, w: cur.w, h: (cur.h - top - gap).max(1) };
        }
        if j == idx {
            break; // later dividers sit inside the remainder, away from idx
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };

    #[test]
    fn empty() {
        assert!(dwindle(AREA, 0, &[], 8).is_empty());
    }

    #[test]
    fn single_window_fills_area() {
        assert_eq!(dwindle(AREA, 1, &[], 8), vec![AREA]);
    }

    #[test]
    fn two_windows_split_side_by_side() {
        let r = dwindle(AREA, 2, &[0.5], 8);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].h, 1080);
        assert_eq!(r[1].h, 1080);
        assert_eq!(r[0].w + 8 + r[1].w, 1920);
    }

    #[test]
    fn per_split_ratios_are_independent() {
        // Grow only the first divider; the second split stays even.
        let r = dwindle(AREA, 3, &[0.7, 0.5], 10);
        assert!(r[0].w > AREA.w / 2, "first window should be ~70% wide");
        assert!((r[1].h - r[2].h).abs() <= 1, "second split should stay even");
    }

    #[test]
    fn third_window_splits_the_right_column() {
        let r = dwindle(AREA, 3, &[0.5, 0.5], 8);
        assert_eq!(r.len(), 3);
        // Second and third windows stack in the right column.
        assert_eq!(r[1].x, r[2].x);
        assert_eq!(r[1].h + 8 + r[2].h, 1080);
    }

    #[test]
    fn drag_right_edge_of_master_grows_first_split() {
        let mut ratios = vec![0.5, 0.5];
        let r = dwindle(AREA, 3, &ratios, 8);
        let dragged = Rect { w: r[0].w + 200, ..r[0] };
        resize_ratios(AREA, 3, &mut ratios, 8, 0, dragged);
        assert!(
            ratios[0] > 0.58 && ratios[0] < 0.64,
            "ratio should grow ~0.6, got {}",
            ratios[0]
        );
        assert_eq!(ratios[1], 0.5, "untouched split must stay");
        // The new layout should honor the drag (±2px rounding).
        let after = dwindle(AREA, 3, &ratios, 8);
        assert!((after[0].w - dragged.w).abs() <= 2);
    }

    #[test]
    fn drag_left_edge_of_stacked_window_adjusts_earlier_split() {
        let mut ratios = vec![0.5, 0.5];
        let r = dwindle(AREA, 3, &ratios, 8);
        // Window 2 sits in the right column; dragging its left edge out
        // moves the *first* divider.
        let dragged = Rect { x: r[2].x - 200, w: r[2].w + 200, ..r[2] };
        resize_ratios(AREA, 3, &mut ratios, 8, 2, dragged);
        assert!(ratios[0] < 0.43, "first split should shrink, got {}", ratios[0]);
        assert_eq!(ratios[1], 0.5);
    }

    #[test]
    fn corner_drag_adjusts_two_splits() {
        let mut ratios = vec![0.5, 0.5];
        let r = dwindle(AREA, 3, &ratios, 8);
        // Window 1 (top of the right column): drag its bottom-left corner —
        // left edge is split 0's divider, bottom edge is split 1's divider.
        let dragged = Rect { x: r[1].x - 100, w: r[1].w + 100, h: r[1].h + 100, ..r[1] };
        resize_ratios(AREA, 3, &mut ratios, 8, 1, dragged);
        assert!(ratios[0] < 0.47);
        assert!(ratios[1] > 0.53);
    }

    #[test]
    fn tiny_jitter_leaves_ratios_alone() {
        let mut ratios = vec![0.5, 0.5];
        let r = dwindle(AREA, 3, &ratios, 8);
        let jitter = Rect { w: r[0].w + 2, ..r[0] };
        resize_ratios(AREA, 3, &mut ratios, 8, 0, jitter);
        assert_eq!(ratios, vec![0.5, 0.5]);
    }

    #[test]
    fn rects_never_overlap() {
        for n in 1..10 {
            let rects = dwindle(AREA, n, &[0.62; 9], 10);
            for (i, a) in rects.iter().enumerate() {
                for b in rects.iter().skip(i + 1) {
                    let overlap = a.x < b.x + b.w
                        && b.x < a.x + a.w
                        && a.y < b.y + b.h
                        && b.y < a.y + a.h;
                    assert!(!overlap, "n={n}: {a:?} overlaps {b:?}");
                }
            }
        }
    }
}
