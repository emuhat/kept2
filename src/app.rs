use std::time::{Duration, Instant};

use arboard::Clipboard;
use skia_safe::{
    BlurStyle, Canvas, Color, Font, FontMgr, MaskFilter, Paint, PaintStyle, Point, Rect, Typeface,
};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

use crate::cell::{Cell, CellSnapshot};

const FONT_BYTES: &[u8] = include_bytes!("../resources/fonts/Figtree.ttf");

const MARGIN_X: f32 = 40.0;
const MARGIN_TOP: f32 = 60.0;
const CELL_GAP: f32 = 20.0;
const FOCUS_PAD: f32 = 10.0;
const FOCUS_RADIUS: f32 = 10.0;
const FOCUS_STROKE: f32 = 1.0;
const FOCUS_RING_ALPHA: u8 = 0x60;
const FOCUS_SHADOW_ALPHA: u8 = 0x30;
const FOCUS_SHADOW_BLUR: f32 = 12.0;
const FOCUS_SHADOW_DY: f32 = 3.0;
const DOC_BOTTOM_PAD: f32 = 24.0;
const SCROLLBAR_INSET: f32 = 4.0;
const SCROLLBAR_WIDTH: f32 = 4.0;
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
const SCROLLBAR_HOLD: Duration = Duration::from_millis(800);
const SCROLLBAR_FADE: Duration = Duration::from_millis(700);
const ZOOM_STEP: f32 = 1.1;
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 3.0;
const COALESCE_INTERVAL: Duration = Duration::from_millis(600);

const MENTION_POPUP_WIDTH: f32 = 220.0;
const MENTION_POPUP_ROW_H: f32 = 28.0;
const MENTION_POPUP_PAD: f32 = 6.0;
const MENTION_POPUP_RADIUS: f32 = 6.0;
const MENTION_POPUP_MAX_VISIBLE: usize = 6;
const MENTION_BODY_FONT_SIZE: f32 = 16.0;

const FAKE_MENTIONS: &[&str] = &[
    "alice", "alex", "alfred", "anna", "bob", "carol", "dave", "eve",
    "frank", "grace", "heidi", "ivan", "judy", "karl", "linda",
];

/// Subsequence fuzzy match. Returns `(score, matched_byte_positions)` if every
/// query char appears in `candidate` (case-insensitive) in order; None otherwise.
/// Bonuses: start-of-string, post-separator, contiguous-with-previous-match.
/// Length penalty so shorter candidates win ties.
fn fuzzy_score(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let q_lower = query.to_lowercase();
    let c_lower = candidate.to_lowercase();
    let q = q_lower.as_bytes();
    let c = c_lower.as_bytes();

    let mut matches: Vec<usize> = Vec::with_capacity(q.len());
    let mut qi = 0;
    let mut score: i32 = 0;
    let mut prev_match: Option<usize> = None;

    for i in 0..c.len() {
        if qi >= q.len() {
            break;
        }
        if c[i] == q[qi] {
            matches.push(i);
            if i == 0 {
                score += 8;
            } else if !c[i - 1].is_ascii_alphanumeric() {
                score += 4;
            }
            if let Some(prev) = prev_match {
                if i == prev + 1 {
                    score += 5;
                }
            }
            score += 1;
            prev_match = Some(i);
            qi += 1;
        }
    }

    if qi < q.len() {
        return None;
    }
    score -= (c.len() as i32) / 4;
    Some((score, matches))
}

fn filter_mentions(query: &str) -> Vec<(&'static str, Vec<usize>)> {
    if query.is_empty() {
        return FAKE_MENTIONS.iter().map(|&n| (n, Vec::new())).collect();
    }
    let mut scored: Vec<(i32, &'static str, Vec<usize>)> = FAKE_MENTIONS
        .iter()
        .filter_map(|&name| fuzzy_score(query, name).map(|(s, m)| (s, name, m)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, n, m)| (n, m)).collect()
}

struct MentionPopup {
    /// Cell index the popup is anchored to.
    cell_idx: usize,
    /// For outline cells, the specific bullet's id. None for plain cells.
    bullet_id: Option<u64>,
    /// Byte position of the '@' in the active textbox.
    anchor_byte: usize,
    /// Currently typed query (text after the '@', no whitespace).
    query: String,
    /// Index of the highlighted item in the filtered list.
    selected: usize,
}

enum UndoOp {
    CellEdit {
        cell_idx: usize,
        pre: CellSnapshot,
        post: CellSnapshot,
    },
    InsertCell {
        at_idx: usize,
        pre_focused: usize,
        outline: bool,
    },
    DeleteCell {
        at_idx: usize,
        snapshot: CellSnapshot,
        pre_focused: usize,
    },
}

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
    font_scale: f32,
    pending_caret_scroll: bool,
    undo_stack: Vec<UndoOp>,
    redo_stack: Vec<UndoOp>,
    last_edit_time: Option<Instant>,
    coalesce_break: bool,
    mention_popup: Option<MentionPopup>,
    clipboard: Option<Clipboard>,
}

impl KeptApp {
    pub fn new() -> Self {
        let typeface = FontMgr::new()
            .new_from_data(FONT_BYTES, None)
            .expect("failed to load embedded TTF");
        let mut cells: Vec<Cell> = SEED_TEXTS
            .iter()
            .map(|s| Cell::new(typeface.clone(), (*s).to_string()))
            .collect();
        // Seed a couple of test links so the rendering is visible without
        // needing a creation UI yet. Ctrl+Click follows the URL.
        if let Some(c) = cells.get_mut(0) {
            // "clicking" in "First cell — try clicking between cells…"
            c.add_link_to_first(19..27, "https://example.com/click".to_string());
        }
        if let Some(c) = cells.get_mut(1) {
            // "intentional" in "Kept is a small, intentional space …"
            c.add_link_to_first(17..28, "https://example.com/intent".to_string());
        }
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
            font_scale: 1.0,
            pending_caret_scroll: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit_time: None,
            coalesce_break: false,
            mention_popup: None,
            clipboard: Clipboard::new().ok(),
        }
    }

    fn set_font_scale(&mut self, scale: f32) -> bool {
        let s = scale.clamp(ZOOM_MIN, ZOOM_MAX);
        if (s - self.font_scale).abs() < f32::EPSILON {
            return false;
        }
        self.font_scale = s;
        for cell in &mut self.cells {
            cell.set_font_scale(s);
        }
        self.pending_caret_scroll = true;
        true
    }

    fn zoom_in(&mut self) -> bool {
        self.set_font_scale(self.font_scale * ZOOM_STEP)
    }

    fn zoom_out(&mut self) -> bool {
        self.set_font_scale(self.font_scale / ZOOM_STEP)
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

        // Capture focused-cell geometry up front. The card backdrop (drawn
        // *before* cell content) and the focus ring (drawn after) both use
        // this so they stay in lockstep — at most one frame of lag when the
        // cell grows from typing, but they always match each other.
        let focused_geom = self
            .cells
            .get(self.focused)
            .filter(|c| c.height() > 0.0)
            .map(|c| (c.x_origin(), c.y_origin(), c.width(), c.height()));

        if let Some((cx, cy, cw, ch)) = focused_geom {
            let card_rect = Rect::new(
                cx - FOCUS_PAD,
                cy - FOCUS_PAD,
                cx + cw + FOCUS_PAD,
                cy + ch + FOCUS_PAD,
            );
            // Drop shadow: blurred dark rect, offset down a few px.
            let mut shadow_paint = Paint::default();
            shadow_paint.set_anti_alias(true);
            shadow_paint.set_color(Color::from_argb(FOCUS_SHADOW_ALPHA, 0, 0, 0));
            shadow_paint.set_mask_filter(MaskFilter::blur(
                BlurStyle::Normal,
                FOCUS_SHADOW_BLUR,
                false,
            ));
            let shadow_rect = Rect::new(
                card_rect.left,
                card_rect.top + FOCUS_SHADOW_DY,
                card_rect.right,
                card_rect.bottom + FOCUS_SHADOW_DY,
            );
            canvas.draw_round_rect(shadow_rect, FOCUS_RADIUS, FOCUS_RADIUS, &shadow_paint);
            // White card fill.
            let mut fill_paint = Paint::default();
            fill_paint.set_anti_alias(true);
            fill_paint.set_color(Color::WHITE);
            canvas.draw_round_rect(card_rect, FOCUS_RADIUS, FOCUS_RADIUS, &fill_paint);
        }

        let mut y = MARGIN_TOP;
        let cell_width = (width - MARGIN_X * 2.0).max(80.0);
        for (i, cell) in self.cells.iter_mut().enumerate() {
            let h = cell.tick(canvas, MARGIN_X, y, cell_width, i == self.focused);
            y += h + CELL_GAP;
        }

        // Subtle focus ring using the same captured geometry as the card.
        if let Some((cx, cy, cw, ch)) = focused_geom {
            let mut focus_paint = Paint::default();
            focus_paint.set_anti_alias(true);
            focus_paint.set_style(PaintStyle::Stroke);
            focus_paint.set_stroke_width(FOCUS_STROKE);
            focus_paint.set_color(Color::from_argb(FOCUS_RING_ALPHA, 0x4a, 0x90, 0xe2));
            let rect = Rect::new(
                cx - FOCUS_PAD,
                cy - FOCUS_PAD,
                cx + cw + FOCUS_PAD,
                cy + ch + FOCUS_PAD,
            );
            canvas.draw_round_rect(rect, FOCUS_RADIUS, FOCUS_RADIUS, &focus_paint);
        }

        self.render_mention_popup(canvas);

        canvas.restore();

        // Update bookkeeping for scroll math + clamp again in case content shrank.
        self.doc_height = y - CELL_GAP + DOC_BOTTOM_PAD;
        self.viewport_height = height.max(0.0);
        self.max_scroll = (self.doc_height - self.viewport_height).max(0.0);
        self.scroll_y = self.scroll_y.min(self.max_scroll);

        // After cells are laid out (y_origin/height fresh), honor any caret-into-view
        // request from this tick's events. Effect lands on the next frame.
        if std::mem::take(&mut self.pending_caret_scroll) {
            self.scroll_caret_into_view();
        }

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
        // While the @-mention popup is open, intercept navigation/commit/dismiss
        // keys; everything else falls through to the cell, after which we sync
        // the popup against the new text+caret state.
        if event.state == ElementState::Pressed && self.mention_popup.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.mention_popup = None;
                    return true;
                }
                Key::Named(NamedKey::ArrowUp) => {
                    self.mention_popup_move(-1);
                    return true;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    self.mention_popup_move(1);
                    return true;
                }
                Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) => {
                    // Selection commit is non-functional for now — just dismiss.
                    self.mention_popup = None;
                    return true;
                }
                _ => {}
            }
        }

        if event.state == ElementState::Pressed && modifiers.state().control_key() {
            match &event.logical_key {
                Key::Named(NamedKey::Enter) => {
                    let outline = modifiers.state().shift_key();
                    return self.insert_cell_after_focused(outline);
                }
                Key::Named(NamedKey::Delete) => {
                    return self.delete_focused_cell();
                }
                Key::Named(NamedKey::ArrowUp) => {
                    if self.focused > 0 {
                        self.focused -= 1;
                        self.coalesce_break = true;
                        self.scroll_to_focused();
                        return true;
                    }
                    return false;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    if self.focused + 1 < self.cells.len() {
                        self.focused += 1;
                        self.coalesce_break = true;
                        self.scroll_to_focused();
                        return true;
                    }
                    return false;
                }
                Key::Character(s) if s.as_str() == "=" || s.as_str() == "+" => {
                    return self.zoom_in();
                }
                Key::Character(s) if s.as_str() == "-" => {
                    return self.zoom_out();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("z") => {
                    return if modifiers.state().shift_key() {
                        self.redo()
                    } else {
                        self.undo()
                    };
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("c") => {
                    return self.copy_to_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("x") => {
                    return self.cut_to_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("v") => {
                    return self.paste_from_clipboard();
                }
                _ => {}
            }
        }

        // Cross-cell arrow nav: a plain ArrowUp/Down at the focused cell's
        // top/bottom edge moves focus to the adjacent cell, with the caret
        // landing at the end (Up) or start (Down) of the destination.
        if event.state == ElementState::Pressed
            && !modifiers.state().shift_key()
            && !modifiers.state().control_key()
            && !modifiers.state().alt_key()
        {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowUp) => {
                    if self.focused > 0
                        && self
                            .cells
                            .get(self.focused)
                            .map_or(false, |c| c.at_top_edge())
                    {
                        self.focused -= 1;
                        self.cells[self.focused].place_caret_at_end();
                        self.coalesce_break = true;
                        self.pending_caret_scroll = true;
                        return true;
                    }
                }
                Key::Named(NamedKey::ArrowDown) => {
                    if self.focused + 1 < self.cells.len()
                        && self
                            .cells
                            .get(self.focused)
                            .map_or(false, |c| c.at_bottom_edge())
                    {
                        self.focused += 1;
                        self.cells[self.focused].place_caret_at_start();
                        self.coalesce_break = true;
                        self.pending_caret_scroll = true;
                        return true;
                    }
                }
                _ => {}
            }
        }

        let pre = self.cells.get(self.focused).map(|c| c.snapshot());
        let popup_was_open = self.mention_popup.is_some();
        let handled = if let Some(cell) = self.cells.get_mut(self.focused) {
            cell.handle_key(event, modifiers)
        } else {
            false
        };
        if handled {
            if let Some(pre) = pre {
                let post = self.cells[self.focused].snapshot();
                if !pre.doc_eq(&post) {
                    self.record_edit(pre, post);
                } else {
                    // Cursor-only event: break coalescing so the next text edit
                    // starts a fresh undo entry.
                    self.coalesce_break = true;
                }
            }
            self.pending_caret_scroll = true;

            // Maybe open the @-mention popup (if user just typed '@'), then
            // sync against the current text+caret state.
            if !popup_was_open && event.text.as_deref() == Some("@") {
                self.try_open_mention_popup();
            }
            self.sync_mention_popup();
        }
        handled
    }

    fn try_open_mention_popup(&mut self) {
        let Some(cell) = self.cells.get(self.focused) else {
            return;
        };
        let Some((text, caret)) = cell.focused_text_and_caret() else {
            return;
        };
        if caret == 0 {
            return;
        }
        // Caret should be just past the '@'.
        if text.get(caret - 1..caret) != Some("@") {
            return;
        }
        self.mention_popup = Some(MentionPopup {
            cell_idx: self.focused,
            bullet_id: cell.focused_bullet_id(),
            anchor_byte: caret - 1,
            query: String::new(),
            selected: 0,
        });
    }

    fn sync_mention_popup(&mut self) {
        let Some(popup) = self.mention_popup.as_ref() else {
            return;
        };
        // Cell focus must still match.
        if self.focused != popup.cell_idx {
            self.mention_popup = None;
            return;
        }
        let Some(cell) = self.cells.get(self.focused) else {
            self.mention_popup = None;
            return;
        };
        // Bullet must still match (outline only).
        if cell.focused_bullet_id() != popup.bullet_id {
            self.mention_popup = None;
            return;
        }
        let Some((text, caret)) = cell.focused_text_and_caret() else {
            self.mention_popup = None;
            return;
        };
        // The '@' must still be at anchor_byte.
        if text.get(popup.anchor_byte..).map_or(true, |s| !s.starts_with('@')) {
            self.mention_popup = None;
            return;
        }
        // Caret must be at or past the '@' itself.
        if caret < popup.anchor_byte + 1 {
            self.mention_popup = None;
            return;
        }
        // Query is everything between the '@' and the caret. Whitespace breaks it.
        let Some(query) = text.get(popup.anchor_byte + 1..caret) else {
            self.mention_popup = None;
            return;
        };
        if query.chars().any(|c| c.is_whitespace()) {
            self.mention_popup = None;
            return;
        }
        if let Some(p) = self.mention_popup.as_mut() {
            p.query = query.to_string();
            let count = filter_mentions(&p.query).len().min(MENTION_POPUP_MAX_VISIBLE);
            if count == 0 {
                p.selected = 0;
            } else if p.selected >= count {
                p.selected = count - 1;
            }
        }
    }

    fn copy_to_clipboard(&mut self) -> bool {
        let text = self
            .cells
            .get(self.focused)
            .map(|c| c.copy_text())
            .unwrap_or_default();
        if text.is_empty() {
            return false;
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text);
        }
        true
    }

    fn cut_to_clipboard(&mut self) -> bool {
        let pre = self.cells.get(self.focused).map(|c| c.snapshot());
        let cut = match self.cells.get_mut(self.focused) {
            Some(c) => c.cut_text(),
            None => return false,
        };
        if cut.is_empty() {
            return false;
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(cut);
        }
        // Record the deletion in the undo stack.
        if let (Some(pre), Some(post)) = (pre, self.cells.get(self.focused).map(|c| c.snapshot())) {
            if !pre.doc_eq(&post) {
                self.record_edit(pre, post);
            }
        }
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    fn paste_from_clipboard(&mut self) -> bool {
        let Some(cb) = self.clipboard.as_mut() else {
            return false;
        };
        let text = match cb.get_text() {
            Ok(t) => t,
            Err(_) => return false,
        };
        if text.is_empty() {
            return false;
        }
        let pre = self.cells.get(self.focused).map(|c| c.snapshot());
        if let Some(c) = self.cells.get_mut(self.focused) {
            c.paste_text(&text);
        } else {
            return false;
        }
        if let (Some(pre), Some(post)) = (pre, self.cells.get(self.focused).map(|c| c.snapshot())) {
            if !pre.doc_eq(&post) {
                self.record_edit(pre, post);
            }
        }
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    fn render_mention_popup(&self, canvas: &Canvas) {
        let Some(popup) = self.mention_popup.as_ref() else {
            return;
        };
        let Some(cell) = self.cells.get(popup.cell_idx) else {
            return;
        };
        let Some((anchor_x, anchor_y_below)) =
            cell.anchor_doc_pos(popup.bullet_id, popup.anchor_byte)
        else {
            return;
        };

        let scale = self.font_scale;
        let popup_w = MENTION_POPUP_WIDTH * scale;
        let row_h = MENTION_POPUP_ROW_H * scale;
        let pad = MENTION_POPUP_PAD * scale;
        let radius = MENTION_POPUP_RADIUS * scale;

        let items = filter_mentions(&popup.query);
        let visible = items.len().min(MENTION_POPUP_MAX_VISIBLE);
        let popup_h = if visible == 0 {
            row_h + pad * 2.0
        } else {
            (visible as f32) * row_h + pad * 2.0
        };

        let popup_x = anchor_x;
        let popup_y = anchor_y_below + 4.0 * scale;

        // Drop shadow (drawn first, slightly offset).
        let mut shadow_paint = Paint::default();
        shadow_paint.set_anti_alias(true);
        shadow_paint.set_color(Color::from_argb(0x30, 0, 0, 0));
        canvas.draw_round_rect(
            Rect::new(
                popup_x + 1.0,
                popup_y + 2.0,
                popup_x + popup_w + 1.0,
                popup_y + popup_h + 2.0,
            ),
            radius,
            radius,
            &shadow_paint,
        );

        // Background.
        let mut bg_paint = Paint::default();
        bg_paint.set_anti_alias(true);
        bg_paint.set_color(Color::WHITE);
        let popup_rect = Rect::new(popup_x, popup_y, popup_x + popup_w, popup_y + popup_h);
        canvas.draw_round_rect(popup_rect, radius, radius, &bg_paint);

        // Border.
        let mut border_paint = Paint::default();
        border_paint.set_anti_alias(true);
        border_paint.set_style(PaintStyle::Stroke);
        border_paint.set_stroke_width(1.0);
        border_paint.set_color(Color::from_rgb(0xc0, 0xc0, 0xc0));
        canvas.draw_round_rect(popup_rect, radius, radius, &border_paint);

        let body_font = Font::from_typeface(&self.typeface, MENTION_BODY_FONT_SIZE * scale);
        let (_, m) = body_font.metrics();
        let row_text_height = -m.ascent + m.descent;
        let text_offset_in_row = (row_h - row_text_height) * 0.5 + (-m.ascent);

        if items.is_empty() {
            let mut hint_paint = Paint::default();
            hint_paint.set_anti_alias(true);
            hint_paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));
            let baseline = popup_y + pad + text_offset_in_row;
            let label = if popup.query.is_empty() {
                "Type to search…".to_string()
            } else {
                format!("No matches for \"{}\"", popup.query)
            };
            canvas.draw_str(
                label,
                Point::new(popup_x + 12.0 * scale, baseline),
                &body_font,
                &hint_paint,
            );
            return;
        }

        let mut dim_paint = Paint::default();
        dim_paint.set_anti_alias(true);
        dim_paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));

        let mut match_paint = Paint::default();
        match_paint.set_anti_alias(true);
        match_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        let mut hl_paint = Paint::default();
        hl_paint.set_anti_alias(true);
        hl_paint.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));

        let selected = popup.selected.min(visible - 1);
        let mut row_y = popup_y + pad;
        for (i, (item, matches)) in items.iter().take(visible).enumerate() {
            if i == selected {
                canvas.draw_round_rect(
                    Rect::new(
                        popup_x + 4.0 * scale,
                        row_y,
                        popup_x + popup_w - 4.0 * scale,
                        row_y + row_h,
                    ),
                    4.0 * scale,
                    4.0 * scale,
                    &hl_paint,
                );
            }
            let baseline = row_y + text_offset_in_row;
            let text_x = popup_x + 12.0 * scale;
            // Render '@' in dim, then alternate dim / match-paint runs.
            let at_w = body_font.measure_str("@", Some(&dim_paint)).0;
            canvas.draw_str("@", Point::new(text_x, baseline), &body_font, &dim_paint);
            draw_runs_with_matches(
                canvas,
                item,
                matches,
                Point::new(text_x + at_w, baseline),
                &body_font,
                &match_paint,
                &dim_paint,
            );
            row_y += row_h;
        }
    }

    fn mention_popup_move(&mut self, delta: i32) {
        let Some(p) = self.mention_popup.as_mut() else {
            return;
        };
        let count = filter_mentions(&p.query).len().min(MENTION_POPUP_MAX_VISIBLE);
        if count == 0 {
            return;
        }
        let cur = p.selected.min(count - 1) as i32;
        let new = ((cur + delta).rem_euclid(count as i32)) as usize;
        p.selected = new;
    }

    fn record_edit(&mut self, pre: CellSnapshot, post: CellSnapshot) {
        let now = Instant::now();
        let cell_idx = self.focused;

        let can_coalesce = !self.coalesce_break
            && self
                .last_edit_time
                .map(|t| now.duration_since(t) < COALESCE_INTERVAL)
                .unwrap_or(false)
            && matches!(
                self.undo_stack.last(),
                Some(UndoOp::CellEdit { cell_idx: prev, .. }) if *prev == cell_idx
            );

        if can_coalesce {
            if let Some(UndoOp::CellEdit { post: prev_post, .. }) = self.undo_stack.last_mut() {
                *prev_post = post;
            }
        } else {
            self.undo_stack.push(UndoOp::CellEdit {
                cell_idx,
                pre,
                post,
            });
        }

        self.last_edit_time = Some(now);
        self.redo_stack.clear();
        self.coalesce_break = false;
    }

    fn undo(&mut self) -> bool {
        let Some(op) = self.undo_stack.pop() else {
            return false;
        };
        match &op {
            UndoOp::CellEdit { cell_idx, pre, .. } => {
                self.focused = *cell_idx;
                self.cells[*cell_idx].restore(pre.clone());
            }
            UndoOp::InsertCell {
                at_idx,
                pre_focused,
                ..
            } => {
                if *at_idx < self.cells.len() {
                    self.cells.remove(*at_idx);
                }
                self.focused = (*pre_focused).min(self.cells.len().saturating_sub(1));
            }
            UndoOp::DeleteCell {
                at_idx,
                snapshot,
                pre_focused,
            } => {
                let mut cell = match snapshot {
                    CellSnapshot::Plain(_) => Cell::new(self.typeface.clone(), String::new()),
                    CellSnapshot::Outline(_) => Cell::new_outline(self.typeface.clone()),
                };
                cell.restore(snapshot.clone());
                self.cells.insert(*at_idx, cell);
                self.focused = *pre_focused;
            }
        }
        self.redo_stack.push(op);
        self.dragging_cell = None;
        self.pending_caret_scroll = true;
        self.coalesce_break = true;
        true
    }

    fn redo(&mut self) -> bool {
        let Some(op) = self.redo_stack.pop() else {
            return false;
        };
        match &op {
            UndoOp::CellEdit {
                cell_idx, post, ..
            } => {
                self.focused = *cell_idx;
                self.cells[*cell_idx].restore(post.clone());
            }
            UndoOp::InsertCell {
                at_idx, outline, ..
            } => {
                let mut cell = if *outline {
                    Cell::new_outline(self.typeface.clone())
                } else {
                    Cell::new(self.typeface.clone(), String::new())
                };
                cell.set_font_scale(self.font_scale);
                self.cells.insert(*at_idx, cell);
                self.focused = *at_idx;
            }
            UndoOp::DeleteCell { at_idx, .. } => {
                if *at_idx < self.cells.len() {
                    self.cells.remove(*at_idx);
                }
                if self.focused >= self.cells.len() {
                    self.focused = self.cells.len().saturating_sub(1);
                }
            }
        }
        self.undo_stack.push(op);
        self.dragging_cell = None;
        self.pending_caret_scroll = true;
        self.coalesce_break = true;
        true
    }

    fn delete_focused_cell(&mut self) -> bool {
        // Refuse to delete the last cell — leaves the app with nothing to focus.
        if self.cells.len() <= 1 {
            return false;
        }
        let at_idx = self.focused;
        let pre_focused = self.focused;
        let snapshot = self.cells[at_idx].snapshot();
        self.cells.remove(at_idx);
        if self.focused >= self.cells.len() {
            self.focused = self.cells.len() - 1;
        }
        self.dragging_cell = None;
        self.pending_caret_scroll = true;

        self.undo_stack.push(UndoOp::DeleteCell {
            at_idx,
            snapshot,
            pre_focused,
        });
        self.redo_stack.clear();
        self.coalesce_break = true;
        true
    }

    fn insert_cell_after_focused(&mut self, outline: bool) -> bool {
        // No-op if the focused cell is empty — Ctrl+Enter shouldn't pile up empties.
        if let Some(cell) = self.cells.get(self.focused) {
            if cell.is_empty() {
                return false;
            }
        }
        let pre_focused = self.focused;
        let mut new_cell = if outline {
            Cell::new_outline(self.typeface.clone())
        } else {
            Cell::new(self.typeface.clone(), String::new())
        };
        new_cell.set_font_scale(self.font_scale);
        let insert_at = (self.focused + 1).min(self.cells.len());
        self.cells.insert(insert_at, new_cell);
        self.focused = insert_at;
        // An in-progress drag's cell index would be invalidated by the insert;
        // safest to drop it. The user can't realistically be dragging mid-keypress.
        self.dragging_cell = None;
        // The new cell will be laid out by this tick; the end-of-tick scroll
        // hook then brings its caret into view.
        self.pending_caret_scroll = true;

        self.undo_stack.push(UndoOp::InsertCell {
            at_idx: insert_at,
            pre_focused,
            outline,
        });
        self.redo_stack.clear();
        self.coalesce_break = true;
        true
    }

    /// Bring the primary caret of the focused cell into view if it's outside
    /// the viewport. Used after edits, caret movement, and zoom changes.
    fn scroll_caret_into_view(&mut self) {
        let Some(cell) = self.cells.get(self.focused) else {
            return;
        };
        let Some((top, bot)) = cell.caret_doc_y_band() else {
            return;
        };
        let pad = 8.0_f32;
        let view_top = self.scroll_y;
        let view_bot = self.scroll_y + self.viewport_height;
        let new_scroll = if top < view_top + pad {
            (top - pad).max(0.0)
        } else if bot > view_bot - pad {
            (bot + pad - self.viewport_height).max(0.0)
        } else {
            return;
        };
        // Don't clamp to current max_scroll: a just-grown doc has a stale
        // max_scroll and the next tick will recompute it.
        self.scroll_y = new_scroll.max(0.0);
        self.last_scroll_time = Some(Instant::now());
    }

    /// Bring the focused cell into view if it's outside the current viewport.
    /// Uses last frame's cell geometry; on the first frame everything is at 0
    /// which results in scroll_y = 0, which is correct.
    fn scroll_to_focused(&mut self) {
        let Some(cell) = self.cells.get(self.focused) else {
            return;
        };
        let pad = 8.0_f32;
        let cell_top = cell.y_origin();
        let cell_bot = cell.y_origin() + cell.height();
        let view_top = self.scroll_y;
        let view_bot = self.scroll_y + self.viewport_height;

        let new_scroll = if cell_top < view_top + pad {
            (cell_top - pad).max(0.0)
        } else if cell_bot > view_bot - pad {
            (cell_bot + pad - self.viewport_height).max(0.0)
        } else {
            return;
        };
        self.scroll_y = new_scroll.clamp(0.0, self.max_scroll);
        // Briefly show the scrollbar so the jump is visible.
        self.last_scroll_time = Some(Instant::now());
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, modifiers: &Modifiers) -> bool {
        // Any click dismisses an active @-mention popup.
        self.mention_popup = None;

        let doc_y = y + self.scroll_y;
        let Some(target) = self.find_cell_at(x, doc_y) else {
            return false;
        };
        if target != self.focused {
            self.focused = target;
        }
        // Any click moves/replaces the caret — break coalescing so the next
        // text edit starts a fresh undo entry.
        self.coalesce_break = true;
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

/// Draw `name` starting at `origin`, painting bytes in `match_indices` with
/// `match_paint` and the rest with `dim_paint`. ASCII-safe (matches use byte
/// indices); the FAKE_MENTIONS list is all ASCII so this is fine.
fn draw_runs_with_matches(
    canvas: &Canvas,
    name: &str,
    match_indices: &[usize],
    origin: Point,
    font: &Font,
    match_paint: &Paint,
    dim_paint: &Paint,
) {
    let bytes = name.as_bytes();
    let mut x = origin.x;
    let mut i = 0;
    while i < bytes.len() {
        let in_match = match_indices.contains(&i);
        let mut j = i + 1;
        while j < bytes.len() && match_indices.contains(&j) == in_match {
            j += 1;
        }
        let segment = &name[i..j];
        let paint = if in_match { match_paint } else { dim_paint };
        canvas.draw_str(segment, Point::new(x, origin.y), font, paint);
        x += font.measure_str(segment, Some(paint)).0;
        i = j;
    }
}
