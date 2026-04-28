use std::time::{Duration, Instant};

use skia_safe::{Canvas, Color, Font, FontMgr, Paint, PaintStyle, Point, Rect, Typeface};
use winit::event::{KeyEvent, Modifiers};

use crate::cell::Cell;

const FONT_BYTES: &[u8] = include_bytes!("../resources/fonts/Figtree.ttf");

const MARGIN_X: f32 = 40.0;
const MARGIN_TOP: f32 = 60.0;
const TITLE_FONT_SIZE: f32 = 36.0;
const TITLE_TO_BODY_GAP: f32 = 18.0;
const CELL_GAP: f32 = 20.0;
const FOCUS_PAD: f32 = 6.0;
const FOCUS_RADIUS: f32 = 8.0;
const FOCUS_STROKE: f32 = 1.5;
const DOC_BOTTOM_PAD: f32 = 24.0;
const SCROLLBAR_INSET: f32 = 4.0;
const SCROLLBAR_WIDTH: f32 = 4.0;
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
const SCROLLBAR_HOLD: Duration = Duration::from_millis(800);
const SCROLLBAR_FADE: Duration = Duration::from_millis(700);

const SEED_TEXTS: &[&str] = &[
    "First cell — try clicking between cells. Each one is its own little editor.",
    "Kept is a small, intentional space for the things you actually want to hold on to — the kind \
of details that drift out of inboxes and chat threads before you remember why they mattered. It \
is not a database, not a knowledge graph, not a second brain; it's a sturdy shelf with a few good \
hooks. Open it on a quiet morning, write down the name of someone you'd like to talk to again, \
the title of a book a friend mentioned, a question you haven't yet found the right time to ask. \
Close it. Come back later and find it where you left it, exactly as you put it down, because the \
only feature this app commits to is keeping.",
    "Third cell. Selections in one cell don't bleed into another. Try double-click and triple-click \
in here while a different cell is focused — the click count is per-cell.",
];

pub struct KeptApp {
    typeface: Typeface,
    cells: Vec<Cell>,
    focused: usize,
    dragging_cell: Option<usize>,
    scroll_y: f32,
    max_scroll: f32,
    doc_height: f32,
    viewport_height: f32,
    last_scroll_time: Option<Instant>,
}

impl KeptApp {
    pub fn new() -> Self {
        let typeface = FontMgr::new()
            .new_from_data(FONT_BYTES, None)
            .expect("failed to load embedded TTF");
        let cells = SEED_TEXTS
            .iter()
            .map(|s| Cell::new(typeface.clone(), (*s).to_string()))
            .collect();
        Self {
            typeface,
            cells,
            focused: 0,
            dragging_cell: None,
            scroll_y: 0.0,
            max_scroll: 0.0,
            doc_height: 0.0,
            viewport_height: 0.0,
            last_scroll_time: None,
        }
    }

    pub fn scroll_by(&mut self, dy: f32) -> bool {
        let new_y = (self.scroll_y + dy).clamp(0.0, self.max_scroll);
        if new_y == self.scroll_y {
            return false;
        }
        self.scroll_y = new_y;
        self.last_scroll_time = Some(Instant::now());
        true
    }

    pub fn tick(&mut self, canvas: &Canvas, width: f32, height: f32) {
        canvas.clear(Color::from_rgb(0xfa, 0xf7, 0xf2));

        // Clamp scroll using last frame's max_scroll before drawing this frame.
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll);

        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        // Document space — translate so doc y=0 lands at window y = -scroll_y.
        canvas.save();
        canvas.translate((0.0, -self.scroll_y));

        let mut y = MARGIN_TOP;

        let title_font = Font::from_typeface(&self.typeface, TITLE_FONT_SIZE);
        let (_, title_metrics) = title_font.metrics();

        y += -title_metrics.ascent;
        canvas.draw_str("Kept", Point::new(MARGIN_X, y), &title_font, &text_paint);
        y += title_metrics.descent + title_metrics.leading + TITLE_TO_BODY_GAP;

        let cell_width = (width - MARGIN_X * 2.0).max(80.0);
        for (i, cell) in self.cells.iter_mut().enumerate() {
            let h = cell.tick(canvas, MARGIN_X, y, cell_width, i == self.focused);
            y += h + CELL_GAP;
        }

        // Focus rect for the active cell (still in document coords).
        if let Some(cell) = self.cells.get(self.focused) {
            let mut focus_paint = Paint::default();
            focus_paint.set_anti_alias(true);
            focus_paint.set_style(PaintStyle::Stroke);
            focus_paint.set_stroke_width(FOCUS_STROKE);
            focus_paint.set_color(Color::from_argb(0xb0, 0x4a, 0x90, 0xe2));
            let rect = Rect::new(
                cell.x_origin() - FOCUS_PAD,
                cell.y_origin() - FOCUS_PAD,
                cell.x_origin() + cell.width() + FOCUS_PAD,
                cell.y_origin() + cell.height() + FOCUS_PAD,
            );
            canvas.draw_round_rect(rect, FOCUS_RADIUS, FOCUS_RADIUS, &focus_paint);
        }

        canvas.restore();

        // Update bookkeeping for scroll math + clamp again in case content shrank.
        self.doc_height = y - CELL_GAP + DOC_BOTTOM_PAD;
        self.viewport_height = height.max(0.0);
        self.max_scroll = (self.doc_height - self.viewport_height).max(0.0);
        self.scroll_y = self.scroll_y.min(self.max_scroll);

        // Scrollbar lives in window coords (no translate), so it doesn't scroll.
        if self.max_scroll > 0.0 {
            let alpha = scrollbar_alpha(self.last_scroll_time);
            if alpha > 0.0 {
                let track_top = 6.0_f32;
                let track_bot = self.viewport_height - 6.0;
                let track_len = (track_bot - track_top).max(1.0);
                let raw_thumb = (self.viewport_height / self.doc_height) * track_len;
                let thumb_h = raw_thumb.max(SCROLLBAR_MIN_THUMB).min(track_len);
                let thumb_top = track_top
                    + (self.scroll_y / self.max_scroll) * (track_len - thumb_h);
                let thumb_bot = thumb_top + thumb_h;
                let bar_x = width - SCROLLBAR_INSET - SCROLLBAR_WIDTH;

                let mut sb_paint = Paint::default();
                sb_paint.set_anti_alias(true);
                let alpha_byte = (alpha * 0xb0 as f32).round() as u8;
                sb_paint.set_color(Color::from_argb(alpha_byte, 0x1c, 0x1c, 0x1c));
                let r = SCROLLBAR_WIDTH * 0.5;
                canvas.draw_round_rect(
                    Rect::new(bar_x, thumb_top, bar_x + SCROLLBAR_WIDTH, thumb_bot),
                    r,
                    r,
                    &sb_paint,
                );
            }
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        if let Some(cell) = self.cells.get_mut(self.focused) {
            cell.handle_key(event, modifiers)
        } else {
            false
        }
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, modifiers: &Modifiers) -> bool {
        let doc_y = y + self.scroll_y;
        let Some(target) = self.find_cell_at(x, doc_y) else {
            return false;
        };
        if target != self.focused {
            self.focused = target;
        }
        self.dragging_cell = Some(target);
        self.cells[target].mouse_down(x, doc_y, modifiers)
    }

    pub fn mouse_drag_to(&mut self, x: f32, y: f32) -> bool {
        let doc_y = y + self.scroll_y;
        if let Some(idx) = self.dragging_cell {
            self.cells[idx].mouse_drag_to(x, doc_y)
        } else {
            false
        }
    }

    pub fn mouse_up(&mut self) -> bool {
        if let Some(idx) = self.dragging_cell.take() {
            self.cells[idx].mouse_up()
        } else {
            false
        }
    }

    /// Pick the cell that contains `(x, doc_y)` — `doc_y` must already include
    /// any scroll offset. Each cell's clickable region is its
    /// rendered rect plus half of `CELL_GAP` on each interior side (so clicks in
    /// the gap snap to whichever cell owns that half). Returns `None` for clicks
    /// above the first cell, below the last cell, or outside the cell width.
    fn find_cell_at(&self, x: f32, y: f32) -> Option<usize> {
        if self.cells.is_empty() {
            return None;
        }
        let half_gap = CELL_GAP * 0.5;
        let last = self.cells.len() - 1;
        for (i, cell) in self.cells.iter().enumerate() {
            let in_x = x >= cell.x_origin() && x < cell.x_origin() + cell.width();
            let top = if i == 0 {
                cell.y_origin()
            } else {
                cell.y_origin() - half_gap
            };
            let bot = if i == last {
                cell.y_origin() + cell.height()
            } else {
                cell.y_origin() + cell.height() + half_gap
            };
            if in_x && y >= top && y < bot {
                return Some(i);
            }
        }
        None
    }
}

/// Opacity for the scrollbar thumb. Full for `SCROLLBAR_HOLD` after the last
/// scroll, then linear fade to 0 over `SCROLLBAR_FADE`. Returns 0 if there has
/// never been a scroll event.
fn scrollbar_alpha(last: Option<Instant>) -> f32 {
    let Some(t) = last else { return 0.0 };
    let elapsed = t.elapsed();
    if elapsed <= SCROLLBAR_HOLD {
        1.0
    } else if elapsed >= SCROLLBAR_HOLD + SCROLLBAR_FADE {
        0.0
    } else {
        let into_fade = elapsed - SCROLLBAR_HOLD;
        1.0 - (into_fade.as_secs_f32() / SCROLLBAR_FADE.as_secs_f32())
    }
}
