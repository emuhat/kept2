use std::ops::Range;

use skia_safe::{Canvas, Color, Font, FontMgr, Paint, Point, Rect, Typeface};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

const FONT_BYTES: &[u8] = include_bytes!("../resources/fonts/Figtree.ttf");

const INITIAL_TEXT: &str = "Kept is a small, intentional space for the things you actually want \
to hold on to — the kind of details that drift out of inboxes and chat threads before you \
remember why they mattered. It is not a database, not a knowledge graph, not a second brain; \
it's a sturdy shelf with a few good hooks. Open it on a quiet morning, write down the name \
of someone you'd like to talk to again, the title of a book a friend mentioned, a question \
you haven't yet found the right time to ask. Close it. Come back later and find it where \
you left it, exactly as you put it down, because the only feature this app commits to is \
keeping.";

const MARGIN_X: f32 = 40.0;
const MARGIN_TOP: f32 = 60.0;
const BODY_FONT_SIZE: f32 = 18.0;
const TITLE_FONT_SIZE: f32 = 36.0;
const CARET_WIDTH: f32 = 1.5;

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(at: usize) -> Self {
        Self { anchor: at, head: at }
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.head
    }

    pub fn range(&self) -> Range<usize> {
        let lo = self.anchor.min(self.head);
        let hi = self.anchor.max(self.head);
        lo..hi
    }
}

pub struct Selections {
    pub items: Vec<Selection>,
    pub primary: usize,
}

impl Selections {
    pub fn single_caret(at: usize) -> Self {
        Self {
            items: vec![Selection::caret(at)],
            primary: 0,
        }
    }

    /// Sort by `range().start` and merge overlapping/touching selections, preserving
    /// each merged selection's direction. Re-finds the primary by tracking which range
    /// contained the previous primary's head.
    pub fn normalize(&mut self) {
        if self.items.is_empty() {
            self.primary = 0;
            return;
        }
        let primary_head = self.items[self.primary].head;

        self.items.sort_by_key(|s| s.range().start);

        let mut merged: Vec<Selection> = Vec::with_capacity(self.items.len());
        for sel in self.items.drain(..) {
            if let Some(last) = merged.last_mut() {
                if sel.range().start <= last.range().end {
                    let new_lo = last.range().start.min(sel.range().start);
                    let new_hi = last.range().end.max(sel.range().end);
                    if last.anchor <= last.head {
                        last.anchor = new_lo;
                        last.head = new_hi;
                    } else {
                        last.anchor = new_hi;
                        last.head = new_lo;
                    }
                    continue;
                }
            }
            merged.push(sel);
        }
        self.items = merged;

        self.primary = self
            .items
            .iter()
            .position(|s| {
                let r = s.range();
                r.start <= primary_head && primary_head <= r.end
            })
            .unwrap_or(0);
    }
}

pub struct Edit {
    pub range: Range<usize>,
    pub replacement: String,
}

/// Standard edit-position transform with right-gravity for pure insertions: a caret
/// sitting exactly at the insertion point advances past the inserted bytes.
fn transform_index(i: usize, start: usize, del: usize, ins: usize) -> usize {
    if i < start {
        i
    } else if i == start && del == 0 {
        i + ins
    } else if i >= start + del {
        i - del + ins
    } else {
        start + ins
    }
}

pub struct KeptApp {
    typeface: Typeface,
    text: String,
    sels: Selections,
    body_lines: Vec<Range<usize>>,
    body_lines_width: f32,
}

impl KeptApp {
    pub fn new() -> Self {
        let typeface = FontMgr::new()
            .new_from_data(FONT_BYTES, None)
            .expect("failed to load embedded TTF");
        Self {
            typeface,
            text: INITIAL_TEXT.to_string(),
            sels: Selections::single_caret(0),
            body_lines: Vec::new(),
            body_lines_width: f32::NAN,
        }
    }

    pub fn tick(&mut self, canvas: &Canvas, width: f32, _height: f32) {
        canvas.clear(Color::from_rgb(0xfa, 0xf7, 0xf2));

        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        let title_font = Font::from_typeface(&self.typeface, TITLE_FONT_SIZE);
        let body_font = Font::from_typeface(&self.typeface, BODY_FONT_SIZE);

        let max_text_width = (width - MARGIN_X * 2.0).max(80.0);
        let mut y = MARGIN_TOP;

        let (_, title_metrics) = title_font.metrics();
        y += -title_metrics.ascent;
        canvas.draw_str("Kept", Point::new(MARGIN_X, y), &title_font, &text_paint);
        y += title_metrics.descent + title_metrics.leading + 18.0;

        let (_, body_metrics) = body_font.metrics();
        let line_step = -body_metrics.ascent + body_metrics.descent + body_metrics.leading;
        let line_extra = line_step * 0.25;

        if self.body_lines_width != max_text_width {
            self.body_lines = wrap_text(&self.text, &body_font, &text_paint, max_text_width);
            self.body_lines_width = max_text_width;
        }

        // Pre-compute one baseline per visual line. Empty text still gets a single
        // baseline so the caret has somewhere to land.
        let line_count = self.body_lines.len().max(1);
        let mut baselines = Vec::with_capacity(line_count);
        for _ in 0..line_count {
            y += -body_metrics.ascent;
            baselines.push(y);
            y += body_metrics.descent + body_metrics.leading + line_extra;
        }

        // Selection highlights (drawn first so text and carets sit on top).
        let mut hl_paint = Paint::default();
        hl_paint.set_anti_alias(true);
        hl_paint.set_color(Color::from_argb(0x60, 0x4a, 0x90, 0xe2));
        for sel in &self.sels.items {
            if sel.is_collapsed() {
                continue;
            }
            let r = sel.range();
            for (li, line) in self.body_lines.iter().enumerate() {
                let s = r.start.max(line.start);
                let e = r.end.min(line.end);
                if s >= e {
                    continue;
                }
                let prefix = &self.text[line.start..s];
                let span = &self.text[s..e];
                let x0 = MARGIN_X + body_font.measure_str(prefix, Some(&text_paint)).0;
                let x1 = x0 + body_font.measure_str(span, Some(&text_paint)).0;
                let baseline = baselines[li];
                let top = baseline + body_metrics.ascent;
                let bot = baseline + body_metrics.descent;
                canvas.draw_rect(Rect::new(x0, top, x1, bot), &hl_paint);
            }
        }

        // Body text.
        for (li, line) in self.body_lines.iter().enumerate() {
            canvas.draw_str(
                &self.text[line.clone()],
                Point::new(MARGIN_X, baselines[li]),
                &body_font,
                &text_paint,
            );
        }

        // Carets.
        let mut caret_paint = Paint::default();
        caret_paint.set_anti_alias(false);
        caret_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
        for sel in &self.sels.items {
            let (li, offset) = locate_caret(&self.body_lines, sel.head);
            let baseline = baselines[li];
            let x = if self.body_lines.is_empty() {
                MARGIN_X
            } else {
                let line = &self.body_lines[li];
                let prefix_end = (line.start + offset).min(line.end);
                MARGIN_X
                    + body_font
                        .measure_str(&self.text[line.start..prefix_end], Some(&text_paint))
                        .0
            };
            let top = baseline + body_metrics.ascent;
            let bot = baseline + body_metrics.descent;
            canvas.draw_rect(Rect::new(x, top, x + CARET_WIDTH, bot), &caret_paint);
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        let shift = modifiers.state().shift_key();
        match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => {
                self.move_horizontal(-1, shift);
                true
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.move_horizontal(1, shift);
                true
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.move_vertical(-1, shift);
                true
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.move_vertical(1, shift);
                true
            }
            Key::Named(NamedKey::Home) => {
                self.move_to_line_edge(false, shift);
                true
            }
            Key::Named(NamedKey::End) => {
                self.move_to_line_edge(true, shift);
                true
            }
            Key::Named(NamedKey::Backspace) => {
                self.backspace();
                true
            }
            Key::Named(NamedKey::Delete) => {
                self.forward_delete();
                true
            }
            // Hard line breaks would need wrap support for empty paragraphs; deferred.
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) => false,
            _ => {
                if let Some(s) = &event.text {
                    if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                        self.insert_text(s);
                        return true;
                    }
                }
                false
            }
        }
    }

    fn move_horizontal(&mut self, delta: i32, shift: bool) {
        let text = &self.text;
        for sel in &mut self.sels.items {
            if !shift && !sel.is_collapsed() {
                let r = sel.range();
                let target = if delta > 0 { r.end } else { r.start };
                sel.anchor = target;
                sel.head = target;
            } else {
                sel.head = if delta > 0 {
                    next_char_boundary(text, sel.head)
                } else {
                    prev_char_boundary(text, sel.head)
                };
                if !shift {
                    sel.anchor = sel.head;
                }
            }
        }
        self.sels.normalize();
    }

    fn move_vertical(&mut self, delta: i32, shift: bool) {
        if self.body_lines.is_empty() {
            return;
        }
        let last = self.body_lines.len() as i32 - 1;
        let body_lines = &self.body_lines;
        let text = &self.text;
        for sel in &mut self.sels.items {
            let (line_idx, offset) = locate_caret(body_lines, sel.head);
            let target = (line_idx as i32 + delta).clamp(0, last) as usize;
            if target == line_idx {
                continue;
            }
            let target_line = &body_lines[target];
            let line_len = target_line.end - target_line.start;
            // Without a goal column, we map the same byte offset onto the target line and
            // walk back to the nearest char boundary. Visual column is approximate.
            let mut new_offset = offset.min(line_len);
            while new_offset > 0 && !text.is_char_boundary(target_line.start + new_offset) {
                new_offset -= 1;
            }
            sel.head = target_line.start + new_offset;
            if !shift {
                sel.anchor = sel.head;
            }
        }
        self.sels.normalize();
    }

    fn move_to_line_edge(&mut self, end: bool, shift: bool) {
        if self.body_lines.is_empty() {
            return;
        }
        let body_lines = &self.body_lines;
        for sel in &mut self.sels.items {
            let (line_idx, _) = locate_caret(body_lines, sel.head);
            let line = &body_lines[line_idx];
            sel.head = if end { line.end } else { line.start };
            if !shift {
                sel.anchor = sel.head;
            }
        }
        self.sels.normalize();
    }

    fn backspace(&mut self) {
        let mut edits: Vec<Edit> = Vec::new();
        for sel in &self.sels.items {
            if sel.is_collapsed() {
                if sel.head == 0 {
                    continue;
                }
                let prev = prev_char_boundary(&self.text, sel.head);
                edits.push(Edit {
                    range: prev..sel.head,
                    replacement: String::new(),
                });
            } else {
                edits.push(Edit {
                    range: sel.range(),
                    replacement: String::new(),
                });
            }
        }
        self.apply_edits_right_to_left(edits);
    }

    fn forward_delete(&mut self) {
        let mut edits: Vec<Edit> = Vec::new();
        for sel in &self.sels.items {
            if sel.is_collapsed() {
                if sel.head >= self.text.len() {
                    continue;
                }
                let next = next_char_boundary(&self.text, sel.head);
                edits.push(Edit {
                    range: sel.head..next,
                    replacement: String::new(),
                });
            } else {
                edits.push(Edit {
                    range: sel.range(),
                    replacement: String::new(),
                });
            }
        }
        self.apply_edits_right_to_left(edits);
    }

    fn insert_text(&mut self, s: &str) {
        let mut edits: Vec<Edit> = Vec::new();
        for sel in &self.sels.items {
            edits.push(Edit {
                range: sel.range(),
                replacement: s.to_string(),
            });
        }
        self.apply_edits_right_to_left(edits);
    }

    fn apply_edits_right_to_left(&mut self, mut edits: Vec<Edit>) {
        edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
        for edit in &edits {
            self.apply_edit(edit);
        }
    }

    fn apply_edit(&mut self, edit: &Edit) {
        let start = edit.range.start;
        let del = edit.range.end - edit.range.start;
        let ins = edit.replacement.len();
        self.text.replace_range(edit.range.clone(), &edit.replacement);
        for sel in &mut self.sels.items {
            sel.anchor = transform_index(sel.anchor, start, del, ins);
            sel.head = transform_index(sel.head, start, del, ins);
        }
        self.sels.normalize();
        self.rewrap();
    }

    /// Re-run wrap with the cached width so `body_lines` stays consistent with `text`
    /// between ticks (e.g. for keyboard-driven movement that reads `body_lines`).
    fn rewrap(&mut self) {
        if self.body_lines_width.is_nan() {
            self.body_lines.clear();
            return;
        }
        let body_font = Font::from_typeface(&self.typeface, BODY_FONT_SIZE);
        let paint = Paint::default();
        self.body_lines = wrap_text(&self.text, &body_font, &paint, self.body_lines_width);
    }
}

/// Locate which visual line contains `idx` and the byte offset within that line.
/// Returns `(0, 0)` for an empty `lines` slice; this pairs with the render code's
/// "at least one baseline" rule.
fn locate_caret(lines: &[Range<usize>], idx: usize) -> (usize, usize) {
    if lines.is_empty() {
        return (0, 0);
    }
    for (i, line) in lines.iter().enumerate() {
        if idx <= line.end {
            let local = idx.saturating_sub(line.start).min(line.end - line.start);
            return (i, local);
        }
    }
    let last = lines.len() - 1;
    let line = &lines[last];
    (last, line.end - line.start)
}

fn next_char_boundary(text: &str, idx: usize) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    let mut i = idx + 1;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn prev_char_boundary(text: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx - 1;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Wrap `text` into a contiguous partition: every byte belongs to exactly one line,
/// so trailing whitespace at a wrap boundary stays on the line above it and the
/// caret can sit inside it. The first word of each line is always accepted, even
/// if it alone exceeds `max_width`.
fn wrap_text(text: &str, font: &Font, paint: &Paint, max_width: f32) -> Vec<Range<usize>> {
    if text.is_empty() {
        return Vec::new();
    }
    let words = word_ranges(text);
    if words.is_empty() {
        return vec![0..text.len()];
    }

    let mut lines = Vec::new();
    let mut line_start: usize = 0;
    let mut have_word = false;

    for word in &words {
        if !have_word {
            have_word = true;
            continue;
        }
        let candidate = &text[line_start..word.end];
        if font.measure_str(candidate, Some(paint)).0 > max_width {
            // Commit the line up to the start of this word — the gap between the
            // previous line's last word and this word goes onto the line above.
            lines.push(line_start..word.start);
            line_start = word.start;
        }
    }
    lines.push(line_start..text.len());
    lines
}

fn word_ranges(text: &str) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                words.push(s..idx);
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(s) = start {
        words.push(s..text.len());
    }
    words
}
