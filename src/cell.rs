use std::ops::Range;
use std::time::{Duration, Instant};

use skia_safe::{Canvas, Color, Font, Paint, Point, Rect, Typeface};
use uuid::Uuid;
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

const BODY_FONT_SIZE: f32 = 18.0;
const HEADING_FONT_SCALE: f32 = 1.12;
/// Trailing `#tag` tokens on a heading line render at this fraction of body
/// font size, in muted gray, with extra space separating them from the title.
const HEADING_TAG_FONT_SCALE: f32 = 0.85;
const HEADING_TAG_GAP: f32 = 12.0;
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

#[derive(Clone, Debug)]
pub struct LinkSpan {
    pub range: Range<usize>,
    pub url: String,
}

/// A point-in-time clone of a cell's document state. Used by undo/redo to
/// roll a cell back to a previous text + selection + zoom configuration.
/// View-only state (drag, click count, line cache, geometry) is excluded.
#[derive(Clone)]
pub struct TextBoxSnapshot {
    pub text: String,
    pub sels: Selections,
    pub font_scale: f32,
    pub links: Vec<LinkSpan>,
}

/// Whether the platform "primary" modifier is held — Cmd on macOS, Ctrl
/// everywhere else. Use this for app shortcuts (Ctrl+Enter / Cmd+Enter, etc.)
/// instead of reading `control_key()` directly so Mac builds feel native.
pub(crate) fn primary_mod(state: winit::keyboard::ModifiersState) -> bool {
    #[cfg(target_os = "macos")]
    {
        state.super_key()
    }
    #[cfg(not(target_os = "macos"))]
    {
        state.control_key()
    }
}

/// Whether the "word-navigation" modifier is held — Option on macOS (Mac
/// convention is Option+Arrow for word jumps; Cmd+Arrow is line/doc edges)
/// and Ctrl elsewhere. Use this for word-nav and word-delete only; app
/// shortcuts should use `primary_mod`.
pub(crate) fn word_mod(state: winit::keyboard::ModifiersState) -> bool {
    #[cfg(target_os = "macos")]
    {
        state.alt_key()
    }
    #[cfg(not(target_os = "macos"))]
    {
        state.control_key()
    }
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

/// Like `transform_index`, but with left-gravity at the boundary: an
/// insertion exactly at `i` does not push `i` forward. Used for link end
/// positions so typing right after a link doesn't extend it.
fn transform_index_closed_end(i: usize, start: usize, del: usize, ins: usize) -> usize {
    if i < start {
        i
    } else if i == start && del == 0 {
        i
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
    /// Aligned with `body_lines`: true for lines belonging to a leading
    /// markdown heading (text starts with `"# "` — heading is the first
    /// paragraph). Used to render that paragraph in bold.
    line_is_heading: Vec<bool>,
    /// Aligned with `body_lines`: for heading lines that contain trailing
    /// `#tag` tokens, holds `(title_end_offset, first_tag_offset)` line-
    /// relative byte offsets. The title spans `[0, title_end_offset)`,
    /// `[title_end_offset, first_tag_offset)` is whitespace rendered as a
    /// gap, and the tag area runs from `first_tag_offset` to the line end.
    line_tag_layout: Vec<Option<(usize, usize)>>,
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
    links: Vec<LinkSpan>,
}

impl TextBox {
    pub fn new(typeface: Typeface, initial_text: String) -> Self {
        Self {
            typeface,
            text: initial_text,
            sels: Selections::single_caret(0),
            body_lines: Vec::new(),
            line_is_heading: Vec::new(),
            line_tag_layout: Vec::new(),
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
            links: Vec::new(),
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
    /// Drops any links that no longer fit within the new text.
    pub fn replace_text(&mut self, new_text: String) {
        let new_len = new_text.len();
        self.text = new_text;
        self.body_lines_width = f32::NAN;
        self.sels = Selections::single_caret(0);
        self.links.retain(|l| l.range.end <= new_len);
    }

    /// Split this textbox's links at byte offset `at`, returning links that
    /// belonged to the suffix half (with their ranges rebased to start from 0
    /// in the suffix). Self keeps links that lie wholly in the prefix half.
    /// Links that straddle `at` are dropped from both halves.
    pub fn split_links_at(&mut self, at: usize) -> Vec<LinkSpan> {
        let mut suffix = Vec::new();
        self.links.retain(|l| {
            if l.range.end <= at {
                true // wholly in prefix — keep
            } else if l.range.start >= at {
                // wholly in suffix — move
                suffix.push(LinkSpan {
                    range: (l.range.start - at)..(l.range.end - at),
                    url: l.url.clone(),
                });
                false
            } else {
                // straddles `at` — drop
                false
            }
        });
        suffix
    }

    /// Append `new_text` and shift any links from `extra_links` by the current
    /// text length before adding (used after merging two bullets).
    pub fn append_with_links(&mut self, new_text: &str, extra_links: Vec<LinkSpan>) {
        let offset = self.text.len();
        self.text.push_str(new_text);
        self.body_lines_width = f32::NAN;
        for l in extra_links {
            self.links.push(LinkSpan {
                range: (l.range.start + offset)..(l.range.end + offset),
                url: l.url,
            });
        }
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
        let visible_end = trim_nl_end(&self.text, line);
        let prefix_end = byte.min(visible_end);
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
            links: self.links.clone(),
        }
    }

    pub fn add_link(&mut self, range: Range<usize>, url: String) {
        if range.start < range.end && range.end <= self.text.len() {
            self.links.push(LinkSpan { range, url });
        }
    }

    /// If this textbox starts with a markdown heading (`# …`), return the
    /// title portion: text after `# ` up to (but not including) the first
    /// trailing `#tag` token, with whitespace trimmed.
    pub fn heading_title(&self) -> Option<String> {
        if !self.text.starts_with("# ") {
            return None;
        }
        let heading_end = self.text.find('\n').unwrap_or(self.text.len());
        let tags = parse_heading_tags(&self.text, heading_end);
        let bytes = self.text.as_bytes();
        let mut end = tags.first().map(|r| r.start).unwrap_or(heading_end);
        while end > 2 && (bytes[end - 1] as char).is_whitespace() {
            end -= 1;
        }
        if end <= 2 {
            return None;
        }
        Some(self.text[2..end].to_string())
    }

    /// Distinct tag names (without the leading `#`) on this textbox's
    /// heading, in source order. Bare `#` is skipped.
    pub fn heading_tag_names(&self) -> Vec<String> {
        if !self.text.starts_with("# ") {
            return Vec::new();
        }
        let heading_end = self.text.find('\n').unwrap_or(self.text.len());
        let mut out: Vec<String> = Vec::new();
        for r in parse_heading_tags(&self.text, heading_end) {
            if r.end > r.start + 1 {
                let name = self.text[r.start + 1..r.end].to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }

    pub fn link_at(&self, byte: usize) -> Option<&LinkSpan> {
        self.links.iter().find(|l| byte >= l.range.start && byte < l.range.end)
    }

    pub fn links(&self) -> &[LinkSpan] {
        &self.links
    }

    /// Text covered by the primary selection. Empty if collapsed.
    pub fn copy_primary_selection(&self) -> String {
        let Some((anchor, head)) = self.primary_caret() else {
            return String::new();
        };
        if anchor == head {
            return String::new();
        }
        let lo = anchor.min(head);
        let hi = anchor.max(head);
        self.text[lo..hi].to_string()
    }

    /// Returns the cut text and removes it from the textbox via `apply_edit`
    /// (so undo records the change).
    pub fn cut_primary_selection(&mut self) -> String {
        let Some((anchor, head)) = self.primary_caret() else {
            return String::new();
        };
        if anchor == head {
            return String::new();
        }
        let lo = anchor.min(head);
        let hi = anchor.max(head);
        let cut_text = self.text[lo..hi].to_string();
        self.apply_edit(&Edit {
            range: lo..hi,
            replacement: String::new(),
        });
        cut_text
    }

    /// Insert `s` at every cursor (replacing each cursor's selection).
    pub fn paste(&mut self, s: &str) {
        if !s.is_empty() {
            self.insert_text(s);
        }
    }

    /// Restore document state from a snapshot. Resets transient input state
    /// (mouse drag, click count) and invalidates the wrap cache so the next
    /// tick re-wraps at the (possibly different) font scale.
    pub fn restore(&mut self, snap: TextBoxSnapshot) {
        self.text = snap.text;
        self.sels = snap.sels;
        self.font_scale = snap.font_scale;
        self.links = snap.links;
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

    /// Bold + slightly larger variant for rendering markdown-style headings.
    fn heading_font(&self) -> Font {
        let mut f = Font::from_typeface(
            &self.typeface,
            BODY_FONT_SIZE * HEADING_FONT_SCALE * self.font_scale,
        );
        f.set_embolden(true);
        f
    }

    /// Smaller (non-bold) variant used for `#tag` tokens trailing a heading.
    fn tag_font(&self) -> Font {
        Font::from_typeface(
            &self.typeface,
            BODY_FONT_SIZE * HEADING_TAG_FONT_SCALE * self.font_scale,
        )
    }

    /// Recompute `line_tag_layout` from the current text + body_lines +
    /// line_is_heading state. Each entry is `(title_end_offset,
    /// first_tag_offset)` relative to the line's start byte; line is
    /// otherwise represented as `None` (no tag styling).
    fn recompute_line_tag_layout(&mut self) {
        self.line_tag_layout.clear();
        self.line_tag_layout.resize(self.body_lines.len(), None);
        // Tags only exist on heading lines. Find heading paragraph end.
        if !self.text.starts_with("# ") {
            return;
        }
        let heading_end = self.text.find('\n').unwrap_or(self.text.len());
        let tags = parse_heading_tags(&self.text, heading_end);
        if tags.is_empty() {
            return;
        }
        let first_tag_start = tags[0].start;
        // Walk back from first_tag_start over whitespace to find title_end.
        let bytes = self.text.as_bytes();
        let mut title_end = first_tag_start;
        while title_end > 0 && (bytes[title_end - 1] as char).is_whitespace() {
            title_end -= 1;
        }
        // Find the line that contains first_tag_start; record layout for it.
        for (li, line) in self.body_lines.iter().enumerate() {
            if !self.line_is_heading.get(li).copied().unwrap_or(false) {
                continue;
            }
            if first_tag_start >= line.start && first_tag_start < line.end {
                let line_title_end = title_end.saturating_sub(line.start);
                let line_first_tag = first_tag_start - line.start;
                self.line_tag_layout[li] = Some((line_title_end, line_first_tag));
                break;
            }
        }
    }

    /// Render the cell at `(x, y)` with `width`. Returns the height consumed,
    /// which the container uses to position the next cell. `focused` controls
    /// whether selection highlights render; `show_caret` separately gates the
    /// blinking caret (so view mode shows selection but no caret).
    pub fn tick(
        &mut self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32 {
        self.x_origin = x;
        self.y_origin = y;
        self.width = width;

        let body_font = self.body_font();
        let heading_font = self.heading_font();
        let tag_font = self.tag_font();
        let tag_gap = HEADING_TAG_GAP * self.font_scale;
        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
        let mut tag_paint = Paint::default();
        tag_paint.set_anti_alias(true);
        tag_paint.set_color(Color::from_rgb(0x90, 0x90, 0x90));

        let max_text_width = width.max(80.0);

        if self.body_lines_width != max_text_width {
            let (lines, headings) = wrap_text_styled(
                &self.text,
                &body_font,
                &heading_font,
                &text_paint,
                max_text_width,
            );
            self.body_lines = lines;
            self.line_is_heading = headings;
            self.body_lines_width = max_text_width;
            self.recompute_line_tag_layout();
        }

        let (_, body_metrics) = body_font.metrics();
        let (_, heading_metrics) = heading_font.metrics();
        let line_metrics_for = |li: usize| {
            if self.line_is_heading.get(li).copied().unwrap_or(false) {
                heading_metrics
            } else {
                body_metrics
            }
        };

        // Cell-local baselines; absolute = local + y. Each line steps by its
        // own font's metrics so heading lines (taller font) get more space.
        let line_count = self.body_lines.len().max(1);
        let mut baselines_local: Vec<f32> = Vec::with_capacity(line_count);
        let mut line_advances: Vec<f32> = Vec::with_capacity(line_count);
        let mut cur_local = 0.0_f32;
        for li in 0..line_count {
            let m = line_metrics_for(li);
            let step = -m.ascent + m.descent + m.leading;
            let extra = step * 0.25;
            cur_local += -m.ascent;
            baselines_local.push(cur_local);
            line_advances.push(step + extra);
            cur_local += m.descent + m.leading + extra;
        }

        self.line_bands.clear();
        for (li, &b) in baselines_local.iter().enumerate() {
            let m = line_metrics_for(li);
            let top = b + m.ascent;
            self.line_bands.push((top, top + line_advances[li]));
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
                    // Don't extend the highlight past the visible end (the '\n'
                    // is part of the line range but isn't rendered).
                    let visible_end = trim_nl_end(&self.text, line);
                    let s = r.start.max(line.start).min(visible_end);
                    let e = r.end.min(visible_end);
                    if s >= e {
                        continue;
                    }
                    let line_text = &self.text[line.start..line.end];
                    let main_font = if self.line_is_heading.get(li).copied().unwrap_or(false) {
                        &heading_font
                    } else {
                        &body_font
                    };
                    let layout = self.line_tag_layout.get(li).copied().flatten();
                    let x0 = x + line_x_at_offset(
                        line_text,
                        s - line.start,
                        layout,
                        main_font,
                        &tag_font,
                        &text_paint,
                        tag_gap,
                    );
                    let x1 = x + line_x_at_offset(
                        line_text,
                        e - line.start,
                        layout,
                        main_font,
                        &tag_font,
                        &text_paint,
                        tag_gap,
                    );
                    let baseline = baselines_local[li] + y;
                    let m = line_metrics_for(li);
                    let top = baseline + m.ascent;
                    let bot = baseline + m.descent;
                    canvas.draw_rect(Rect::new(x0, top, x1, bot), &hl_paint);
                }
            }
        }

        // Body text. If any links exist, walk each line as runs alternating
        // between plain and linked styling.
        let mut link_paint = Paint::default();
        link_paint.set_anti_alias(true);
        link_paint.set_color(Color::from_rgb(0x1a, 0x66, 0xc4));
        let mut underline_paint = Paint::default();
        underline_paint.set_anti_alias(true);
        underline_paint.set_color(Color::from_rgb(0x1a, 0x66, 0xc4));
        underline_paint.set_stroke_width(1.0);
        for (li, line) in self.body_lines.iter().enumerate() {
            let baseline = baselines_local[li] + y;
            // Trailing '\n' is part of the line range but not drawn.
            let visible_end = trim_nl_end(&self.text, line);
            let visible_line = line.start..visible_end;
            let line_font = if self.line_is_heading.get(li).copied().unwrap_or(false) {
                &heading_font
            } else {
                &body_font
            };
            let layout = self.line_tag_layout.get(li).copied().flatten();
            if let Some((title_end, first_tag)) = layout {
                let line_text = &self.text[line.start..line.end];
                let visible_offset = visible_end - line.start;
                let title_end = title_end.min(visible_offset);
                let first_tag = first_tag.min(visible_offset);
                if title_end > 0 {
                    canvas.draw_str(
                        &line_text[..title_end],
                        Point::new(x, baseline),
                        line_font,
                        &text_paint,
                    );
                }
                if first_tag < visible_offset {
                    let title_w = line_font
                        .measure_str(&line_text[..title_end], Some(&text_paint))
                        .0;
                    canvas.draw_str(
                        &line_text[first_tag..visible_offset],
                        Point::new(x + title_w + tag_gap, baseline),
                        &tag_font,
                        &tag_paint,
                    );
                }
            } else if self.links.is_empty() {
                canvas.draw_str(
                    &self.text[visible_line.clone()],
                    Point::new(x, baseline),
                    line_font,
                    &text_paint,
                );
            } else {
                draw_line_with_links(
                    canvas,
                    &self.text,
                    &visible_line,
                    &self.links,
                    x,
                    baseline,
                    line_font,
                    &text_paint,
                    &link_paint,
                    &underline_paint,
                );
            }
        }

        if show_caret {
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
                    let visible_end = trim_nl_end(&self.text, line);
                    let prefix_end = (line.start + offset).min(visible_end);
                    let line_text = &self.text[line.start..line.end];
                    let main_font = if self.line_is_heading.get(li).copied().unwrap_or(false) {
                        &heading_font
                    } else {
                        &body_font
                    };
                    let layout = self.line_tag_layout.get(li).copied().flatten();
                    x + line_x_at_offset(
                        line_text,
                        prefix_end - line.start,
                        layout,
                        main_font,
                        &tag_font,
                        &text_paint,
                        tag_gap,
                    )
                };
                let m = line_metrics_for(li);
                let top = baseline + m.ascent;
                let bot = baseline + m.descent;
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
        let mods = modifiers.state();
        let shift = mods.shift_key();
        // Word nav (Ctrl on Linux/Win, Option on Mac). `word` here gates the
        // word-versus-char branches in the arrow / Home / End handlers below.
        let word = word_mod(mods);
        match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => {
                if word {
                    self.move_horizontal_word(-1, shift);
                } else {
                    self.move_horizontal(-1, shift);
                }
                true
            }
            Key::Named(NamedKey::ArrowRight) => {
                if word {
                    self.move_horizontal_word(1, shift);
                } else {
                    self.move_horizontal(1, shift);
                }
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
                if word_mod(modifiers.state()) {
                    self.word_backspace();
                } else {
                    self.backspace();
                }
                true
            }
            Key::Named(NamedKey::Delete) => {
                if word_mod(modifiers.state()) {
                    self.word_forward_delete();
                } else {
                    self.forward_delete();
                }
                true
            }
            Key::Named(NamedKey::Enter) => {
                self.insert_text("\n");
                true
            }
            Key::Named(NamedKey::Tab) => false,
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

    pub fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
        let lx = abs_x - self.x_origin;
        let ly = abs_y - self.y_origin;

        let (idx, affinity) = self.hit_test(lx, ly);
        let mods = modifiers.state();

        // Plain click on a link opens it in view mode; primary-modifier+click
        // opens it while editing (so plain clicks in edit mode still move the
        // caret). The primary modifier is Cmd on Mac, Ctrl elsewhere.
        let plain_in_view = !editing && !mods.shift_key() && !mods.alt_key();
        let modified_in_edit =
            editing && primary_mod(mods) && !mods.shift_key() && !mods.alt_key();
        if plain_in_view || modified_in_edit {
            if let Some(link) = self.link_at(idx) {
                open_url(&link.url);
                return true;
            }
        }

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
        let visible_end = trim_nl_end(&self.text, line);
        let line_text = &self.text[line.start..visible_end];
        let local_x = lx.max(0.0);

        let main_font = if self.line_is_heading.get(line_idx).copied().unwrap_or(false) {
            self.heading_font()
        } else {
            self.body_font()
        };
        let tag_font = self.tag_font();
        let tag_gap = HEADING_TAG_GAP * self.font_scale;
        let layout = self.line_tag_layout.get(line_idx).copied().flatten();
        let paint = Paint::default();

        let mut prev_offset = 0usize;
        let mut prev_w = 0.0f32;

        for (offset, _) in line_text.char_indices().skip(1) {
            let w = line_x_at_offset(
                line_text,
                offset,
                layout,
                &main_font,
                &tag_font,
                &paint,
                tag_gap,
            );
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

        let total_w = line_x_at_offset(
            line_text,
            line_text.len(),
            layout,
            &main_font,
            &tag_font,
            &paint,
            tag_gap,
        );
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

    /// Word-wise horizontal movement (Ctrl+Arrow). Moves each cursor's head to
    /// the next class boundary in the direction of motion: Right lands at the
    /// end of the run starting at the caret; Left lands at the start of the
    /// run ending just before the caret. Without `shift`, anchor follows head
    /// (collapses any existing selection).
    fn move_horizontal_word(&mut self, delta: i32, shift: bool) {
        let text = &self.text;
        for sel in &mut self.sels.items {
            let new_head = if delta > 0 {
                find_word_right_of(text, sel.head).end
            } else {
                find_word_left_of(text, sel.head).start
            };
            if new_head != sel.head {
                sel.head = new_head;
                sel.affinity = Affinity::Downstream;
            }
            if !shift {
                sel.anchor = sel.head;
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

    #[cfg(test)]
    pub fn backspace_for_test(&mut self) {
        self.backspace();
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

    /// Replace `range` with `text` and add a link covering the inserted text.
    /// Used to insert mentions like a person's title with a `kept://…` URL.
    /// Caret lands at the end of the inserted text.
    pub fn replace_with_link(&mut self, range: Range<usize>, text: String, url: String) {
        let start = range.start;
        let inserted_len = text.len();
        self.apply_edit(&Edit {
            range,
            replacement: text,
        });
        let end = start + inserted_len;
        if inserted_len > 0 && end <= self.text.len() {
            self.links.push(LinkSpan {
                range: start..end,
                url,
            });
        }
        self.set_caret_at(end);
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
        // Update link spans. `start` uses right-gravity (insertion at the
        // link's leading edge keeps the new chars OUTSIDE the link); `end`
        // uses left-gravity (insertion at the link's trailing edge keeps the
        // new chars OUTSIDE too). Net effect: a link's bounds are "closed" —
        // typing right before or right after it produces plain text. Edits
        // inside the link still grow/shrink it; edits that fully cover it
        // leave a degenerate range and the `start < end` check drops it.
        self.links.retain_mut(|link| {
            link.range.start = transform_index(link.range.start, start, del, ins);
            link.range.end = transform_index_closed_end(link.range.end, start, del, ins);
            link.range.start < link.range.end
        });
        self.rewrap();
    }

    fn rewrap(&mut self) {
        if self.body_lines_width.is_nan() {
            self.body_lines.clear();
            self.line_is_heading.clear();
            self.line_tag_layout.clear();
            return;
        }
        let body_font = self.body_font();
        let heading_font = self.heading_font();
        let paint = Paint::default();
        let (lines, headings) = wrap_text_styled(
            &self.text,
            &body_font,
            &heading_font,
            &paint,
            self.body_lines_width,
        );
        self.body_lines = lines;
        self.line_is_heading = headings;
        self.recompute_line_tag_layout();
    }
}

/// Draw the visible portion of `line` from `text` at `(text_x, baseline)`,
/// switching to `link_paint` (and drawing an underline below the baseline)
/// for byte ranges that fall inside any of `links`.
fn draw_line_with_links(
    canvas: &Canvas,
    text: &str,
    line: &Range<usize>,
    links: &[LinkSpan],
    text_x: f32,
    baseline: f32,
    font: &Font,
    text_paint: &Paint,
    link_paint: &Paint,
    underline_paint: &Paint,
) {
    let mut pos = line.start;
    let mut x = text_x;
    while pos < line.end {
        let in_link = links.iter().find(|l| pos >= l.range.start && pos < l.range.end);
        let run_end = match in_link {
            Some(l) => l.range.end.min(line.end),
            None => links
                .iter()
                .filter(|l| l.range.start > pos && l.range.start < line.end)
                .map(|l| l.range.start)
                .min()
                .unwrap_or(line.end),
        };
        let segment = &text[pos..run_end];
        let paint = if in_link.is_some() { link_paint } else { text_paint };
        canvas.draw_str(segment, Point::new(x, baseline), font, paint);
        let w = font.measure_str(segment, Some(paint)).0;
        if in_link.is_some() {
            canvas.draw_line(
                (x, baseline + 2.0),
                (x + w, baseline + 2.0),
                underline_paint,
            );
        }
        x += w;
        pos = run_end;
    }
}

/// End byte of the visible portion of a line: drops a single trailing `'\n'`
/// (which lives at the end of the line range but isn't drawn).
/// X-pixel offset within a line up to byte `target_offset`, accounting for
/// heading-tag layout: title bytes use `main_font`, the gap between title and
/// first tag is `tag_gap`, and tag-area bytes use `tag_font`.
fn line_x_at_offset(
    line_text: &str,
    target_offset: usize,
    tag_layout: Option<(usize, usize)>,
    main_font: &Font,
    tag_font: &Font,
    paint: &Paint,
    tag_gap: f32,
) -> f32 {
    let target = target_offset.min(line_text.len());
    let (title_end, first_tag) = match tag_layout {
        Some((te, ft)) => (te, ft),
        None => return main_font.measure_str(&line_text[..target], Some(paint)).0,
    };
    if target <= title_end {
        main_font.measure_str(&line_text[..target], Some(paint)).0
    } else if target <= first_tag {
        main_font.measure_str(&line_text[..title_end], Some(paint)).0 + tag_gap
    } else {
        main_font.measure_str(&line_text[..title_end], Some(paint)).0
            + tag_gap
            + tag_font
                .measure_str(&line_text[first_tag..target], Some(paint))
                .0
    }
}

/// Parse trailing `#tag` tokens from a heading paragraph. `text` is the
/// full source text; `heading_end` is the byte offset where the heading
/// paragraph ends (exclusive). Returns absolute-in-`text` byte ranges for
/// each tag, in source order. Tags are whitespace-separated tokens starting
/// with `#`. A bare `#` (length 1) counts as a tag-in-progress so the user
/// gets stable rendering as they type out the next tag's name. Tokens at
/// positions 0..2 are the leading `# ` heading marker and are never tags.
fn parse_heading_tags(text: &str, heading_end: usize) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut tags: Vec<Range<usize>> = Vec::new();
    let mut end = heading_end;
    loop {
        // Skip trailing whitespace.
        while end > 0 && (bytes[end - 1] as char).is_whitespace() {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        // Find start of last word.
        let mut start = end;
        while start > 0 && !(bytes[start - 1] as char).is_whitespace() {
            start -= 1;
        }
        // Past the leading `# ` marker, any `#`-prefixed word is a tag —
        // even a bare `#` (the user is mid-typing a new tag).
        if start >= 2 && start < end && bytes[start] == b'#' {
            tags.push(start..end);
            end = start;
        } else {
            break;
        }
    }
    tags.reverse();
    tags
}

fn trim_nl_end(text: &str, line: &Range<usize>) -> usize {
    if line.end > line.start && text.as_bytes().get(line.end - 1) == Some(&b'\n') {
        line.end - 1
    } else {
        line.end
    }
}

pub(crate) fn open_url(url: &str) {
    eprintln!("Opening link: {url}");
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
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

/// Wrap into a contiguous partition of the text, treating `'\n'` as a hard
/// paragraph break. Each visual line's range includes its trailing `'\n'`
/// (if any). Empty paragraphs produce empty visual lines.
/// Wrap `text` paragraph-by-paragraph, using `heading_font` for the first
/// paragraph if it starts with `"# "`, and `body_font` otherwise. Returns
/// `(lines, is_heading)` aligned by index.
fn wrap_text_styled(
    text: &str,
    body_font: &Font,
    heading_font: &Font,
    paint: &Paint,
    max_width: f32,
) -> (Vec<Range<usize>>, Vec<bool>) {
    if text.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let has_heading = text.starts_with("# ");
    let mut lines: Vec<Range<usize>> = Vec::new();
    let mut is_heading: Vec<bool> = Vec::new();
    let mut start = 0usize;
    let mut first_paragraph = true;
    loop {
        let nl = text[start..].find('\n').map(|p| start + p);
        let para_end = nl.unwrap_or(text.len());
        let use_heading = first_paragraph && has_heading;
        let font = if use_heading { heading_font } else { body_font };
        let prev = lines.len();
        wrap_paragraph_into(text, start, para_end, font, paint, max_width, &mut lines);
        let consumed_to = nl.map(|i| i + 1).unwrap_or(text.len());
        if let Some(last) = lines.last_mut() {
            last.end = consumed_to;
        }
        for _ in prev..lines.len() {
            is_heading.push(use_heading);
        }
        first_paragraph = false;
        match nl {
            Some(i) => start = i + 1,
            None => break,
        }
    }
    (lines, is_heading)
}

/// Word-wrap a single paragraph (no '\n' inside) and append its lines to `out`.
/// Empty paragraphs emit one empty range so each paragraph contributes ≥ 1 line.
fn wrap_paragraph_into(
    text: &str,
    start: usize,
    end: usize,
    font: &Font,
    paint: &Paint,
    max_width: f32,
    out: &mut Vec<Range<usize>>,
) {
    if start >= end {
        out.push(start..end);
        return;
    }
    // Collect word ranges within [start, end) (whitespace-delimited).
    let mut words: Vec<Range<usize>> = Vec::new();
    let mut word_start: Option<usize> = None;
    for (i, c) in text[start..end].char_indices() {
        let abs = start + i;
        if c.is_whitespace() {
            if let Some(s) = word_start.take() {
                words.push(s..abs);
            }
        } else if word_start.is_none() {
            word_start = Some(abs);
        }
    }
    if let Some(s) = word_start {
        words.push(s..end);
    }
    if words.is_empty() {
        out.push(start..end);
        return;
    }
    let mut line_start = start;
    let mut have_word = false;
    for word in &words {
        if !have_word {
            have_word = true;
            continue;
        }
        let candidate = &text[line_start..word.end];
        if font.measure_str(candidate, Some(paint)).0 > max_width {
            out.push(line_start..word.start);
            line_start = word.start;
        }
    }
    out.push(line_start..end);
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
    pub id: Uuid,
    pub textbox: TextBoxSnapshot,
    pub depth: u32,
}

#[derive(Clone)]
pub struct OutlineSnapshot {
    pub bullets: Vec<BulletSnapshot>,
    pub focused_bullet: Uuid,
}

pub struct Bullet {
    id: Uuid,
    textbox: TextBox,
    depth: u32,
}

impl Bullet {
    pub fn new(id: Uuid, textbox: TextBox, depth: u32) -> Self {
        Self { id, textbox, depth }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn textbox(&self) -> &TextBox {
        &self.textbox
    }
}

struct OutlineDrag {
    origin_id: Uuid,
    mode: DragMode,
}

enum DragMode {
    /// Drag started inside one bullet; delegating to that bullet's textbox.
    TextBox,
    /// Drag has crossed bullet boundaries; we own the selection. `head_id`
    /// is the bullet currently under the cursor.
    BulletRange { head_id: Uuid },
}

#[derive(Clone, Copy)]
struct BulletSelection {
    anchor_id: Uuid,
    head_id: Uuid,
}

pub struct OutlineCell {
    typeface: Typeface,
    bullets: Vec<Bullet>,
    focused_bullet: Uuid,
    drag: Option<OutlineDrag>,
    bullet_selection: Option<BulletSelection>,
    x_origin: f32,
    y_origin: f32,
    width: f32,
    height: f32,
    font_scale: f32,
}

impl OutlineCell {
    pub fn new(typeface: Typeface) -> Self {
        let id = Uuid::now_v7();
        let initial = Bullet {
            id,
            textbox: TextBox::new(typeface.clone(), String::new()),
            depth: 0,
        };
        Self {
            typeface,
            bullets: vec![initial],
            focused_bullet: id,
            drag: None,
            bullet_selection: None,
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            font_scale: 1.0,
        }
    }

    /// Reconstruct an `OutlineCell` from persisted bullets. Bullets must be
    /// non-empty (the OutlineCell invariant). Used by the persistence layer.
    pub fn from_bullets(typeface: Typeface, bullets: Vec<Bullet>) -> Self {
        let focused_bullet = bullets
            .first()
            .map(|b| b.id)
            .unwrap_or_else(Uuid::now_v7);
        let bullets = if bullets.is_empty() {
            let id = Uuid::now_v7();
            vec![Bullet {
                id,
                textbox: TextBox::new(typeface.clone(), String::new()),
                depth: 0,
            }]
        } else {
            bullets
        };
        Self {
            typeface,
            bullets,
            focused_bullet,
            drag: None,
            bullet_selection: None,
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            font_scale: 1.0,
        }
    }

    pub fn bullets(&self) -> &[Bullet] {
        &self.bullets
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

    fn bullet_idx_by_id(&self, id: Uuid) -> Option<usize> {
        self.bullets.iter().position(|b| b.id == id)
    }

    fn focused_index(&self) -> Option<usize> {
        self.bullets.iter().position(|b| b.id == self.focused_bullet)
    }

    pub fn tick(
        &mut self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32 {
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
        for (idx, bullet) in self.bullets.iter_mut().enumerate() {
            // The first bullet, when it's a markdown heading, is rendered as
            // a title: its bullet marker is suppressed and the `#` shifts left
            // to occupy the marker's slot.
            let is_title_bullet = idx == 0 && bullet.textbox.text().starts_with("# ");

            let depth_offset = (bullet.depth as f32) * indent_per_level;
            if !is_title_bullet {
                let marker_x = x + depth_offset + indent_per_level / 2.0;
                let marker_y = cur_y + line_height / 2.0;
                canvas.draw_circle((marker_x, marker_y), radius, &bullet_paint);
            }

            let (text_x, text_w) = if is_title_bullet {
                (x + depth_offset, (width - depth_offset).max(40.0))
            } else {
                (
                    x + depth_offset + indent_per_level,
                    (width - depth_offset - indent_per_level).max(40.0),
                )
            };
            let is_focused_bullet = focused && bullet.id == self.focused_bullet;
            // Selection (highlight) for the active bullet whenever the cell is
            // focused. Caret only when also editing.
            let bullet_focused = is_focused_bullet && !suppress_caret;
            let bullet_show_caret = show_caret && !suppress_caret && bullet.id == self.focused_bullet;
            let h =
                bullet
                    .textbox
                    .tick(canvas, text_x, cur_y, text_w, bullet_focused, bullet_show_caret);
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
                Key::Named(NamedKey::Enter)
                    if !primary_mod(mods) && !mods.alt_key() && !mods.shift_key() =>
                {
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
                    if !primary_mod(mods)
                        && !word_mod(mods)
                        && self.focused_at_text_start() =>
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

    pub fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
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
        self.bullets[idx]
            .textbox
            .mouse_down(abs_x, abs_y, modifiers, editing)
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

    pub fn add_link_to_first(&mut self, range: Range<usize>, url: String) {
        if let Some(b) = self.bullets.first_mut() {
            b.textbox.add_link(range, url);
        }
    }

    pub fn replace_in_bullet_with_link(
        &mut self,
        bullet_id: Uuid,
        range: Range<usize>,
        text: String,
        url: String,
    ) {
        if let Some(b) = self.bullets.iter_mut().find(|b| b.id == bullet_id) {
            b.textbox.replace_with_link(range, text, url);
        }
    }

    /// Plain-text representation of the current selection — joined bullet text
    /// (one bullet per line, indented with 4 spaces per depth) when a multi-
    /// bullet selection is active, otherwise the focused bullet's textbox
    /// selection.
    pub fn copy_text(&self) -> String {
        if let Some(sel) = self.bullet_selection {
            self.copy_bullet_range(sel)
        } else if let Some(idx) = self.focused_index() {
            self.bullets[idx].textbox.copy_primary_selection()
        } else {
            String::new()
        }
    }

    pub fn cut_text(&mut self) -> String {
        if let Some(sel) = self.bullet_selection {
            let text = self.copy_bullet_range(sel);
            self.delete_bullet_selection();
            text
        } else if let Some(idx) = self.focused_index() {
            self.bullets[idx].textbox.cut_primary_selection()
        } else {
            String::new()
        }
    }

    pub fn paste_text(&mut self, s: &str) {
        if self.bullet_selection.is_some() {
            self.delete_bullet_selection();
        }
        if let Some(idx) = self.focused_index() {
            self.bullets[idx].textbox.paste(s);
        }
    }

    fn copy_bullet_range(&self, sel: BulletSelection) -> String {
        let (Some(ai), Some(hi)) = (
            self.bullet_idx_by_id(sel.anchor_id),
            self.bullet_idx_by_id(sel.head_id),
        ) else {
            return String::new();
        };
        let lo = ai.min(hi);
        let high = ai.max(hi);
        self.bullets[lo..=high]
            .iter()
            .map(|b| {
                let indent = "    ".repeat(b.depth as usize);
                format!("{}{}", indent, b.textbox.text())
            })
            .collect::<Vec<_>>()
            .join("\n")
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

    pub fn focused_bullet_id(&self) -> Uuid {
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
    pub fn anchor_doc_pos(&self, bullet_id: Uuid, byte: usize) -> Option<(f32, f32)> {
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

    #[cfg(test)]
    pub fn split_focused_for_test(&mut self) -> bool {
        self.split_focused()
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

        // Partition links between the two halves before truncating text
        // (replace_text would otherwise drop the suffix-side links wholesale).
        let suffix_links = self.bullets[idx].textbox.split_links_at(head);

        // Trim original to prefix; caret position there doesn't matter (focus moves).
        self.bullets[idx].textbox.replace_text(prefix);
        let prefix_len = self.bullets[idx].textbox.text().len();
        self.bullets[idx].textbox.set_caret_at(prefix_len);

        let new_id = Uuid::now_v7();
        let mut new_tb = TextBox::new(self.typeface.clone(), String::new());
        new_tb.append_with_links(&suffix, suffix_links);
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
            let new_id = Uuid::now_v7();
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
        let merged_text = self.bullets[idx].textbox.text().to_string();
        let merged_links = self.bullets[idx].textbox.links().to_vec();
        self.bullets.remove(idx);
        let prev = &mut self.bullets[prev_idx];
        prev.textbox
            .append_with_links(&merged_text, merged_links);
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
pub struct CellSnapshot {
    pub timestamp: i64,
    pub edited_at: i64,
    pub context_hint_id: Option<Uuid>,
    pub kind: CellSnapshotKind,
}

#[derive(Clone)]
pub enum CellSnapshotKind {
    Plain(TextBoxSnapshot),
    Outline(OutlineSnapshot),
}

impl CellSnapshot {
    /// Document content equality (ignores selection + timestamp state). Used
    /// by undo to detect "cursor moved but text didn't change" events that
    /// shouldn't record a new undo entry.
    pub fn doc_eq(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (CellSnapshotKind::Plain(a), CellSnapshotKind::Plain(b)) => a.text == b.text,
            (CellSnapshotKind::Outline(a), CellSnapshotKind::Outline(b)) => {
                a.bullets.len() == b.bullets.len()
                    && a.bullets.iter().zip(b.bullets.iter()).all(|(x, y)| {
                        x.depth == y.depth && x.textbox.text == y.textbox.text
                    })
            }
            _ => false,
        }
    }
}

pub struct Cell {
    pub id: Uuid,
    pub kind: CellKind,
    /// Stream position. Set once at creation; never moves.
    pub timestamp: i64,
    /// Bumped on any content change. Display-only metadata.
    pub edited_at: i64,
    /// Optional hint about which context the cell was created in.
    /// Does NOT determine visibility — that's purely timestamp-based.
    pub context_hint_id: Option<Uuid>,
}

pub enum CellKind {
    Plain(TextBox),
    Outline(OutlineCell),
}

impl Cell {
    pub fn new(typeface: Typeface, initial_text: String) -> Self {
        let now = now_epoch_ms();
        Self {
            id: Uuid::now_v7(),
            kind: CellKind::Plain(TextBox::new(typeface, initial_text)),
            timestamp: now,
            edited_at: now,
            context_hint_id: None,
        }
    }

    pub fn new_outline(typeface: Typeface) -> Self {
        let now = now_epoch_ms();
        Self {
            id: Uuid::now_v7(),
            kind: CellKind::Outline(OutlineCell::new(typeface)),
            timestamp: now,
            edited_at: now,
            context_hint_id: None,
        }
    }

    /// Reconstruct from raw parts (used by the persistence layer).
    pub fn from_parts(
        id: Uuid,
        kind: CellKind,
        timestamp: i64,
        edited_at: i64,
        context_hint_id: Option<Uuid>,
    ) -> Self {
        Self { id, kind, timestamp, edited_at, context_hint_id }
    }

    /// Reconstruct a cell from a snapshot + id, using `typeface` for fresh
    /// TextBox / OutlineCell instances. Used by undo of delete and redo of
    /// insert to recreate cells with their original identity intact.
    pub fn from_snapshot(id: Uuid, snap: CellSnapshot, typeface: &Typeface) -> Self {
        let kind = match snap.kind {
            CellSnapshotKind::Plain(tbs) => {
                let mut tb = TextBox::new(typeface.clone(), String::new());
                tb.restore(tbs);
                CellKind::Plain(tb)
            }
            CellSnapshotKind::Outline(os) => {
                let mut oc = OutlineCell::new(typeface.clone());
                oc.restore(os);
                CellKind::Outline(oc)
            }
        };
        Self::from_parts(id, kind, snap.timestamp, snap.edited_at, snap.context_hint_id)
    }

    /// Add a link span to the cell's first textbox (plain) or first bullet
    /// (outline). Used by seed setup; future link-creation UI will go through
    /// a richer path scoped to the focused textbox.
    pub fn add_link_to_first(&mut self, range: Range<usize>, url: String) {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.add_link(range, url),
            CellKind::Outline(oc) => oc.add_link_to_first(range, url),
        }
    }

    /// Replace `range` with `text` in the focused textbox (the cell itself
    /// for Plain, or the bullet matching `bullet_id` for Outline) and link
    /// the inserted text to `url`.
    pub fn replace_focused_with_link(
        &mut self,
        bullet_id: Option<Uuid>,
        range: Range<usize>,
        text: String,
        url: String,
    ) {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.replace_with_link(range, text, url),
            CellKind::Outline(oc) => {
                if let Some(bid) = bullet_id {
                    oc.replace_in_bullet_with_link(bid, range, text, url);
                }
            }
        }
    }

    pub fn copy_text(&self) -> String {
        match &self.kind {
            CellKind::Plain(tb) => tb.copy_primary_selection(),
            CellKind::Outline(oc) => oc.copy_text(),
        }
    }

    /// Heading title for this cell, if any. Plain cells use the first
    /// paragraph; outline cells use the first bullet's heading.
    pub fn heading_title(&self) -> Option<String> {
        match &self.kind {
            CellKind::Plain(tb) => tb.heading_title(),
            CellKind::Outline(oc) => oc.bullets().first().and_then(|b| b.textbox().heading_title()),
        }
    }

    /// All distinct heading-tag names attached to this cell (any heading
    /// bullet for outlines).
    pub fn heading_tag_names(&self) -> Vec<String> {
        match &self.kind {
            CellKind::Plain(tb) => tb.heading_tag_names(),
            CellKind::Outline(oc) => {
                let mut out: Vec<String> = Vec::new();
                for b in oc.bullets() {
                    for n in b.textbox().heading_tag_names() {
                        if !out.contains(&n) {
                            out.push(n);
                        }
                    }
                }
                out
            }
        }
    }

    /// Full text of the cell, ignoring selection state. Used as the fallback
    /// for view-mode copy when no selection is active. Outline cells join
    /// bullets with newlines, indenting nested bullets two spaces per depth.
    pub fn full_text(&self) -> String {
        match &self.kind {
            CellKind::Plain(tb) => tb.text().to_string(),
            CellKind::Outline(oc) => {
                let mut out = String::new();
                for (i, b) in oc.bullets().iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    for _ in 0..b.depth() {
                        out.push_str("  ");
                    }
                    out.push_str(b.textbox().text());
                }
                out
            }
        }
    }

    pub fn cut_text(&mut self) -> String {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.cut_primary_selection(),
            CellKind::Outline(oc) => oc.cut_text(),
        }
    }

    pub fn paste_text(&mut self, s: &str) {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.paste(s),
            CellKind::Outline(oc) => oc.paste_text(s),
        }
    }

    pub fn tick(
        &mut self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32 {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.tick(canvas, x, y, width, focused, show_caret),
            CellKind::Outline(oc) => oc.tick(canvas, x, y, width, focused, show_caret),
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.handle_key(event, modifiers),
            CellKind::Outline(oc) => oc.handle_key(event, modifiers),
        }
    }

    pub fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.mouse_down(abs_x, abs_y, modifiers, editing),
            CellKind::Outline(oc) => oc.mouse_down(abs_x, abs_y, modifiers, editing),
        }
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.mouse_drag_to(abs_x, abs_y),
            CellKind::Outline(oc) => oc.mouse_drag_to(abs_x, abs_y),
        }
    }

    pub fn mouse_up(&mut self) -> bool {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.mouse_up(),
            CellKind::Outline(oc) => oc.mouse_up(),
        }
    }

    pub fn x_origin(&self) -> f32 {
        match &self.kind {
            CellKind::Plain(tb) => tb.x_origin(),
            CellKind::Outline(oc) => oc.x_origin,
        }
    }

    pub fn y_origin(&self) -> f32 {
        match &self.kind {
            CellKind::Plain(tb) => tb.y_origin(),
            CellKind::Outline(oc) => oc.y_origin,
        }
    }

    pub fn width(&self) -> f32 {
        match &self.kind {
            CellKind::Plain(tb) => tb.width(),
            CellKind::Outline(oc) => oc.width,
        }
    }

    pub fn height(&self) -> f32 {
        match &self.kind {
            CellKind::Plain(tb) => tb.height(),
            CellKind::Outline(oc) => oc.height,
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.kind {
            CellKind::Plain(tb) => tb.is_empty(),
            CellKind::Outline(oc) => oc.is_empty(),
        }
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.set_font_scale(scale),
            CellKind::Outline(oc) => oc.set_font_scale(scale),
        }
    }

    pub fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        match &self.kind {
            CellKind::Plain(tb) => tb.caret_doc_y_band(),
            CellKind::Outline(oc) => oc.caret_doc_y_band(),
        }
    }

    pub fn at_top_edge(&self) -> bool {
        match &self.kind {
            CellKind::Plain(tb) => tb.at_top_visual_line(),
            CellKind::Outline(oc) => oc.at_top_edge(),
        }
    }

    pub fn at_bottom_edge(&self) -> bool {
        match &self.kind {
            CellKind::Plain(tb) => tb.at_bottom_visual_line(),
            CellKind::Outline(oc) => oc.at_bottom_edge(),
        }
    }

    pub fn place_caret_at_start(&mut self) {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.set_caret_at(0),
            CellKind::Outline(oc) => oc.place_caret_at_start(),
        }
    }

    pub fn place_caret_at_end(&mut self) {
        match &mut self.kind {
            CellKind::Plain(tb) => {
                let end = tb.text().len();
                tb.set_caret_at(end);
            }
            CellKind::Outline(oc) => oc.place_caret_at_end(),
        }
    }

    /// `(text, caret_byte)` for the active text input — the cell's textbox
    /// for plain cells, or the focused bullet's textbox for outline cells.
    pub fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        match &self.kind {
            CellKind::Plain(tb) => tb.primary_caret().map(|(_, h)| (tb.text(), h)),
            CellKind::Outline(oc) => oc.focused_text_and_caret(),
        }
    }

    /// Outline cells: ID of the focused bullet. Plain cells: None.
    pub fn focused_bullet_id(&self) -> Option<Uuid> {
        match &self.kind {
            CellKind::Plain(_) => None,
            CellKind::Outline(oc) => Some(oc.focused_bullet_id()),
        }
    }

    /// Anchor position for an overlay tied to byte `byte` in this cell's
    /// active textbox (focused bullet for outline). Used by the @-mention popup.
    pub fn anchor_doc_pos(
        &self,
        bullet_id: Option<Uuid>,
        byte: usize,
    ) -> Option<(f32, f32)> {
        match (&self.kind, bullet_id) {
            (CellKind::Plain(tb), None) => {
                let (x, _) = tb.doc_position_of_byte(byte)?;
                let (_, bot) = tb.line_y_band_of_byte(byte)?;
                Some((x, bot))
            }
            (CellKind::Outline(oc), Some(id)) => oc.anchor_doc_pos(id, byte),
            _ => None,
        }
    }

    pub fn snapshot(&self) -> CellSnapshot {
        CellSnapshot {
            timestamp: self.timestamp,
            edited_at: self.edited_at,
            context_hint_id: self.context_hint_id,
            kind: match &self.kind {
                CellKind::Plain(tb) => CellSnapshotKind::Plain(tb.snapshot()),
                CellKind::Outline(oc) => CellSnapshotKind::Outline(oc.snapshot()),
            },
        }
    }

    /// Restore from a snapshot of the same variant. Variant mismatches are a
    /// bug (undo stack and live state disagree); fall through silently rather
    /// than panic. All metadata (timestamp, edited_at, context_hint_id) is
    /// preserved from the snapshot.
    pub fn restore(&mut self, snap: CellSnapshot) {
        self.timestamp = snap.timestamp;
        self.edited_at = snap.edited_at;
        self.context_hint_id = snap.context_hint_id;
        match (&mut self.kind, snap.kind) {
            (CellKind::Plain(tb), CellSnapshotKind::Plain(tbs)) => tb.restore(tbs),
            (CellKind::Outline(oc), CellSnapshotKind::Outline(os)) => oc.restore(os),
            _ => {}
        }
    }
}

pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::FontMgr;

    fn typeface() -> Typeface {
        FontMgr::new()
            .new_from_data(include_bytes!("../resources/fonts/Figtree.ttf"), None)
            .expect("font loads")
    }

    #[test]
    fn outline_split_preserves_suffix_link() {
        // After split, a link that was wholly in the suffix half should
        // appear on the new bullet (not be silently dropped).
        let mut tb = TextBox::new(typeface(), "head | LINK".to_string());
        tb.add_link(7..11, "https://example.com/".to_string());
        tb.set_caret_at(5); // split at the '|'
        let mut oc = OutlineCell::from_bullets(
            typeface(),
            vec![Bullet::new(Uuid::now_v7(), tb, 0)],
        );
        oc.split_focused_for_test();
        let bullets = oc.bullets();
        assert_eq!(bullets.len(), 2);
        assert_eq!(bullets[0].textbox().text(), "head ");
        assert!(bullets[0].textbox().links().is_empty());
        assert_eq!(bullets[1].textbox().text(), "| LINK");
        let new_links = bullets[1].textbox().links();
        assert_eq!(new_links.len(), 1, "suffix link rebased onto new bullet");
        assert_eq!(new_links[0].range, 2..6);
    }

    #[test]
    fn outline_bullet_link_survives_split_then_undo() {
        let mut tb = TextBox::new(typeface(), "before LINK after".to_string());
        tb.add_link(7..11, "https://example.com/".to_string());
        tb.set_caret_at(9);
        let bullet_id = Uuid::now_v7();
        let mut oc = OutlineCell::from_bullets(
            typeface(),
            vec![Bullet::new(bullet_id, tb, 0)],
        );
        let pre = oc.snapshot();
        // Simulate what split_focused does: shorten this bullet's text and
        // append a new bullet with the suffix. The original's `links` Vec
        // is left pointing past the new (shorter) text — the buggy state
        // before our fix.
        oc.split_focused_for_test();
        // Restore from pre.
        oc.restore(pre);
        let bullets = oc.bullets();
        assert_eq!(bullets.len(), 1, "single bullet restored");
        assert_eq!(bullets[0].textbox().text(), "before LINK after");
        let links = bullets[0].textbox().links();
        assert_eq!(links.len(), 1, "link restored on undo");
        assert_eq!(links[0].range, 7..11);
    }

    #[test]
    fn typing_after_link_does_not_extend_it() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.replace_with_link(0..0, "Alice".to_string(), "kept://a".to_string());
        assert_eq!(tb.text(), "Alice");
        assert_eq!(tb.links()[0].range, 0..5);
        // Caret is at end of inserted text. Type a space + word.
        tb.insert_text(" hi");
        assert_eq!(tb.text(), "Alice hi");
        // Link must still cover only "Alice", not the trailing chars.
        assert_eq!(tb.links()[0].range, 0..5);
    }

    #[test]
    fn typing_inside_link_still_extends_it() {
        let mut tb = TextBox::new(typeface(), "see Alice here".to_string());
        tb.add_link(4..9, "kept://a".to_string()); // covers "Alice"
        tb.set_caret_at(7); // inside, between "li" and "ce"
        tb.insert_text("X");
        // Link extends to cover the inserted byte: now [4, 10) over "AliXce".
        assert_eq!(tb.text(), "see AliXce here");
        assert_eq!(tb.links()[0].range, 4..10);
    }

    #[test]
    fn replace_with_link_inserts_link_over_text() {
        let mut tb = TextBox::new(typeface(), "see @al here".to_string());
        // Replace "@al" (bytes 4..7) with "Alice Smith", linking it.
        tb.replace_with_link(4..7, "Alice Smith".to_string(), "kept://abc".to_string());
        assert_eq!(tb.text(), "see Alice Smith here");
        let links = tb.links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].range, 4..15); // "Alice Smith" = 11 bytes
        assert_eq!(links[0].url, "kept://abc");
    }

    #[test]
    fn link_survives_enter_then_backspace() {
        // User reproduction: caret in mid-link, Enter, Backspace. The link
        // should return to its pre-Enter state, not vanish on the deletion.
        let mut tb = TextBox::new(typeface(), "before LINK after".to_string());
        tb.add_link(7..11, "https://example.com/".to_string());
        tb.set_caret_at(9);
        // Enter: extend link across the \n.
        tb.insert_text("\n");
        assert_eq!(tb.text(), "before LI\nNK after");
        assert_eq!(tb.links()[0].range, 7..12, "link spans the \\n");
        // Backspace: delete the \n. Caret is at byte 10 (after \n). Delete
        // [9, 10) — exercise the same edit shape as a real Backspace.
        tb.set_caret_at(10);
        tb.backspace_for_test();
        assert_eq!(tb.text(), "before LINK after");
        assert_eq!(
            tb.links().len(),
            1,
            "link survives delete-of-newline-inside-link"
        );
        assert_eq!(tb.links()[0].range, 7..11);
    }

    #[test]
    fn plain_link_survives_multi_edit_undo_sequence() {
        let mut tb = TextBox::new(typeface(), "before LINK after".to_string());
        tb.add_link(7..11, "https://example.com/".to_string());
        // Stack of pre-snapshots, one per edit, mirroring what the app records.
        let snap0 = tb.snapshot();
        tb.set_caret_at(9);
        tb.insert_text("X"); // -> "before LIXNK after", link 7..12
        let snap1 = tb.snapshot();
        tb.insert_text("Y"); // -> "before LIXYNK after", link 7..13
        let snap2 = tb.snapshot();
        tb.insert_text("\n"); // -> "before LIXY\nNK after", link 7..14
        // Expect link still there spanning both visual lines.
        assert_eq!(tb.links().len(), 1);
        assert_eq!(tb.links()[0].range, 7..14);
        // Undo last → should restore to snap2.
        tb.restore(snap2);
        assert_eq!(tb.text(), "before LIXYNK after");
        assert_eq!(tb.links()[0].range, 7..13);
        // Undo again → snap1.
        tb.restore(snap1);
        assert_eq!(tb.text(), "before LIXNK after");
        assert_eq!(tb.links()[0].range, 7..12);
        // Undo to original.
        tb.restore(snap0);
        assert_eq!(tb.text(), "before LINK after");
        assert_eq!(tb.links()[0].range, 7..11);
    }

    #[test]
    fn plain_link_survives_edit_then_undo() {
        let mut tb = TextBox::new(typeface(), "before LINK after".to_string());
        tb.add_link(7..11, "https://example.com/".to_string());
        let pre = tb.snapshot();
        // Caret in middle of link, type chars; then insert a newline.
        tb.set_caret_at(9);
        tb.insert_text("X");
        tb.insert_text("Y");
        tb.insert_text("\n");
        // The link should still exist and span across the newline.
        assert_eq!(tb.links().len(), 1, "link survived edit");
        let after = tb.links()[0].range.clone();
        assert!(after.end > after.start);
        // Restore from snapshot.
        tb.restore(pre.clone());
        assert_eq!(tb.text(), "before LINK after");
        assert_eq!(tb.links().len(), 1, "link restored on undo");
        assert_eq!(tb.links()[0].range, 7..11);
        assert_eq!(tb.links()[0].url, "https://example.com/");
    }
}
