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
use super::common::{BODY_FONT_SIZE, INACTIVE_ALPHA, LinkSpan, TextBoxSnapshot, primary_mod, word_mod};
use super::reference::{EmbeddedReference, ReferenceTarget};

const BULLET_INDENT: f32 = 22.0;
const BULLET_RADIUS: f32 = 3.0;

#[derive(Clone)]
pub struct BulletSnapshot {
    pub id: Uuid,
    pub textbox: TextBoxSnapshot,
    pub depth: u32,
    /// `Some(t)` when this bullet was closed at epoch ms `t`; `None`
    /// when open. Effective open state for visibility/render cascades:
    /// a bullet is effectively open iff its own `closed_at` is None AND
    /// every ancestor bullet is also effectively open — see
    /// `OutlineCell::compute_effective_open`.
    pub closed_at: Option<i64>,
    /// `Some(t)` schedules this bullet to surface in the Current view
    /// at epoch ms `t`. `None` when no resurface is scheduled.
    pub resurface_after: Option<i64>,
}

#[derive(Clone)]
pub struct OutlineSnapshot {
    pub bullets: Vec<BulletSnapshot>,
    pub focused_bullet: Uuid,
    /// Pinned read-only embed (envelope outlines only). Snapshots
    /// carry only the target — caches are session-only and rebuild
    /// lazily on the next render.
    pub reference_header: Option<ReferenceTarget>,
}

pub struct Bullet {
    id: Uuid,
    textbox: TextBox,
    depth: u32,
    /// `Some(t)` when the user has closed this bullet (subtree).
    /// `None` when open. Effective open state cascades by ancestry —
    /// `OutlineCell::compute_effective_open` returns false for any
    /// descendant of a closed bullet, regardless of the descendant's
    /// own flag. Toggled via the bullet right-click "Close/Reopen" row.
    closed_at: Option<i64>,
    /// `Some(t)` schedules this bullet's subtree to appear in the
    /// Current view at epoch ms `t`. `None` when no resurface is
    /// scheduled. Does not affect timeline visibility.
    resurface_after: Option<i64>,
}

impl Bullet {
    pub fn new(id: Uuid, textbox: TextBox, depth: u32) -> Self {
        Self {
            id,
            textbox,
            depth,
            closed_at: None,
            resurface_after: None,
        }
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

    /// Mutable accessor for the bullet's textbox. Used by Cell-level
    /// span operations (e.g. global tag-strip) that need to reach
    /// every editable surface in the cell.
    pub fn textbox_mut(&mut self) -> &mut TextBox {
        &mut self.textbox
    }

    pub fn is_open(&self) -> bool {
        self.closed_at.is_none()
    }

    pub fn closed_at(&self) -> Option<i64> {
        self.closed_at
    }

    pub fn set_closed_at(&mut self, closed_at: Option<i64>) {
        self.closed_at = closed_at;
    }

    pub fn resurface_after(&self) -> Option<i64> {
        self.resurface_after
    }

    pub fn set_resurface_after(&mut self, when: Option<i64>) {
        self.resurface_after = when;
    }

    /// Drain any pending link URL the bullet's textbox stashed during
    /// the most recent `mouse_down`. Used by the `Cell` aggregator that
    /// feeds `KeptApp::handle_link_click`. `kept-tag://<name>` URLs ride
    /// this same channel — the app routes them to the tag view.
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        self.textbox.take_pending_link_url()
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
    /// Drag started inside the envelope header's read-only embed.
    /// Forwards drag/up to the header cache; cross-zone drag (header
    /// → bullets) is unsupported in v1 — once the cursor leaves the
    /// header band the drag ends quietly.
    Header,
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
    /// Per-frame mirror of the app's global "Show archived" toggle.
    /// When false (the default), `tick` skips bullets whose
    /// effective-active state is false (the bullet's own flag, AND of
    /// its ancestors). When true, those bullets render with a
    /// dimmed alpha layer instead of being filtered out. Reset to
    /// false at the top of each tick by the app's render dispatch
    /// before the toggle is re-applied for this frame.
    show_inactive: bool,
    /// Optional read-only embed pinned above the bullets. Set by the
    /// "Envelope" action — the user wraps a Reference cell in an
    /// outline so they can write notes around it, and this slot holds
    /// the original reference target plus its preview cache. The app
    /// layer renders the embed before ticking the bullet body.
    /// `None` for normal outlines.
    reference_header: Option<EmbeddedReference>,
    /// Y-band the app layer wrote during the most recent render of
    /// the header embed (doc-space). Used to route hit-tests inside
    /// the band to the cache cell. `(0.0, 0.0)` before the first
    /// render; ignored unless `reference_header.is_some()`.
    header_y_origin: f32,
    header_height: f32,
}

impl OutlineCell {
    pub fn new(typeface: Typeface) -> Self {
        let id = Uuid::now_v7();
        let initial = Bullet {
            id,
            textbox: TextBox::new(typeface.clone(), String::new()),
            depth: 0,
            closed_at: None,
            resurface_after: None,
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
            show_inactive: false,
            reference_header: None,
            header_y_origin: 0.0,
            header_height: 0.0,
        }
    }

    /// Build an outline pre-loaded with a reference header — the
    /// "Envelope" action's seed shape. Single empty text bullet
    /// underneath; caret naturally lands there for the user to start
    /// typing notes around the embedded reference.
    pub fn with_envelope(typeface: Typeface, target: ReferenceTarget) -> Self {
        let mut oc = Self::new(typeface);
        oc.reference_header = Some(EmbeddedReference::new(target));
        oc
    }

    /// Reconstruct an `OutlineCell` from persisted bullets. Bullets must be
    /// non-empty (the OutlineCell invariant). Used by the persistence layer.
    pub fn from_bullets(typeface: Typeface, bullets: Vec<Bullet>) -> Self {
        Self::from_bullets_with_header(typeface, bullets, None)
    }

    /// Variant of `from_bullets` that also takes an optional header
    /// target — used by the persistence layer to reconstruct an
    /// envelope outline.
    pub fn from_bullets_with_header(
        typeface: Typeface,
        bullets: Vec<Bullet>,
        reference_header: Option<EmbeddedReference>,
    ) -> Self {
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
                closed_at: None,
                resurface_after: None,
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
            show_inactive: false,
            reference_header,
            header_y_origin: 0.0,
            header_height: 0.0,
        }
    }

    pub fn bullets(&self) -> &[Bullet] {
        &self.bullets
    }

    /// True iff a bullet-range selection is active (multi-bullet
    /// drag, Ctrl+Right-click subtree select, or programmatic
    /// `select_subtree`). Distinct from per-textbox text selection.
    pub fn has_bullet_selection(&self) -> bool {
        self.bullet_selection.is_some()
    }

    /// Mutable bullet slice for cell-level span operations that
    /// touch every editable surface. Most callers should reach for
    /// the existing tick / edit / split helpers instead — this is
    /// the bulk-mutate escape hatch.

    /// Borrow the optional reference header (envelope outlines).
    pub fn reference_header(&self) -> Option<&EmbeddedReference> {
        self.reference_header.as_ref()
    }

    /// Mutable borrow — used by the app layer's render dispatch to
    /// install / refresh the cache.
    pub fn reference_header_mut(&mut self) -> Option<&mut EmbeddedReference> {
        self.reference_header.as_mut()
    }

    /// True iff this outline carries an envelope header.
    pub fn has_reference_header(&self) -> bool {
        self.reference_header.is_some()
    }

    /// Doc-space `(top, bottom)` of the header band recorded by the
    /// most recent render. None when there's no header or no render
    /// has happened yet (`header_height == 0`).
    pub fn header_y_band(&self) -> Option<(f32, f32)> {
        if self.reference_header.is_none() || self.header_height <= 0.0 {
            return None;
        }
        Some((self.header_y_origin, self.header_y_origin + self.header_height))
    }

    /// Stash the header band's geometry. Called by the app layer's
    /// envelope render dispatch after laying out the embed wrapper.
    pub fn set_reference_header_geometry(&mut self, y: f32, h: f32) {
        self.header_y_origin = y;
        self.header_height = h;
    }

    /// Stash the per-frame bullet-visibility hint. The app calls this
    /// before render based on the active view's tag filter; tick reads
    /// it. None ⇒ render all bullets.
    pub fn set_bullet_filter(&mut self, ids: Option<HashSet<Uuid>>) {
        self.bullet_filter = ids;
    }

    /// Per-frame mirror of the app's global "Show archived" toggle —
    /// `tick` reads it to decide whether effective-inactive bullets
    /// are skipped (false) or rendered with a dim alpha layer (true).
    /// Reset by the app's cell-render dispatch each frame so a stale
    /// value doesn't persist after the toggle flips.
    pub fn set_show_inactive(&mut self, on: bool) {
        self.show_inactive = on;
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
    /// Envelope outlines also drain from the header cache (read-only
    /// embed) so a click on a link inside the embedded preview wakes
    /// up the same nav path as a top-level reference would.
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        if let Some(header) = self.reference_header.as_mut() {
            if let Some(cache) = header.cache_mut() {
                if let Some(url) = cache.take_pending_link_url() {
                    return Some(url);
                }
            }
        }
        for b in &mut self.bullets {
            if let Some(url) = b.take_pending_link_url() {
                return Some(url);
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
        bullet_paint.set_color(crate::color::bullet_marker());

        // Per-bullet visibility layers two independent filters:
        //  1. Tag filter — when the active view restricts to a tag,
        //     only matching bullets and their subtrees render.
        //     Depth is renormalized so the shallowest visible bullet
        //     sits flush-left (`depth_offset_subtract`).
        //  2. Active filter — effective-inactive bullets (own flag
        //     OR an ancestor's flag is false) are hidden unless the
        //     global `show_inactive` toggle is on. Depth is NOT
        //     renormalized for this case — the user is just hiding
        //     selected sub-outlines, not reframing the document.
        // A bullet renders only if BOTH pass.
        let tag_visible: Vec<bool> = match &self.bullet_filter {
            Some(set) => self.bullets.iter().map(|b| set.contains(&b.id)).collect(),
            None => vec![true; self.bullets.len()],
        };
        let depth_offset_subtract: u32 = if self.bullet_filter.is_some() {
            self.bullets
                .iter()
                .zip(tag_visible.iter())
                .filter_map(|(b, v)| if *v { Some(b.depth) } else { None })
                .min()
                .unwrap_or(0)
        } else {
            0
        };
        let effective_open = self.compute_effective_open();
        let visible: Vec<bool> = (0..self.bullets.len())
            .map(|i| tag_visible[i] && (effective_open[i] || self.show_inactive))
            .collect();

        let mut bullet_y_bands: Vec<(f32, f32)> = Vec::with_capacity(self.bullets.len());
        let suppress_caret = active_indices.is_some();
        let mut cur_y = y;
        // Bullet-range highlight is opaque — same as per-textbox
        // selection — so the bullet content has to render ON TOP of
        // the highlight, not beneath it. We do this in one pass:
        // for each bullet, `layout()` first to get the height,
        // draw the highlight if this bullet is in the active range,
        // then draw marker + `tick()` (which re-runs `layout` but is
        // a cheap no-op when width hasn't changed).
        let in_active_range = |idx: usize| -> bool {
            match active_indices {
                Some((lo, hi)) => focused && idx >= lo && idx <= hi,
                None => false,
            }
        };
        let hl_color = if show_caret {
            crate::color::text_selection_edit()
        } else {
            crate::color::text_selection_view()
        };
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
            let text_x = x + depth_offset + indent_per_level;
            let text_w = (width - depth_offset - indent_per_level).max(40.0);

            // Layout-only first so we know this bullet's height
            // before drawing its highlight underlay.
            bullet.textbox.layout(text_x, cur_y, text_w);
            let h = bullet.textbox.height();

            // Effectively-closed bullets reach this point only when
            // `show_inactive` is on (otherwise they were filtered out
            // above). Wrap their draw block — highlight + marker +
            // textbox — in an alpha layer so the dim treatment
            // composites uniformly with text, tag overdraw, and link
            // underlines. The highlight participates so the row
            // doesn't pop visually while everything else fades.
            let bullet_inactive = !effective_open[idx];
            if bullet_inactive {
                canvas.save_layer_alpha(None, INACTIVE_ALPHA as u32);
            }

            if in_active_range(idx) {
                let mut hl_paint = Paint::default();
                hl_paint.set_anti_alias(true);
                hl_paint.set_color(hl_color);
                canvas.draw_rect(Rect::new(x, cur_y, x + width, cur_y + h), &hl_paint);
            }

            canvas.draw_circle((marker_x, marker_y), radius, &bullet_paint);

            let is_focused_bullet = focused && bullet.id == self.focused_bullet;
            // Selection (highlight) for the active bullet whenever the cell is
            // focused. Caret only when also editing.
            let bullet_focused = is_focused_bullet && !suppress_caret;
            let bullet_show_caret = show_caret && !suppress_caret && bullet.id == self.focused_bullet;
            // `tick` re-runs `layout` internally; the second call is
            // a no-op (cached lines reused when width hasn't
            // changed) so the prelayout above is essentially free.
            let drawn_h = bullet.textbox.tick(
                canvas, text_x, cur_y, text_w, bullet_focused, bullet_show_caret,
            );
            if bullet_inactive {
                canvas.restore();
            }
            bullet_y_bands.push((cur_y, cur_y + drawn_h));
            cur_y += drawn_h;
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
                Key::Named(NamedKey::Enter)
                    if !primary_mod(mods) && !mods.alt_key() && !mods.shift_key() =>
                {
                    // Enter replaces the selection like a printable
                    // character does: drop the selected bullets,
                    // then run the normal "Enter splits the focused
                    // bullet at the caret" path. After delete the
                    // caret is at the end of the surviving previous
                    // bullet, so the split produces an empty new
                    // bullet right after it.
                    self.delete_bullet_selection();
                    return self.split_focused();
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
                    // Typing a printable character replaces the
                    // bullet selection — same shape as typing over
                    // a text selection inside a textbox. We delete
                    // the selected bullets (which leaves the caret
                    // on the surviving neighbor) and forward the
                    // keystroke so the typed char lands there.
                    // Anything that isn't a printable keystroke
                    // (shortcuts, function keys, etc.) just clears
                    // the selection.
                    let typed = !primary_mod(mods)
                        && event.text.as_deref().map_or(false, |s| {
                            !s.is_empty() && s.chars().any(|c| !c.is_control())
                        });
                    if typed {
                        self.delete_bullet_selection();
                        let focused_id = self.focused_bullet;
                        if let Some(bullet) =
                            self.bullets.iter_mut().find(|b| b.id == focused_id)
                        {
                            bullet.textbox.handle_key(event, modifiers);
                        }
                        return true;
                    }
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

        // App-wide "only one thing is selected at a time" within
        // this cell: every click retires every text-selection that
        // ISN'T on the target. The target's own selection state is
        // preserved so shift-click still extends it.
        if self.point_in_header_band(abs_y) {
            // Envelope header: forward to the read-only cache for
            // selection drag (same shape as a top-level Reference
            // cell). origin_id is a sentinel — it's never matched
            // against a bullet — and the mode flag drives drag_to /
            // mouse_up routing. Bullets aren't the target → wipe
            // every bullet textbox's selection.
            for b in &mut self.bullets {
                b.textbox.clear_selection();
            }
            if let Some(header) = self.reference_header.as_mut() {
                if let Some(cache) = header.cache_mut() {
                    cache.mouse_down(abs_x, abs_y, modifiers, false);
                }
            }
            self.drag = Some(OutlineDrag {
                origin_id: Uuid::nil(),
                mode: DragMode::Header,
            });
            return true;
        }

        // Click landed in a bullet. Retire selections on every
        // *other* surface — the envelope header cache (recursively
        // clears its own title + body) and every other bullet's
        // textbox — so only the clicked bullet holds a selection
        // after this.
        if let Some(header) = self.reference_header.as_mut() {
            if let Some(cache) = header.cache_mut() {
                cache.clear_all_selections();
            }
        }
        let idx = self.bullet_idx_at_y(abs_y);
        if idx >= self.bullets.len() {
            return false;
        }
        for (i, b) in self.bullets.iter_mut().enumerate() {
            if i != idx {
                b.textbox.clear_selection();
            }
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
        if matches!(drag.mode, DragMode::Header) {
            return self
                .reference_header
                .as_mut()
                .and_then(|h| h.cache_mut())
                .map(|cache| cache.mouse_drag_to(abs_x, abs_y))
                .unwrap_or(false);
        }
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
            DragMode::Header => self
                .reference_header
                .as_mut()
                .and_then(|h| h.cache_mut())
                .map(|cache| cache.mouse_up())
                .unwrap_or(false),
        }
    }

    /// Bullets-only emptiness check. Envelope outlines are NEVER
    /// considered empty — the embedded reference is the cell's anchor
    /// even when the user hasn't typed anything yet, and the idle
    /// empty-cell flush in the app layer would otherwise discard a
    /// freshly-created envelope before the user can use it.
    pub fn is_empty(&self) -> bool {
        if self.reference_header.is_some() {
            return false;
        }
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

    /// True if a link in the bullet under `(abs_x, abs_y)` (or the
    /// header cache, for envelope outlines) is hit.
    pub fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        if self.point_in_header_band(abs_y) {
            return self
                .reference_header
                .as_ref()
                .and_then(|h| h.cache_ref())
                .map(|cache| cache.link_at_doc_pos(abs_x, abs_y))
                .unwrap_or(false);
        }
        if abs_y < self.y_origin || abs_y > self.y_origin + self.height {
            return false;
        }
        let idx = self.bullet_idx_at_y(abs_y);
        if idx >= self.bullets.len() {
            return false;
        }
        self.bullets[idx].textbox.link_at_doc_pos(abs_x, abs_y)
    }

    /// True iff `abs_y` falls inside the rendered header band (only
    /// possible on envelope outlines after at least one render pass).
    pub fn point_in_header_band(&self, abs_y: f32) -> bool {
        match self.header_y_band() {
            Some((top, bot)) => abs_y >= top && abs_y < bot,
            None => false,
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

    /// Insert a list of pasted bullets as siblings AFTER the
    /// focused bullet, at the focused bullet's depth. Each input
    /// tuple is `(rel_depth, text, links)` where `rel_depth` is
    /// the bullet's depth in the original copied set. The minimum
    /// of those becomes the focused bullet's depth — i.e. the
    /// pasted top-level joins at the focused depth, nested items
    /// keep their relative offset.
    ///
    /// If a bullet selection is active, it's deleted first so the
    /// paste replaces the selection. Empty input is a no-op.
    /// Focus moves to the first inserted bullet so subsequent
    /// keystrokes land inside the paste.
    pub fn insert_bullets_after_focused(
        &mut self,
        bullets: Vec<(u32, String, Vec<crate::cell::LinkSpan>)>,
    ) {
        if bullets.is_empty() {
            return;
        }
        if self.bullet_selection.is_some() {
            self.delete_bullet_selection();
        }
        let Some(insert_at) = self.focused_index() else {
            return;
        };
        let focused_depth = self.bullets[insert_at].depth;
        let min_rel = bullets.iter().map(|(d, _, _)| *d).min().unwrap_or(0);
        let scale = self.font_scale;
        let mut new_focus: Option<Uuid> = None;
        for (offset, (rel_depth, text, links)) in bullets.into_iter().enumerate() {
            let abs_depth = focused_depth + (rel_depth - min_rel);
            let id = Uuid::now_v7();
            let mut tb = TextBox::new(self.typeface.clone(), String::new());
            tb.append_with_links(&text, links);
            tb.set_font_scale(scale);
            tb.set_caret_at(text.len());
            self.bullets.insert(
                insert_at + 1 + offset,
                Bullet {
                    id,
                    textbox: tb,
                    depth: abs_depth,
                    closed_at: None,
                    resurface_after: None,
                },
            );
            if new_focus.is_none() {
                new_focus = Some(id);
            }
        }
        if let Some(id) = new_focus {
            self.focused_bullet = id;
        }
    }

    /// Bullet-selection copy, but returning `(rel_depth, text, links)`
    /// for each selected bullet so the clipboard layer can build a
    /// structured `KeptPayload::Outline`. Depths are rebased so the
    /// shallowest selected bullet is 0; nested items keep their
    /// relative offset. Returns None when no bullet selection is
    /// active (caller falls back to TextBox-selection copy).
    pub fn copy_bullet_selection_with_links(
        &self,
    ) -> Option<Vec<(u32, String, Vec<LinkSpan>)>> {
        let sel = self.bullet_selection?;
        let ai = self.bullet_idx_by_id(sel.anchor_id)?;
        let hi = self.bullet_idx_by_id(sel.head_id)?;
        let lo = ai.min(hi);
        let hi = ai.max(hi);
        let min_depth = (lo..=hi).map(|i| self.bullets[i].depth).min().unwrap_or(0);
        let mut out = Vec::with_capacity(hi - lo + 1);
        for i in lo..=hi {
            let b = &self.bullets[i];
            let rel = b.depth - min_depth;
            let text = b.textbox.text().to_string();
            let links: Vec<LinkSpan> = b.textbox.links().to_vec();
            out.push((rel, text, links));
        }
        Some(out)
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

    /// Compute the effective open state for every bullet, parallel to
    /// `bullets()`. A bullet is *effectively* open iff its own
    /// `closed_at` is None AND every ancestor (any bullet at strictly
    /// lower depth that precedes this one in the flat list, up to
    /// depth 0) is open. Walks the outline once with a per-depth
    /// "lowest closed ancestor" tracker so a single closed bullet at
    /// depth `d` makes its whole subtree effectively closed — even if
    /// the descendants' own flags say open. Used by render to skip /
    /// dim, and by the bullet visibility gate.
    pub fn compute_effective_open(&self) -> Vec<bool> {
        let mut effective = Vec::with_capacity(self.bullets.len());
        // Stack of (depth, is_open) pairs for the current ancestor
        // chain. The chain shrinks back when we return to a shallower
        // depth and grows when we descend.
        let mut chain: Vec<(u32, bool)> = Vec::new();
        for b in &self.bullets {
            while let Some(&(d, _)) = chain.last() {
                if d >= b.depth {
                    chain.pop();
                } else {
                    break;
                }
            }
            let ancestors_open = chain.iter().all(|&(_, a)| a);
            let self_open = b.is_open();
            let eff = self_open && ancestors_open;
            effective.push(eff);
            chain.push((b.depth, self_open));
        }
        effective
    }

    /// Look up a bullet's index by id. Used by
    /// `set_bullet_closed_at` (and the right-click toggle dispatch) so
    /// callers don't need to scan the bullet list themselves. Returns
    /// None if the id isn't in the cell.
    pub fn bullet_idx(&self, bullet_id: Uuid) -> Option<usize> {
        self.bullets.iter().position(|b| b.id == bullet_id)
    }

    /// Mutate a bullet's `closed_at` flag. Returns whether the bullet
    /// exists and the flag actually changed.
    pub fn set_bullet_closed_at(&mut self, bullet_id: Uuid, closed_at: Option<i64>) -> bool {
        let Some(i) = self.bullet_idx(bullet_id) else {
            return false;
        };
        if self.bullets[i].closed_at == closed_at {
            return false;
        }
        self.bullets[i].closed_at = closed_at;
        true
    }

    /// Mutate a bullet's `resurface_after` field. Returns whether the
    /// bullet exists and the field actually changed.
    pub fn set_bullet_resurface_after(&mut self, bullet_id: Uuid, when: Option<i64>) -> bool {
        let Some(i) = self.bullet_idx(bullet_id) else {
            return false;
        };
        if self.bullets[i].resurface_after == when {
            return false;
        }
        self.bullets[i].resurface_after = when;
        true
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

    /// Clear every visible selection inside this outline: drops the
    /// bullet sub-tree highlight, collapses any text drag-select on
    /// each bullet, and recursively clears the envelope header's
    /// embedded cache (so a stale highlight inside an embed doesn't
    /// linger when focus moves away). Used by the app-level mouse_down
    /// dispatch to retire highlights on cells that aren't the click
    /// target.
    pub fn clear_all_selections(&mut self) {
        self.bullet_selection = None;
        for b in &mut self.bullets {
            b.textbox.clear_selection();
        }
        if let Some(h) = self.reference_header.as_mut() {
            if let Some(cache) = h.cache_mut() {
                cache.clear_all_selections();
            }
        }
    }

    /// Two-step Ctrl+A:
    /// 1. First press → select all text inside the focused
    ///    bullet's textbox.
    /// 2. Second press (focused bullet's text already fully
    ///    selected, OR a bullet-range selection is already active
    ///    but doesn't cover everything) → escalate to a
    ///    bullet-range selection covering every bullet in the
    ///    outline. Text-level selections are wiped so the
    ///    single-selection invariant holds.
    /// Empty focused bullets skip step 1 (nothing to select) and
    /// go straight to step 2 on the first press, which makes
    /// "Ctrl+A on an empty bullet" usefully select the whole
    /// outline. When the bullet-range already covers everything
    /// the call is a no-op.
    pub fn select_all_in_focused(&mut self) {
        if self.bullet_selection_covers_all() {
            return;
        }
        let escalate = self.bullet_selection.is_some() || {
            let Some(idx) = self.focused_index() else { return };
            let tb = &self.bullets[idx].textbox;
            let len = tb.text().len();
            let text_fully_selected = match tb.primary_caret() {
                Some((a, h)) if len > 0 => a.min(h) == 0 && a.max(h) == len,
                _ => false,
            };
            text_fully_selected || tb.text().is_empty()
        };
        if escalate {
            for b in &mut self.bullets {
                b.textbox.clear_selection();
            }
            let (Some(first), Some(last)) = (
                self.bullets.first().map(|b| b.id),
                self.bullets.last().map(|b| b.id),
            ) else {
                return;
            };
            self.bullet_selection = Some(BulletSelection {
                anchor_id: first,
                head_id: last,
            });
            return;
        }
        if let Some(idx) = self.focused_index() {
            self.bullets[idx].textbox.select_all();
        }
    }

    /// True iff `bullet_selection` is set and spans the first
    /// bullet through the last (regardless of anchor/head order).
    fn bullet_selection_covers_all(&self) -> bool {
        let Some(sel) = self.bullet_selection else { return false };
        let pos_of = |id: Uuid| self.bullets.iter().position(|b| b.id == id);
        let Some(a) = pos_of(sel.anchor_id) else { return false };
        let Some(h) = pos_of(sel.head_id) else { return false };
        let lo = a.min(h);
        let hi = a.max(h);
        lo == 0 && hi + 1 == self.bullets.len()
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
                    closed_at: b.closed_at,
                    resurface_after: b.resurface_after,
                })
                .collect(),
            focused_bullet: self.focused_bullet,
            reference_header: self.reference_header.as_ref().map(|h| h.target()),
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
                    closed_at: bs.closed_at,
                    resurface_after: bs.resurface_after,
                }
            })
            .collect();
        self.focused_bullet = snap.focused_bullet;
        self.drag = None;
        self.bullet_selection = None;
        // Restore the header from the snapshot's target (cache is
        // session-only and rebuilds lazily). Reset the cached
        // geometry so the next render writes fresh values.
        self.reference_header = snap.reference_header.map(EmbeddedReference::new);
        self.header_y_origin = 0.0;
        self.header_height = 0.0;
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
                // New bullets start open with no resurface schedule.
                // Effective-open cascade (via ancestry) still
                // applies, so a new bullet under a closed parent
                // renders as effectively-closed without forcing the
                // per-bullet flag down here.
                closed_at: None,
                resurface_after: None,
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
                closed_at: None,
                resurface_after: None,
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

impl super::body::CellBody for OutlineCell {
    fn tick(
        &mut self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32 {
        OutlineCell::tick(self, canvas, x, y, width, focused, show_caret)
    }

    fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        OutlineCell::handle_key(self, event, modifiers)
    }

    fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
        OutlineCell::mouse_down(self, abs_x, abs_y, modifiers, editing)
    }

    /// Iterates the bullets' textboxes only. Does NOT descend into the
    /// `reference_header` cache — that cache is owned by an
    /// `EmbeddedReference` and the methods that should see it (e.g.
    /// `take_pending_link_url`, `clear_all_selections`) are overridden
    /// below to dispatch the header first, then fall through.
    fn for_each_textbox(&self, f: &mut dyn FnMut(&TextBox)) {
        for b in &self.bullets {
            f(b.textbox());
        }
    }

    fn for_each_textbox_mut(&mut self, f: &mut dyn FnMut(&mut TextBox)) {
        for b in &mut self.bullets {
            f(b.textbox_mut());
        }
    }

    fn typeface(&self) -> &Typeface {
        OutlineCell::typeface(self)
    }

    fn font_scale(&self) -> f32 {
        OutlineCell::font_scale(self)
    }

    fn set_font_scale(&mut self, scale: f32) {
        OutlineCell::set_font_scale(self, scale);
    }

    fn is_empty(&self) -> bool {
        OutlineCell::is_empty(self)
    }

    fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        OutlineCell::caret_doc_y_band(self)
    }

    fn at_top_edge(&self) -> bool {
        OutlineCell::at_top_edge(self)
    }

    fn at_bottom_edge(&self) -> bool {
        OutlineCell::at_bottom_edge(self)
    }

    fn place_caret_at_start(&mut self) {
        OutlineCell::place_caret_at_start(self);
    }

    fn place_caret_at_end(&mut self) {
        OutlineCell::place_caret_at_end(self);
    }

    fn select_all_focused(&mut self) {
        self.select_all_in_focused();
    }

    fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        OutlineCell::focused_text_and_caret(self)
    }

    fn focused_textbox(&self) -> Option<&TextBox> {
        OutlineCell::focused_textbox(self)
    }

    fn focused_bullet_id(&self) -> Option<Uuid> {
        Some(OutlineCell::focused_bullet_id(self))
    }

    fn anchor_doc_pos_at_focused(&self, byte: usize) -> Option<(f32, f32)> {
        let bid = OutlineCell::focused_bullet_id(self);
        OutlineCell::anchor_doc_pos(self, bid, byte)
    }

    fn copy_text(&self) -> String {
        OutlineCell::copy_text(self)
    }

    fn cut_text(&mut self) -> String {
        OutlineCell::cut_text(self)
    }

    fn paste_text(&mut self, s: &str) {
        OutlineCell::paste_text(self, s);
    }

    fn paste_with_links(
        &mut self,
        text: &str,
        links: &[super::common::LinkSpan],
    ) {
        if self.bullet_selection.is_some() {
            self.delete_bullet_selection();
        }
        if let Some(idx) = self.focused_index() {
            self.bullets[idx].textbox.paste_with_links(text, links);
        }
    }

    fn replace_at_focused_with_link(
        &mut self,
        range: Range<usize>,
        text: String,
        url: String,
    ) {
        let bid = OutlineCell::focused_bullet_id(self);
        self.replace_in_bullet_with_link(bid, range, text, url);
    }

    fn replace_at_focused_with_text(&mut self, range: Range<usize>, text: String) {
        let bid = OutlineCell::focused_bullet_id(self);
        self.replace_in_bullet_with_text(bid, range, text);
    }

    fn replace_at_focused_with_tag(&mut self, range: Range<usize>, text: String) {
        let bid = OutlineCell::focused_bullet_id(self);
        self.replace_in_bullet_with_tag(bid, range, text);
    }

    fn add_link_to_first(&mut self, range: Range<usize>, url: String) {
        OutlineCell::add_link_to_first(self, range, url);
    }

    fn full_text(&self) -> String {
        let mut out = String::new();
        for (i, b) in self.bullets.iter().enumerate() {
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

    // --- Header-aware overrides ----------------------------------------
    // Envelope outlines hold a `reference_header` whose cached cell lives
    // outside `for_each_textbox`'s domain. The methods below descend into
    // the header cache first, then fall through to the bullets via the
    // inherent impls (which already do both).

    fn take_pending_link_url(&mut self) -> Option<String> {
        OutlineCell::take_pending_link_url(self)
    }

    fn clear_all_selections(&mut self) {
        OutlineCell::clear_all_selections(self);
    }

    fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        OutlineCell::link_at_doc_pos(self, abs_x, abs_y)
    }

    /// Outline drag carries a `DragState` (TextBox / BulletRange /
    /// Header modes) with rich promotion logic when a drag crosses
    /// bullet boundaries. The trait's default (loop every textbox) is
    /// the wrong shape; route to the inherent instead.
    fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        OutlineCell::mouse_drag_to(self, abs_x, abs_y)
    }

    fn mouse_up(&mut self) -> bool {
        OutlineCell::mouse_up(self)
    }
}
