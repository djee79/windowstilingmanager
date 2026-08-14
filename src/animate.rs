//! Window movement animation: instead of teleporting on retile, windows
//! glide to their slot over ~150ms with an ease-out curve, Hyprland-style.
//!
//! Driven by a thread timer (~60fps) serviced from the main message loop.
//! Retargeting mid-flight is fine — the animation restarts from wherever the
//! window currently is, so rapid layout changes stay smooth.

use crate::layout::Rect;
use crate::window::Window;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::Instant;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

const FRAME_MS: u32 = 15; // ~66fps ceiling; WM_TIMER granularity is ~10-15ms

struct Anim {
    from: Rect,
    to: Rect,
    /// DWM shadow offsets captured once at start — stable per window.
    offsets: (i32, i32, i32, i32),
    start: Instant,
    duration_ms: u32,
}

thread_local! {
    static ANIMS: RefCell<HashMap<isize, Anim>> = RefCell::new(HashMap::new());
    static TIMER_ID: Cell<usize> = const { Cell::new(0) };
}

/// Move a window toward `target`, animated over `duration_ms` (0 = instant).
pub fn set_target(w: Window, target: Rect, duration_ms: u32) {
    if duration_ms == 0 || !w.is_visible() {
        ANIMS.with(|a| a.borrow_mut().remove(&w.0));
        w.apply_rect(target);
        return;
    }
    let from = w.visible_rect();
    if from == target {
        ANIMS.with(|a| a.borrow_mut().remove(&w.0));
        return;
    }
    w.prepare_for_move();
    let anim = Anim {
        from,
        to: target,
        offsets: w.frame_offsets(),
        start: Instant::now(),
        duration_ms,
    };
    ANIMS.with(|a| {
        a.borrow_mut().insert(w.0, anim);
    });
    ensure_timer();
}

pub fn is_anim_timer(id: usize) -> bool {
    id != 0 && TIMER_ID.with(|t| t.get()) == id
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn lerp(a: i32, b: i32, t: f32) -> i32 {
    a + ((b - a) as f32 * t) as i32
}

/// Advance all animations one frame. Called from the message loop on
/// WM_TIMER; also kills the timer once everything has settled.
pub fn tick() {
    let now = Instant::now();
    let mut finished: Vec<isize> = Vec::new();
    ANIMS.with(|cell| {
        let mut anims = cell.borrow_mut();
        for (handle, anim) in anims.iter() {
            let w = Window(*handle);
            if !w.is_valid() || !w.is_visible() {
                finished.push(*handle);
                continue;
            }
            let elapsed = now.duration_since(anim.start).as_millis() as f32;
            let t = (elapsed / anim.duration_ms as f32).min(1.0);
            let e = ease_out_cubic(t);
            let rect = Rect {
                x: lerp(anim.from.x, anim.to.x, e),
                y: lerp(anim.from.y, anim.to.y, e),
                w: lerp(anim.from.w, anim.to.w, e),
                h: lerp(anim.from.h, anim.to.h, e),
            };
            w.place_visible(rect, anim.offsets);
            if t >= 1.0 {
                finished.push(*handle);
            }
        }
        for h in &finished {
            anims.remove(h);
        }
        if anims.is_empty() {
            stop_timer();
        }
    });
    // Keep the focus frame glued to its window while it glides.
    crate::border::update();
}

fn ensure_timer() {
    TIMER_ID.with(|t| {
        if t.get() == 0 {
            let id = unsafe { SetTimer(None, 0, FRAME_MS, None) };
            t.set(id);
        }
    });
}

fn stop_timer() {
    TIMER_ID.with(|t| {
        if t.get() != 0 {
            let _ = unsafe { KillTimer(None, t.get()) };
            t.set(0);
        }
    });
}
