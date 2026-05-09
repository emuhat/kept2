use std::ops::Range;

use skia_safe::{Canvas, Typeface};
use uuid::Uuid;
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

mod common;
mod grid;
mod outline;
mod poppop;
mod reference;
mod table;
mod textbox;
mod wrap;

pub use common::{TagSpan, TextBoxSnapshot, now_epoch_ms};
pub use outline::{Bullet, OutlineCell, OutlineSnapshot};
#[cfg(test)]
pub use outline::BulletSnapshot;
pub use poppop::PopPopCell;
pub use reference::{EmbeddedReference, ReferenceCell, ReferenceTarget};
pub use table::{TableCell, TableSnapshot};
pub use textbox::TextBox;

pub(crate) use common::{INACTIVE_ALPHA, TITLE_BODY_GAP, open_url, primary_mod};
pub use wrap::parse_inline_tags;
use wrap::parse_heading_tags;


// ---------------------------------------------------------------------------
// Cell — the public cell type. Either a plain text editor (`TextBox`) or an
// outline cell. The container dispatches on the variant.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CellSnapshot {
    pub timestamp: i64,
    pub edited_at: i64,
    pub context_hint_id: Option<Uuid>,
    /// Snapshot of the optional title TextBox. None when the cell has no
    /// title slot.
    pub title: Option<TextBoxSnapshot>,
    pub kind: CellSnapshotKind,
    /// Active/inactive flag (the "archived" status). True by default;
    /// false when the user has marked this cell inactive via the cell
    /// context menu. Driven by `UndoOp::SetCellActive` and gated by
    /// the global `show_inactive_cells` toggle for visibility.
    pub active: bool,
}

#[derive(Clone)]
pub enum CellSnapshotKind {
    Plain(TextBoxSnapshot),
    Outline(OutlineSnapshot),
    /// PopPop cells share input shape with Plain — a single TextBox — so the
    /// snapshot is identical. The PopPop variant exists so restore can
    /// re-attach to a PopPop cell rather than collapsing it back to Plain.
    PopPop(TextBoxSnapshot),
    Table(TableSnapshot),
    /// Reference cells store only the target pointer. Restore recreates the
    /// pointer; live-vs-dangling state is determined fresh at render time.
    Reference(ReferenceTarget),
}

impl CellSnapshot {
    /// Document content equality (ignores selection + timestamp state). Used
    /// by undo to detect "cursor moved but text didn't change" events that
    /// shouldn't record a new undo entry.
    pub fn doc_eq(&self, other: &Self) -> bool {
        let title_eq = match (&self.title, &other.title) {
            (None, None) => true,
            (Some(a), Some(b)) => a.text == b.text,
            _ => false,
        };
        if !title_eq {
            return false;
        }
        match (&self.kind, &other.kind) {
            (CellSnapshotKind::Plain(a), CellSnapshotKind::Plain(b)) => a.text == b.text,
            (CellSnapshotKind::Outline(a), CellSnapshotKind::Outline(b)) => {
                a.reference_header == b.reference_header
                    && a.bullets.len() == b.bullets.len()
                    && a.bullets.iter().zip(b.bullets.iter()).all(|(x, y)| {
                        x.depth == y.depth && x.textbox.text == y.textbox.text
                    })
            }
            (CellSnapshotKind::PopPop(a), CellSnapshotKind::PopPop(b)) => a.text == b.text,
            (CellSnapshotKind::Table(a), CellSnapshotKind::Table(b)) => {
                a.cells.len() == b.cells.len()
                    && a.cells.iter().zip(b.cells.iter()).all(|(ar, br)| {
                        ar.len() == br.len()
                            && ar.iter().zip(br.iter()).all(|(ae, be)| {
                                ae.readonly == be.readonly && ae.textbox.text == be.textbox.text
                            })
                    })
            }
            _ => false,
        }
    }

}

pub struct Cell {
    pub id: Uuid,
    pub kind: CellKind,
    /// Optional title TextBox rendered above the body. Created via Ctrl+H,
    /// rendered with `force_heading=true` (heading font, trailing #tags
    /// styled). Tag indexing keys off this field exclusively. None means
    /// "no title slot" — body has no auto-heading anymore.
    title: Option<TextBox>,
    /// When true, keystrokes / caret / selection live on the `title`
    /// TextBox; when false, they go to the kind-specific body element.
    /// Meaningless when `title` is None.
    pub title_focused: bool,
    /// Cell-level geometry recorded by `tick`. Spans the title slot (when
    /// present) plus the body, so focus rings, hit-tests, and cell
    /// separators see the cell as one unit. Zero before the first tick.
    cell_x: f32,
    cell_y: f32,
    cell_w: f32,
    cell_h: f32,
    /// Stream position. Set once at creation; never moves.
    pub timestamp: i64,
    /// Bumped on any content change. Display-only metadata.
    pub edited_at: i64,
    /// Optional hint about which context the cell was created in.
    /// Does NOT determine visibility — that's purely timestamp-based.
    pub context_hint_id: Option<Uuid>,
    /// "Archived" flag. True by default; set false via the cell
    /// context menu's "Mark inactive" row. Inactive cells are
    /// hidden from views (timeline, sidebar dates, search, entity
    /// references) unless the global `show_inactive_cells` toggle is
    /// on, in which case they render dimmed. Persisted in the cell
    /// JSON; visibility/render is gated entirely at the app layer
    /// (this struct just carries the flag).
    pub active: bool,
}

pub enum CellKind {
    Plain(TextBox),
    Outline(OutlineCell),
    PopPop(PopPopCell),
    Table(TableCell),
    /// Read-only embed of another cell or bullet sub-tree. Has no editable
    /// content of its own; click navigates to the target.
    Reference(ReferenceCell),
}

impl CellKind {
    /// Build a fresh `CellKind` mirroring `self`'s text + links + per-row
    /// flags, using `typeface` for the new widgets and `scale` as the
    /// font scale. Returns `None` for `Reference` — references can't
    /// target other references, so a Reference-typed source means the
    /// embed has nothing renderable and the caller surfaces a
    /// placeholder.
    ///
    /// Used by the reference-embed cache: each reference cell needs its
    /// own deep clone of the target's content so it can render and accept
    /// selection-only mouse input without touching the original.
    pub fn clone_for_scale(&self, typeface: &Typeface, scale: f32) -> Option<CellKind> {
        match self {
            CellKind::Plain(tb) => {
                let new_tb = tb.clone_for_cache(typeface.clone(), scale);
                Some(CellKind::Plain(new_tb))
            }
            CellKind::Outline(oc) => {
                let bullets: Vec<Bullet> = oc
                    .bullets()
                    .iter()
                    .map(|b| {
                        let tb = b.textbox().clone_for_cache(typeface.clone(), scale);
                        Bullet::new(b.id(), tb, b.depth())
                    })
                    .collect();
                // Envelope outlines preserve their header in the cache
                // so recursive embed rendering can resolve nested
                // references. Carry the target only — the nested cache
                // rebuilds lazily via the staleness check on the next
                // render pass (and is depth-capped in
                // `build_reference_cache`).
                let header = oc
                    .reference_header()
                    .map(|h| EmbeddedReference::new(h.target()));
                let mut new_oc =
                    OutlineCell::from_bullets_with_header(typeface.clone(), bullets, header);
                new_oc.set_font_scale(scale);
                Some(CellKind::Outline(new_oc))
            }
            CellKind::PopPop(pc) => {
                let mut new_pc = PopPopCell::new(typeface.clone());
                new_pc.set_font_scale(scale);
                new_pc.restore(pc.snapshot());
                Some(CellKind::PopPop(new_pc))
            }
            CellKind::Table(tc) => {
                let triples: Vec<Vec<(String, Vec<(Range<usize>, String)>, bool)>> = tc
                    .rows_view()
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|e| {
                                let text = e.textbox.text().to_string();
                                let links: Vec<(Range<usize>, String)> = e
                                    .textbox
                                    .links()
                                    .iter()
                                    .map(|l| (l.range.clone(), l.url.clone()))
                                    .collect();
                                (text, links, e.readonly)
                            })
                            .collect()
                    })
                    .collect();
                let mut new_tc = TableCell::from_records(typeface.clone(), triples);
                new_tc.set_font_scale(scale);
                Some(CellKind::Table(new_tc))
            }
            CellKind::Reference(_) => None,
        }
    }
}

impl Cell {
    pub fn new(typeface: Typeface, initial_text: String) -> Self {
        let now = now_epoch_ms();
        Self {
            id: Uuid::now_v7(),
            kind: CellKind::Plain(TextBox::new(typeface, initial_text)),
            title: None,
            title_focused: false,
            cell_x: 0.0,
            cell_y: 0.0,
            cell_w: 0.0,
            cell_h: 0.0,
            timestamp: now,
            edited_at: now,
            context_hint_id: None,
            active: true,
        }
    }

    pub fn new_outline(typeface: Typeface) -> Self {
        let now = now_epoch_ms();
        Self {
            id: Uuid::now_v7(),
            kind: CellKind::Outline(OutlineCell::new(typeface)),
            title: None,
            title_focused: false,
            cell_x: 0.0,
            cell_y: 0.0,
            cell_w: 0.0,
            cell_h: 0.0,
            timestamp: now,
            edited_at: now,
            context_hint_id: None,
            active: true,
        }
    }

    pub fn new_poppop(typeface: Typeface) -> Self {
        let now = now_epoch_ms();
        Self {
            id: Uuid::now_v7(),
            kind: CellKind::PopPop(PopPopCell::new(typeface)),
            title: None,
            title_focused: false,
            cell_x: 0.0,
            cell_y: 0.0,
            cell_w: 0.0,
            cell_h: 0.0,
            timestamp: now,
            edited_at: now,
            context_hint_id: None,
            active: true,
        }
    }

    /// Create a reference (read-only embed) cell. Title slot is left None —
    /// references inherit identity from their target, not their own title.
    /// Typeface is unused at construction; included for symmetry with the
    /// other constructors and for any future per-reference rendering needs.
    pub fn new_reference(typeface: Typeface, target: ReferenceTarget) -> Self {
        let now = now_epoch_ms();
        Self {
            id: Uuid::now_v7(),
            kind: CellKind::Reference(ReferenceCell::new(typeface, target)),
            title: None,
            title_focused: false,
            cell_x: 0.0,
            cell_y: 0.0,
            cell_w: 0.0,
            cell_h: 0.0,
            timestamp: now,
            edited_at: now,
            context_hint_id: None,
            active: true,
        }
    }

    /// Reconstruct from raw parts (used by the persistence layer).
    pub fn from_parts(
        id: Uuid,
        kind: CellKind,
        title: Option<TextBox>,
        timestamp: i64,
        edited_at: i64,
        context_hint_id: Option<Uuid>,
        active: bool,
    ) -> Self {
        Self {
            id,
            kind,
            title,
            title_focused: false,
            cell_x: 0.0,
            cell_y: 0.0,
            cell_w: 0.0,
            cell_h: 0.0,
            timestamp,
            edited_at,
            context_hint_id,
            active,
        }
    }

    /// Lazily create the title TextBox if none. Returns a mutable reference.
    /// New title TextBoxes have `force_heading=true` and inherit the body's
    /// font scale so they match the rest of the cell visually.
    fn ensure_title(&mut self) -> &mut TextBox {
        if self.title.is_none() {
            let typeface = self.body_typeface().clone();
            let scale = self.body_font_scale();
            let mut tb = TextBox::new(typeface, String::new());
            tb.set_force_heading(true);
            tb.set_font_scale(scale);
            self.title = Some(tb);
        }
        self.title.as_mut().expect("just created")
    }

    /// Ctrl+H handler: create a title if missing and focus it; otherwise
    /// just focus the existing title. Non-destructive — does not remove an
    /// existing title or its content. Returns true if focus moved or a
    /// title was created.
    pub fn toggle_title_focus(&mut self) -> bool {
        // Reference cells are read-only — no title slot.
        if matches!(self.kind, CellKind::Reference(_)) {
            return false;
        }
        let created = self.title.is_none();
        self.ensure_title();
        if !self.title_focused || created {
            self.title_focused = true;
            return true;
        }
        false
    }

    /// Reach into the body for typeface (every CellKind owns at least one
    /// TextBox internally). Used by `ensure_title`.
    fn body_typeface(&self) -> Typeface {
        match &self.kind {
            CellKind::Plain(tb) => tb.typeface().clone(),
            CellKind::Outline(oc) => oc.typeface().clone(),
            CellKind::PopPop(pc) => pc.textbox().typeface().clone(),
            CellKind::Table(tc) => tc.typeface().clone(),
            CellKind::Reference(rc) => rc.typeface().clone(),
        }
    }

    /// Cell-wide font scale. Pulled from the body since body sets are the
    /// authoritative source via `set_font_scale`.
    fn body_font_scale(&self) -> f32 {
        match &self.kind {
            CellKind::Plain(tb) => tb.font_scale(),
            CellKind::Outline(oc) => oc.font_scale(),
            CellKind::PopPop(pc) => pc.textbox().font_scale(),
            CellKind::Table(tc) => tc.font_scale(),
            CellKind::Reference(rc) => rc.font_scale(),
        }
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
            CellSnapshotKind::PopPop(tbs) => {
                let mut pc = PopPopCell::new(typeface.clone());
                pc.restore(tbs);
                CellKind::PopPop(pc)
            }
            CellSnapshotKind::Table(ts) => {
                let mut tc = TableCell::new(typeface.clone());
                tc.restore(ts);
                CellKind::Table(tc)
            }
            CellSnapshotKind::Reference(target) => {
                CellKind::Reference(ReferenceCell::new(typeface.clone(), target))
            }
        };
        let title = snap.title.map(|tbs| {
            let mut tb = TextBox::new(typeface.clone(), String::new());
            tb.set_force_heading(true);
            tb.restore(tbs);
            tb
        });
        Self::from_parts(
            id,
            kind,
            title,
            snap.timestamp,
            snap.edited_at,
            snap.context_hint_id,
            snap.active,
        )
    }

    /// Add a link span to the cell's first textbox (plain) or first bullet
    /// (outline). Used by seed setup; future link-creation UI will go through
    /// a richer path scoped to the focused textbox.
    pub fn add_link_to_first(&mut self, range: Range<usize>, url: String) {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.add_link(range, url),
            CellKind::Outline(oc) => oc.add_link_to_first(range, url),
            CellKind::PopPop(pc) => pc.textbox_mut().add_link(range, url),
            CellKind::Table(tc) => {
                if let Some(entry) = tc.cell_at_mut(0, 0) {
                    entry.textbox.add_link(range, url);
                }
            }
            // No body text to attach a link to.
            CellKind::Reference(_) => {}
        }
    }

    /// True if document-space position `(abs_x, abs_y)` lands on a link in
    /// this cell's most recently rendered layout. Drives the hand cursor.
    pub fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        if let Some(title) = self.title.as_ref() {
            if title.link_at_doc_pos(abs_x, abs_y) {
                return true;
            }
        }
        match &self.kind {
            CellKind::Plain(tb) => tb.link_at_doc_pos(abs_x, abs_y),
            CellKind::Outline(oc) => oc.link_at_doc_pos(abs_x, abs_y),
            CellKind::PopPop(pc) => pc.link_at_doc_pos(abs_x, abs_y),
            CellKind::Table(tc) => tc.link_at_doc_pos(abs_x, abs_y),
            // Forward to the cache: links inside an embed body get the
            // hand cursor on hover, just like inline links anywhere else.
            // The reference *body itself* (outside any link span) stays
            // default — clicking blank space focuses, doesn't navigate.
            CellKind::Reference(rc) => match rc.cache_ref() {
                Some(cache) => cache.link_at_doc_pos(abs_x, abs_y),
                None => false,
            },
        }
    }

    /// True if `(abs_x, abs_y)` lands on an inline `#tag` substring in
    /// this cell. Sibling of `link_at_doc_pos` — same hand-cursor
    /// affordance, different navigation target.
    pub fn tag_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        if let Some(title) = self.title.as_ref() {
            if title.tag_at_doc_pos(abs_x, abs_y) {
                return true;
            }
        }
        match &self.kind {
            CellKind::Plain(tb) => tb.tag_at_doc_pos(abs_x, abs_y),
            CellKind::Outline(oc) => oc.tag_at_doc_pos(abs_x, abs_y),
            CellKind::PopPop(_) => false,
            CellKind::Table(tc) => tc.tag_at_doc_pos(abs_x, abs_y),
            CellKind::Reference(rc) => match rc.cache_ref() {
                Some(cache) => cache.tag_at_doc_pos(abs_x, abs_y),
                None => false,
            },
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
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.replace_with_link(range, text, url);
            }
            return;
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.replace_with_link(range, text, url),
            CellKind::Outline(oc) => {
                if let Some(bid) = bullet_id {
                    oc.replace_in_bullet_with_link(bid, range, text, url);
                }
            }
            CellKind::PopPop(pc) => pc.textbox_mut().replace_with_link(range, text, url),
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                if let Some(entry) = tc.cell_at_mut(r, c) {
                    if !entry.readonly {
                        entry.textbox.replace_with_link(range, text, url);
                    }
                }
            }
            // Reference cells have no editable text to attach a link to.
            CellKind::Reference(_) => {}
        }
    }

    /// Plain-text variant of `replace_focused_with_link` — replaces
    /// `range` in the focused slot (title, plain body, focused outline
    /// bullet, etc.) with `text`, with no link span attached. Used by
    /// `#`-tag autocomplete commit.
    pub fn replace_focused_with_text(
        &mut self,
        bullet_id: Option<Uuid>,
        range: Range<usize>,
        text: String,
    ) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.replace_with_text(range, text);
            }
            return;
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.replace_with_text(range, text),
            CellKind::Outline(oc) => {
                if let Some(bid) = bullet_id {
                    oc.replace_in_bullet_with_text(bid, range, text);
                }
            }
            CellKind::PopPop(pc) => pc.textbox_mut().replace_with_text(range, text),
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                if let Some(entry) = tc.cell_at_mut(r, c) {
                    if !entry.readonly {
                        entry.textbox.replace_with_text(range, text);
                    }
                }
            }
            CellKind::Reference(_) => {}
        }
    }

    /// Replace `range` in the focused editable slot with `text` (which
    /// must start with `#`) and mark the inserted span as a tag — the
    /// committed form of the `#`-mention popup. PopPop is excluded
    /// since `#` there is the comment marker, not a tag prefix.
    pub fn replace_focused_with_tag(
        &mut self,
        bullet_id: Option<Uuid>,
        range: Range<usize>,
        text: String,
    ) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.replace_with_tag(range, text);
            }
            return;
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.replace_with_tag(range, text),
            CellKind::Outline(oc) => {
                if let Some(bid) = bullet_id {
                    oc.replace_in_bullet_with_tag(bid, range, text);
                }
            }
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                if let Some(entry) = tc.cell_at_mut(r, c) {
                    if !entry.readonly {
                        entry.textbox.replace_with_tag(range, text);
                    }
                }
            }
            CellKind::PopPop(_) | CellKind::Reference(_) => {}
        }
    }

    pub fn copy_text(&self) -> String {
        if self.title_focused {
            if let Some(title) = self.title.as_ref() {
                let s = title.copy_primary_selection();
                if !s.is_empty() {
                    return s;
                }
            }
        }
        match &self.kind {
            CellKind::Plain(tb) => tb.copy_primary_selection(),
            CellKind::Outline(oc) => oc.copy_text(),
            CellKind::PopPop(pc) => pc.copy_selection(),
            CellKind::Table(tc) => tc.copy_selection(),
            // Forward to the cached preview so Cmd+C grabs the selection
            // the user dragged inside the embed body.
            CellKind::Reference(rc) => match rc.cache_ref() {
                Some(cache) => cache.copy_text(),
                None => String::new(),
            },
        }
    }

    /// Cell title, if any: the title slot's text with trailing #tags
    /// stripped. None when there is no title slot or the title contains
    /// only tags / whitespace.
    #[allow(dead_code)]
    pub fn heading_title(&self) -> Option<String> {
        let title_tb = self.title.as_ref()?;
        let text = title_tb.text();
        let title_end = text.find('\n').unwrap_or(text.len());
        let tags = parse_heading_tags(text, title_end);
        let bytes = text.as_bytes();
        let mut end = tags.first().map(|r| r.start).unwrap_or(title_end);
        while end > 0 && (bytes[end - 1] as char).is_whitespace() {
            end -= 1;
        }
        if end == 0 {
            return None;
        }
        Some(text[..end].to_string())
    }

    /// All distinct heading-tag names attached to this cell. Sourced from
    /// the title TextBox; cells without a title contribute no tags.
    pub fn heading_tag_names(&self) -> Vec<String> {
        match &self.title {
            Some(tb) => tb.heading_tag_names(),
            None => Vec::new(),
        }
    }

    /// All distinct tag names attached to this cell, aggregating:
    /// - **Title** trailing `#tags` (cell-level intent).
    /// - **Body** inline `#tags` — `#word` preceded by whitespace or
    ///   start-of-text. Applies to Plain text, Outline bullets, and
    ///   Table cells.
    /// - **PopPop** body is opted out (`#` is the comment-line marker).
    /// - **Reference** cells own no editable text.
    ///
    /// This is what the query matcher and the persistence layer key on,
    /// so adding `#urgent` anywhere in any editable slot puts the cell
    /// into the `#urgent` filter view.
    /// Drop every `TagSpan` whose covered substring is `#name`,
    /// across every textbox the cell owns (title + body shape's
    /// textboxes). Used by the sidebar's "Delete tag" right-click
    /// to remove the tag from any cell still carrying it. Returns
    /// whether anything changed (so the caller can mark dirty +
    /// touch `edited_at` only on actual mutations).
    pub fn remove_tags_named(&mut self, name: &str) -> bool {
        let mut changed = false;
        if let Some(title) = self.title.as_mut() {
            changed |= title.remove_tags_named(name);
        }
        match &mut self.kind {
            CellKind::Plain(tb) => changed |= tb.remove_tags_named(name),
            CellKind::Outline(oc) => {
                for b in oc.bullets_mut() {
                    changed |= b.textbox_mut().remove_tags_named(name);
                }
            }
            CellKind::Table(tc) => {
                for r in 0..tc.rows() {
                    for c in 0..tc.cols() {
                        if let Some(entry) = tc.cell_at_mut(r, c) {
                            changed |= entry.textbox.remove_tags_named(name);
                        }
                    }
                }
            }
            // PopPop body opts out of inline tags; Reference cells own
            // no editable text. Nothing to strip in either.
            CellKind::PopPop(_) | CellKind::Reference(_) => {}
        }
        changed
    }

    pub fn all_tag_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.heading_tag_names();
        let mut push = |name: String| {
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        };
        let mut absorb = |tb: &TextBox| {
            for n in tb.all_tag_names() {
                push(n);
            }
        };
        match &self.kind {
            CellKind::Plain(tb) => absorb(tb),
            CellKind::Outline(oc) => {
                for b in oc.bullets() {
                    absorb(b.textbox());
                }
            }
            CellKind::Table(tc) => {
                for r in 0..tc.rows() {
                    for c in 0..tc.cols() {
                        if let Some(entry) = tc.cell_at(r, c) {
                            absorb(&entry.textbox);
                        }
                    }
                }
            }
            CellKind::PopPop(_) | CellKind::Reference(_) => {}
        }
        out
    }

    /// True iff the focused editable slot's caret currently sits inside
    /// (or right at the end of) a `#tag` token. Used by the persistence
    /// flush to defer saving the cell while a tag is mid-edit — without
    /// this gate, every keystroke commits a new partial-name tag to the
    /// DB, which yanks the cell out of any tag-filtered view as the
    /// spelling shifts. Also gates the visibility filter so the focused
    /// cell stays on screen during the rename.
    ///
    /// Per-slot rules:
    /// - **Title**: trailing-tag rule (matches `parse_heading_tags`),
    ///   since the title's tag list is "all trailing `#tokens`".
    /// - **Plain body / Outline bullet / Table cell**: inline rule
    ///   (matches `parse_inline_tags`) — `#word` preceded by whitespace
    ///   or start-of-text counts.
    /// - **PopPop body**: always false. `#` is the comment-line marker
    ///   in PopPop and has no tag semantics.
    /// - **Reference cell**: always false. Read-only embed.
    ///
    /// Edge: caret == r.start (right before the `#`) is *not* considered
    /// in-progress — the user is positioned to type chars before the
    /// tag, which doesn't change the tag itself. caret == r.end (just
    /// after the last char) IS in-progress — the next keystroke would
    /// extend the tag.
    pub fn caret_in_in_progress_tag(&self) -> bool {
        // Span-based tags: only commit-via-popup ranges count. Caret
        // inside a TagSpan means the user is editing an existing tag
        // in place — defer save until they leave the span. Typed
        // `#X` with no span is just text and never defers (it never
        // would have made a tag in the first place).
        let in_tag = |tags: &[crate::cell::TagSpan], caret: usize| -> bool {
            tags.iter()
                .any(|t| caret > t.range.start && caret <= t.range.end)
        };
        if self.title_focused {
            let Some(title) = self.title.as_ref() else {
                return false;
            };
            let Some((_, caret)) = title.primary_caret() else {
                return false;
            };
            return in_tag(title.tags(), caret);
        }
        if matches!(&self.kind, CellKind::PopPop(_)) {
            return false;
        }
        // Body slot: locate the focused TextBox and check its tags.
        let tb: Option<&TextBox> = match &self.kind {
            CellKind::Plain(tb) => Some(tb),
            CellKind::Outline(oc) => oc.focused_textbox(),
            CellKind::Table(tc) => tc.focused_textbox(),
            CellKind::PopPop(_) | CellKind::Reference(_) => None,
        };
        let Some(tb) = tb else { return false };
        let Some((_, caret)) = tb.primary_caret() else {
            return false;
        };
        in_tag(tb.tags(), caret)
    }

    /// Full text of the cell, ignoring selection state. Title (if any) is
    /// prepended on its own line so the search popup can match against it.
    /// Outline cells join bullets with newlines (indented two spaces per
    /// depth); tables join cells with tabs and rows with newlines.
    pub fn full_text(&self) -> String {
        let body = match &self.kind {
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
            CellKind::PopPop(pc) => pc.textbox().text().to_string(),
            CellKind::Table(tc) => tc.full_text(),
            // Reference cells contribute nothing to search-indexable text;
            // search would otherwise return the same content twice (once
            // for the original cell, once for each embed).
            CellKind::Reference(_) => String::new(),
        };
        match self.title.as_ref() {
            Some(t) if !t.text().is_empty() => {
                let mut out = String::with_capacity(t.text().len() + body.len() + 1);
                out.push_str(t.text());
                out.push('\n');
                out.push_str(&body);
                out
            }
            _ => body,
        }
    }

    /// Drain the first link URL stashed by any inner `TextBox` during
    /// the most recent `mouse_down`. The app layer calls this after
    /// dispatching the click and routes the URL via `handle_link_click`.
    /// Walks title + body + nested elements in order.
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        if let Some(t) = self.title.as_mut() {
            if let Some(url) = t.take_pending_link_url() {
                return Some(url);
            }
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.take_pending_link_url(),
            CellKind::Outline(oc) => oc.take_pending_link_url(),
            CellKind::PopPop(pc) => pc.take_pending_link_url(),
            CellKind::Table(tc) => tc.take_pending_link_url(),
            // Forward to the cache so links inside an embed body are
            // clickable just like inline links anywhere else.
            CellKind::Reference(rc) => match rc.cache_mut() {
                Some(cache) => cache.take_pending_link_url(),
                None => None,
            },
        }
    }

    /// Drain the first inline-tag name (without `#`) stashed by any
    /// inner `TextBox` during the most recent `mouse_down`. The app
    /// layer routes it through `push_view(Query::tag(name))` — same
    /// destination as the sidebar tag-row click.
    pub fn take_pending_tag_name(&mut self) -> Option<String> {
        if let Some(t) = self.title.as_mut() {
            if let Some(name) = t.take_pending_tag_name() {
                return Some(name);
            }
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.take_pending_tag_name(),
            CellKind::Outline(oc) => oc.take_pending_tag_name(),
            CellKind::PopPop(_) => None,
            CellKind::Table(tc) => tc.take_pending_tag_name(),
            CellKind::Reference(rc) => match rc.cache_mut() {
                Some(cache) => cache.take_pending_tag_name(),
                None => None,
            },
        }
    }


    /// Every link URL in the cell — title (if any), body, all inner
    /// elements. Used by the query executor to resolve `kept://<uuid>`
    /// references.
    pub fn all_link_urls(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(t) = self.title.as_ref() {
            for l in t.links() {
                out.push(l.url.clone());
            }
        }
        match &self.kind {
            CellKind::Plain(tb) => {
                for l in tb.links() {
                    out.push(l.url.clone());
                }
            }
            CellKind::Outline(oc) => {
                for b in oc.bullets() {
                    for l in b.textbox().links() {
                        out.push(l.url.clone());
                    }
                }
            }
            CellKind::PopPop(pc) => {
                for l in pc.textbox().links() {
                    out.push(l.url.clone());
                }
            }
            CellKind::Table(tc) => {
                for row in tc.rows_view() {
                    for entry in row {
                        for l in entry.textbox.links() {
                            out.push(l.url.clone());
                        }
                    }
                }
            }
            // No links inside a reference embed.
            CellKind::Reference(_) => {}
        }
        out
    }

    pub fn cut_text(&mut self) -> String {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                return title.cut_primary_selection();
            }
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.cut_primary_selection(),
            CellKind::Outline(oc) => oc.cut_text(),
            CellKind::PopPop(pc) => pc.textbox_mut().cut_primary_selection(),
            CellKind::Table(tc) => tc.cut_focused(),
            CellKind::Reference(_) => String::new(),
        }
    }

    pub fn paste_text(&mut self, s: &str) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.paste(s);
                return;
            }
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.paste(s),
            CellKind::Outline(oc) => oc.paste_text(s),
            CellKind::PopPop(pc) => pc.textbox_mut().paste(s),
            CellKind::Table(tc) => tc.paste_focused(s),
            // Pasting into a reference is a no-op (read-only).
            CellKind::Reference(_) => {}
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
        // Layout/render the title slot first (if present), then a thin rule,
        // then the body. Caret + selection live on whichever side is
        // `title_focused`-gated; the other side gets focused=false so its
        // caret is suppressed.
        // An empty title that nobody's editing is just visual padding —
        // drop it so the cell collapses back to body-only.
        if !self.title_focused
            && self.title.as_ref().map(|t| t.is_empty()).unwrap_or(false)
        {
            self.title = None;
        }
        let title_focused = self.title_focused;
        let mut consumed = 0.0_f32;
        let mut body_y = y;
        if let Some(title) = self.title.as_mut() {
            let scale = title.font_scale();
            let pad = TITLE_BODY_GAP * scale;
            let title_h = title.tick(
                canvas,
                x,
                y,
                width,
                focused && title_focused,
                show_caret && title_focused,
            );
            // Just vertical breathing room between title and body — no rule.
            // The heading font on the title is cue enough; a full-width line
            // here reads as "two cells" instead of "title + body."
            let block = title_h + pad;
            consumed += block;
            body_y = y + block;
        }
        let body_focused = focused && !title_focused;
        let body_caret = show_caret && !title_focused;
        let body_h = match &mut self.kind {
            CellKind::Plain(tb) => tb.tick(canvas, x, body_y, width, body_focused, body_caret),
            CellKind::Outline(oc) => oc.tick(canvas, x, body_y, width, body_focused, body_caret),
            CellKind::PopPop(pc) => pc.tick(canvas, x, body_y, width, body_focused, body_caret),
            CellKind::Table(tc) => tc.tick(canvas, x, body_y, width, body_focused, body_caret),
            // Reference cells render via the app layer (which can see the
            // full cell list to look up the target). `Cell::tick` is never
            // called for them — see the dispatch in app.rs's cell-render
            // loop. Returning 0 here would corrupt geometry if it ever did.
            CellKind::Reference(_) => 0.0,
        };
        let total_h = consumed + body_h;
        // Record cell-level geometry so focus ring, hit-test, kebab placement,
        // and inter-cell separators see the title + body as a single unit.
        self.cell_x = x;
        self.cell_y = y;
        self.cell_w = width;
        self.cell_h = total_h;
        total_h
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        // Cross-slot transitions: ArrowUp at top of body crosses up into the
        // title; ArrowDown at bottom of title crosses down into the body.
        // Done before forwarding so the inner element doesn't see the key.
        if event.state == ElementState::Pressed && self.title.is_some() {
            let mods = modifiers.state();
            match &event.logical_key {
                Key::Named(NamedKey::ArrowUp) if !mods.shift_key() && !self.title_focused => {
                    if self.body_at_top_edge() {
                        self.title_focused = true;
                        self.place_caret_at_end_of_title();
                        return true;
                    }
                }
                Key::Named(NamedKey::ArrowDown) if !mods.shift_key() && self.title_focused => {
                    let title_at_bottom = self
                        .title
                        .as_ref()
                        .map(|t| t.at_bottom_visual_line())
                        .unwrap_or(true);
                    if title_at_bottom {
                        self.unfocus_title_drop_if_empty();
                        self.place_caret_at_start_of_body();
                        return true;
                    }
                }
                // Enter inside the title commits + drops into the body. The
                // title is single-line; newlines belong in the body.
                Key::Named(NamedKey::Enter)
                    if !mods.shift_key() && self.title_focused =>
                {
                    self.unfocus_title_drop_if_empty();
                    self.place_caret_at_start_of_body();
                    return true;
                }
                _ => {}
            }
        }

        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                return title.handle_key(event, modifiers);
            }
            // Stale title_focused with no title — fall through to body.
            self.title_focused = false;
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.handle_key(event, modifiers),
            CellKind::Outline(oc) => oc.handle_key(event, modifiers),
            CellKind::PopPop(pc) => pc.handle_key(event, modifiers),
            CellKind::Table(tc) => tc.handle_key(event, modifiers),
            // Reference cells consume nothing — keys bubble back up so the
            // app's cell-level arrow nav can step past them.
            CellKind::Reference(_) => false,
        }
    }

    pub fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
        // If the click lands in the title's vertical band, focus the title;
        // otherwise focus the body. The title's y_origin/height come from
        // the last `tick` so an unrendered cell falls through harmlessly.
        if let Some(title) = self.title.as_mut() {
            let top = title.y_origin();
            let bot = top + title.height();
            if abs_y >= top && abs_y < bot {
                self.title_focused = true;
                return title.mouse_down(abs_x, abs_y, modifiers, editing);
            }
        }
        self.unfocus_title_drop_if_empty();
        match &mut self.kind {
            CellKind::Plain(tb) => tb.mouse_down(abs_x, abs_y, modifiers, editing),
            CellKind::Outline(oc) => oc.mouse_down(abs_x, abs_y, modifiers, editing),
            CellKind::PopPop(pc) => pc.mouse_down(abs_x, abs_y, modifiers, editing),
            CellKind::Table(tc) => tc.mouse_down(abs_x, abs_y, modifiers, editing),
            // Reference cells: forward into the cached preview so
            // drag-select inside the embed works the same as in any
            // normal cell. `editing=false` regardless of the outer cell's
            // edit state — the cache is read-only by design (no caret,
            // selection only). Navigation to the source still fires on
            // Enter (handled in app.rs::handle_key).
            CellKind::Reference(rc) => {
                if let Some(cache) = rc.cache_mut() {
                    cache.mouse_down(abs_x, abs_y, modifiers, false);
                }
                true
            }
        }
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        // Forward to title too — only the textbox with an active drag responds.
        let mut any = false;
        if let Some(title) = self.title.as_mut() {
            if title.mouse_drag_to(abs_x, abs_y) {
                any = true;
            }
        }
        let body = match &mut self.kind {
            CellKind::Plain(tb) => tb.mouse_drag_to(abs_x, abs_y),
            CellKind::Outline(oc) => oc.mouse_drag_to(abs_x, abs_y),
            CellKind::PopPop(pc) => pc.mouse_drag_to(abs_x, abs_y),
            CellKind::Table(tc) => tc.mouse_drag_to(abs_x, abs_y),
            // Forward into the cached preview so drag-extend-select works.
            CellKind::Reference(rc) => match rc.cache_mut() {
                Some(cache) => cache.mouse_drag_to(abs_x, abs_y),
                None => false,
            },
        };
        any || body
    }

    pub fn mouse_up(&mut self) -> bool {
        let mut any = false;
        if let Some(title) = self.title.as_mut() {
            if title.mouse_up() {
                any = true;
            }
        }
        let body = match &mut self.kind {
            CellKind::Plain(tb) => tb.mouse_up(),
            CellKind::Outline(oc) => oc.mouse_up(),
            CellKind::PopPop(pc) => pc.mouse_up(),
            CellKind::Table(tc) => tc.mouse_up(),
            CellKind::Reference(rc) => match rc.cache_mut() {
                Some(cache) => cache.mouse_up(),
                None => false,
            },
        };
        any || body
    }

    /// True iff the body's keyboard caret is on the body's topmost visual
    /// line. Used by Cell::handle_key to detect "ArrowUp should escape into
    /// the title slot." Outline / Table delegate to their existing edge
    /// helpers which already account for inner-cell focus.
    fn body_at_top_edge(&self) -> bool {
        match &self.kind {
            CellKind::Plain(tb) => tb.at_top_visual_line(),
            CellKind::Outline(oc) => oc.at_top_edge(),
            CellKind::PopPop(pc) => pc.textbox().at_top_visual_line(),
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                r == 0
                    && tc
                        .cell_at(r, c)
                        .map(|e| e.textbox.at_top_visual_line())
                        .unwrap_or(true)
            }
            // Reference cells have no caret, so "at top edge" is always
            // true — arrow-up over a Reference cell jumps to the cell above.
            CellKind::Reference(_) => true,
        }
    }

    fn place_caret_at_end_of_title(&mut self) {
        if let Some(title) = self.title.as_mut() {
            let end = title.text().len();
            title.set_caret_at(end);
        }
    }

    /// Move focus off the title and into the body. If the title was empty,
    /// drop it entirely — an empty title slot is just visual noise.
    fn unfocus_title_drop_if_empty(&mut self) {
        self.title_focused = false;
        if self.title.as_ref().map(|t| t.is_empty()).unwrap_or(false) {
            self.title = None;
        }
    }

    fn place_caret_at_start_of_body(&mut self) {
        match &mut self.kind {
            CellKind::Plain(tb) => tb.set_caret_at(0),
            CellKind::Outline(oc) => oc.place_caret_at_start(),
            CellKind::PopPop(pc) => pc.textbox_mut().set_caret_at(0),
            CellKind::Table(tc) => {
                if let Some(entry) = tc.cell_at_mut(0, 0) {
                    entry.textbox.set_caret_at(0);
                }
            }
            // No caret to place.
            CellKind::Reference(_) => {}
        }
    }

    pub fn title(&self) -> Option<&TextBox> {
        self.title.as_ref()
    }

    pub fn title_mut(&mut self) -> Option<&mut TextBox> {
        self.title.as_mut()
    }

    #[allow(dead_code)]
    pub fn set_title(&mut self, title: Option<TextBox>) {
        self.title = title;
    }

    pub fn x_origin(&self) -> f32 {
        self.cell_x
    }

    pub fn y_origin(&self) -> f32 {
        self.cell_y
    }

    pub fn width(&self) -> f32 {
        self.cell_w
    }

    pub fn height(&self) -> f32 {
        self.cell_h
    }

    /// Record the cell-level geometry from outside `tick`. Used by the
    /// app's render-reference-cell path: reference cells don't go through
    /// `Cell::tick` (which is the only other writer), so without this
    /// `find_cell_at` would still see zero width/height and clicks would
    /// fall through.
    pub fn set_view_geometry(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.cell_x = x;
        self.cell_y = y;
        self.cell_w = width;
        self.cell_h = height;
    }

    pub fn is_empty(&self) -> bool {
        let title_empty = self.title.as_ref().map(|t| t.is_empty()).unwrap_or(true);
        let body_empty = match &self.kind {
            CellKind::Plain(tb) => tb.is_empty(),
            CellKind::Outline(oc) => oc.is_empty(),
            CellKind::PopPop(pc) => pc.textbox().is_empty(),
            CellKind::Table(tc) => tc.is_empty(),
            // Reference cells are never "empty" — they're a pointer, which
            // is content even when its target is gone.
            CellKind::Reference(_) => false,
        };
        title_empty && body_empty
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        if let Some(title) = self.title.as_mut() {
            title.set_font_scale(scale);
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.set_font_scale(scale),
            CellKind::Outline(oc) => oc.set_font_scale(scale),
            CellKind::PopPop(pc) => pc.set_font_scale(scale),
            CellKind::Table(tc) => tc.set_font_scale(scale),
            CellKind::Reference(rc) => rc.set_font_scale(scale),
        }
    }

    pub fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        if self.title_focused {
            if let Some(title) = self.title.as_ref() {
                return title.caret_doc_y_band();
            }
        }
        match &self.kind {
            CellKind::Plain(tb) => tb.caret_doc_y_band(),
            CellKind::Outline(oc) => oc.caret_doc_y_band(),
            CellKind::PopPop(pc) => pc.textbox().caret_doc_y_band(),
            CellKind::Table(tc) => tc.caret_doc_y_band(),
            CellKind::Reference(_) => None,
        }
    }

    pub fn at_top_edge(&self) -> bool {
        // Caret is at the cell's top edge iff the active focus area's caret
        // is at its own topmost line AND there's nothing above it within
        // this cell. Title above body means body is never at top edge for
        // cross-cell-nav purposes; title is always the top edge when focused.
        if self.title_focused {
            return self
                .title
                .as_ref()
                .map(|t| t.at_top_visual_line())
                .unwrap_or(true);
        }
        if self.title.is_some() {
            return false;
        }
        match &self.kind {
            CellKind::Plain(tb) => tb.at_top_visual_line(),
            CellKind::Outline(oc) => oc.at_top_edge(),
            CellKind::PopPop(pc) => pc.textbox().at_top_visual_line(),
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                r == 0
                    && tc
                        .cell_at(r, c)
                        .map(|e| e.textbox.at_top_visual_line())
                        .unwrap_or(true)
            }
            // Reference cells are always at both edges — there's no caret
            // to hold the focus, so arrow keys should immediately cross out.
            CellKind::Reference(_) => true,
        }
    }

    pub fn at_bottom_edge(&self) -> bool {
        // Title focused → body is below us, so we're not at the bottom edge.
        if self.title_focused {
            return false;
        }
        match &self.kind {
            CellKind::Plain(tb) => tb.at_bottom_visual_line(),
            CellKind::Outline(oc) => oc.at_bottom_edge(),
            CellKind::PopPop(pc) => pc.textbox().at_bottom_visual_line(),
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                r + 1 == tc.rows()
                    && tc
                        .cell_at(r, c)
                        .map(|e| e.textbox.at_bottom_visual_line())
                        .unwrap_or(true)
            }
            CellKind::Reference(_) => true,
        }
    }

    pub fn place_caret_at_start(&mut self) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.set_caret_at(0);
                return;
            }
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.set_caret_at(0),
            CellKind::Outline(oc) => oc.place_caret_at_start(),
            CellKind::PopPop(pc) => pc.textbox_mut().set_caret_at(0),
            CellKind::Table(tc) => {
                if let Some(entry) = tc.cell_at_mut(0, 0) {
                    entry.textbox.set_caret_at(0);
                }
            }
            CellKind::Reference(_) => {}
        }
    }

    pub fn place_caret_at_end(&mut self) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                let end = title.text().len();
                title.set_caret_at(end);
                return;
            }
        }
        match &mut self.kind {
            CellKind::Plain(tb) => {
                let end = tb.text().len();
                tb.set_caret_at(end);
            }
            CellKind::Outline(oc) => oc.place_caret_at_end(),
            CellKind::PopPop(pc) => {
                let end = pc.textbox().text().len();
                pc.textbox_mut().set_caret_at(end);
            }
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                if let Some(entry) = tc.cell_at_mut(r, c) {
                    let end = entry.textbox.text().len();
                    entry.textbox.set_caret_at(end);
                }
            }
            CellKind::Reference(_) => {}
        }
    }

    /// Drop every visible selection on this cell — title text drag,
    /// body text drag, outline bullet sub-tree highlight, table cell
    /// drag, and any embedded reference cache (recursively, so an
    /// envelope outline's header cache or a nested embed loses its
    /// stale highlight too). Used by the app-level mouse_down dispatch
    /// to retire highlights on cells that aren't the click target —
    /// otherwise an old highlight stays on screen and visually
    /// competes with the new selection.
    pub fn clear_all_selections(&mut self) {
        if let Some(title) = self.title.as_mut() {
            title.clear_selection();
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.clear_selection(),
            CellKind::Outline(oc) => oc.clear_all_selections(),
            CellKind::PopPop(pc) => pc.textbox_mut().clear_selection(),
            CellKind::Table(tc) => tc.clear_all_selections(),
            CellKind::Reference(rc) => {
                if let Some(cache) = rc.cache_mut() {
                    cache.clear_all_selections();
                }
            }
        }
    }

    /// Select all text in the cell's active text input — title when focused,
    /// otherwise the kind-specific body element.
    pub fn select_all_focused(&mut self) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.select_all();
                return;
            }
        }
        match &mut self.kind {
            CellKind::Plain(tb) => tb.select_all(),
            CellKind::Outline(oc) => oc.select_all_in_focused(),
            CellKind::PopPop(pc) => pc.textbox_mut().select_all(),
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                if let Some(entry) = tc.cell_at_mut(r, c) {
                    entry.textbox.select_all();
                }
            }
            CellKind::Reference(rc) => {
                if let Some(cache) = rc.cache_mut() {
                    cache.select_all_focused();
                }
            }
        }
    }

    /// `(text, caret_byte)` for the active text input — title when focused,
    /// otherwise the kind-specific body element (or focused inner element
    /// for outline / table).
    pub fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        if self.title_focused {
            if let Some(title) = self.title.as_ref() {
                return title.primary_caret().map(|(_, h)| (title.text(), h));
            }
        }
        match &self.kind {
            CellKind::Plain(tb) => tb.primary_caret().map(|(_, h)| (tb.text(), h)),
            CellKind::Outline(oc) => oc.focused_text_and_caret(),
            CellKind::PopPop(pc) => pc
                .textbox()
                .primary_caret()
                .map(|(_, h)| (pc.textbox().text(), h)),
            CellKind::Table(tc) => {
                let (r, c) = tc.focused_index();
                let entry = tc.cell_at(r, c)?;
                entry.textbox.primary_caret().map(|(_, h)| (entry.textbox.text(), h))
            }
            CellKind::Reference(_) => None,
        }
    }

    /// Outline cells: ID of the focused bullet (None when title is focused).
    /// Plain/PopPop/Table: None always.
    pub fn focused_bullet_id(&self) -> Option<Uuid> {
        if self.title_focused {
            return None;
        }
        match &self.kind {
            CellKind::Plain(_) => None,
            CellKind::Outline(oc) => Some(oc.focused_bullet_id()),
            CellKind::PopPop(_) => None,
            CellKind::Table(_) => None,
            CellKind::Reference(_) => None,
        }
    }

    /// Anchor position for an overlay tied to byte `byte` in this cell's
    /// active textbox (focused bullet for outline; title when focused).
    /// Used by the @-mention popup.
    pub fn anchor_doc_pos(
        &self,
        bullet_id: Option<Uuid>,
        byte: usize,
    ) -> Option<(f32, f32)> {
        if self.title_focused && bullet_id.is_none() {
            if let Some(title) = self.title.as_ref() {
                let (x, _) = title.doc_position_of_byte(byte)?;
                let (_, bot) = title.line_y_band_of_byte(byte)?;
                return Some((x, bot));
            }
        }
        match (&self.kind, bullet_id) {
            (CellKind::Plain(tb), None) => {
                let (x, _) = tb.doc_position_of_byte(byte)?;
                let (_, bot) = tb.line_y_band_of_byte(byte)?;
                Some((x, bot))
            }
            (CellKind::Outline(oc), Some(id)) => oc.anchor_doc_pos(id, byte),
            (CellKind::PopPop(pc), None) => {
                let (x, _) = pc.textbox().doc_position_of_byte(byte)?;
                let (_, bot) = pc.textbox().line_y_band_of_byte(byte)?;
                Some((x, bot))
            }
            (CellKind::Table(tc), None) => {
                let (r, c) = tc.focused_index();
                let entry = tc.cell_at(r, c)?;
                let (x, _) = entry.textbox.doc_position_of_byte(byte)?;
                let (_, bot) = entry.textbox.line_y_band_of_byte(byte)?;
                Some((x, bot))
            }
            _ => None,
        }
    }

    pub fn snapshot(&self) -> CellSnapshot {
        CellSnapshot {
            timestamp: self.timestamp,
            edited_at: self.edited_at,
            context_hint_id: self.context_hint_id,
            title: self.title.as_ref().map(|t| t.snapshot()),
            kind: match &self.kind {
                CellKind::Plain(tb) => CellSnapshotKind::Plain(tb.snapshot()),
                CellKind::Outline(oc) => CellSnapshotKind::Outline(oc.snapshot()),
                CellKind::PopPop(pc) => CellSnapshotKind::PopPop(pc.snapshot()),
                CellKind::Table(tc) => CellSnapshotKind::Table(tc.snapshot()),
                CellKind::Reference(rc) => CellSnapshotKind::Reference(rc.target()),
            },
            active: self.active,
        }
    }

    /// Restore from a snapshot of the same variant. Variant mismatches are a
    /// bug (undo stack and live state disagree); fall through silently rather
    /// than panic. All metadata (timestamp, edited_at, context_hint_id, the
    /// title slot, and the active flag) is preserved from the snapshot.
    pub fn restore(&mut self, snap: CellSnapshot) {
        self.timestamp = snap.timestamp;
        self.edited_at = snap.edited_at;
        self.context_hint_id = snap.context_hint_id;
        self.active = snap.active;
        self.title = snap.title.map(|tbs| {
            let typeface = self.body_typeface();
            let mut tb = TextBox::new(typeface, String::new());
            tb.set_force_heading(true);
            tb.set_font_scale(self.body_font_scale());
            tb.restore(tbs);
            tb
        });
        if self.title.is_none() {
            self.title_focused = false;
        }
        match (&mut self.kind, snap.kind) {
            (CellKind::Plain(tb), CellSnapshotKind::Plain(tbs)) => tb.restore(tbs),
            (CellKind::Outline(oc), CellSnapshotKind::Outline(os)) => oc.restore(os),
            (CellKind::PopPop(pc), CellSnapshotKind::PopPop(tbs)) => pc.restore(tbs),
            (CellKind::Table(tc), CellSnapshotKind::Table(ts)) => tc.restore(ts),
            (CellKind::Reference(rc), CellSnapshotKind::Reference(target)) => {
                rc.set_target(target);
            }
            _ => {}
        }
    }
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
    fn clone_for_cache_preserves_links_and_tags_on_textbox() {
        // The single source of truth for "deep-copy a TextBox into an
        // embed cache" must carry every render-affecting span across:
        // text, font scale, heading mode, links, AND tags. Embedded
        // titles in particular surface tags, so dropping them was the
        // bug that motivated this helper.
        let mut tb = TextBox::new(typeface(), "Patrick #person and link".to_string());
        tb.set_force_heading(true);
        tb.set_font_scale(1.25);
        tb.add_link(20..24, "https://example.com/".to_string());
        tb.add_tag(8..15);

        let cloned = tb.clone_for_cache(typeface(), 1.0);
        assert_eq!(cloned.text(), "Patrick #person and link");
        assert_eq!(cloned.font_scale(), 1.0, "scale override applies");
        let cloned_tags = cloned.tags();
        assert_eq!(cloned_tags.len(), 1, "tag span survives the clone");
        assert_eq!(cloned_tags[0].range, 8..15);
        let cloned_links = cloned.links();
        assert_eq!(cloned_links.len(), 1, "link span survives the clone");
        assert_eq!(cloned_links[0].range, 20..24);
    }

    #[test]
    fn cell_active_round_trips_through_snapshot() {
        // Toggling Cell.active is a structural edit that flows
        // through the same snapshot path the undo system uses;
        // restore() must replay the flag, not just the kind.
        let mut cell = Cell::new(typeface(), "x".to_string());
        assert!(cell.active, "new cells default to active");
        cell.active = false;

        let snap = cell.snapshot();
        assert!(!snap.active, "snapshot captures the flag");

        let mut reborn = Cell::new(typeface(), String::new());
        reborn.restore(snap);
        assert!(!reborn.active, "restore replays the flag");
    }

    #[test]
    fn bullet_active_round_trips_through_outline_snapshot() {
        let mut oc = OutlineCell::new(typeface());
        let bid = oc.bullets()[0].id();
        oc.set_bullet_active(bid, false);

        let snap = oc.snapshot();
        assert_eq!(
            snap.bullets[0].active, false,
            "BulletSnapshot carries active"
        );

        let mut reborn = OutlineCell::new(typeface());
        reborn.restore(snap);
        assert!(
            !reborn.bullets()[0].active(),
            "outline restore round-trips bullet active"
        );
    }

    #[test]
    fn effective_active_cascades_through_outline_ancestors() {
        // Build an outline at depths [0, 1, 2, 1, 2]. Mark the FIRST
        // depth-1 bullet inactive; its depth-2 child should report
        // effectively-inactive even though its own flag is active.
        // The next depth-1 sibling and its depth-2 child stay active —
        // cascade only reaches descendants of the inactive ancestor.
        let mut oc = OutlineCell::new(typeface());
        // Replace the seed bullet by snapshot construction (cleanest
        // path that keeps Bullet ids stable across builds).
        let snap_bullets = vec![
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "root".to_string()).snapshot(),
                depth: 0,
                active: true,
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "child A".to_string()).snapshot(),
                depth: 1,
                active: false, // ← this is the archived sub-outline root
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "grandchild A".to_string()).snapshot(),
                depth: 2,
                active: true,
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "child B".to_string()).snapshot(),
                depth: 1,
                active: true,
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "grandchild B".to_string()).snapshot(),
                depth: 2,
                active: true,
            },
        ];
        let snap = OutlineSnapshot {
            bullets: snap_bullets,
            focused_bullet: Uuid::now_v7(),
            reference_header: None,
        };
        oc.restore(snap);

        let eff = oc.compute_effective_active();
        assert_eq!(eff.len(), 5);
        assert!(eff[0], "root active");
        assert!(!eff[1], "self-inactive child A");
        assert!(!eff[2], "grandchild A inactive via ancestor cascade");
        assert!(eff[3], "child B unaffected");
        assert!(eff[4], "grandchild B unaffected");
    }

    #[test]
    fn envelope_outline_round_trips_through_snapshot() {
        // OutlineSnapshot carries the header target (cache is
        // session-only and rebuilds lazily). A snapshot-then-restore
        // recovers both the header and the bullet text exactly.
        let target_id = Uuid::now_v7();
        let mut oc = OutlineCell::with_envelope(
            typeface(),
            ReferenceTarget::WholeCell(target_id),
        );
        let bullet_id = oc.bullets()[0].id();
        oc.replace_in_bullet_with_text(bullet_id, 0..0, "my note".to_string());

        let snap = oc.snapshot();
        let mut reborn = OutlineCell::new(typeface());
        reborn.restore(snap);

        let header = reborn
            .reference_header()
            .expect("header restored from snapshot");
        match header.target() {
            ReferenceTarget::WholeCell(id) => assert_eq!(id, target_id),
            _ => panic!("WholeCell target should round-trip"),
        }
        assert_eq!(reborn.bullets().len(), 1);
        assert_eq!(reborn.bullets()[0].textbox().text(), "my note");
    }

    #[test]
    fn envelope_outline_is_never_empty() {
        // The empty-cell flush in the app layer would otherwise eat a
        // freshly-created envelope before the user typed anything.
        // Carrying a header target counts as content.
        let target_id = Uuid::now_v7();
        let oc = OutlineCell::with_envelope(
            typeface(),
            ReferenceTarget::WholeCell(target_id),
        );
        assert!(!oc.is_empty());
    }

    #[test]
    fn clone_for_scale_preserves_envelope_header_target() {
        // Recursive embed rendering depends on `clone_for_scale`
        // carrying the envelope's header target through to the cache
        // cell. The cache field is left empty here — `build_reference_cache`
        // populates it (recursively) during the build pass.
        let target_id = Uuid::now_v7();
        let mut oc = OutlineCell::with_envelope(
            typeface(),
            ReferenceTarget::WholeCell(target_id),
        );
        let bid = oc.bullets()[0].id();
        oc.replace_in_bullet_with_text(bid, 0..0, "envelope notes".to_string());
        let kind = CellKind::Outline(oc);

        let cloned = kind
            .clone_for_scale(&typeface(), 1.0)
            .expect("envelope outline clones");
        let CellKind::Outline(new_oc) = cloned else {
            panic!("clone_for_scale on Outline must yield Outline");
        };
        let header = new_oc
            .reference_header()
            .expect("header preserved through clone_for_scale");
        match header.target() {
            ReferenceTarget::WholeCell(id) => assert_eq!(id, target_id),
            _ => panic!("WholeCell target survives the clone"),
        }
        assert!(
            header.cache_ref().is_none(),
            "cache is left empty for `build_reference_cache` to populate"
        );
        assert_eq!(new_oc.bullets()[0].textbox().text(), "envelope notes");
    }

    #[test]
    fn envelope_snapshot_carries_target_for_unwrap() {
        // Unwrap pulls the header target off the live cell to build a
        // bare Reference with the same pointer. Independently of the
        // app layer, verify the data flow: an envelope's snapshot
        // surfaces the same target the reverse op would consume.
        let target = ReferenceTarget::Subtree {
            cell_id: Uuid::now_v7(),
            bullet_id: Uuid::now_v7(),
        };
        let mut oc = OutlineCell::with_envelope(typeface(), target);
        let bid = oc.bullets()[0].id();
        oc.replace_in_bullet_with_text(bid, 0..0, "user notes".to_string());

        let snap = oc.snapshot();
        assert_eq!(snap.reference_header, Some(target));
        // The pre-snapshot also carries the bullet text — that's
        // what makes the unwrap undoable: Ctrl+Z restores it via
        // OutlineCell::restore.
        assert_eq!(snap.bullets.len(), 1);
        assert_eq!(snap.bullets[0].textbox.text, "user notes");
    }

    #[test]
    fn outline_split_with_active_selection_replaces_it() {
        // Highlighting a word and pressing Enter should delete the
        // selection first and then split at the resulting caret —
        // matching every other text input. Pre-fix, the selected run
        // stayed and the split landed at the selection's head.
        let mut tb = TextBox::new(typeface(), "alpha BRAVO charlie".to_string());
        // Select "BRAVO" (bytes 6..11). Head at end so the caret lands
        // there after delete.
        tb.select_range(6, 11);
        let mut oc = OutlineCell::from_bullets(
            typeface(),
            vec![Bullet::new(Uuid::now_v7(), tb, 0)],
        );
        oc.split_focused_for_test();
        let bullets = oc.bullets();
        assert_eq!(bullets.len(), 2, "split produced two bullets");
        assert_eq!(
            bullets[0].textbox().text(),
            "alpha ",
            "selected word removed from prefix bullet",
        );
        assert_eq!(
            bullets[1].textbox().text(),
            " charlie",
            "suffix bullet starts where the selection ended",
        );
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
    fn paste_url_creates_link() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.paste("https://example.com");
        assert_eq!(tb.text(), "https://example.com");
        let links = tb.links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].range, 0..19);
        assert_eq!(links[0].url, "https://example.com");
    }

    #[test]
    fn paste_url_inside_text_lands_at_correct_offset() {
        let mut tb = TextBox::new(typeface(), "before  after".to_string());
        tb.set_caret_at(7); // between the two spaces
        tb.paste("see https://example.com here");
        assert_eq!(tb.text(), "before see https://example.com here after");
        let links = tb.links();
        assert_eq!(links.len(), 1);
        // "https://example.com" sits at byte 11..30 of the new text.
        assert_eq!(links[0].range, 11..30);
        assert_eq!(links[0].url, "https://example.com");
    }

    #[test]
    fn paste_url_trims_trailing_punctuation() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.paste("Visit https://example.com.");
        // Sentence period stays in the text; the link doesn't include it.
        assert_eq!(tb.text(), "Visit https://example.com.");
        let links = tb.links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].range, 6..25);
        assert_eq!(links[0].url, "https://example.com");
    }

    #[test]
    fn paste_plain_text_creates_no_links() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.paste("nothing to see here");
        assert!(tb.links().is_empty());
    }

    #[test]
    fn paste_multiple_urls_creates_multiple_links() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.paste("a https://one.com b http://two.org c");
        assert_eq!(tb.links().len(), 2);
        let urls: Vec<&str> = tb.links().iter().map(|l| l.url.as_str()).collect();
        assert!(urls.contains(&"https://one.com"));
        assert!(urls.contains(&"http://two.org"));
    }

    #[test]
    fn textbox_undo_reverts_typed_text() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.insert_text("hello");
        assert_eq!(tb.text(), "hello");
        assert!(tb.undo());
        assert_eq!(tb.text(), "");
        // Nothing left to undo.
        assert!(!tb.undo());
    }

    #[test]
    fn textbox_redo_replays_undone_edit() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.insert_text("hello");
        tb.undo();
        assert!(tb.redo());
        assert_eq!(tb.text(), "hello");
        assert!(!tb.redo());
    }

    #[test]
    fn textbox_undo_reverts_paste_including_links() {
        // The auto-linkified paste path adds LinkSpans after
        // insert_text returns. Undo must restore the pre-paste state
        // — empty text AND empty links.
        let mut tb = TextBox::new(typeface(), String::new());
        tb.paste("see https://example.com");
        assert!(!tb.text().is_empty());
        assert_eq!(tb.links().len(), 1);
        assert!(tb.undo());
        assert_eq!(tb.text(), "");
        assert!(
            tb.links().is_empty(),
            "link added during paste must vanish on undo",
        );
    }

    #[test]
    fn textbox_typed_hashtag_without_span_is_not_a_tag() {
        // The whole point of span-based tags: text alone doesn't make
        // a tag. heading_tag_names / all_tag_names / tag_at all read
        // from spans, so typed `#X` with no commit-via-popup span
        // produces nothing. Migration backfills exist for legacy
        // data, but a plain TextBox::new doesn't trigger them.
        let mut tb = TextBox::new(typeface(), "Notes #urgent".to_string());
        tb.set_force_heading(true);
        assert!(tb.heading_tag_names().is_empty());
        assert!(tb.all_tag_names().is_empty());
        assert_eq!(tb.tag_at(7), None); // inside `#urgent`
    }

    #[test]
    fn textbox_replace_with_tag_creates_span() {
        // Mirror what commit_tag_mention does: replace `#query` with
        // `#tagname` and mark it. After this, heading_tag_names
        // surfaces the new tag.
        let mut tb = TextBox::new(typeface(), "Notes #u".to_string());
        tb.set_force_heading(true);
        // `#u` lives at bytes 6..8; replace with `#urgent`.
        tb.replace_with_tag(6..8, "#urgent".to_string());
        assert_eq!(tb.text(), "Notes #urgent");
        assert_eq!(tb.tags().len(), 1);
        assert_eq!(tb.heading_tag_names(), vec!["urgent".to_string()]);
    }

    #[test]
    fn textbox_replace_with_tag_visible_immediately() {
        // Render-time tag styling reads `self.tags` directly (no
        // layout cache), so a span pushed by replace_with_tag is
        // picked up on the very next frame regardless of whether the
        // textbox's width has changed.
        let mut tb = TextBox::new(typeface(), "Notes #u".to_string());
        tb.set_force_heading(true);
        tb.tick(
            &skia_safe::surfaces::raster_n32_premul((400, 200))
                .unwrap()
                .canvas(),
            0.0,
            0.0,
            400.0,
            false,
            false,
        );
        assert!(tb.tags().is_empty());
        tb.replace_with_tag(6..8, "#urgent".to_string());
        assert_eq!(tb.tags().len(), 1);
        assert_eq!(tb.tags()[0].range, 6..13);
    }

    #[test]
    fn textbox_migrate_tags_from_text_seeds_legacy_spans() {
        // Round-trip simulator for v6→v7 backfill: a freshly-loaded
        // TextBox has no spans; migrate scans trailing/inline tokens
        // and adds them. Idempotent — second call is a no-op.
        let mut tb = TextBox::new(typeface(), "Notes #urgent".to_string());
        tb.set_force_heading(true);
        tb.migrate_tags_from_text();
        assert_eq!(tb.tags().len(), 1);
        assert_eq!(tb.heading_tag_names(), vec!["urgent".to_string()]);
        tb.migrate_tags_from_text();
        assert_eq!(tb.tags().len(), 1, "migration is idempotent");
    }

    #[test]
    fn textbox_new_edit_clears_redo_stack() {
        // Bursts coalesce by default; break_coalesce between forces
        // separate undo entries so this test exercises the redo-clear
        // path explicitly.
        let mut tb = TextBox::new(typeface(), String::new());
        tb.insert_text("a");
        tb.break_coalesce();
        tb.insert_text("b");
        tb.undo(); // text = "a"; redo has "ab"
        tb.break_coalesce();
        tb.insert_text("c"); // redo cleared by the new edit
        assert_eq!(tb.text(), "ac");
        assert!(!tb.redo(), "redo cleared by the new edit");
    }

    #[test]
    fn textbox_typing_burst_coalesces_into_one_undo() {
        // Five rapid keystrokes should be one undo entry — undoing
        // returns to the start of the burst, not one char back.
        let mut tb = TextBox::new(typeface(), String::new());
        for c in "hello".chars() {
            tb.insert_text(&c.to_string());
        }
        assert_eq!(tb.text(), "hello");
        assert!(tb.undo());
        assert_eq!(tb.text(), "", "burst undid as a single unit");
        assert!(!tb.undo(), "no second undo entry");
    }

    #[test]
    fn textbox_break_coalesce_starts_new_undo_entry() {
        // A deliberate gesture (mouse click, arrow nav) between two
        // bursts should make them separately undoable. Simulate the
        // gesture with break_coalesce.
        let mut tb = TextBox::new(typeface(), String::new());
        tb.insert_text("foo");
        tb.break_coalesce();
        tb.insert_text("bar");
        assert_eq!(tb.text(), "foobar");
        assert!(tb.undo());
        assert_eq!(tb.text(), "foo", "second burst undone alone");
        assert!(tb.undo());
        assert_eq!(tb.text(), "", "first burst undone next");
    }

    #[test]
    fn textbox_paste_does_not_coalesce_with_following_typing() {
        // Paste sets break_coalesce so subsequent typing is its own
        // undo entry, matching the app-level pattern.
        let mut tb = TextBox::new(typeface(), String::new());
        tb.paste("pasted ");
        tb.insert_text("typed");
        assert_eq!(tb.text(), "pasted typed");
        assert!(tb.undo());
        assert_eq!(tb.text(), "pasted ", "typed burst undone first");
        assert!(tb.undo());
        assert_eq!(tb.text(), "", "paste undone next");
    }

    #[test]
    fn long_word_breaks_inside_when_no_whitespace() {
        // A single token wider than max_width must be hard-broken at
        // char boundaries — otherwise the line renders past the right
        // edge of the cell. Pre-fix, this returned one line containing
        // the whole 200-char string regardless of max_width.
        use super::wrap::wrap_paragraph_into;
        let tf = typeface();
        let font = skia_safe::Font::from_typeface(&tf, 16.0);
        let paint = skia_safe::Paint::default();
        let long: String = "a".repeat(200);
        let mut out = Vec::new();
        wrap_paragraph_into(&long, 0, long.len(), &font, &paint, 100.0, &mut out);
        assert!(out.len() > 1, "long word must break into multiple lines");
        // Every emitted line should fit (allowing the very last to be
        // short; the final piece is the remainder).
        for line in &out {
            let w = font.measure_str(&long[line.start..line.end], Some(&paint)).0;
            assert!(
                w <= 100.0 || (line.end - line.start) <= 1,
                "line width {} exceeds max_width 100",
                w,
            );
        }
        // Lines together cover the whole input contiguously.
        assert_eq!(out.first().unwrap().start, 0);
        assert_eq!(out.last().unwrap().end, long.len());
    }

    #[test]
    fn long_word_followed_by_short_word_wraps_correctly() {
        use super::wrap::wrap_paragraph_into;
        let tf = typeface();
        let font = skia_safe::Font::from_typeface(&tf, 16.0);
        let paint = skia_safe::Paint::default();
        let text = format!("{} tail", "x".repeat(200));
        let mut out = Vec::new();
        wrap_paragraph_into(&text, 0, text.len(), &font, &paint, 100.0, &mut out);
        // The trailing "tail" word should still be present at the end
        // of the final line, not stranded.
        let last = out.last().unwrap();
        assert!(
            text[last.start..last.end].ends_with("tail"),
            "tail word must land on the final line, got {:?}",
            &text[last.start..last.end],
        );
    }

    #[test]
    fn poppop_body_has_no_auto_heading() {
        // After v4 the `# ` prefix no longer triggers heading rendering on
        // body text. A PopPop input that opens with `# Foo` is treated as a
        // plain calc line, and its source-line bands all report
        // is_heading=false.
        let mut tb = TextBox::new(typeface(), "# Notes\n2 + 2\nfoo".to_string());
        tb.tick(
            &skia_safe::surfaces::raster_n32_premul((400, 200))
                .unwrap()
                .canvas(),
            0.0,
            0.0,
            400.0,
            false,
            false,
        );
        let bands = tb.source_line_y_bands();
        assert_eq!(bands.len(), 3);
        for &(_, _, is_heading) in &bands {
            assert!(!is_heading, "body lines never auto-classify as heading");
        }
        assert!(tb.heading_tag_names().is_empty());
    }

    #[test]
    fn poppop_evaluates_committed_lines() {
        // "2 + 3\nx" — first line is committed, second is the line being
        // typed (last → skipped). Output is one line: "5". No errors.
        let mut e = ::poppop::Engine::new();
        let (out, errs) = super::poppop::compute_poppop_output("2 + 3\nx", &mut e);
        assert_eq!(out, vec!["5".to_string()]);
        assert!(errs.is_empty());
    }

    #[test]
    fn poppop_engine_state_threads_across_lines() {
        // Assignment on line 1 binds x; line 2 reads it. Both lines are
        // committed thanks to the trailing newline (final paragraph is
        // empty and skipped as the "last").
        let mut e = ::poppop::Engine::new();
        let (out, errs) = super::poppop::compute_poppop_output("x = 4\nx * 2\n", &mut e);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "4");
        assert_eq!(out[1], "8");
        assert!(errs.is_empty());
    }

    #[test]
    fn poppop_comments_are_skipped_and_not_evaluated() {
        // Lines starting with `#` are notes — they don't go through the
        // engine and they emit no output (the row stays blank). Whatever
        // follows still evaluates with the engine state untouched.
        let mut e = ::poppop::Engine::new();
        let (out, errs) = super::poppop::compute_poppop_output("# rent calc\nx = 1200\nx * 12\n", &mut e);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], "");      // comment row
        assert_eq!(out[1], "1200");  // x = 1200 binds + emits the value
        assert_eq!(out[2], "14400"); // x * 12 sees x = 1200
        assert!(errs.is_empty());
    }

    #[test]
    fn poppop_blank_and_error_lines_are_empty() {
        // Blank line → empty (no error). Parse error (`1 +`) → empty
        // output, recorded in `errs`. Valid line produces its formatted
        // result. All three are committed because of the trailing newline.
        let mut e = ::poppop::Engine::new();
        let (out, errs) = super::poppop::compute_poppop_output("\n1 +\n3 + 4\n", &mut e);
        assert_eq!(
            out,
            vec![String::new(), String::new(), "7".to_string()]
        );
        // Only the `1 +` line (paragraph 1) errors. Blank lines don't
        // hit the engine; valid lines don't error.
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, 1);
        assert!(!errs[0].1.is_empty(), "error message is non-empty");
    }

    #[test]
    fn poppop_undefined_variable_surfaces_as_error() {
        // Reading an unbound name produces an `undefined variable: …`
        // error from the engine. The output column for that row stays
        // blank; the error is reported via the second return.
        let mut e = ::poppop::Engine::new();
        let (out, errs) = super::poppop::compute_poppop_output("y\n", &mut e);
        assert_eq!(out, vec![String::new()]);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, 0);
        // Should mention the offending name.
        assert!(
            errs[0].1.contains('y'),
            "error message should mention the variable: {:?}",
            errs[0].1
        );
    }

    #[test]
    fn poppop_error_message_is_single_line() {
        // Pest parse errors produce multi-line messages with visual
        // pointers. We trim to the first line so the message renders in
        // the single-line slot below the input row.
        let mut e = ::poppop::Engine::new();
        let (_out, errs) = super::poppop::compute_poppop_output("1 +\n", &mut e);
        assert_eq!(errs.len(), 1);
        assert!(
            !errs[0].1.contains('\n'),
            "error message must not contain newlines: {:?}",
            errs[0].1
        );
    }

    #[test]
    fn force_heading_marks_every_line_as_heading() {
        let mut tb = TextBox::new(typeface(), "Notes #urgent #person".to_string());
        tb.set_force_heading(true);
        tb.migrate_tags_from_text();
        tb.tick(
            &skia_safe::surfaces::raster_n32_premul((400, 200))
                .unwrap()
                .canvas(),
            0.0,
            0.0,
            400.0,
            false,
            false,
        );
        let bands = tb.source_line_y_bands();
        assert_eq!(bands.len(), 1);
        assert!(bands[0].2, "title line is heading without `# ` prefix");
        let tags = tb.heading_tag_names();
        assert!(tags.contains(&"urgent".to_string()));
        assert!(tags.contains(&"person".to_string()));
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

    // ----- Table cell -----

    #[test]
    fn table_default_dimensions_are_3x3() {
        let tc = TableCell::new(typeface());
        assert_eq!(tc.rows(), 3);
        assert_eq!(tc.cols(), 3);
        assert_eq!(tc.focused_index(), (0, 0));
        assert!(tc.is_empty());
    }

    #[test]
    fn table_focus_moves_with_tab() {
        let mut tc = TableCell::new(typeface());
        // Forward through every cell in row-major order.
        for col in 1..3 {
            assert!(tc.step_focus(true));
            assert_eq!(tc.focused_index(), (0, col));
        }
        // (0,2) → wrap to (1,0).
        assert!(tc.step_focus(true));
        assert_eq!(tc.focused_index(), (1, 0));
        // Walk all the way to (2,2) and verify the next forward step is a no-op.
        while tc.focused_index() != (2, 2) {
            assert!(tc.step_focus(true));
        }
        assert!(!tc.step_focus(true), "no-op past last cell");
        assert_eq!(tc.focused_index(), (2, 2));
        // Shift+Tab from (2,2) goes to (2,1).
        assert!(tc.step_focus(false));
        assert_eq!(tc.focused_index(), (2, 1));
    }

    #[test]
    fn cell_toggle_title_focus_creates_title() {
        let mut cell = Cell::new(typeface(), String::new());
        assert!(cell.title.is_none());
        assert!(cell.toggle_title_focus());
        assert!(cell.title.is_some());
        assert!(cell.title_focused);
        // Idempotent: a second call with the title already focused returns
        // false (no observable change).
        assert!(!cell.toggle_title_focus());
        assert!(cell.title_focused);
    }

    #[test]
    fn cell_toggle_title_focus_focuses_existing() {
        let mut cell = Cell::new(typeface(), String::new());
        cell.toggle_title_focus();
        cell.title_focused = false;
        assert!(cell.toggle_title_focus());
        assert!(cell.title_focused);
    }

    #[test]
    fn title_trailing_tags_parse_without_hash_prefix() {
        let mut cell = Cell::new(typeface(), String::new());
        cell.toggle_title_focus();
        let title = cell.title.as_mut().unwrap();
        title.replace_text("My Notes #urgent #person".to_string());
        // Tag spans only exist post-popup-commit; for fixtures, run
        // the legacy text-parse migration to populate them — mirrors
        // the v7 backfill on load.
        title.migrate_tags_from_text();
        let tags = cell.heading_tag_names();
        assert!(tags.contains(&"urgent".to_string()));
        assert!(tags.contains(&"person".to_string()));
        assert_eq!(cell.heading_title().as_deref(), Some("My Notes"));
    }

    #[test]
    fn parse_inline_tags_finds_word_boundary_tags_only() {
        use crate::cell::parse_inline_tags;
        // Whitespace- or start-led `#word` tokens; embedded `#` (e.g.,
        // URL fragment) does not qualify.
        let text = "alpha #foo and #bar plus url#frag and #";
        let names: Vec<&str> = parse_inline_tags(text)
            .iter()
            .map(|r| &text[r.start..r.end])
            .collect();
        assert_eq!(names, vec!["#foo", "#bar", "#"]);
    }

    #[test]
    fn all_tag_names_aggregates_title_and_body_for_plain() {
        let mut cell = Cell::new(typeface(), "follow up #urgent later".to_string());
        if let CellKind::Plain(tb) = &mut cell.kind {
            tb.migrate_tags_from_text();
        }
        cell.toggle_title_focus();
        let title = cell.title.as_mut().unwrap();
        title.replace_text("Note #person".to_string());
        title.migrate_tags_from_text();
        let tags = cell.all_tag_names();
        assert!(tags.contains(&"person".to_string()));
        assert!(tags.contains(&"urgent".to_string()));
    }

    #[test]
    fn all_tag_names_picks_up_outline_bullet_tags() {
        let mut tb = TextBox::new(typeface(), "buy milk #shopping".to_string());
        tb.migrate_tags_from_text();
        let oc = OutlineCell::from_bullets(
            typeface(),
            vec![Bullet::new(Uuid::now_v7(), tb, 0)],
        );
        let mut cell = Cell::new(typeface(), String::new());
        cell.kind = CellKind::Outline(oc);
        let tags = cell.all_tag_names();
        assert!(tags.contains(&"shopping".to_string()));
    }

    #[test]
    fn all_tag_names_excludes_poppop_body() {
        let mut cell = Cell::new_poppop(typeface());
        if let CellKind::PopPop(pc) = &mut cell.kind {
            pc.textbox_mut()
                .replace_text("# this is a poppop comment, not a tag\n1 + 1".to_string());
        }
        let tags = cell.all_tag_names();
        assert!(tags.is_empty(), "PopPop body must not contribute tags");
    }

    #[test]
    fn caret_in_in_progress_tag_detects_body_edit() {
        // Plain cell with `#foo` mid-body, caret right after `o` (inside
        // the tag): predicate fires.
        let mut cell = Cell::new(typeface(), String::new());
        if let CellKind::Plain(tb) = &mut cell.kind {
            tb.replace_text("note #foo".to_string());
            tb.migrate_tags_from_text();
            tb.set_caret_at(9); // end-of-text, inside `#foo` (5..9)
        }
        cell.title_focused = false;
        assert!(cell.caret_in_in_progress_tag());
    }

    #[test]
    fn textbox_tag_at_returns_name_for_byte_inside_inline_tag() {
        let mut tb = TextBox::new(typeface(), "follow up #shopping later".to_string());
        tb.migrate_tags_from_text();
        // `#shopping` lives at bytes 10..19. Byte 14 (mid-tag) hits.
        assert_eq!(tb.tag_at(14).as_deref(), Some("shopping"));
        // Byte 9 (the space just before `#`) doesn't.
        assert_eq!(tb.tag_at(9), None);
        // Byte 19 (just past the last char) doesn't.
        assert_eq!(tb.tag_at(19), None);
    }

    #[test]
    fn outline_bullets_matching_any_tag_includes_subtree_descendants() {
        // depth: 0 (#urgent), 1 (no tag), 1 (no tag), 0 (no tag), 1 (#urgent)
        // Expected match for `urgent`: bullets 0,1,2 (subtree of 0) and
        // bullet 4 (its own subtree). Bullet 3 stays out.
        let mk = |t: &str, d: u32| {
            Bullet::new(
                Uuid::now_v7(),
                TextBox::new(typeface(), t.to_string()),
                d,
            )
        };
        let bullets = vec![
            mk("root #urgent", 0),
            mk("child a", 1),
            mk("child b", 1),
            mk("sibling root", 0),
            mk("nested #urgent", 1),
        ];
        let ids: Vec<Uuid> = bullets.iter().map(|b| b.id()).collect();
        let oc = OutlineCell::from_bullets(typeface(), bullets);
        let m = oc.bullets_matching_any_tag(&["urgent".to_string()]);
        assert!(m.contains(&ids[0]));
        assert!(m.contains(&ids[1]));
        assert!(m.contains(&ids[2]));
        assert!(!m.contains(&ids[3]));
        assert!(m.contains(&ids[4]));
    }

    #[test]
    fn caret_in_in_progress_tag_false_when_caret_outside_tag() {
        // Caret AFTER the tag's whitespace boundary: not in progress.
        let mut cell = Cell::new(typeface(), String::new());
        if let CellKind::Plain(tb) = &mut cell.kind {
            tb.replace_text("note #foo ".to_string());
            tb.set_caret_at(10); // after the trailing space
        }
        cell.title_focused = false;
        assert!(!cell.caret_in_in_progress_tag());
    }

    #[test]
    fn reference_cache_is_stale_when_no_cache_yet() {
        // Fresh ReferenceCell: cache is None. Any present target → stale
        // (caller must build cache).
        let target_id = Uuid::now_v7();
        let rc = ReferenceCell::new(typeface(), ReferenceTarget::WholeCell(target_id));
        assert!(rc.cache_is_stale_for(Some(123)));
        // Target gone AND cache empty → not stale (nothing to do).
        assert!(!rc.cache_is_stale_for(None));
    }

    #[test]
    fn reference_cache_is_stale_when_source_edited_at_changes() {
        // Cache built at time A; source's edited_at bumped to B → stale.
        let target_id = Uuid::now_v7();
        let mut rc = ReferenceCell::new(typeface(), ReferenceTarget::WholeCell(target_id));
        // Install a fake cache (a Plain cell stand-in; the staleness check
        // doesn't inspect contents — only the edited_at key).
        let dummy = Cell::new(typeface(), String::new());
        rc.install_cache(Some(dummy), Some(100));
        assert!(!rc.cache_is_stale_for(Some(100)), "same edited_at → fresh");
        assert!(rc.cache_is_stale_for(Some(101)), "bumped edited_at → stale");
    }

    #[test]
    fn reference_cache_is_stale_when_target_disappears() {
        // Cache exists but the target was deleted (None edited_at) →
        // stale, so the render path will clear the cache.
        let target_id = Uuid::now_v7();
        let mut rc = ReferenceCell::new(typeface(), ReferenceTarget::WholeCell(target_id));
        let dummy = Cell::new(typeface(), String::new());
        rc.install_cache(Some(dummy), Some(100));
        assert!(rc.cache_is_stale_for(None));
    }

    #[test]
    fn reference_cache_install_clears_when_passed_none() {
        // install_cache(None, None) is the "drop the cache" path used when
        // the target goes missing or chains.
        let target_id = Uuid::now_v7();
        let mut rc = ReferenceCell::new(typeface(), ReferenceTarget::WholeCell(target_id));
        let dummy = Cell::new(typeface(), String::new());
        rc.install_cache(Some(dummy), Some(100));
        assert!(rc.cache_ref().is_some());
        rc.install_cache(None, None);
        assert!(rc.cache_ref().is_none());
        // After clearing, "target gone" is not stale (nothing to rebuild).
        assert!(!rc.cache_is_stale_for(None));
    }

    #[test]
    fn table_snapshot_round_trip() {
        let mut tc = TableCell::new(typeface());
        tc.cell_at_mut(0, 0).unwrap().textbox.replace_text("alpha".to_string());
        tc.cell_at_mut(1, 2).unwrap().textbox.replace_text("hello".to_string());
        tc.cell_at_mut(2, 0).unwrap().readonly = true;
        let snap = tc.snapshot();
        // Mutate after snapshot.
        tc.cell_at_mut(0, 0).unwrap().textbox.replace_text("DIFFERENT".to_string());
        tc.cell_at_mut(1, 2).unwrap().textbox.replace_text(String::new());
        tc.cell_at_mut(2, 0).unwrap().readonly = false;
        // Restore.
        tc.restore(snap);
        assert_eq!(tc.cell_at(0, 0).unwrap().textbox.text(), "alpha");
        assert_eq!(tc.cell_at(1, 2).unwrap().textbox.text(), "hello");
        assert!(tc.cell_at(2, 0).unwrap().readonly);
        // Other cells are still empty + editable.
        assert!(tc.cell_at(0, 1).unwrap().textbox.is_empty());
        assert!(!tc.cell_at(0, 1).unwrap().readonly);
    }
}
