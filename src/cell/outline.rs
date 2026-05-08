//! `OutlineCell` — a flat list of bullets with explicit depth. Display
//! order is the list order; tree shape is implicit (a bullet's children
//! are the contiguous run of subsequent bullets at depth > self).

use std::collections::HashSet;
use std::ops::Range;

use skia_safe::{Canvas, Font, Paint, Rect, Typeface};
use uuid::Uuid;
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

use super::TextBox;
use super::common::{BODY_FONT_SIZE, TextBoxSnapshot, primary_mod, word_mod};

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

    /// Drain any pending link URL the bullet's textbox stashed during
    /// the most recent `mouse_down`. Used by the `Cell` aggregator that
    /// feeds `KeptApp::handle_link_click`.
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        self.textbox.take_pending_link_url()
    }

    /// Drain any pending inline-tag name (mirrors
    /// `take_pending_link_url`).
    pub fn take_pending_tag_name(&mut self) -> Option<String> {
        self.textbox.take_pending_tag_name()
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
    /// Per-frame "show only these bullets" hint. Set by the app's cell-
    /// render dispatch when the active view filters by `#tag` AND this
    /// cell matches via body bullets only (title doesn't carry the tag).
    /// Cleared (None) for focused cells — interaction always operates
    /// on the full outline so navigation / hit-testing stay coherent.
    /// Not persisted; re-derived each frame from the active filter.
    bullet_filter: Option<HashSet<Uuid>>,
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
            bullet_filter: None,
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
            bullet_filter: None,
        }
    }

    pub fn bullets(&self) -> &[Bullet] {
        &self.bullets
    }

    /// Stash the per-frame bullet-visibility hint. The app calls this
    /// before render based on the active view's tag filter; tick reads
    /// it. None ⇒ render all bullets.
    pub fn set_bullet_filter(&mut self, ids: Option<HashSet<Uuid>>) {
        self.bullet_filter = ids;
    }

    /// Bullet IDs whose text contains any of `include_tags` (inline
    /// tags only — `#word` preceded by whitespace or start-of-text),
    /// plus all of their subtree descendants. Used by the app to
    /// compute the per-frame `bullet_filter` for tag-filtered views.
    /// Names compared case-insensitively to match `query::matches`.
    pub fn bullets_matching_any_tag(&self, include_tags: &[String]) -> HashSet<Uuid> {
        if include_tags.is_empty() {
            return HashSet::new();
        }
        let lowered: Vec<String> = include_tags
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let mut out: HashSet<Uuid> = HashSet::new();
        for i in 0..self.bullets.len() {
            let text = self.bullets[i].textbox.text();
            let hit = crate::cell::parse_inline_tags(text).iter().any(|r| {
                if r.end > r.start + 1 {
                    let name = text[r.start + 1..r.end].to_ascii_lowercase();
                    lowered.iter().any(|t| t == &name)
                } else {
                    false
                }
            });
            if hit {
                let root_depth = self.bullets[i].depth;
                out.insert(self.bullets[i].id);
                let mut k = i + 1;
                while k < self.bullets.len() && self.bullets[k].depth > root_depth {
                    out.insert(self.bullets[k].id);
                    k += 1;
                }
            }
        }
        out
    }

    pub fn typeface(&self) -> &Typeface {
        &self.typeface
    }

    pub fn font_scale(&self) -> f32 {
        self.font_scale
    }

    /// Drain the first pending link URL across all bullets. At most one
    /// can be set per `mouse_down` (only the clicked bullet stashes it).
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        for b in &mut self.bullets {
            if let Some(url) = b.take_pending_link_url() {
                return Some(url);
            }
        }
        None
    }

    /// Drain the first pending inline-tag name across all bullets.
    pub fn take_pending_tag_name(&mut self) -> Option<String> {
        for b in &mut self.bullets {
            if let Some(name) = b.take_pending_tag_name() {
                return Some(name);
            }
        }
        None
    }

    /// Resolve the bullet index containing the given absolute y. Clamps to
    /// first/last on out-of-bounds and on small inter-bullet gaps.
    fn bullet_idx_at_y(&self, abs_y: f32) -> usize {
        // Honor the per-frame bullet_filter: filtered-out bullets weren't
        // ticked this frame, so their textbox.y_origin() / height carry
        // stale values from a previous render. Treat them as invisible
        // for hit-testing so clicks never land on a hidden bullet.
        let visible = |i: usize| match &self.bullet_filter {
            Some(set) => set.contains(&self.bullets[i].id),
            None => true,
        };
        if let Some(idx) = (0..self.bullets.len()).find(|&i| {
            if !visible(i) {
                return false;
            }
            let b = &self.bullets[i];
            let top = b.textbox.y_origin();
            let bot = top + b.textbox.height();
            abs_y >= top && abs_y < bot
        }) {
            return idx;
        }
        if abs_y < self.y_origin {
            (0..self.bullets.len()).find(|&i| visible(i)).unwrap_or(0)
        } else {
            (0..self.bullets.len())
                .rev()
                .find(|&i| visible(i))
                .unwrap_or_else(|| self.bullets.len().saturating_sub(1))
        }
    }

    fn bullet_idx_by_id(&self, id: Uuid) -> Option<usize> {
        self.bullets.iter().position(|b| b.id == id)
    }

    /// Bullet under doc-space y (id + text). Strict: returns None when the
    /// y isn't inside any bullet's render band (e.g., the click landed
    /// above the first bullet's top edge or below the last bullet's bottom
    /// edge of the outline). Used by the right-click context menu to
    /// surface "Copy bullet sub-tree as embed."
    pub fn bullet_at_doc_y(&self, abs_y: f32) -> Option<(Uuid, String)> {
        for b in &self.bullets {
            let top = b.textbox.y_origin();
            let bot = top + b.textbox.height();
            if abs_y >= top && abs_y < bot {
                return Some((b.id, b.textbox.text().to_string()));
            }
        }
        None
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
        bullet_paint.set_color(crate::color::BULLET_MARKER);

        // Per-bullet visibility for tag-filtered views. None ⇒ all
        // bullets visible; Some ⇒ render only listed IDs, depth-
        // normalized so the shallowest visible bullet sits flush-left.
        let visible: Vec<bool> = match &self.bullet_filter {
            Some(set) => self.bullets.iter().map(|b| set.contains(&b.id)).collect(),
            None => vec![true; self.bullets.len()],
        };
        let depth_offset_subtract: u32 = if self.bullet_filter.is_some() {
            self.bullets
                .iter()
                .zip(visible.iter())
                .filter_map(|(b, v)| if *v { Some(b.depth) } else { None })
                .min()
                .unwrap_or(0)
        } else {
            0
        };

        let mut bullet_y_bands: Vec<(f32, f32)> = Vec::with_capacity(self.bullets.len());
        let suppress_caret = active_indices.is_some();
        let mut cur_y = y;
        for (idx, bullet) in self.bullets.iter_mut().enumerate() {
            if !visible[idx] {
                // Skipped (filtered out). Push a degenerate band so
                // index-keyed callers (active-overlay, hit-test) stay
                // aligned and never land on a filtered bullet.
                bullet_y_bands.push((cur_y, cur_y));
                continue;
            }
            let normalized_depth = bullet.depth.saturating_sub(depth_offset_subtract);
            let depth_offset = (normalized_depth as f32) * indent_per_level;
            let marker_x = x + depth_offset + indent_per_level / 2.0;
            let marker_y = cur_y + line_height / 2.0;
            canvas.draw_circle((marker_x, marker_y), radius, &bullet_paint);

            let text_x = x + depth_offset + indent_per_level;
            let text_w = (width - depth_offset - indent_per_level).max(40.0);
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
                hl_paint.set_color(crate::color::ACCENT_BLUE_SELECTION);
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

    /// True if a link in the bullet under `(abs_x, abs_y)` is hit.
    pub fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        if abs_y < self.y_origin || abs_y > self.y_origin + self.height {
            return false;
        }
        let idx = self.bullet_idx_at_y(abs_y);
        if idx >= self.bullets.len() {
            return false;
        }
        self.bullets[idx].textbox.link_at_doc_pos(abs_x, abs_y)
    }

    /// True if an inline `#tag` in the bullet under `(abs_x, abs_y)`
    /// is hit. Sibling of `link_at_doc_pos`.
    pub fn tag_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        if abs_y < self.y_origin || abs_y > self.y_origin + self.height {
            return false;
        }
        let idx = self.bullet_idx_at_y(abs_y);
        if idx >= self.bullets.len() {
            return false;
        }
        self.bullets[idx].textbox.tag_at_doc_pos(abs_x, abs_y)
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

    /// Plain-text replacement variant of `replace_in_bullet_with_link`,
    /// used by `#`-tag autocomplete (no URL attached).
    pub fn replace_in_bullet_with_text(
        &mut self,
        bullet_id: Uuid,
        range: Range<usize>,
        text: String,
    ) {
        if let Some(b) = self.bullets.iter_mut().find(|b| b.id == bullet_id) {
            b.textbox.replace_with_text(range, text);
        }
    }

    pub fn replace_in_bullet_with_tag(
        &mut self,
        bullet_id: Uuid,
        range: Range<usize>,
        text: String,
    ) {
        if let Some(b) = self.bullets.iter_mut().find(|b| b.id == bullet_id) {
            b.textbox.replace_with_tag(range, text);
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

    /// Move focus to the bullet with `id` if present. Returns true if the
    /// bullet was found (and focus changed); false otherwise. Used by
    /// reference-embed click navigation to land on a specific bullet.
    pub fn set_focused_bullet(&mut self, id: Uuid) -> bool {
        if self.bullet_idx_by_id(id).is_none() {
            return false;
        }
        if self.focused_bullet == id {
            return true;
        }
        self.focused_bullet = id;
        self.bullet_selection = None;
        true
    }

    /// Range of indices for the sub-tree rooted at `bullet_id`: the bullet
    /// itself plus all immediately-following bullets with strictly greater
    /// depth, until depth drops back to <= the root. Single bullet with no
    /// children → range of length 1. Used by reference-embed rendering and
    /// (eventually) Stage 2's bullet-as-reference work.
    pub fn subtree_range(&self, bullet_id: Uuid) -> Option<std::ops::Range<usize>> {
        let i = self.bullet_idx_by_id(bullet_id)?;
        let root_depth = self.bullets[i].depth;
        let mut k = i + 1;
        while k < self.bullets.len() && self.bullets[k].depth > root_depth {
            k += 1;
        }
        Some(i..k)
    }

    /// Select the sub-tree rooted at `bullet_id` — the bullet itself
    /// plus all of its descendants. Drives the same highlight + bullet-
    /// selection key handling as a multi-bullet drag-select, so any
    /// existing op (delete, indent / outdent, copy) works on the
    /// resulting range. Also moves the focused bullet to the root so
    /// keyboard nav stays inside the selection. Returns false if
    /// `bullet_id` isn't in the cell.
    pub fn select_subtree(&mut self, bullet_id: Uuid) -> bool {
        let Some(range) = self.subtree_range(bullet_id) else {
            return false;
        };
        let head_id = self.bullets[range.end - 1].id;
        self.bullet_selection = Some(BulletSelection {
            anchor_id: bullet_id,
            head_id,
        });
        self.focused_bullet = bullet_id;
        true
    }

    /// Select all text inside the focused bullet's textbox.
    pub fn select_all_in_focused(&mut self) {
        if let Some(idx) = self.focused_index() {
            self.bullets[idx].textbox.select_all();
        }
    }

    pub fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        let idx = self.focused_index()?;
        let tb = &self.bullets[idx].textbox;
        tb.primary_caret().map(|(_, h)| (tb.text(), h))
    }

    pub fn focused_textbox(&self) -> Option<&TextBox> {
        let idx = self.focused_index()?;
        Some(&self.bullets[idx].textbox)
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
        // Enter on a non-empty selection deletes the selection first,
        // then splits at the resulting caret — matching the behavior
        // every other text input has (TextBox's own Enter does the
        // same via insert_text("\n")). Without this the bullet keeps
        // the highlighted run and the split lands at its head.
        self.bullets[idx].textbox.delete_selection();
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
