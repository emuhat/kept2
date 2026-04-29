use std::ops::Range;
use std::time::{Duration, Instant};

use skia_safe::{Canvas, Color, Font, Paint, Point, Rect, Typeface};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

const BODY_FONT_SIZE: f32 = 18.0;
const CARET_WIDTH: f32 = 1.5;
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_DIST: f32 = 5.0;

/// Disambiguates a byte index that sits at a soft-wrap boundary. The same byte
/// equals both `line[i].end` and `line[i+1].start`; affinity picks which side the
/// caret is on for rendering and "current line" lookups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Affinity {
    Upstream,
    #[default]
    Downstream,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    pub affinity: Affinity,
}

impl Selection {
    pub fn caret(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
            affinity: Affinity::Downstream,
        }
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

#[derive(Clone)]
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

/// A point-in-time clone of a cell's document state. Used by undo/redo to
/// roll a cell back to a previous text + selection + zoom configuration.
/// View-only state (drag, click count, line cache, geometry) is excluded.
#[derive(Clone)]
pub struct TextBoxSnapshot {
    pub text: String,
    pub sels: Selections,
    pub font_scale: f32,
}

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

#[derive(Clone)]
enum DragKind {
    Char,
    Word(Range<usize>),
    Line(Range<usize>),
}

struct DragState {
    sel_idx: usize,
    kind: DragKind,
}

/// One editable cell: owns its text, selections, wrap cache, mouse/click state,
/// and rendering. Coordinates received from the container are absolute; the cell
/// converts to local (top-left of its content area = (0, 0)) at the entry points.
pub struct TextBox {
    typeface: Typeface,
    text: String,
    sels: Selections,
    body_lines: Vec<Range<usize>>,
    body_lines_width: f32,
    /// Cell-local y-bands per visual line: top-of-cell = 0.
    line_bands: Vec<(f32, f32)>,
    x_origin: f32,
    y_origin: f32,
    width: f32,
    height: f32,
    mouse_drag: Option<DragState>,
    last_click_time: Option<Instant>,
    /// Cell-local — preserved across cell-origin shifts so multi-click still works
    /// when previous cells grow or shrink.
    last_click_pos: (f32, f32),
    click_count: u8,
    font_scale: f32,
}

impl TextBox {
    pub fn new(typeface: Typeface, initial_text: String) -> Self {
        Self {
            typeface,
            text: initial_text,
            sels: Selections::single_caret(0),
            body_lines: Vec::new(),
            body_lines_width: f32::NAN,
            line_bands: Vec::new(),
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            mouse_drag: None,
            last_click_time: None,
            last_click_pos: (0.0, 0.0),
            click_count: 0,
            font_scale: 1.0,
        }
    }

    pub fn x_origin(&self) -> f32 {
        self.x_origin
    }

    pub fn y_origin(&self) -> f32 {
        self.y_origin
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn height(&self) -> f32 {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn font_scale(&self) -> f32 {
        self.font_scale
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the entire text and reset the selection. Caller is responsible
    /// for setting a sensible caret position via `set_caret_at` afterward.
    pub fn replace_text(&mut self, new_text: String) {
        self.text = new_text;
        self.body_lines_width = f32::NAN;
        self.sels = Selections::single_caret(0);
    }

    pub fn set_caret_at(&mut self, idx: usize) {
        let clamped = idx.min(self.text.len());
        // Walk back to a char boundary if we landed mid-codepoint.
        let mut i = clamped;
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        self.sels = Selections::single_caret(i);
    }

    /// `(anchor, head)` of the primary selection, if any.
    pub fn primary_caret(&self) -> Option<(usize, usize)> {
        let s = self.sels.items.get(self.sels.primary)?;
        Some((s.anchor, s.head))
    }

    /// Visual line index containing the primary caret (0-based). Only meaningful
    /// after at least one tick has populated `body_lines`; before that, returns 0.
    pub fn primary_visual_line(&self) -> usize {
        let Some(s) = self.sels.items.get(self.sels.primary) else {
            return 0;
        };
        let (li, _) = locate_caret(&self.body_lines, s.head, s.affinity);
        li
    }

    pub fn visual_line_count(&self) -> usize {
        self.body_lines.len().max(1)
    }

    pub fn at_top_visual_line(&self) -> bool {
        self.primary_visual_line() == 0
    }

    pub fn at_bottom_visual_line(&self) -> bool {
        self.primary_visual_line() + 1 >= self.visual_line_count()
    }

    /// Absolute (x, baseline_y) of the byte at `byte` on its visual line.
    pub fn doc_position_of_byte(&self, byte: usize) -> Option<(f32, f32)> {
        if self.body_lines.is_empty() {
            return None;
        }
        let (li, _) = locate_caret(&self.body_lines, byte, Affinity::Downstream);
        let line = self.body_lines.get(li)?;
        let prefix_end = byte.min(line.end);
        let body_font = self.body_font();
        let paint = Paint::default();
        let local_x = body_font
            .measure_str(&self.text[line.start..prefix_end], Some(&paint))
            .0;
        let (_, m) = body_font.metrics();
        let line_step = -m.ascent + m.descent + m.leading;
        let line_extra = line_step * 0.25;
        let line_advance = line_step + line_extra;
        let baseline_local = (li as f32) * line_advance + (-m.ascent);
        Some((self.x_origin + local_x, self.y_origin + baseline_local))
    }

    /// Absolute (top, bottom) y of the visual line containing `byte`.
    pub fn line_y_band_of_byte(&self, byte: usize) -> Option<(f32, f32)> {
        if self.body_lines.is_empty() {
            return None;
        }
        let (li, _) = locate_caret(&self.body_lines, byte, Affinity::Downstream);
        let body_font = self.body_font();
        let (_, m) = body_font.metrics();
        let line_step = -m.ascent + m.descent + m.leading;
        let line_extra = line_step * 0.25;
        let line_advance = line_step + line_extra;
        let top_local = (li as f32) * line_advance;
        Some((
            self.y_origin + top_local,
            self.y_origin + top_local + line_advance,
        ))
    }

    pub fn snapshot(&self) -> TextBoxSnapshot {
        TextBoxSnapshot {
            text: self.text.clone(),
            sels: self.sels.clone(),
            font_scale: self.font_scale,
        }
    }

    /// Restore document state from a snapshot. Resets transient input state
    /// (mouse drag, click count) and invalidates the wrap cache so the next
    /// tick re-wraps at the (possibly different) font scale.
    pub fn restore(&mut self, snap: TextBoxSnapshot) {
        self.text = snap.text;
        self.sels = snap.sels;
        self.font_scale = snap.font_scale;
        self.body_lines_width = f32::NAN;
        self.mouse_drag = None;
        self.click_count = 0;
        self.last_click_time = None;
    }

    /// (top, bottom) of the primary caret's visual line, in document coordinates.
    /// Uses fresh `body_metrics` (so it's correct after a font-scale change)
    /// combined with last-rendered `y_origin`. For the focused cell that's about
    /// to be re-rendered, `y_origin` is still valid because edits don't move
    /// cells above it.
    pub fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        let sel = self.sels.items.get(self.sels.primary)?;
        let font = self.body_font();
        let (_, m) = font.metrics();
        let line_step = -m.ascent + m.descent + m.leading;
        let line_extra = line_step * 0.25;
        let line_advance = line_step + line_extra;
        let (line_idx, _) = locate_caret(&self.body_lines, sel.head, sel.affinity);
        let top_local = (line_idx as f32) * line_advance;
        let bot_local = top_local + line_advance;
        Some((top_local + self.y_origin, bot_local + self.y_origin))
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        if (self.font_scale - scale).abs() > f32::EPSILON {
            self.font_scale = scale;
            // Wrap depends on font size; invalidate the cache.
            self.body_lines_width = f32::NAN;
        }
    }

    fn body_font(&self) -> Font {
        Font::from_typeface(&self.typeface, BODY_FONT_SIZE * self.font_scale)
    }

    /// Render the cell at `(x, y)` with `width`. Returns the height consumed,
    /// which the container uses to position the next cell. `focused` controls
    /// whether selection highlights and carets render.
    pub fn tick(&mut self, canvas: &Canvas, x: f32, y: f32, width: f32, focused: bool) -> f32 {
        self.x_origin = x;
        self.y_origin = y;
        self.width = width;

        let body_font = self.body_font();
        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        let max_text_width = width.max(80.0);

        if self.body_lines_width != max_text_width {
            self.body_lines = wrap_text(&self.text, &body_font, &text_paint, max_text_width);
            self.body_lines_width = max_text_width;
        }

        let (_, body_metrics) = body_font.metrics();
        let line_step = -body_metrics.ascent + body_metrics.descent + body_metrics.leading;
        let line_extra = line_step * 0.25;

        // Cell-local baselines; absolute = local + y.
        let line_count = self.body_lines.len().max(1);
        let mut baselines_local: Vec<f32> = Vec::with_capacity(line_count);
        let mut cur_local = 0.0_f32;
        for _ in 0..line_count {
            cur_local += -body_metrics.ascent;
            baselines_local.push(cur_local);
            cur_local += body_metrics.descent + body_metrics.leading + line_extra;
        }

        let line_advance = line_step + line_extra;
        self.line_bands.clear();
        for &b in &baselines_local {
            let top = b + body_metrics.ascent;
            self.line_bands.push((top, top + line_advance));
        }

        if focused {
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
                    let x0 = x + body_font.measure_str(prefix, Some(&text_paint)).0;
                    let x1 = x0 + body_font.measure_str(span, Some(&text_paint)).0;
                    let baseline = baselines_local[li] + y;
                    let top = baseline + body_metrics.ascent;
                    let bot = baseline + body_metrics.descent;
                    canvas.draw_rect(Rect::new(x0, top, x1, bot), &hl_paint);
                }
            }
        }

        for (li, line) in self.body_lines.iter().enumerate() {
            canvas.draw_str(
                &self.text[line.clone()],
                Point::new(x, baselines_local[li] + y),
                &body_font,
                &text_paint,
            );
        }

        if focused {
            let mut caret_paint = Paint::default();
            caret_paint.set_anti_alias(false);
            caret_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
            for sel in &self.sels.items {
                let (li, offset) = locate_caret(&self.body_lines, sel.head, sel.affinity);
                let baseline = baselines_local[li] + y;
                let caret_x = if self.body_lines.is_empty() {
                    x
                } else {
                    let line = &self.body_lines[li];
                    let prefix_end = (line.start + offset).min(line.end);
                    x + body_font
                        .measure_str(&self.text[line.start..prefix_end], Some(&text_paint))
                        .0
                };
                let top = baseline + body_metrics.ascent;
                let bot = baseline + body_metrics.descent;
                canvas.draw_rect(
                    Rect::new(caret_x, top, caret_x + CARET_WIDTH, bot),
                    &caret_paint,
                );
            }
        }

        self.height = cur_local;
        cur_local
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
                if modifiers.state().control_key() {
                    self.word_backspace();
                } else {
                    self.backspace();
                }
                true
            }
            Key::Named(NamedKey::Delete) => {
                if modifiers.state().control_key() {
                    self.word_forward_delete();
                } else {
                    self.forward_delete();
                }
                true
            }
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

    pub fn mouse_down(&mut self, abs_x: f32, abs_y: f32, modifiers: &Modifiers) -> bool {
        let lx = abs_x - self.x_origin;
        let ly = abs_y - self.y_origin;

        let (idx, affinity) = self.hit_test(lx, ly);
        let mods = modifiers.state();
        let now = Instant::now();

        let allow_multi = !mods.shift_key();
        let within_threshold = self
            .last_click_time
            .map(|t| {
                now.duration_since(t) <= MULTI_CLICK_INTERVAL
                    && (lx - self.last_click_pos.0).abs() <= MULTI_CLICK_DIST
                    && (ly - self.last_click_pos.1).abs() <= MULTI_CLICK_DIST
            })
            .unwrap_or(false);
        self.click_count = if allow_multi && within_threshold {
            (self.click_count + 1).min(3)
        } else {
            1
        };
        self.last_click_time = Some(now);
        self.last_click_pos = (lx, ly);

        if mods.shift_key() {
            if self.sels.primary < self.sels.items.len() {
                let s = &mut self.sels.items[self.sels.primary];
                s.head = idx;
                s.affinity = affinity;
                self.mouse_drag = Some(DragState {
                    sel_idx: self.sels.primary,
                    kind: DragKind::Char,
                });
            }
            return true;
        }

        let (anchor, head, head_aff, kind) =
            resolve_click_unit(&self.text, &self.body_lines, idx, affinity, self.click_count);

        if mods.alt_key() {
            self.sels.items.push(Selection {
                anchor,
                head,
                affinity: head_aff,
            });
            let new_idx = self.sels.items.len() - 1;
            self.sels.primary = new_idx;
            self.mouse_drag = Some(DragState {
                sel_idx: new_idx,
                kind,
            });
        } else {
            self.sels.items.clear();
            self.sels.items.push(Selection {
                anchor,
                head,
                affinity: head_aff,
            });
            self.sels.primary = 0;
            self.mouse_drag = Some(DragState { sel_idx: 0, kind });
        }
        true
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        let lx = abs_x - self.x_origin;
        let ly = abs_y - self.y_origin;

        let (sel_idx, kind) = match self.mouse_drag.as_ref() {
            Some(d) => (d.sel_idx, d.kind.clone()),
            None => return false,
        };
        if sel_idx >= self.sels.items.len() {
            return false;
        }
        let (idx, affinity) = self.hit_test(lx, ly);

        let (new_anchor, new_head, new_aff) = match kind {
            DragKind::Char => {
                let s = &self.sels.items[sel_idx];
                (s.anchor, idx, affinity)
            }
            DragKind::Word(initial) => {
                let hit = find_word_at(&self.text, idx);
                if hit.start >= initial.start {
                    (initial.start, hit.end, Affinity::Downstream)
                } else {
                    (initial.end, hit.start, Affinity::Downstream)
                }
            }
            DragKind::Line(initial) => {
                let hit = find_line_at(&self.body_lines, idx, affinity);
                if hit.start >= initial.start {
                    let aff = if hit.end == self.text.len() {
                        Affinity::Downstream
                    } else {
                        Affinity::Upstream
                    };
                    (initial.start, hit.end, aff)
                } else {
                    (initial.end, hit.start, Affinity::Downstream)
                }
            }
        };

        let s = &mut self.sels.items[sel_idx];
        if s.anchor == new_anchor && s.head == new_head && s.affinity == new_aff {
            return false;
        }
        s.anchor = new_anchor;
        s.head = new_head;
        s.affinity = new_aff;
        true
    }

    pub fn mouse_up(&mut self) -> bool {
        if self.mouse_drag.take().is_some() {
            self.sels.normalize();
            true
        } else {
            false
        }
    }

    fn hit_test(&self, lx: f32, ly: f32) -> (usize, Affinity) {
        if self.body_lines.is_empty() || self.line_bands.is_empty() {
            return (0, Affinity::Downstream);
        }
        let line_idx = self.find_line_at_y(ly);
        let line = &self.body_lines[line_idx];
        let line_text = &self.text[line.clone()];
        let local_x = lx.max(0.0);

        let body_font = self.body_font();
        let paint = Paint::default();

        let mut prev_offset = 0usize;
        let mut prev_w = 0.0f32;

        for (offset, _) in line_text.char_indices().skip(1) {
            let w = body_font
                .measure_str(&line_text[..offset], Some(&paint))
                .0;
            if w >= local_x {
                let chosen = if (local_x - prev_w) < (w - local_x) {
                    prev_offset
                } else {
                    offset
                };
                let idx = line.start + chosen;
                return (idx, hit_affinity(line_idx, idx, &self.body_lines));
            }
            prev_w = w;
            prev_offset = offset;
        }

        let total_w = body_font.measure_str(line_text, Some(&paint)).0;
        let chosen = if (local_x - prev_w) < (total_w - local_x) {
            prev_offset
        } else {
            line_text.len()
        };
        let idx = line.start + chosen;
        (idx, hit_affinity(line_idx, idx, &self.body_lines))
    }

    fn find_line_at_y(&self, ly: f32) -> usize {
        if ly < self.line_bands[0].0 {
            return 0;
        }
        for (i, &(_top, bot)) in self.line_bands.iter().enumerate() {
            if ly < bot {
                return i;
            }
        }
        self.line_bands.len() - 1
    }

    fn move_horizontal(&mut self, delta: i32, shift: bool) {
        let text = &self.text;
        let body_lines = &self.body_lines;
        for sel in &mut self.sels.items {
            if !shift && !sel.is_collapsed() {
                let r = sel.range();
                let target = if delta > 0 { r.end } else { r.start };
                let new_aff = if (delta > 0 && sel.head >= sel.anchor)
                    || (delta < 0 && sel.head <= sel.anchor)
                {
                    sel.affinity
                } else {
                    Affinity::Downstream
                };
                sel.anchor = target;
                sel.head = target;
                sel.affinity = new_aff;
            } else {
                let (new_head, new_aff) =
                    step_horizontal(text, body_lines, sel.head, sel.affinity, delta);
                sel.head = new_head;
                sel.affinity = new_aff;
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
            let (line_idx, offset) = locate_caret(body_lines, sel.head, sel.affinity);
            let target = (line_idx as i32 + delta).clamp(0, last) as usize;
            if target == line_idx {
                continue;
            }
            let target_line = &body_lines[target];
            let line_len = target_line.end - target_line.start;
            let mut new_offset = offset.min(line_len);
            while new_offset > 0 && !text.is_char_boundary(target_line.start + new_offset) {
                new_offset -= 1;
            }
            sel.head = target_line.start + new_offset;
            sel.affinity = if new_offset == 0 && target > 0 {
                Affinity::Downstream
            } else if new_offset == line_len && target + 1 < body_lines.len() {
                Affinity::Upstream
            } else {
                Affinity::Downstream
            };
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
            let (line_idx, _) = locate_caret(body_lines, sel.head, sel.affinity);
            let line = &body_lines[line_idx];
            if end {
                sel.head = line.end;
                sel.affinity = if line_idx + 1 < body_lines.len() {
                    Affinity::Upstream
                } else {
                    Affinity::Downstream
                };
            } else {
                sel.head = line.start;
                sel.affinity = Affinity::Downstream;
            }
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

    fn word_backspace(&mut self) {
        let mut edits: Vec<Edit> = Vec::new();
        for sel in &self.sels.items {
            if sel.is_collapsed() {
                let range = find_word_left_of(&self.text, sel.head);
                if range.start < range.end {
                    edits.push(Edit {
                        range,
                        replacement: String::new(),
                    });
                }
            } else {
                edits.push(Edit {
                    range: sel.range(),
                    replacement: String::new(),
                });
            }
        }
        self.apply_edits_right_to_left(edits);
    }

    fn word_forward_delete(&mut self) {
        let mut edits: Vec<Edit> = Vec::new();
        for sel in &self.sels.items {
            if sel.is_collapsed() {
                let range = find_word_right_of(&self.text, sel.head);
                if range.start < range.end {
                    edits.push(Edit {
                        range,
                        replacement: String::new(),
                    });
                }
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
            sel.affinity = Affinity::Downstream;
        }
        self.sels.normalize();
        self.rewrap();
    }

    fn rewrap(&mut self) {
        if self.body_lines_width.is_nan() {
            self.body_lines.clear();
            return;
        }
        let body_font = self.body_font();
        let paint = Paint::default();
        self.body_lines = wrap_text(&self.text, &body_font, &paint, self.body_lines_width);
    }
}

fn locate_caret(lines: &[Range<usize>], idx: usize, affinity: Affinity) -> (usize, usize) {
    if lines.is_empty() {
        return (0, 0);
    }
    for (i, line) in lines.iter().enumerate() {
        if idx <= line.end {
            if idx == line.end && affinity == Affinity::Downstream && i + 1 < lines.len() {
                return (i + 1, 0);
            }
            let local = idx.saturating_sub(line.start).min(line.end - line.start);
            return (i, local);
        }
    }
    let last = lines.len() - 1;
    let line = &lines[last];
    (last, line.end - line.start)
}

fn is_wrap_boundary(body_lines: &[Range<usize>], idx: usize) -> bool {
    if body_lines.len() < 2 {
        return false;
    }
    body_lines[..body_lines.len() - 1]
        .iter()
        .any(|line| line.end == idx)
}

fn step_horizontal(
    text: &str,
    body_lines: &[Range<usize>],
    head: usize,
    affinity: Affinity,
    dir: i32,
) -> (usize, Affinity) {
    if dir > 0 {
        if affinity == Affinity::Upstream && is_wrap_boundary(body_lines, head) {
            return (head, Affinity::Downstream);
        }
        let next = next_char_boundary(text, head);
        let new_aff = if is_wrap_boundary(body_lines, next) {
            Affinity::Upstream
        } else {
            Affinity::Downstream
        };
        (next, new_aff)
    } else {
        if affinity == Affinity::Downstream && is_wrap_boundary(body_lines, head) {
            return (head, Affinity::Upstream);
        }
        let prev = prev_char_boundary(text, head);
        (prev, Affinity::Downstream)
    }
}

fn hit_affinity(line_idx: usize, idx: usize, body_lines: &[Range<usize>]) -> Affinity {
    let line = &body_lines[line_idx];
    if idx == line.start && line_idx > 0 {
        Affinity::Downstream
    } else if idx == line.end && line_idx + 1 < body_lines.len() {
        Affinity::Upstream
    } else {
        Affinity::Downstream
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Other,
}

fn char_class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

fn find_word_at(text: &str, idx: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let class_at = |i: usize| -> Option<CharClass> {
        text.get(i..).and_then(|s| s.chars().next()).map(char_class)
    };
    let class_left = |i: usize| -> Option<CharClass> {
        if i == 0 {
            None
        } else {
            class_at(prev_char_boundary(text, i))
        }
    };

    let here = class_at(idx);
    let left = class_left(idx);

    let (probe, class) = match (left, here) {
        (Some(l), Some(h)) if l != h => {
            if l == CharClass::Word {
                (prev_char_boundary(text, idx), CharClass::Word)
            } else {
                (idx, h)
            }
        }
        (_, Some(h)) => (idx, h),
        (Some(l), None) => (prev_char_boundary(text, idx), l),
        (None, None) => return idx..idx,
    };

    let mut start = probe;
    while start > 0 {
        let p = prev_char_boundary(text, start);
        if class_at(p) == Some(class) {
            start = p;
        } else {
            break;
        }
    }
    let mut end = next_char_boundary(text, probe);
    while end < text.len() && class_at(end) == Some(class) {
        end = next_char_boundary(text, end);
    }
    start..end
}

fn class_at_byte(text: &str, i: usize) -> Option<CharClass> {
    text.get(i..).and_then(|s| s.chars().next()).map(char_class)
}

/// Range from the start of the run containing the char just before `idx` up to
/// `idx`. For Ctrl+Backspace: peek left, walk back through the same-class run.
/// Empty range if `idx == 0`.
fn find_word_left_of(text: &str, idx: usize) -> Range<usize> {
    if idx == 0 {
        return 0..0;
    }
    let prev = prev_char_boundary(text, idx);
    let target = class_at_byte(text, prev);
    let mut start = prev;
    while start > 0 {
        let p = prev_char_boundary(text, start);
        if class_at_byte(text, p) == target {
            start = p;
        } else {
            break;
        }
    }
    start..idx
}

/// Range from `idx` to the end of the run starting at `idx`. For Ctrl+Delete.
/// Empty range if `idx >= text.len()`.
fn find_word_right_of(text: &str, idx: usize) -> Range<usize> {
    if idx >= text.len() {
        return idx..idx;
    }
    let target = class_at_byte(text, idx);
    let mut end = next_char_boundary(text, idx);
    while end < text.len() && class_at_byte(text, end) == target {
        end = next_char_boundary(text, end);
    }
    idx..end
}

fn find_line_at(body_lines: &[Range<usize>], idx: usize, affinity: Affinity) -> Range<usize> {
    if body_lines.is_empty() {
        return 0..0;
    }
    let (line_idx, _) = locate_caret(body_lines, idx, affinity);
    body_lines[line_idx].clone()
}

fn resolve_click_unit(
    text: &str,
    body_lines: &[Range<usize>],
    idx: usize,
    affinity: Affinity,
    click_count: u8,
) -> (usize, usize, Affinity, DragKind) {
    match click_count {
        2 => {
            let word = find_word_at(text, idx);
            (
                word.start,
                word.end,
                Affinity::Downstream,
                DragKind::Word(word),
            )
        }
        3 => {
            let line = find_line_at(body_lines, idx, affinity);
            let head_aff = if line.end == text.len() {
                Affinity::Downstream
            } else {
                Affinity::Upstream
            };
            (line.start, line.end, head_aff, DragKind::Line(line))
        }
        _ => (idx, idx, affinity, DragKind::Char),
    }
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

// ---------------------------------------------------------------------------
// OutlineCell — a flat list of bullets with explicit depth. Display order is
// the list order; tree shape is implicit (a bullet's children are the
// contiguous run of subsequent bullets at depth > self).
// ---------------------------------------------------------------------------

const BULLET_INDENT: f32 = 22.0;
const BULLET_RADIUS: f32 = 3.0;

#[derive(Clone)]
pub struct BulletSnapshot {
    pub id: u64,
    pub textbox: TextBoxSnapshot,
    pub depth: u32,
}

#[derive(Clone)]
pub struct OutlineSnapshot {
    pub bullets: Vec<BulletSnapshot>,
    pub focused_bullet: u64,
    pub next_id: u64,
}

pub struct Bullet {
    id: u64,
    textbox: TextBox,
    depth: u32,
}

struct OutlineDrag {
    origin_id: u64,
    mode: DragMode,
}

enum DragMode {
    /// Drag started inside one bullet; delegating to that bullet's textbox.
    TextBox,
    /// Drag has crossed bullet boundaries; we own the selection. `head_id`
    /// is the bullet currently under the cursor.
    BulletRange { head_id: u64 },
}

#[derive(Clone, Copy)]
struct BulletSelection {
    anchor_id: u64,
    head_id: u64,
}

pub struct OutlineCell {
    typeface: Typeface,
    bullets: Vec<Bullet>,
    focused_bullet: u64,
    drag: Option<OutlineDrag>,
    bullet_selection: Option<BulletSelection>,
    next_id: u64,
    x_origin: f32,
    y_origin: f32,
    width: f32,
    height: f32,
    font_scale: f32,
}

impl OutlineCell {
    pub fn new(typeface: Typeface) -> Self {
        let initial = Bullet {
            id: 0,
            textbox: TextBox::new(typeface.clone(), String::new()),
            depth: 0,
        };
        Self {
            typeface,
            bullets: vec![initial],
            focused_bullet: 0,
            drag: None,
            bullet_selection: None,
            next_id: 1,
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            font_scale: 1.0,
        }
    }

    /// Resolve the bullet index containing the given absolute y. Clamps to
    /// first/last on out-of-bounds and on small inter-bullet gaps.
    fn bullet_idx_at_y(&self, abs_y: f32) -> usize {
        if let Some(idx) = self.bullets.iter().position(|b| {
            let top = b.textbox.y_origin();
            let bot = top + b.textbox.height();
            abs_y >= top && abs_y < bot
        }) {
            return idx;
        }
        if abs_y < self.y_origin {
            0
        } else {
            self.bullets.len().saturating_sub(1)
        }
    }

    fn bullet_idx_by_id(&self, id: u64) -> Option<usize> {
        self.bullets.iter().position(|b| b.id == id)
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn focused_index(&self) -> Option<usize> {
        self.bullets.iter().position(|b| b.id == self.focused_bullet)
    }

    pub fn tick(&mut self, canvas: &Canvas, x: f32, y: f32, width: f32, focused: bool) -> f32 {
        self.x_origin = x;
        self.y_origin = y;
        self.width = width;

        let scale = self.font_scale;
        let indent_per_level = BULLET_INDENT * scale;
        let radius = BULLET_RADIUS * scale;

        let body_font = Font::from_typeface(&self.typeface, BODY_FONT_SIZE * scale);
        let (_, m) = body_font.metrics();
        let line_height = -m.ascent + m.descent;

        // Active highlight range: an in-flight bullet-range drag wins, otherwise
        // a persisted bullet_selection.
        let active = match (&self.drag, &self.bullet_selection) {
            (
                Some(OutlineDrag {
                    origin_id,
                    mode: DragMode::BulletRange { head_id },
                }),
                _,
            ) => Some((*origin_id, *head_id)),
            (_, Some(sel)) => Some((sel.anchor_id, sel.head_id)),
            _ => None,
        };
        let active_indices = active.and_then(|(a, h)| {
            let ai = self.bullet_idx_by_id(a)?;
            let hi = self.bullet_idx_by_id(h)?;
            Some((ai.min(hi), ai.max(hi)))
        });

        let mut bullet_paint = Paint::default();
        bullet_paint.set_anti_alias(true);
        bullet_paint.set_color(Color::from_rgb(0x60, 0x60, 0x60));

        let mut bullet_y_bands: Vec<(f32, f32)> = Vec::with_capacity(self.bullets.len());
        let suppress_caret = active_indices.is_some();
        let mut cur_y = y;
        for bullet in &mut self.bullets {
            let depth_offset = (bullet.depth as f32) * indent_per_level;
            let marker_x = x + depth_offset + indent_per_level / 2.0;
            let marker_y = cur_y + line_height / 2.0;
            canvas.draw_circle((marker_x, marker_y), radius, &bullet_paint);

            let text_x = x + depth_offset + indent_per_level;
            let text_w = (width - depth_offset - indent_per_level).max(40.0);
            let bullet_focused = focused && !suppress_caret && bullet.id == self.focused_bullet;
            let h = bullet.textbox.tick(canvas, text_x, cur_y, text_w, bullet_focused);
            bullet_y_bands.push((cur_y, cur_y + h));
            cur_y += h;
        }

        // Bullet-range overlay (only when this cell is focused).
        if focused {
            if let Some((lo, hi)) = active_indices {
                let mut hl_paint = Paint::default();
                hl_paint.set_anti_alias(true);
                hl_paint.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));
                for i in lo..=hi {
                    if let Some(&(top, bot)) = bullet_y_bands.get(i) {
                        canvas.draw_rect(Rect::new(x, top, x + width, bot), &hl_paint);
                    }
                }
            }
        }

        self.height = cur_y - y;
        self.height
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        // Bullet-selection mode key handling.
        if event.state == ElementState::Pressed && self.bullet_selection.is_some() {
            let mods = modifiers.state();
            match &event.logical_key {
                Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete) => {
                    return self.delete_bullet_selection();
                }
                Key::Named(NamedKey::Tab) => {
                    return if mods.shift_key() {
                        self.outdent_bullet_selection()
                    } else {
                        self.indent_bullet_selection()
                    };
                }
                Key::Named(NamedKey::ArrowUp) if mods.shift_key() => {
                    return self.extend_bullet_selection_up();
                }
                Key::Named(NamedKey::ArrowDown) if mods.shift_key() => {
                    return self.extend_bullet_selection_down();
                }
                // Lone modifier presses don't dismiss the selection.
                Key::Named(NamedKey::Shift)
                | Key::Named(NamedKey::Control)
                | Key::Named(NamedKey::Alt)
                | Key::Named(NamedKey::Super)
                | Key::Named(NamedKey::Meta) => {
                    return false;
                }
                _ => {
                    self.bullet_selection = None;
                    return true;
                }
            }
        }
        if event.state == ElementState::Pressed {
            let mods = modifiers.state();
            match &event.logical_key {
                Key::Named(NamedKey::Enter) if !mods.control_key() && !mods.alt_key() => {
                    return self.split_focused();
                }
                Key::Named(NamedKey::Tab) => {
                    return if mods.shift_key() {
                        self.outdent_focused()
                    } else {
                        self.indent_focused()
                    };
                }
                Key::Named(NamedKey::Backspace)
                    if !mods.control_key() && self.focused_at_text_start() =>
                {
                    return self.merge_focused_into_prev();
                }
                Key::Named(NamedKey::ArrowUp) if !mods.shift_key() => {
                    if self.focused_at_top_visual_line() && self.spill_up() {
                        return true;
                    }
                }
                Key::Named(NamedKey::ArrowDown) if !mods.shift_key() => {
                    if self.focused_at_bottom_visual_line() && self.spill_down() {
                        return true;
                    }
                }
                Key::Named(NamedKey::ArrowUp) if mods.shift_key() => {
                    if self.focused_at_top_visual_line() && self.promote_bullet_sel_up() {
                        return true;
                    }
                }
                Key::Named(NamedKey::ArrowDown) if mods.shift_key() => {
                    if self.focused_at_bottom_visual_line() && self.promote_bullet_sel_down() {
                        return true;
                    }
                }
                _ => {}
            }
        }
        if let Some(idx) = self.focused_index() {
            self.bullets[idx].textbox.handle_key(event, modifiers)
        } else {
            false
        }
    }

    pub fn mouse_down(&mut self, abs_x: f32, abs_y: f32, modifiers: &Modifiers) -> bool {
        // Any new click clears any persisted multi-bullet selection.
        self.bullet_selection = None;

        let idx = self.bullet_idx_at_y(abs_y);
        if idx >= self.bullets.len() {
            return false;
        }
        let id = self.bullets[idx].id;
        self.focused_bullet = id;
        self.drag = Some(OutlineDrag {
            origin_id: id,
            mode: DragMode::TextBox,
        });
        self.bullets[idx].textbox.mouse_down(abs_x, abs_y, modifiers)
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        let Some(drag) = self.drag.as_ref() else {
            return false;
        };
        let origin_id = drag.origin_id;
        let in_bullet_mode = matches!(drag.mode, DragMode::BulletRange { .. });

        if self.bullets.is_empty() {
            return false;
        }
        let current_idx = self.bullet_idx_at_y(abs_y);
        let current_id = self.bullets[current_idx].id;

        if in_bullet_mode {
            if let Some(d) = self.drag.as_mut() {
                if let DragMode::BulletRange { head_id } = &mut d.mode {
                    if *head_id == current_id {
                        return false;
                    }
                    *head_id = current_id;
                    return true;
                }
            }
            return false;
        }

        // TextBox mode.
        if current_id != origin_id {
            // Promote: end the origin textbox's drag, then own the selection.
            if let Some(b) = self.bullets.iter_mut().find(|b| b.id == origin_id) {
                b.textbox.mouse_up();
            }
            if let Some(d) = self.drag.as_mut() {
                d.mode = DragMode::BulletRange {
                    head_id: current_id,
                };
            }
            return true;
        }

        // Same bullet — delegate.
        if let Some(b) = self.bullets.iter_mut().find(|b| b.id == origin_id) {
            return b.textbox.mouse_drag_to(abs_x, abs_y);
        }
        false
    }

    pub fn mouse_up(&mut self) -> bool {
        let Some(drag) = self.drag.take() else {
            return false;
        };
        match drag.mode {
            DragMode::TextBox => {
                if let Some(b) = self.bullets.iter_mut().find(|b| b.id == drag.origin_id) {
                    return b.textbox.mouse_up();
                }
                false
            }
            DragMode::BulletRange { head_id } => {
                self.bullet_selection = Some(BulletSelection {
                    anchor_id: drag.origin_id,
                    head_id,
                });
                self.focused_bullet = head_id;
                true
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bullets.iter().all(|b| b.textbox.is_empty())
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        self.font_scale = scale;
        for b in &mut self.bullets {
            b.textbox.set_font_scale(scale);
        }
    }

    pub fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        let idx = self.focused_index()?;
        self.bullets[idx].textbox.caret_doc_y_band()
    }

    /// True iff the user is at the *cell's* top edge — the focused bullet is
    /// the first bullet and its textbox caret is on its first visual line.
    pub fn at_top_edge(&self) -> bool {
        match self.bullets.first() {
            Some(b) => self.focused_bullet == b.id && b.textbox.at_top_visual_line(),
            None => true,
        }
    }

    /// True iff the user is at the *cell's* bottom edge — the focused bullet
    /// is the last bullet and its textbox caret is on its last visual line.
    pub fn at_bottom_edge(&self) -> bool {
        match self.bullets.last() {
            Some(b) => self.focused_bullet == b.id && b.textbox.at_bottom_visual_line(),
            None => true,
        }
    }

    /// Place the caret at the very start of the cell (first bullet, offset 0).
    /// Used by the container when arrow-nav arrives from the previous cell.
    pub fn place_caret_at_start(&mut self) {
        self.bullet_selection = None;
        if self.bullets.is_empty() {
            return;
        }
        self.bullets[0].textbox.set_caret_at(0);
        self.focused_bullet = self.bullets[0].id;
    }

    pub fn focused_bullet_id(&self) -> u64 {
        self.focused_bullet
    }

    pub fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        let idx = self.focused_index()?;
        let tb = &self.bullets[idx].textbox;
        tb.primary_caret().map(|(_, h)| (tb.text(), h))
    }

    /// Anchor a popup at byte `byte` in the bullet identified by `bullet_id`.
    /// Returns `(abs_x, abs_y_below_line)` so the popup can render below the
    /// line containing the byte. Returns None if the bullet is gone or the
    /// byte is out of range.
    pub fn anchor_doc_pos(&self, bullet_id: u64, byte: usize) -> Option<(f32, f32)> {
        let idx = self.bullet_idx_by_id(bullet_id)?;
        let tb = &self.bullets[idx].textbox;
        let (x, _) = tb.doc_position_of_byte(byte)?;
        let (_, bot) = tb.line_y_band_of_byte(byte)?;
        Some((x, bot))
    }

    /// Place the caret at the very end of the cell (last bullet, end of text).
    pub fn place_caret_at_end(&mut self) {
        self.bullet_selection = None;
        if self.bullets.is_empty() {
            return;
        }
        let idx = self.bullets.len() - 1;
        let end = self.bullets[idx].textbox.text().len();
        self.bullets[idx].textbox.set_caret_at(end);
        self.focused_bullet = self.bullets[idx].id;
    }

    pub fn snapshot(&self) -> OutlineSnapshot {
        OutlineSnapshot {
            bullets: self
                .bullets
                .iter()
                .map(|b| BulletSnapshot {
                    id: b.id,
                    textbox: b.textbox.snapshot(),
                    depth: b.depth,
                })
                .collect(),
            focused_bullet: self.focused_bullet,
            next_id: self.next_id,
        }
    }

    pub fn restore(&mut self, snap: OutlineSnapshot) {
        self.bullets = snap
            .bullets
            .into_iter()
            .map(|bs| {
                let mut tb = TextBox::new(self.typeface.clone(), String::new());
                tb.restore(bs.textbox);
                Bullet {
                    id: bs.id,
                    textbox: tb,
                    depth: bs.depth,
                }
            })
            .collect();
        self.focused_bullet = snap.focused_bullet;
        self.next_id = snap.next_id;
        self.drag = None;
        self.bullet_selection = None;
    }

    // ----- editing operations -----

    fn focused_at_text_start(&self) -> bool {
        self.focused_index()
            .and_then(|i| self.bullets[i].textbox.primary_caret())
            == Some((0, 0))
    }

    fn focused_at_top_visual_line(&self) -> bool {
        self.focused_index()
            .map(|i| self.bullets[i].textbox.primary_visual_line() == 0)
            .unwrap_or(false)
    }

    fn focused_at_bottom_visual_line(&self) -> bool {
        self.focused_index()
            .map(|i| {
                let tb = &self.bullets[i].textbox;
                tb.primary_visual_line() + 1 >= tb.visual_line_count()
            })
            .unwrap_or(false)
    }

    fn split_focused(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        let Some((_anchor, head)) = self.bullets[idx].textbox.primary_caret() else {
            return false;
        };
        let text = self.bullets[idx].textbox.text().to_string();
        let head = head.min(text.len());
        let prefix = text[..head].to_string();
        let suffix = text[head..].to_string();
        let depth = self.bullets[idx].depth;
        let scale = self.bullets[idx].textbox.font_scale();

        // Trim original to prefix; caret position there doesn't matter (focus moves).
        self.bullets[idx].textbox.replace_text(prefix);
        let prefix_len = self.bullets[idx].textbox.text().len();
        self.bullets[idx].textbox.set_caret_at(prefix_len);

        let new_id = self.alloc_id();
        let mut new_tb = TextBox::new(self.typeface.clone(), suffix);
        new_tb.set_font_scale(scale);
        new_tb.set_caret_at(0);
        self.bullets.insert(
            idx + 1,
            Bullet {
                id: new_id,
                textbox: new_tb,
                depth,
            },
        );
        self.focused_bullet = new_id;
        true
    }

    fn indent_focused(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        if idx == 0 {
            return false;
        }
        let my_depth = self.bullets[idx].depth;
        // Need a previous sibling at same depth (with no shallower bullet between).
        let mut has_prev_sibling = false;
        for j in (0..idx).rev() {
            if self.bullets[j].depth == my_depth {
                has_prev_sibling = true;
                break;
            }
            if self.bullets[j].depth < my_depth {
                break;
            }
        }
        if !has_prev_sibling {
            return false;
        }
        // Subtree end: first index k > idx with depth <= my_depth.
        let mut k = idx + 1;
        while k < self.bullets.len() && self.bullets[k].depth > my_depth {
            k += 1;
        }
        for b in &mut self.bullets[idx..k] {
            b.depth += 1;
        }
        true
    }

    fn outdent_focused(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        let my_depth = self.bullets[idx].depth;
        if my_depth == 0 {
            return false;
        }
        let mut k = idx + 1;
        while k < self.bullets.len() && self.bullets[k].depth > my_depth {
            k += 1;
        }
        for b in &mut self.bullets[idx..k] {
            b.depth -= 1;
        }
        true
    }

    fn delete_bullet_selection(&mut self) -> bool {
        let Some(sel) = self.bullet_selection.take() else {
            return false;
        };
        let Some(ai) = self.bullet_idx_by_id(sel.anchor_id) else {
            return false;
        };
        let Some(hi) = self.bullet_idx_by_id(sel.head_id) else {
            return false;
        };
        let lo = ai.min(hi);
        let high = ai.max(hi);
        self.bullets.drain(lo..=high);

        if self.bullets.is_empty() {
            // Maintain the OutlineCell invariant: at least one bullet always.
            let new_id = self.alloc_id();
            let mut tb = TextBox::new(self.typeface.clone(), String::new());
            tb.set_font_scale(self.font_scale);
            self.bullets.push(Bullet {
                id: new_id,
                textbox: tb,
                depth: 0,
            });
            self.focused_bullet = new_id;
            return true;
        }

        // If the deletion left an orphaned subtree at index `lo` (its parent
        // was inside the deleted range), demote it by the same delta until we
        // reach the original subtree boundary.
        if lo < self.bullets.len() {
            let new_front_depth = self.bullets[lo].depth;
            let max_allowed = if lo == 0 {
                0
            } else {
                self.bullets[lo - 1].depth + 1
            };
            if new_front_depth > max_allowed {
                let delta = new_front_depth - max_allowed;
                let mut i = lo;
                while i < self.bullets.len() && self.bullets[i].depth >= new_front_depth {
                    self.bullets[i].depth -= delta;
                    i += 1;
                }
            }
        }

        // Place the caret: prefer the bullet just before the deletion (caret at
        // end of its text); if deletion was at the start, focus the new first.
        if lo > 0 {
            let target_idx = lo - 1;
            let target_id = self.bullets[target_idx].id;
            let end = self.bullets[target_idx].textbox.text().len();
            self.bullets[target_idx].textbox.set_caret_at(end);
            self.focused_bullet = target_id;
        } else {
            self.bullets[0].textbox.set_caret_at(0);
            self.focused_bullet = self.bullets[0].id;
        }
        true
    }

    /// Indent every bullet in the selection (and any trailing descendants of
    /// the last selected bullet) by one level. Requires the first bullet of
    /// the selection to have a previous sibling at its current depth.
    fn indent_bullet_selection(&mut self) -> bool {
        let Some(sel) = self.bullet_selection else {
            return false;
        };
        let (Some(ai), Some(hi)) = (
            self.bullet_idx_by_id(sel.anchor_id),
            self.bullet_idx_by_id(sel.head_id),
        ) else {
            return false;
        };
        let lo = ai.min(hi);
        let high = ai.max(hi);
        if lo == 0 {
            return false;
        }
        // Look for a previous sibling at the same depth as bullets[lo].
        let target_depth = self.bullets[lo].depth;
        let mut has_prev_sibling = false;
        for j in (0..lo).rev() {
            if self.bullets[j].depth == target_depth {
                has_prev_sibling = true;
                break;
            }
            if self.bullets[j].depth < target_depth {
                break;
            }
        }
        if !has_prev_sibling {
            return false;
        }
        // Extend the group to cover trailing descendants of bullets[high].
        let high_depth = self.bullets[high].depth;
        let mut end = high + 1;
        while end < self.bullets.len() && self.bullets[end].depth > high_depth {
            end += 1;
        }
        for b in &mut self.bullets[lo..end] {
            b.depth += 1;
        }
        true
    }

    /// Outdent every bullet in the selection (and trailing descendants of the
    /// last selected bullet) by one level. Refuses if any bullet in the
    /// `[lo..=high]` range is already at depth 0 — outdenting would underflow.
    fn outdent_bullet_selection(&mut self) -> bool {
        let Some(sel) = self.bullet_selection else {
            return false;
        };
        let (Some(ai), Some(hi)) = (
            self.bullet_idx_by_id(sel.anchor_id),
            self.bullet_idx_by_id(sel.head_id),
        ) else {
            return false;
        };
        let lo = ai.min(hi);
        let high = ai.max(hi);
        if self.bullets[lo..=high].iter().any(|b| b.depth == 0) {
            return false;
        }
        let high_depth = self.bullets[high].depth;
        let mut end = high + 1;
        while end < self.bullets.len() && self.bullets[end].depth > high_depth {
            end += 1;
        }
        for b in &mut self.bullets[lo..end] {
            b.depth -= 1;
        }
        true
    }

    fn extend_bullet_selection_up(&mut self) -> bool {
        let head_id = self.bullet_selection.as_ref().map(|s| s.head_id);
        let Some(head_id) = head_id else { return false; };
        let Some(head_idx) = self.bullet_idx_by_id(head_id) else {
            return false;
        };
        if head_idx == 0 {
            return false;
        }
        let new_head = self.bullets[head_idx - 1].id;
        if let Some(s) = self.bullet_selection.as_mut() {
            s.head_id = new_head;
        }
        self.focused_bullet = new_head;
        true
    }

    fn extend_bullet_selection_down(&mut self) -> bool {
        let head_id = self.bullet_selection.as_ref().map(|s| s.head_id);
        let Some(head_id) = head_id else { return false; };
        let Some(head_idx) = self.bullet_idx_by_id(head_id) else {
            return false;
        };
        if head_idx + 1 >= self.bullets.len() {
            return false;
        }
        let new_head = self.bullets[head_idx + 1].id;
        if let Some(s) = self.bullet_selection.as_mut() {
            s.head_id = new_head;
        }
        self.focused_bullet = new_head;
        true
    }

    fn promote_bullet_sel_up(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        if idx == 0 {
            return false;
        }
        let anchor_id = self.bullets[idx].id;
        let head_id = self.bullets[idx - 1].id;
        self.bullet_selection = Some(BulletSelection { anchor_id, head_id });
        self.focused_bullet = head_id;
        true
    }

    fn promote_bullet_sel_down(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        if idx + 1 >= self.bullets.len() {
            return false;
        }
        let anchor_id = self.bullets[idx].id;
        let head_id = self.bullets[idx + 1].id;
        self.bullet_selection = Some(BulletSelection { anchor_id, head_id });
        self.focused_bullet = head_id;
        true
    }

    fn merge_focused_into_prev(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        if idx == 0 {
            return false;
        }
        // Refuse to merge if focused has children — would orphan them. The user
        // can outdent the children first, or delete them.
        let my_depth = self.bullets[idx].depth;
        let has_children = idx + 1 < self.bullets.len()
            && self.bullets[idx + 1].depth > my_depth;
        if has_children {
            return false;
        }
        let prev_idx = idx - 1;
        let prev_id = self.bullets[prev_idx].id;
        let prev_len = self.bullets[prev_idx].textbox.text().len();
        let combined = format!(
            "{}{}",
            self.bullets[prev_idx].textbox.text(),
            self.bullets[idx].textbox.text()
        );
        self.bullets.remove(idx);
        let prev = &mut self.bullets[prev_idx];
        prev.textbox.replace_text(combined);
        prev.textbox.set_caret_at(prev_len);
        self.focused_bullet = prev_id;
        true
    }

    fn spill_up(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        if idx == 0 {
            return false;
        }
        let prev_idx = idx - 1;
        let prev_id = self.bullets[prev_idx].id;
        let end = self.bullets[prev_idx].textbox.text().len();
        self.bullets[prev_idx].textbox.set_caret_at(end);
        self.focused_bullet = prev_id;
        true
    }

    fn spill_down(&mut self) -> bool {
        let Some(idx) = self.focused_index() else {
            return false;
        };
        if idx + 1 >= self.bullets.len() {
            return false;
        }
        let next_idx = idx + 1;
        let next_id = self.bullets[next_idx].id;
        self.bullets[next_idx].textbox.set_caret_at(0);
        self.focused_bullet = next_id;
        true
    }
}

// ---------------------------------------------------------------------------
// Cell — the public cell type. Either a plain text editor (`TextBox`) or an
// outline cell. The container dispatches on the variant.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum CellSnapshot {
    Plain(TextBoxSnapshot),
    Outline(OutlineSnapshot),
}

impl CellSnapshot {
    /// Document content equality (ignores selection state). Used by undo to
    /// detect "cursor moved but text didn't change" events that shouldn't
    /// record a new undo entry. Variant-mismatched snapshots compare unequal,
    /// which only happens if undo state and live state get out of sync — a bug.
    pub fn doc_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CellSnapshot::Plain(a), CellSnapshot::Plain(b)) => a.text == b.text,
            (CellSnapshot::Outline(a), CellSnapshot::Outline(b)) => {
                a.bullets.len() == b.bullets.len()
                    && a.bullets.iter().zip(b.bullets.iter()).all(|(x, y)| {
                        x.depth == y.depth && x.textbox.text == y.textbox.text
                    })
            }
            _ => false,
        }
    }
}

pub enum Cell {
    Plain(TextBox),
    Outline(OutlineCell),
}

impl Cell {
    pub fn new(typeface: Typeface, initial_text: String) -> Self {
        Cell::Plain(TextBox::new(typeface, initial_text))
    }

    pub fn new_outline(typeface: Typeface) -> Self {
        Cell::Outline(OutlineCell::new(typeface))
    }

    pub fn tick(&mut self, canvas: &Canvas, x: f32, y: f32, width: f32, focused: bool) -> f32 {
        match self {
            Cell::Plain(tb) => tb.tick(canvas, x, y, width, focused),
            Cell::Outline(oc) => oc.tick(canvas, x, y, width, focused),
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        match self {
            Cell::Plain(tb) => tb.handle_key(event, modifiers),
            Cell::Outline(oc) => oc.handle_key(event, modifiers),
        }
    }

    pub fn mouse_down(&mut self, abs_x: f32, abs_y: f32, modifiers: &Modifiers) -> bool {
        match self {
            Cell::Plain(tb) => tb.mouse_down(abs_x, abs_y, modifiers),
            Cell::Outline(oc) => oc.mouse_down(abs_x, abs_y, modifiers),
        }
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        match self {
            Cell::Plain(tb) => tb.mouse_drag_to(abs_x, abs_y),
            Cell::Outline(oc) => oc.mouse_drag_to(abs_x, abs_y),
        }
    }

    pub fn mouse_up(&mut self) -> bool {
        match self {
            Cell::Plain(tb) => tb.mouse_up(),
            Cell::Outline(oc) => oc.mouse_up(),
        }
    }

    pub fn x_origin(&self) -> f32 {
        match self {
            Cell::Plain(tb) => tb.x_origin(),
            Cell::Outline(oc) => oc.x_origin,
        }
    }

    pub fn y_origin(&self) -> f32 {
        match self {
            Cell::Plain(tb) => tb.y_origin(),
            Cell::Outline(oc) => oc.y_origin,
        }
    }

    pub fn width(&self) -> f32 {
        match self {
            Cell::Plain(tb) => tb.width(),
            Cell::Outline(oc) => oc.width,
        }
    }

    pub fn height(&self) -> f32 {
        match self {
            Cell::Plain(tb) => tb.height(),
            Cell::Outline(oc) => oc.height,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Cell::Plain(tb) => tb.is_empty(),
            Cell::Outline(oc) => oc.is_empty(),
        }
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        match self {
            Cell::Plain(tb) => tb.set_font_scale(scale),
            Cell::Outline(oc) => oc.set_font_scale(scale),
        }
    }

    pub fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        match self {
            Cell::Plain(tb) => tb.caret_doc_y_band(),
            Cell::Outline(oc) => oc.caret_doc_y_band(),
        }
    }

    pub fn at_top_edge(&self) -> bool {
        match self {
            Cell::Plain(tb) => tb.at_top_visual_line(),
            Cell::Outline(oc) => oc.at_top_edge(),
        }
    }

    pub fn at_bottom_edge(&self) -> bool {
        match self {
            Cell::Plain(tb) => tb.at_bottom_visual_line(),
            Cell::Outline(oc) => oc.at_bottom_edge(),
        }
    }

    pub fn place_caret_at_start(&mut self) {
        match self {
            Cell::Plain(tb) => tb.set_caret_at(0),
            Cell::Outline(oc) => oc.place_caret_at_start(),
        }
    }

    pub fn place_caret_at_end(&mut self) {
        match self {
            Cell::Plain(tb) => {
                let end = tb.text().len();
                tb.set_caret_at(end);
            }
            Cell::Outline(oc) => oc.place_caret_at_end(),
        }
    }

    /// `(text, caret_byte)` for the active text input — the cell's textbox
    /// for plain cells, or the focused bullet's textbox for outline cells.
    pub fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        match self {
            Cell::Plain(tb) => tb.primary_caret().map(|(_, h)| (tb.text(), h)),
            Cell::Outline(oc) => oc.focused_text_and_caret(),
        }
    }

    /// Outline cells: ID of the focused bullet. Plain cells: None.
    pub fn focused_bullet_id(&self) -> Option<u64> {
        match self {
            Cell::Plain(_) => None,
            Cell::Outline(oc) => Some(oc.focused_bullet_id()),
        }
    }

    /// Anchor position for an overlay tied to byte `byte` in this cell's
    /// active textbox (focused bullet for outline). Used by the @-mention popup.
    pub fn anchor_doc_pos(
        &self,
        bullet_id: Option<u64>,
        byte: usize,
    ) -> Option<(f32, f32)> {
        match (self, bullet_id) {
            (Cell::Plain(tb), None) => {
                let (x, _) = tb.doc_position_of_byte(byte)?;
                let (_, bot) = tb.line_y_band_of_byte(byte)?;
                Some((x, bot))
            }
            (Cell::Outline(oc), Some(id)) => oc.anchor_doc_pos(id, byte),
            _ => None,
        }
    }

    pub fn snapshot(&self) -> CellSnapshot {
        match self {
            Cell::Plain(tb) => CellSnapshot::Plain(tb.snapshot()),
            Cell::Outline(oc) => CellSnapshot::Outline(oc.snapshot()),
        }
    }

    /// Restore from a snapshot of the same variant. Variant mismatches are a
    /// bug (undo stack and live state disagree); fall through silently rather
    /// than panic.
    pub fn restore(&mut self, snap: CellSnapshot) {
        match (self, snap) {
            (Cell::Plain(tb), CellSnapshot::Plain(tbs)) => tb.restore(tbs),
            (Cell::Outline(oc), CellSnapshot::Outline(os)) => oc.restore(os),
            _ => {}
        }
    }
}
