//! `TextBox` — the editable text widget that backs every body type
//! (Plain cells directly; Outline bullets, PopPop input/output, Table
//! entries, and Cell title slots all hold one). Owns text + selections
//! + links + line wrap cache + drag state.

use std::ops::Range;
use std::time::{Duration, Instant};

use skia_safe::{Canvas, Color, Font, Paint, Point, Rect, Typeface};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

use super::common::{
    Affinity, BODY_FONT_SIZE, CARET_WIDTH, Edit, HEADING_FONT_SCALE, LinkSpan,
    MULTI_CLICK_DIST, MULTI_CLICK_INTERVAL, Selection, Selections, TagSpan, TextBoxSnapshot,
    line_edge_mod, primary_mod, transform_index, transform_index_closed_end, word_mod,
};
use super::wrap::{
    draw_line_with_links, find_line_at, find_word_at, find_word_left_of, find_word_right_of,
    hit_affinity, line_x_at_offset, locate_caret, next_char_boundary, parse_heading_tags,
    parse_inline_tags, prev_char_boundary, step_horizontal, trim_nl_end, wrap_text_styled,
};

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
    /// Aligned with `body_lines`: true when the source paragraph the line
    /// belongs to is a comment (only meaningful when
    /// `enable_comment_coloring` is set). PopPop input cells use this to
    /// paint `#`-prefixed lines in dark green; everywhere else this stays
    /// all-false and is a no-op.
    line_is_comment: Vec<bool>,
    /// Aligned with `body_lines`: extra vertical space (logical px) added
    /// at the bottom of each visual line. Used by PopPop to reserve room
    /// for an error message immediately under the failing input row;
    /// elsewhere this stays empty (or all-zero) and is a no-op.
    line_extra_below: Vec<f32>,
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
    /// Byte ranges marked as `#tag` tokens. Populated by the mention
    /// popup's commit path and by the persistence layer's legacy
    /// migration (one-shot at load if the saved cell has no spans but
    /// the text has tag-shaped tokens). Typing `#X` without going
    /// through the popup leaves NO span — so no tag.
    tags: Vec<TagSpan>,
    /// Body text color. Default dark gray. Cells (e.g. PopPop's output
    /// column) may override to render their textbox in a custom color.
    text_color: Color,
    /// When true, every visual line renders in the heading font and trailing
    /// `#tags` parse from the end of the first paragraph. Used by the cell
    /// title slot. When false, the textbox is plain body text — no auto-
    /// heading from a leading `# `.
    force_heading: bool,
    /// When true, source paragraphs that start with `#` are classified as
    /// comments at wrap time and painted in dark green at draw time.
    /// PopPop input cells flip this on; everywhere else it's false.
    enable_comment_coloring: bool,
    /// URL set by a click on a link, drained by the app layer post-click.
    /// We can't navigate from inside `mouse_down` because routing
    /// `kept://...` requires the entity / cell caches that live on
    /// `KeptApp`. Buffering here keeps click detection in the cell while
    /// keeping navigation policy in the app.
    pending_link_url: Option<String>,
    /// Tag name (without leading `#`) set by a click on an inline `#tag`
    /// substring. Drained by the app, which navigates to that tag's
    /// filter view via `push_view(Query::tag(name))`. Same buffering
    /// rationale as `pending_link_url`.
    pending_tag_name: Option<String>,
    /// Local undo stack — pre-edit `TextBoxSnapshot`s pushed by every
    /// run through `apply_edits_right_to_left` (the chokepoint for
    /// every text mutation). Drives Cmd+Z inside inline inputs that
    /// don't have a cell-level undo wrapper (search popup, people
    /// rename, people add). For cells, app-level Cmd+Z is intercepted
    /// before the `TextBox` ever sees it, so this stack accumulates
    /// silently and is never popped — bounded by `TEXTBOX_UNDO_CAP`.
    undo_stack: Vec<TextBoxSnapshot>,
    redo_stack: Vec<TextBoxSnapshot>,
    /// Wall-clock time of the most recent edit. Two edits within
    /// `TEXTBOX_COALESCE_INTERVAL` collapse into one undo entry — so
    /// typing "hello" reverts as a single undo, not five.
    last_edit_at: Option<Instant>,
    /// "Decisive" gestures (mouse click, arrow nav, undo/redo, paste,
    /// cut) flip this on; `record_undo` checks it and refuses to
    /// coalesce when set. Mirrors `KeptApp::coalesce_break`.
    coalesce_break: bool,
}

const TEXTBOX_UNDO_CAP: usize = 200;
const TEXTBOX_COALESCE_INTERVAL: Duration = Duration::from_millis(600);

impl TextBox {
    pub fn new(typeface: Typeface, initial_text: String) -> Self {
        Self {
            typeface,
            text: initial_text,
            sels: Selections::single_caret(0),
            body_lines: Vec::new(),
            line_is_heading: Vec::new(),
            line_is_comment: Vec::new(),
            line_extra_below: Vec::new(),
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
            tags: Vec::new(),
            text_color: crate::color::text_primary(),
            force_heading: false,
            enable_comment_coloring: false,
            pending_link_url: None,
            pending_tag_name: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit_at: None,
            coalesce_break: false,
        }
    }

    /// Drain a link URL set by a click in the most recent `mouse_down`.
    /// The app layer calls this after dispatching mouse_down to route
    /// `kept://...` URLs to the entity / cell view (or fall through to
    /// `open_url` for external URLs).
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        self.pending_link_url.take()
    }

    /// Drain an inline-tag name (without leading `#`) set by a click in
    /// the most recent `mouse_down`. The app routes it through
    /// `push_view(Query::tag(name))` — same destination as the sidebar
    /// tag-row click.
    pub fn take_pending_tag_name(&mut self) -> Option<String> {
        self.pending_tag_name.take()
    }

    /// Tag name (without leading `#`) at byte position `byte`, if any.
    /// Uses the same `parse_inline_tags` rule as the persistence and
    /// query layers so visual styling, click target, and DB membership
    /// stay aligned.
    pub fn tag_at(&self, byte: usize) -> Option<String> {
        for tag in &self.tags {
            let r = &tag.range;
            if byte >= r.start && byte < r.end && r.end > r.start + 1 {
                let s = &self.text[r.start..r.end];
                return s.strip_prefix('#').map(|name| name.to_string());
            }
        }
        None
    }

    /// True iff `(abs_x, abs_y)` lands on an inline-tag substring.
    /// Powers the hand cursor on hover (mirrors `link_at_doc_pos`).
    pub fn tag_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        let lx = abs_x - self.x_origin;
        let ly = abs_y - self.y_origin;
        if lx < 0.0 || lx > self.width || ly < 0.0 || ly > self.height {
            return false;
        }
        let (idx, _) = self.hit_test(lx, ly);
        self.tag_at(idx).is_some()
    }

    /// Override the body-text color. Used by PopPopCell for the output
    /// textbox so its sentinel "42" renders in dark blue.
    pub fn set_text_color(&mut self, color: Color) {
        self.text_color = color;
    }

    /// Mark this textbox as the cell's title slot: every line renders in the
    /// heading font and trailing `#tag` tokens get the smaller-font tag
    /// styling. Idempotent; rewraps when the flag changes so layout
    /// reflects the new line metrics.
    pub fn set_force_heading(&mut self, on: bool) {
        if self.force_heading == on {
            return;
        }
        self.force_heading = on;
        // Wrap cache must be rebuilt because line height / font changed.
        self.body_lines_width = f32::NAN;
        self.rewrap();
    }

    /// Per-visual-line extra vertical space (logical px), aligned with
    /// `body_lines`. PopPopCell uses this to reserve room beneath an
    /// erroring input row for the error message; both the input and
    /// output columns get the same extras so rows stay aligned.
    /// Vectors of the wrong length are tolerated — `layout()` resizes to
    /// match `body_lines` next call (zero-padding new lines, truncating
    /// overflow).
    pub fn set_line_extra_below(&mut self, extras: Vec<f32>) {
        self.line_extra_below = extras;
    }

    /// Opt this textbox into comment coloring: source paragraphs that
    /// start with `#` get classified as comments at wrap time and painted
    /// in dark green. Used by `PopPopCell`'s input column.
    pub fn set_enable_comment_coloring(&mut self, on: bool) {
        if self.enable_comment_coloring == on {
            return;
        }
        self.enable_comment_coloring = on;
        // Re-classify lines on next layout.
        self.body_lines_width = f32::NAN;
        self.rewrap();
    }

    #[allow(dead_code)]
    pub fn x_origin(&self) -> f32 {
        self.x_origin
    }

    pub fn y_origin(&self) -> f32 {
        self.y_origin
    }

    #[allow(dead_code)]
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

    pub fn typeface(&self) -> &Typeface {
        &self.typeface
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the entire text and reset the selection. Caller is responsible
    /// for setting a sensible caret position via `set_caret_at` afterward.
    /// Drops any links that no longer fit within the new text.
    pub fn replace_text(&mut self, new_text: String) {
        let new_len = new_text.len();
        self.record_undo();
        self.text = new_text;
        self.body_lines_width = f32::NAN;
        self.sels = Selections::single_caret(0);
        self.links.retain(|l| l.range.end <= new_len);
        // Whole-text replacement is decisive — no coalescing into the
        // next edit.
        self.break_coalesce();
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

    /// Replace selections with a single selection from `anchor` to
    /// `head` (in byte offsets). Both ends are clamped to the nearest
    /// char boundary at or before the requested index. `head` is the
    /// caret position; the visible range is `[min(anchor, head),
    /// max(anchor, head))`. Test-only for now; production callers go
    /// through mouse drag or keyboard shift-arrow.
    #[cfg(test)]
    pub fn select_range(&mut self, anchor: usize, head: usize) {
        let snap = |i: usize| -> usize {
            let mut n = i.min(self.text.len());
            while n > 0 && !self.text.is_char_boundary(n) {
                n -= 1;
            }
            n
        };
        self.sels = Selections {
            items: vec![Selection {
                anchor: snap(anchor),
                head: snap(head),
                affinity: Affinity::Downstream,
            }],
            primary: 0,
        };
    }

    /// True iff any selection has a non-empty range (anchor != head).
    pub fn has_selection(&self) -> bool {
        self.sels.items.iter().any(|s| !s.is_collapsed())
    }

    /// Collapse every selection range — anchor snaps onto head — so no
    /// visible highlight remains. Caret position is preserved. Called
    /// when a new mouse_down elsewhere should retire stale highlights
    /// in cells that aren't the click target (otherwise an old
    /// selection visually competes with the new one).
    pub fn clear_selection(&mut self) {
        for s in &mut self.sels.items {
            s.anchor = s.head;
        }
    }

    /// Replace every non-empty selection with empty text. Carets land at
    /// the deletion point. No-op when nothing is selected.
    pub fn delete_selection(&mut self) {
        if self.has_selection() {
            self.insert_text("");
        }
    }

    /// Replace selections with a single selection that spans the entire text.
    pub fn select_all(&mut self) {
        let len = self.text.len();
        self.sels = Selections {
            items: vec![Selection {
                anchor: 0,
                head: len,
                affinity: Affinity::Downstream,
            }],
            primary: 0,
        };
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
            tags: self.tags.clone(),
        }
    }

    /// Absolute-y `(top, bottom, is_heading)` for each source line (i.e., each
    /// `\n`-separated paragraph), in source order. The bottom of source line N
    /// is the top of source line N+1, or the bottom of the last visual line
    /// for the final paragraph. Used by `PopPopCell` to size row stripes and
    /// align outputs with input rows. Empty after a fresh rewrap if
    /// `body_lines` hasn't been populated yet.
    pub fn source_line_y_bands(&self) -> Vec<(f32, f32, bool)> {
        let bytes = self.text.as_bytes();
        // Collect (li, top_local, is_heading) per paragraph-starting body line.
        let mut entries: Vec<(usize, f32, bool)> = Vec::new();
        for (li, line) in self.body_lines.iter().enumerate() {
            let starts_paragraph = line.start == 0
                || matches!(bytes.get(line.start.saturating_sub(1)), Some(&b'\n'));
            if !starts_paragraph {
                continue;
            }
            if let Some(&(top_local, _)) = self.line_bands.get(li) {
                let is_heading = self.line_is_heading.get(li).copied().unwrap_or(false);
                entries.push((li, top_local, is_heading));
            }
        }
        // Bottom = next entry's top; for the last entry, the bottom of the
        // final visual line in body_lines (line_bands.last().bottom).
        let mut out = Vec::with_capacity(entries.len());
        let total_bottom_local = self
            .line_bands
            .last()
            .map(|&(_, b)| b)
            .unwrap_or(0.0);
        for (i, &(_li, top_local, is_heading)) in entries.iter().enumerate() {
            let bot_local = entries
                .get(i + 1)
                .map(|&(_, t, _)| t)
                .unwrap_or(total_bottom_local);
            out.push((
                self.y_origin + top_local,
                self.y_origin + bot_local,
                is_heading,
            ));
        }
        out
    }

    pub fn add_link(&mut self, range: Range<usize>, url: String) {
        if range.start < range.end && range.end <= self.text.len() {
            self.links.push(LinkSpan { range, url });
        }
    }

    /// Mark a byte range as a `#tag` token. The range should cover the
    /// whole `#tagname` substring (leading `#` included). Used by the
    /// mention popup's tag-commit path and by the persistence layer's
    /// legacy migration. Invalid ranges are silently dropped.
    /// Rendering reads `self.tags` directly each frame (overdraw
    /// pass), so no cache recomputation is needed here.
    /// Drop every `TagSpan` whose covered substring is exactly
    /// `#name`. Used by global tag-deletion (right-click "Delete
    /// tag" in the sidebar): the user has decided this tag should
    /// vanish, so any cell still carrying a span for it gets the
    /// span removed. The underlying text is left alone — only the
    /// styling/semantic span goes away. Returns whether anything
    /// changed (so the caller can mark dirty + push undo entries
    /// only when the operation actually mutated state).
    pub fn remove_tags_named(&mut self, name: &str) -> bool {
        let target = format!("#{name}");
        let before = self.tags.len();
        self.tags.retain(|t| {
            if t.range.end > self.text.len() || t.range.start >= t.range.end {
                // Stale / collapsed span: defensively keep (nothing to
                // compare against). Won't render anyway.
                return true;
            }
            self.text.get(t.range.clone()).map_or(true, |s| s != target)
        });
        self.tags.len() != before
    }

    pub fn add_tag(&mut self, range: Range<usize>) {
        if range.start < range.end
            && range.end <= self.text.len()
            && self.text[range.clone()].starts_with('#')
        {
            self.tags.push(TagSpan { range });
        }
    }

    /// Build a fresh `TextBox` that mirrors `self`'s rendered
    /// content for use as a read-only embed cache (Reference cells,
    /// envelope outline headers, the entity-page references list).
    /// Copies everything that affects how the text *renders* — text,
    /// font scale, heading mode, link spans, tag spans — so the
    /// cache looks identical to the source. View state (selection,
    /// caret, drag, undo stack, layout cache) is intentionally
    /// dropped: the cache is a separate live widget that owns its
    /// own interaction state across frames. `scale` overrides the
    /// source's recorded scale, since callers typically want the
    /// cache to render at the app's current zoom level rather than
    /// whatever the source was last rendered at.
    pub fn clone_for_cache(&self, typeface: Typeface, scale: f32) -> Self {
        let mut new_tb = TextBox::new(typeface, self.text.clone());
        new_tb.set_force_heading(self.force_heading);
        new_tb.set_font_scale(scale);
        for l in &self.links {
            new_tb.add_link(l.range.clone(), l.url.clone());
        }
        for t in &self.tags {
            new_tb.add_tag(t.range.clone());
        }
        new_tb
    }

    pub fn tags(&self) -> &[TagSpan] {
        &self.tags
    }

    /// One-shot legacy migration: scan the current text for tag-shaped
    /// tokens and add `TagSpan`s for each. Intended for cells loaded
    /// from a persisted blob that pre-dates span-based tags. No-op if
    /// the textbox already has tags. Heading mode (`force_heading`)
    /// uses the trailing-tags rule (matching `heading_tag_names`'
    /// previous behavior); body mode uses the inline-tags rule.
    pub fn migrate_tags_from_text(&mut self) {
        if !self.tags.is_empty() {
            return;
        }
        if self.force_heading {
            let heading_end = self.text.find('\n').unwrap_or(self.text.len());
            for r in parse_heading_tags(&self.text, heading_end) {
                if r.end > r.start + 1 {
                    self.tags.push(TagSpan { range: r });
                }
            }
        } else {
            for r in parse_inline_tags(&self.text) {
                if r.end > r.start + 1 {
                    self.tags.push(TagSpan { range: r });
                }
            }
        }
    }

    /// Distinct tag names (without the leading `#`) within the heading
    /// paragraph. Only meaningful when this TextBox is in
    /// `force_heading` mode (i.e., a cell title); plain body TextBoxes
    /// return an empty list. Names come from `TagSpan`s — typed `#X`
    /// without a span is NOT a tag.
    pub fn heading_tag_names(&self) -> Vec<String> {
        if !self.force_heading {
            return Vec::new();
        }
        let heading_end = self.text.find('\n').unwrap_or(self.text.len());
        let mut out: Vec<String> = Vec::new();
        for tag in &self.tags {
            if tag.range.end > heading_end || tag.range.end <= tag.range.start + 1 {
                continue;
            }
            let s = &self.text[tag.range.clone()];
            if let Some(name) = s.strip_prefix('#') {
                let name = name.to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Distinct tag names anywhere in the textbox — body inline tags
    /// for plain bodies, outline bullets, and table entries. Same
    /// span-derived rule as `heading_tag_names`.
    pub fn all_tag_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for tag in &self.tags {
            if tag.range.end <= tag.range.start + 1 {
                continue;
            }
            let s = &self.text[tag.range.clone()];
            if let Some(name) = s.strip_prefix('#') {
                let name = name.to_string();
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

    /// True if absolute document position `(abs_x, abs_y)` falls inside one
    /// of this textbox's link spans, given the most recent layout. Used to
    /// drive the hand cursor on hover.
    pub fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        if self.links.is_empty() {
            return false;
        }
        let lx = abs_x - self.x_origin;
        let ly = abs_y - self.y_origin;
        if lx < 0.0 || lx > self.width || ly < 0.0 || ly > self.height {
            return false;
        }
        let (idx, _) = self.hit_test(lx, ly);
        self.link_at(idx).is_some()
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
        self.record_undo();
        self.apply_edit(&Edit {
            range: lo..hi,
            replacement: String::new(),
        });
        // Cut isn't part of a typing burst — break so the next edit
        // doesn't merge with it.
        self.break_coalesce();
        cut_text
    }

    /// Insert `s` at every cursor (replacing each cursor's selection).
    /// Any `http(s)://…` URLs in the pasted text are auto-linkified —
    /// each pasted region gets a `LinkSpan` over the detected URL.
    pub fn paste(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let urls = detect_urls(s);
        if urls.is_empty() {
            self.insert_text(s);
            // Paste is its own undo unit even when no URLs were
            // detected — break so subsequent typing doesn't merge.
            self.break_coalesce();
            return;
        }
        // Each selection's pre-edit `[a, b)` is replaced by `s` (length
        // `s_len`). After all replacements, the text inserted for the
        // i-th selection (in left-to-right order) lands at byte offset
        // `a_i + cumulative_growth_from_prior_selections`. Compute
        // those landing offsets first, then do the insertion, then add
        // `LinkSpan`s relative to each landing offset.
        let mut starts: Vec<Range<usize>> =
            self.sels.items.iter().map(|sel| sel.range()).collect();
        starts.sort_by_key(|r| r.start);
        let mut insert_starts: Vec<usize> = Vec::with_capacity(starts.len());
        let mut cum: isize = 0;
        let s_len = s.len() as isize;
        for r in &starts {
            let landing = r.start as isize + cum;
            insert_starts.push(landing.max(0) as usize);
            cum += s_len - (r.end - r.start) as isize;
        }
        self.insert_text(s);
        for &start in &insert_starts {
            for (range, url) in &urls {
                let abs = (start + range.start)..(start + range.end);
                if abs.end <= self.text.len() {
                    self.add_link(abs, url.clone());
                }
            }
        }
        // Paste is its own undo unit — break so the next edit doesn't
        // merge with it.
        self.break_coalesce();
    }

    /// Restore document state from a snapshot. Resets transient input state
    /// (mouse drag, click count) and invalidates the wrap cache so the next
    /// tick re-wraps at the (possibly different) font scale.
    pub fn restore(&mut self, snap: TextBoxSnapshot) {
        self.text = snap.text;
        self.sels = snap.sels;
        self.font_scale = snap.font_scale;
        self.links = snap.links;
        self.tags = snap.tags;
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

    /// Pure-state layout step: set origins, wrap if width changed,
    /// populate `line_bands`. Does NOT draw. `tick` calls this first;
    /// PopPopCell calls it directly so it can compute row stripes
    /// before the text gets drawn over them.
    pub fn layout(&mut self, x: f32, y: f32, width: f32) {
        self.x_origin = x;
        self.y_origin = y;
        self.width = width;

        let body_font = self.body_font();
        let heading_font = self.heading_font();
        let paint = Paint::default();

        let max_text_width = width.max(80.0);
        if self.body_lines_width != max_text_width {
            let (lines, headings, comments) = wrap_text_styled(
                &self.text,
                &body_font,
                &heading_font,
                &paint,
                max_text_width,
                self.force_heading,
                self.enable_comment_coloring,
            );
            self.body_lines = lines;
            self.line_is_heading = headings;
            self.line_is_comment = comments;
            self.body_lines_width = max_text_width;
        }
        // Keep `line_extra_below` aligned with `body_lines` length. New
        // entries default to 0 (no extra spacing). PopPopCell may
        // overwrite this via `set_line_extra_below` between layout calls.
        if self.line_extra_below.len() != self.body_lines.len() {
            self.line_extra_below.resize(self.body_lines.len(), 0.0);
        }

        let (_, body_metrics) = body_font.metrics();
        let (_, heading_metrics) = heading_font.metrics();
        let line_count = self.body_lines.len().max(1);
        self.line_bands.clear();
        let mut cur_local = 0.0_f32;
        for li in 0..line_count {
            let m = if self.line_is_heading.get(li).copied().unwrap_or(false) {
                heading_metrics
            } else {
                body_metrics
            };
            let step = -m.ascent + m.descent + m.leading;
            let extra = step * 0.25;
            let line_advance = step + extra
                + self.line_extra_below.get(li).copied().unwrap_or(0.0);
            // top = cur_local; bottom = top + line_advance.
            self.line_bands.push((cur_local, cur_local + line_advance));
            cur_local += line_advance;
        }
        self.height = cur_local;
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
        self.layout(x, y, width);

        let body_font = self.body_font();
        let heading_font = self.heading_font();
        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(self.text_color);

        let (_, body_metrics) = body_font.metrics();
        let (_, heading_metrics) = heading_font.metrics();
        let line_metrics_for = |li: usize| {
            if self.line_is_heading.get(li).copied().unwrap_or(false) {
                heading_metrics
            } else {
                body_metrics
            }
        };

        // Derive cell-local baselines from the (already populated) line_bands:
        // top + (-ascent) is the baseline.
        let baselines_local: Vec<f32> = self
            .line_bands
            .iter()
            .enumerate()
            .map(|(li, &(top, _))| top + (-line_metrics_for(li).ascent))
            .collect();

        if focused {
            let mut hl_paint = Paint::default();
            hl_paint.set_anti_alias(true);
            hl_paint.set_color(crate::color::accent_blue_alpha(0x60));
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
                    let x0 = x
                        + line_x_at_offset(line_text, s - line.start, main_font, &text_paint);
                    let x1 = x
                        + line_x_at_offset(line_text, e - line.start, main_font, &text_paint);
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
        link_paint.set_color(crate::color::link_text());
        let mut underline_paint = Paint::default();
        underline_paint.set_anti_alias(true);
        underline_paint.set_color(crate::color::link_text());
        underline_paint.set_stroke_width(1.0);
        // Dark-green paint used for `#`-comment lines when comment coloring
        // is enabled (PopPop input cells). Built once per tick.
        let mut comment_paint = Paint::default();
        comment_paint.set_anti_alias(true);
        comment_paint.set_color(crate::color::poppop_comment());
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
            let line_paint = if self.line_is_comment.get(li).copied().unwrap_or(false) {
                &comment_paint
            } else {
                &text_paint
            };
            // Uniform line render: heading and body lines both use a
            // single font for the whole line. Tag styling is layered
            // on top via the inline-tag overdraw pass below — the
            // pre-fix model carved the line into title/gap/tag with
            // a smaller font for the tag area, which broke when a
            // `#tag` sat anywhere other than the trailing edge.
            if self.links.is_empty() {
                canvas.draw_str(
                    &self.text[visible_line.clone()],
                    Point::new(x, baseline),
                    line_font,
                    line_paint,
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
                    line_paint,
                    &link_paint,
                    &underline_paint,
                );
            }
        }

        // Tag overlay: re-draw each `TagSpan` substring in the muted
        // ghost color so tags read as metadata, not prose. Same font
        // as the underlying line (heading_font for headings, body_font
        // otherwise) — just an overdraw with a different paint, which
        // is cheap and visually clean at the antialias level. Skips
        // comment lines (PopPop colors them green and exempts them
        // from tag semantics anyway). Driven by `TagSpan`s — typed
        // `#X` without a span (no popup commit) is plain text.
        let inline_tag_ranges: Vec<Range<usize>> =
            self.tags.iter().map(|t| t.range.clone()).collect();
        if !inline_tag_ranges.is_empty() {
            let mut tag_dim_paint = Paint::default();
            tag_dim_paint.set_anti_alias(true);
            tag_dim_paint.set_color(crate::color::text_ghost());
            for (li, line) in self.body_lines.iter().enumerate() {
                if self.line_is_comment.get(li).copied().unwrap_or(false) {
                    continue;
                }
                let line_font = if self.line_is_heading.get(li).copied().unwrap_or(false) {
                    &heading_font
                } else {
                    &body_font
                };
                let visible_end = trim_nl_end(&self.text, line);
                let line_text = &self.text[line.start..line.end];
                let baseline = baselines_local[li] + y;
                for r in &inline_tag_ranges {
                    let s = r.start.max(line.start).min(visible_end);
                    let e = r.end.min(visible_end);
                    if s >= e {
                        continue;
                    }
                    let prefix_w = line_font
                        .measure_str(&line_text[..s - line.start], Some(&text_paint))
                        .0;
                    canvas.draw_str(
                        &self.text[s..e],
                        Point::new(x + prefix_w, baseline),
                        line_font,
                        &tag_dim_paint,
                    );
                }
            }
        }

        if show_caret {
            let mut caret_paint = Paint::default();
            caret_paint.set_anti_alias(false);
            caret_paint.set_color(crate::color::text_primary());
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
                    x + line_x_at_offset(
                        line_text,
                        prefix_end - line.start,
                        main_font,
                        &text_paint,
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

        self.height
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
        // Mac-only: Cmd+Left/Right is line-edge nav (Shift extends selection).
        // Off-Mac, line_edge is always false; users have Home/End for this.
        let line_edge = line_edge_mod(mods);
        match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => {
                if line_edge {
                    self.move_to_line_edge(false, shift);
                } else if word {
                    self.move_horizontal_word(-1, shift);
                } else {
                    self.move_horizontal(-1, shift);
                }
                self.break_coalesce();
                true
            }
            Key::Named(NamedKey::ArrowRight) => {
                if line_edge {
                    self.move_to_line_edge(true, shift);
                } else if word {
                    self.move_horizontal_word(1, shift);
                } else {
                    self.move_horizontal(1, shift);
                }
                self.break_coalesce();
                true
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.move_vertical(-1, shift);
                self.break_coalesce();
                true
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.move_vertical(1, shift);
                self.break_coalesce();
                true
            }
            Key::Named(NamedKey::Home) => {
                self.move_to_line_edge(false, shift);
                self.break_coalesce();
                true
            }
            Key::Named(NamedKey::End) => {
                self.move_to_line_edge(true, shift);
                self.break_coalesce();
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
        // A click is a deliberate caret move — end any in-progress
        // typing burst so the next edit starts a fresh undo entry.
        self.break_coalesce();
        let lx = abs_x - self.x_origin;
        let ly = abs_y - self.y_origin;

        let (idx, affinity) = self.hit_test(lx, ly);
        let mods = modifiers.state();

        // Plain click on a link opens it in view mode; primary-modifier+click
        // opens it while editing (so plain clicks in edit mode still move the
        // caret). The primary modifier is Cmd on Mac, Ctrl elsewhere. Same
        // semantics apply to inline `#tag` substrings — click navigates to
        // that tag's filter view.
        //
        // Alt+click (without other mods) ALSO opens — the app routes it
        // to `open_in_other_pane` so the link/tag opens in the second
        // pane (matches sidebar alt-click). Without this branch the
        // click would fall through to the multi-cursor handler below
        // and the click would visibly do nothing.
        let plain_in_view = !editing && !mods.shift_key() && !mods.alt_key();
        let modified_in_edit =
            editing && primary_mod(mods) && !mods.shift_key() && !mods.alt_key();
        let alt_open = mods.alt_key() && !mods.shift_key() && !primary_mod(mods);
        if plain_in_view || modified_in_edit || alt_open {
            if let Some(name) = self.tag_at(idx) {
                self.pending_tag_name = Some(name);
                return true;
            }
            if let Some(link) = self.link_at(idx) {
                self.pending_link_url = Some(link.url.clone());
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

        // Shift+click extends the primary selection. Shift+Alt+click,
        // however, is the multi-cursor-add chord (the app-level click
        // dispatcher routes plain Alt+click to "open in other pane",
        // so the multi-cursor binding moved here). Let it fall
        // through to the alt branch below by gating the shift path
        // on !alt.
        if mods.shift_key() && !mods.alt_key() {
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
        let paint = Paint::default();

        let mut prev_offset = 0usize;
        let mut prev_w = 0.0f32;

        for (offset, _) in line_text.char_indices().skip(1) {
            let w = line_x_at_offset(line_text, offset, &main_font, &paint);
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

        let total_w = line_x_at_offset(line_text, line_text.len(), &main_font, &paint);
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

    pub(super) fn insert_text(&mut self, s: &str) {
        let mut edits: Vec<Edit> = Vec::new();
        for sel in &self.sels.items {
            edits.push(Edit {
                range: sel.range(),
                replacement: s.to_string(),
            });
        }
        self.apply_edits_right_to_left(edits);
    }

    /// Replace `range` with `text` as plain (un-linked) characters.
    /// Caret lands at the end of the inserted text. Used by the tag
    /// `#`-autocomplete commit, where the inserted token has no URL.
    pub fn replace_with_text(&mut self, range: Range<usize>, text: String) {
        let start = range.start;
        let inserted_len = text.len();
        self.record_undo();
        self.apply_edit(&Edit {
            range,
            replacement: text,
        });
        self.set_caret_at(start + inserted_len);
        self.break_coalesce();
    }

    /// Replace `range` with `text` and add a link covering the inserted text.
    /// Used to insert mentions like a person's title with a `kept://…` URL.
    /// Caret lands at the end of the inserted text.
    pub fn replace_with_link(&mut self, range: Range<usize>, text: String, url: String) {
        let start = range.start;
        let inserted_len = text.len();
        self.record_undo();
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
        self.break_coalesce();
    }

    /// Replace `range` with `text` (which must start with `#`) and
    /// mark the inserted run as a tag span. Caret lands at the end of
    /// the inserted text. Used by the `#`-mention popup commit so the
    /// inserted `#tagname` is recognized as a tag at save time.
    pub fn replace_with_tag(&mut self, range: Range<usize>, text: String) {
        let start = range.start;
        let inserted_len = text.len();
        self.record_undo();
        self.apply_edit(&Edit {
            range,
            replacement: text,
        });
        let end = start + inserted_len;
        // Route through `add_tag` so the heading-tag layout cache
        // gets recomputed — `apply_edit` above already ran rewrap,
        // but with the OLD `self.tags`. Without this the new span
        // lands in `self.tags` (sidebar sees it) but the title
        // doesn't render with tag styling until the next rewrap.
        if inserted_len > 0 {
            self.add_tag(start..end);
        }
        self.set_caret_at(end);
        self.break_coalesce();
    }

    fn apply_edits_right_to_left(&mut self, mut edits: Vec<Edit>) {
        if edits.is_empty() {
            return;
        }
        // One undo entry per "user action" — every text mutation funnels
        // through this method, so a single record_undo here covers
        // typing, paste, cut, delete_selection, and the Backspace /
        // Delete / Enter branches of handle_key.
        self.record_undo();
        edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
        for edit in &edits {
            self.apply_edit(edit);
        }
    }

    fn record_undo(&mut self) {
        let now = Instant::now();
        // Coalesce consecutive edits within the time window unless a
        // decisive gesture (mouse click, arrow nav, paste, undo/redo)
        // ended the burst. When we coalesce, the existing top-of-stack
        // snapshot already captures the start-of-burst state — just
        // skip the push.
        let can_coalesce = !self.coalesce_break
            && !self.undo_stack.is_empty()
            && self
                .last_edit_at
                .map(|t| now.duration_since(t) < TEXTBOX_COALESCE_INTERVAL)
                .unwrap_or(false);
        if !can_coalesce {
            let snap = self.snapshot();
            self.undo_stack.push(snap);
            if self.undo_stack.len() > TEXTBOX_UNDO_CAP {
                // Drop the oldest entry. `Vec::remove(0)` is O(n) but
                // the stack is bounded by TEXTBOX_UNDO_CAP and trim
                // happens at most once per edit.
                self.undo_stack.remove(0);
            }
        }
        // Any edit invalidates the redo stack — even a coalesced one.
        // Otherwise: type, undo, type-within-window would coalesce
        // (skip the push) and leave a stale redo entry pointing at
        // the abandoned future.
        self.redo_stack.clear();
        self.last_edit_at = Some(now);
        self.coalesce_break = false;
    }

    /// End the current typing burst — the next edit starts a fresh
    /// undo entry instead of coalescing into the previous one. Called
    /// from gestures that move the caret deliberately (mouse_down,
    /// arrow nav) and from edits that aren't part of a typing burst
    /// (paste, cut, undo, redo).
    pub fn break_coalesce(&mut self) {
        self.coalesce_break = true;
    }

    /// Roll the textbox back one edit. Returns true iff there was
    /// something to undo. The current state is pushed onto the redo
    /// stack so a subsequent `redo()` can replay it.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        let cur = self.snapshot();
        self.restore(prev);
        self.redo_stack.push(cur);
        self.coalesce_break = true;
        true
    }

    /// Replay one undone edit. Returns true iff there was a redo entry.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        let cur = self.snapshot();
        self.restore(next);
        self.undo_stack.push(cur);
        self.coalesce_break = true;
        true
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
        // Tag spans use the same closed-bound gravity as links: typing
        // adjacent to a tag stays outside it; edits that erase the
        // tag's text leave a degenerate range and drop. If the user
        // backspaces away the leading `#`, the span becomes degenerate
        // by failing the `starts_with('#')` revalidation below.
        self.tags.retain_mut(|tag| {
            tag.range.start = transform_index(tag.range.start, start, del, ins);
            tag.range.end = transform_index_closed_end(tag.range.end, start, del, ins);
            tag.range.start < tag.range.end
                && tag.range.end <= self.text.len()
                && self.text[tag.range.clone()].starts_with('#')
        });
        self.rewrap();
    }

    fn rewrap(&mut self) {
        if self.body_lines_width.is_nan() {
            self.body_lines.clear();
            self.line_is_heading.clear();
            self.line_is_comment.clear();
            self.line_extra_below.clear();
            return;
        }
        let body_font = self.body_font();
        let heading_font = self.heading_font();
        let paint = Paint::default();
        let (lines, headings, comments) = wrap_text_styled(
            &self.text,
            &body_font,
            &heading_font,
            &paint,
            self.body_lines_width,
            self.force_heading,
            self.enable_comment_coloring,
        );
        self.body_lines = lines;
        self.line_is_heading = headings;
        self.line_is_comment = comments;
    }
}

/// Resolve a single-vs-double-vs-triple-click into a selection range and
/// the drag mode the caller should track. Lives here (rather than in
/// `wrap`) because it returns the textbox-internal `DragKind`.
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

/// Find every `http://` and `https://` URL in `s`. Each URL extends from
/// the scheme through the first whitespace (or end of string), with
/// trailing punctuation that's commonly NOT part of URLs (`.,;:!?])}`)
/// peeled off — so "see https://example.com." linkifies the URL without
/// the trailing period. Returns `(byte_range_in_s, url_string)` pairs.
fn detect_urls(s: &str) -> Vec<(Range<usize>, String)> {
    let mut out: Vec<(Range<usize>, String)> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let prefix_len = if s[i..].starts_with("https://") {
            8
        } else if s[i..].starts_with("http://") {
            7
        } else {
            i += s[i..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            continue;
        };
        let scheme_start = i;
        let body_start = i + prefix_len;
        let mut end = body_start;
        for c in s[body_start..].chars() {
            if c.is_whitespace() {
                break;
            }
            end += c.len_utf8();
        }
        // Peel trailing punctuation that's almost never part of a URL.
        while end > body_start {
            let last = s[..end].chars().last().unwrap();
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}') {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if end > body_start {
            let url = s[scheme_start..end].to_string();
            out.push((scheme_start..end, url));
            i = end;
        } else {
            // Bare "http://" with nothing after: skip past the scheme.
            i = body_start;
        }
    }
    out
}
