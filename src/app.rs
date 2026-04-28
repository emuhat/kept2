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
        }
    }

    pub fn tick(&mut self, canvas: &Canvas, width: f32, _height: f32) {
        canvas.clear(Color::from_rgb(0xfa, 0xf7, 0xf2));

        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

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

        // Focus rect for the active cell.
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
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        if let Some(cell) = self.cells.get_mut(self.focused) {
            cell.handle_key(event, modifiers)
        } else {
            false
        }
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, modifiers: &Modifiers) -> bool {
        let Some(target) = self.find_cell_at(x, y) else {
            return false;
        };
        if target != self.focused {
            self.focused = target;
        }
        self.dragging_cell = Some(target);
        self.cells[target].mouse_down(x, y, modifiers)
    }

    pub fn mouse_drag_to(&mut self, x: f32, y: f32) -> bool {
        if let Some(idx) = self.dragging_cell {
            self.cells[idx].mouse_drag_to(x, y)
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

    /// Pick the cell that contains `(x, y)`. Each cell's clickable region is its
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
