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
pub struct Cell {
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
}

impl Cell {
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

    /// Render the cell at `(x, y)` with `width`. Returns the height consumed,
    /// which the container uses to position the next cell. `focused` controls
    /// whether selection highlights and carets render.
    pub fn tick(&mut self, canvas: &Canvas, x: f32, y: f32, width: f32, focused: bool) -> f32 {
        self.x_origin = x;
        self.y_origin = y;
        self.width = width;

        let body_font = Font::from_typeface(&self.typeface, BODY_FONT_SIZE);
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
                self.backspace();
                true
            }
            Key::Named(NamedKey::Delete) => {
                self.forward_delete();
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

        let body_font = Font::from_typeface(&self.typeface, BODY_FONT_SIZE);
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
        let body_font = Font::from_typeface(&self.typeface, BODY_FONT_SIZE);
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
