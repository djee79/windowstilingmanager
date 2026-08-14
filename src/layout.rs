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
