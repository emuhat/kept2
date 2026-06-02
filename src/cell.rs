use std::ops::Range;

use skia_safe::{Canvas, Typeface};
use uuid::Uuid;
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

mod body;
mod common;
mod grid;
mod outline;
mod plain;
mod poppop;
mod reference;
mod table;
mod textbox;
mod wrap;

pub use body::CellBody;
pub use common::{
    KEPT_TAG_SCHEME, LinkSpan, TextBoxSnapshot, is_kept_tag_url, now_epoch_ms, tag_name_from_url,
};
pub use outline::{Bullet, OutlineCell, OutlineSnapshot};
#[cfg(test)]
pub use outline::BulletSnapshot;
pub use plain::PlainCell;
pub use poppop::PopPopCell;
pub use reference::{EmbeddedReference, ReferenceCell, ReferenceTarget};
pub use table::{TableCell, TableSnapshot};
pub use textbox::TextBox;

pub(crate) use common::{INACTIVE_ALPHA, TITLE_BODY_GAP, open_url, primary_mod};
pub use wrap::parse_inline_tags;


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
    /// `Some(t)` when the cell was closed at epoch ms `t`; `None` when
    /// open. Replaces the legacy `active: bool` archive flag.
    pub closed_at: Option<i64>,
    /// `Some(t)` schedules the cell to resurface in the Current view at
    /// epoch ms `t`. `None` means no scheduled resurfacing.
    pub resurface_after: Option<i64>,
    /// Scratch flag. Carried so undo/redo of a Move-to-Timeline
    /// action (whenever we add it) can flip the bit back.
    pub scratch: bool,
    /// Whether this cell appears in the Inbox view. Set automatically
    /// on yeet and on snooze-expiry; cleared on thread attach and on
    /// snooze. Also manually toggled via "Send to Inbox" / "Release from Inbox".
    pub inbox: bool,
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
    /// `Some(t)` when the user has closed this cell at epoch ms `t`;
    /// `None` when the cell is open. Closed cells are hidden from
    /// default views (timeline, sidebar dates, search, entity
    /// references) unless the global `show_inactive_cells` toggle is
    /// on, in which case they render dimmed. Replaces the older
    /// `active: bool` flag; the field is the source of truth and
    /// `is_open()` is the canonical accessor.
    pub closed_at: Option<i64>,
    /// `Some(t)` schedules the cell to resurface in the Current view
    /// at epoch ms `t`. Snoozing a cell sets this to a future time;
    /// when `now >= t`, the cell becomes eligible for the Current
    /// view (it does NOT disappear from the timeline). `None` means
    /// no scheduled resurfacing.
    pub resurface_after: Option<i64>,
    /// True when this cell lives in the Scratch view rather than
    /// the principled timeline. Scratch cells are excluded from
    /// search, can't be referenced (no `kept://` URLs), and are
    /// auto-deleted by the TTL pruner once they age past
    /// `SCRATCH_TTL_MS`. Flipped to false by the "Move to Timeline"
    /// bar-menu action which also re-stamps the timestamp.
    pub scratch: bool,
    /// Whether this cell appears in the Inbox view. Set automatically
    /// on yeet and on snooze-expiry; cleared on thread attach and on
    /// snooze. Also manually toggled via "Send to Inbox" / "Release from Inbox".
    pub inbox: bool,
}

pub enum CellKind {
    Plain(PlainCell),
    Outline(OutlineCell),
    PopPop(PopPopCell),
    Table(TableCell),
    /// Read-only embed of another cell or bullet sub-tree. Has no editable
    /// content of its own; click navigates to the target.
    Reference(ReferenceCell),
}

impl CellKind {
    /// Borrow the variant's body as a `CellBody` trait object. This is the
    /// single 5-arm match — every other dispatch site can go through it
    /// instead of writing its own per-variant match.
    pub fn body(&self) -> &dyn CellBody {
        match self {
            CellKind::Plain(pc) => pc,
            CellKind::Outline(oc) => oc,
            CellKind::PopPop(pc) => pc,
            CellKind::Table(tc) => tc,
            CellKind::Reference(rc) => rc,
        }
    }

    pub fn body_mut(&mut self) -> &mut dyn CellBody {
        match self {
            CellKind::Plain(pc) => pc,
            CellKind::Outline(oc) => oc,
            CellKind::PopPop(pc) => pc,
            CellKind::Table(tc) => tc,
            CellKind::Reference(rc) => rc,
        }
    }
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
            CellKind::Plain(pc) => {
                let new_pc = pc.clone_for_cache(typeface.clone(), scale);
                Some(CellKind::Plain(new_pc))
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
            kind: CellKind::Plain(PlainCell::new(typeface, initial_text)),
            title: None,
            title_focused: false,
            cell_x: 0.0,
            cell_y: 0.0,
            cell_w: 0.0,
            cell_h: 0.0,
            timestamp: now,
            edited_at: now,
            context_hint_id: None,
            closed_at: None,
            resurface_after: None,
            scratch: false,
            inbox: false,
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
            closed_at: None,
            resurface_after: None,
            scratch: false,
            inbox: false,
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
            closed_at: None,
            resurface_after: None,
            scratch: false,
            inbox: false,
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
            closed_at: None,
            resurface_after: None,
            scratch: false,
            inbox: false,
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
        closed_at: Option<i64>,
        resurface_after: Option<i64>,
        scratch: bool,
        inbox: bool,
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
            closed_at,
            resurface_after,
            scratch,
            inbox,
        }
    }

    /// True when the cell is open (no close timestamp). Canonical
    /// accessor — every former `cell.active` read should route through
    /// here.
    pub fn is_open(&self) -> bool {
        self.closed_at.is_none()
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
        self.kind.body().typeface().clone()
    }

    /// Cell-wide font scale. Pulled from the body since body sets are the
    /// authoritative source via `set_font_scale`.
    fn body_font_scale(&self) -> f32 {
        self.kind.body().font_scale()
    }

    /// Reconstruct a cell from a snapshot + id, using `typeface` for fresh
    /// TextBox / OutlineCell instances. Used by undo of delete and redo of
    /// insert to recreate cells with their original identity intact.
    pub fn from_snapshot(id: Uuid, snap: CellSnapshot, typeface: &Typeface) -> Self {
        let kind = match snap.kind {
            CellSnapshotKind::Plain(tbs) => {
                let mut tb = TextBox::new(typeface.clone(), String::new());
                tb.restore(tbs);
                CellKind::Plain(PlainCell::from_textbox(tb))
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
            snap.closed_at,
            snap.resurface_after,
            snap.scratch,
            snap.inbox,
        )
    }

    /// Add a link span to the cell's first textbox (plain) or first bullet
    /// (outline). Used by seed setup; future link-creation UI will go through
    /// a richer path scoped to the focused textbox.
    pub fn add_link_to_first(&mut self, range: Range<usize>, url: String) {
        self.kind.body_mut().add_link_to_first(range, url);
    }

    /// True if document-space position `(abs_x, abs_y)` lands on a link in
    /// this cell's most recently rendered layout. Drives the hand cursor.
    pub fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        if let Some(title) = self.title.as_ref() {
            if title.link_at_doc_pos(abs_x, abs_y) {
                return true;
            }
        }
        self.kind.body().link_at_doc_pos(abs_x, abs_y)
    }

    /// Replace `range` with `text` in the focused editable slot (title
    /// when focused, otherwise the kind's focused inner element) and link
    /// the inserted text to `url`.
    pub fn replace_focused_with_link(
        &mut self,
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
        self.kind.body_mut().replace_at_focused_with_link(range, text, url);
    }

    /// Plain-text variant of `replace_focused_with_link` — replaces
    /// `range` in the focused slot with `text`, no link span attached.
    /// Used by `#`-tag autocomplete commit.
    pub fn replace_focused_with_text(&mut self, range: Range<usize>, text: String) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.replace_with_text(range, text);
            }
            return;
        }
        self.kind.body_mut().replace_at_focused_with_text(range, text);
    }

    /// Replace `range` in the focused editable slot with `text` (which
    /// must start with `#`) and mark the inserted span as a tag — the
    /// committed form of the `#`-mention popup. PopPop is excluded
    /// since `#` there is the comment marker, not a tag prefix (its
    /// `CellBody` impl makes this method a no-op).
    pub fn replace_focused_with_tag(&mut self, range: Range<usize>, text: String) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.replace_with_tag(range, text);
            }
            return;
        }
        self.kind.body_mut().replace_at_focused_with_tag(range, text);
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
        self.kind.body().copy_text()
    }

    /// Return `(text, links)` for whichever textbox in this cell
    /// currently holds a non-empty selection — searching every
    /// surface the cell can hold a selection on: title, body
    /// textboxes (visited via `for_each_textbox`, which already
    /// descends into a `Reference` cell's cache), and the envelope
    /// outline's `reference_header` cache (which isn't part of
    /// `for_each_textbox` per the asymmetric-cache-descent rule).
    ///
    /// Relies on the "only one selection at a time" invariant
    /// enforced in the mouse_down chain — at most one of the
    /// textboxes above has a non-empty selection. Used by the copy
    /// path so a selection inside an embed gets copied even when
    /// it's not the focused bullet's textbox.
    pub fn copy_primary_selection_with_links(&self) -> Option<(String, Vec<LinkSpan>)> {
        if let Some(title) = self.title.as_ref() {
            if let Some(sel) = title.copy_primary_selection_with_links() {
                return Some(sel);
            }
        }
        let mut hit: Option<(String, Vec<LinkSpan>)> = None;
        self.kind.body().for_each_textbox(&mut |tb| {
            if hit.is_none() {
                if let Some(sel) = tb.copy_primary_selection_with_links() {
                    hit = Some(sel);
                }
            }
        });
        if hit.is_some() {
            return hit;
        }
        if let CellKind::Outline(oc) = &self.kind {
            if let Some(h) = oc.reference_header() {
                if let Some(cache) = h.cache_ref() {
                    return cache.copy_primary_selection_with_links();
                }
            }
        }
        None
    }

    /// Cell title, if any: the title slot's text with trailing #tags
    /// stripped. None when there is no title slot or the title contains
    /// only tags / whitespace. "Trailing tags" here means committed
    /// kept-tag link spans within the heading paragraph that, together
    /// with intervening whitespace, run all the way to the heading's
    /// end. A `#X` in the middle of the title is body content, not
    /// trailing.
    #[allow(dead_code)]
    pub fn heading_title(&self) -> Option<String> {
        let title_tb = self.title.as_ref()?;
        let text = title_tb.text();
        let title_end = text.find('\n').unwrap_or(text.len());
        let bytes = text.as_bytes();
        // Walk backward through whitespace + kept-tag spans until we
        // hit a non-tag, non-whitespace byte. That's the end of the
        // human title.
        let mut tag_starts: Vec<usize> = title_tb
            .links()
            .iter()
            .filter(|l| is_kept_tag_url(&l.url) && l.range.end <= title_end)
            .map(|l| l.range.start)
            .collect();
        tag_starts.sort_unstable();
        let mut end = title_end;
        loop {
            while end > 0 && (bytes[end - 1] as char).is_whitespace() {
                end -= 1;
            }
            // If a kept-tag span ends exactly at `end`, retreat past it.
            let preceding_tag = title_tb
                .links()
                .iter()
                .find(|l| is_kept_tag_url(&l.url) && l.range.end == end);
            match preceding_tag {
                Some(l) => end = l.range.start,
                None => break,
            }
        }
        let _ = tag_starts;
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
        // The trait default walks textboxes; PopPop/Reference override
        // to no-op (PopPop opts out of tag semantics; Reference would
        // mutate cache without dirtying source).
        changed | self.kind.body_mut().remove_tags_named(name)
    }

    pub fn all_tag_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.heading_tag_names();
        // Trait default extends `out` with body textbox tags, de-duped.
        // PopPop / Reference override to no-op so they contribute nothing.
        self.kind.body().all_tag_names_into(&mut out);
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
        // Span-based tags: only `kept-tag://` link spans count. Caret
        // inside one means the user is editing an existing tag in
        // place — defer save until they leave the span. Typed `#X`
        // with no span is just text and never defers (it never would
        // have made a tag in the first place).
        let in_tag = |links: &[LinkSpan], caret: usize| -> bool {
            links.iter().any(|l| {
                is_kept_tag_url(&l.url) && caret > l.range.start && caret <= l.range.end
            })
        };
        if self.title_focused {
            let Some(title) = self.title.as_ref() else {
                return false;
            };
            let Some((_, caret)) = title.primary_caret() else {
                return false;
            };
            return in_tag(title.links(), caret);
        }
        // PopPop opts out of tag semantics (its `focused_textbox()`
        // returns None); Reference has no editable focus either. Both
        // bottom out at None here.
        let Some(tb) = self.kind.body().focused_textbox() else {
            return false;
        };
        let Some((_, caret)) = tb.primary_caret() else {
            return false;
        };
        in_tag(tb.links(), caret)
    }

    /// Full text of the cell, ignoring selection state. Title (if any) is
    /// prepended on its own line so the search popup can match against it.
    /// Outline cells join bullets with newlines (indented two spaces per
    /// depth); tables join cells with tabs and rows with newlines.
    pub fn full_text(&self) -> String {
        let body = self.kind.body().full_text();
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
        self.kind.body_mut().take_pending_link_url()
    }



    /// Every link URL in the cell — title (if any) + body. Used by the
    /// query executor to resolve `kept://<uuid>` references. Reference
    /// cells contribute none (their `all_link_urls_into` is overridden
    /// to no-op so we don't double-count source links).
    pub fn all_link_urls(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(t) = self.title.as_ref() {
            for l in t.links() {
                out.push(l.url.clone());
            }
        }
        self.kind.body().all_link_urls_into(&mut out);
        out
    }

    pub fn cut_text(&mut self) -> String {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                return title.cut_primary_selection();
            }
        }
        self.kind.body_mut().cut_text()
    }

    /// Paste with explicit `LinkSpan`s carried from the clipboard.
    /// Same dispatch shape as `paste_text` but reaches each cell
    /// kind's focused TextBox to call `paste_with_links` (no URL
    /// auto-detection — link metadata is authoritative).
    pub fn paste_into_focused_with_links(
        &mut self,
        text: &str,
        links: &[LinkSpan],
    ) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.paste_with_links(text, links);
                return;
            }
        }
        self.kind.body_mut().paste_with_links(text, links);
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
        // Reference cells render through the app layer (which has access
        // to the cell list to look up their target); their `CellBody::tick`
        // returns 0.0 so this dispatch is safe even though `Cell::tick` is
        // not normally called on them.
        let body_h = self
            .kind
            .body_mut()
            .tick(canvas, x, body_y, width, body_focused, body_caret);
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
        self.kind.body_mut().handle_key(event, modifiers)
    }

    pub fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
        // If the click lands in the title's vertical band, focus the
        // title; otherwise focus the body. The title's y_origin /
        // height come from the last `tick` so an unrendered cell
        // falls through harmlessly.
        //
        // App-wide we present "only one thing selected at a time".
        // The other-cells cleanup happens in `dispatch_doc_click`;
        // here we cover the within-cell case: a click on the title
        // wipes the body's selections (and vice versa) so a stale
        // highlight never persists across a title↔body click. The
        // *target* textbox's selection is preserved so shift-click
        // still extends it.
        let title_band = self.title.as_ref().map(|t| {
            let top = t.y_origin();
            (top, top + t.height())
        });
        let on_title = matches!(title_band, Some((top, bot)) if abs_y >= top && abs_y < bot);
        if on_title {
            self.kind.body_mut().clear_all_selections();
            self.title_focused = true;
            if let Some(title) = self.title.as_mut() {
                return title.mouse_down(abs_x, abs_y, modifiers, editing);
            }
            return false;
        }
        self.unfocus_title_drop_if_empty();
        if let Some(title) = self.title.as_mut() {
            title.clear_selection();
        }
        self.kind.body_mut().mouse_down(abs_x, abs_y, modifiers, editing)
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        // Forward to title too — only the textbox with an active drag responds.
        let mut any = false;
        if let Some(title) = self.title.as_mut() {
            if title.mouse_drag_to(abs_x, abs_y) {
                any = true;
            }
        }
        any | self.kind.body_mut().mouse_drag_to(abs_x, abs_y)
    }

    pub fn mouse_up(&mut self) -> bool {
        let mut any = false;
        if let Some(title) = self.title.as_mut() {
            if title.mouse_up() {
                any = true;
            }
        }
        any | self.kind.body_mut().mouse_up()
    }

    /// True iff the body's keyboard caret is on the body's topmost visual
    /// line. Used by Cell::handle_key to detect "ArrowUp should escape into
    /// the title slot." Each CellBody impl already knows its own edge rules.
    fn body_at_top_edge(&self) -> bool {
        self.kind.body().at_top_edge()
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
        self.kind.body_mut().place_caret_at_start();
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
        // ReferenceCell overrides is_empty() to false (the pointer is
        // content); other kinds bottom out at their inner textboxes.
        title_empty && self.kind.body().is_empty()
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        if let Some(title) = self.title.as_mut() {
            title.set_font_scale(scale);
        }
        self.kind.body_mut().set_font_scale(scale);
    }

    pub fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        if self.title_focused {
            if let Some(title) = self.title.as_ref() {
                return title.caret_doc_y_band();
            }
        }
        self.kind.body().caret_doc_y_band()
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
        self.kind.body().at_top_edge()
    }

    pub fn at_bottom_edge(&self) -> bool {
        // Title focused → body is below us, so we're not at the bottom edge.
        if self.title_focused {
            return false;
        }
        self.kind.body().at_bottom_edge()
    }

    pub fn place_caret_at_start(&mut self) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                title.set_caret_at(0);
                return;
            }
        }
        self.kind.body_mut().place_caret_at_start();
    }

    pub fn place_caret_at_end(&mut self) {
        if self.title_focused {
            if let Some(title) = self.title.as_mut() {
                let end = title.text().len();
                title.set_caret_at(end);
                return;
            }
        }
        self.kind.body_mut().place_caret_at_end();
    }

    /// True iff any visible selection exists on this cell — text
    /// drag on the title, on the body's focused textbox, on any of
    /// the body's non-focused textboxes (outline bullets, table
    /// cells, popppop input/output), or an outline bullet-range
    /// selection. Used when entering edit mode to decide whether
    /// to keep the existing selection or jump the caret to the
    /// end. Reference cells' embed cache also contributes — drag
    /// inside an embedded preview counts.
    pub fn has_any_selection(&self) -> bool {
        if let Some(title) = self.title.as_ref() {
            if title.has_selection() {
                return true;
            }
        }
        let mut any = false;
        self.kind.body().for_each_textbox(&mut |tb| {
            if tb.has_selection() {
                any = true;
            }
        });
        if any {
            return true;
        }
        if let CellKind::Outline(oc) = &self.kind {
            if oc.has_bullet_selection() {
                return true;
            }
        }
        false
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
        self.kind.body_mut().clear_all_selections();
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
        self.kind.body_mut().select_all_focused();
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
        self.kind.body().focused_text_and_caret()
    }

    /// Outline cells: ID of the focused bullet. Every other kind returns
    /// None — including when the title is focused (the focus is on the
    /// title, not on any bullet).
    pub fn focused_bullet_id(&self) -> Option<Uuid> {
        if self.title_focused {
            return None;
        }
        self.kind.body().focused_bullet_id()
    }

    /// Anchor position for an overlay tied to byte `byte` in this cell's
    /// active textbox (focused bullet for outline; title when focused).
    /// Used by the @-mention popup.
    pub fn anchor_doc_pos(&self, byte: usize) -> Option<(f32, f32)> {
        if self.title_focused {
            if let Some(title) = self.title.as_ref() {
                let (x, _) = title.doc_position_of_byte(byte)?;
                let (_, bot) = title.line_y_band_of_byte(byte)?;
                return Some((x, bot));
            }
        }
        self.kind.body().anchor_doc_pos_at_focused(byte)
    }

    pub fn snapshot(&self) -> CellSnapshot {
        CellSnapshot {
            timestamp: self.timestamp,
            edited_at: self.edited_at,
            context_hint_id: self.context_hint_id,
            title: self.title.as_ref().map(|t| t.snapshot()),
            kind: match &self.kind {
                CellKind::Plain(pc) => CellSnapshotKind::Plain(pc.body().snapshot()),
                CellKind::Outline(oc) => CellSnapshotKind::Outline(oc.snapshot()),
                CellKind::PopPop(pc) => CellSnapshotKind::PopPop(pc.snapshot()),
                CellKind::Table(tc) => CellSnapshotKind::Table(tc.snapshot()),
                CellKind::Reference(rc) => CellSnapshotKind::Reference(rc.target()),
            },
            closed_at: self.closed_at,
            resurface_after: self.resurface_after,
            scratch: self.scratch,
            inbox: self.inbox,
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
        self.closed_at = snap.closed_at;
        self.resurface_after = snap.resurface_after;
        self.scratch = snap.scratch;
        self.inbox = snap.inbox;
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
            (CellKind::Plain(pc), CellSnapshotKind::Plain(tbs)) => pc.body_mut().restore(tbs),
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
        // text, font scale, heading mode, and all link spans (which
        // now includes committed `kept-tag://` tag spans). Embedded
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
        let cloned_links = cloned.links();
        assert_eq!(cloned_links.len(), 2, "link + tag spans survive the clone");
        let tag = cloned_links
            .iter()
            .find(|l| is_kept_tag_url(&l.url))
            .expect("tag link present");
        assert_eq!(tag.range, 8..15);
        assert_eq!(tag.url, "kept-tag://person");
        let external = cloned_links
            .iter()
            .find(|l| !is_kept_tag_url(&l.url))
            .expect("external link present");
        assert_eq!(external.range, 20..24);
    }

    /// REGRESSION: a clipboard payload with explicit link spans
    /// must transfer those spans onto the destination textbox
    /// (kept→kept paste). Tests `paste_with_links` directly with
    /// non-empty link metadata.
    #[test]
    fn paste_with_links_applies_explicit_spans() {
        let mut tb = TextBox::new(typeface(), String::new());
        let url = "https://example.com/".to_string();
        // Selection ranges are byte offsets in `s` (the pasted text).
        let span = LinkSpan { range: 6..11, url: url.clone() };
        tb.paste_with_links("hello world", &[span]);
        let links = tb.links();
        assert_eq!(links.len(), 1, "explicit link span survived paste");
        assert_eq!(links[0].url, url);
        assert_eq!(&tb.text()[links[0].range.clone()], "world");
    }

    /// REGRESSION: empty link list + a paste containing a bare URL
    /// must fall back to URL auto-detection so plain-text pastes
    /// of "https://x.com" still produce a clickable link (the old
    /// `paste` behavior). Without this fallback, the new payload
    /// path silently drops every URL that arrives without an
    /// HTML/marker wrapper.
    #[test]
    fn paste_with_links_falls_back_to_url_autodetect_when_empty() {
        let mut tb = TextBox::new(typeface(), String::new());
        tb.paste_with_links("visit https://example.com/ today", &[]);
        let links = tb.links();
        assert_eq!(
            links.len(),
            1,
            "URL in plain text must auto-linkify even via paste_with_links"
        );
        assert_eq!(links[0].url, "https://example.com/");
    }

    #[test]
    fn cell_closed_at_round_trips_through_snapshot() {
        // Closing a cell is a structural edit that flows through the
        // same snapshot path the undo system uses; restore() must
        // replay the flag, not just the kind.
        let mut cell = Cell::new(typeface(), "x".to_string());
        assert!(cell.is_open(), "new cells default to open");
        cell.closed_at = Some(123);

        let snap = cell.snapshot();
        assert_eq!(snap.closed_at, Some(123), "snapshot captures closed_at");

        let mut reborn = Cell::new(typeface(), String::new());
        reborn.restore(snap);
        assert_eq!(reborn.closed_at, Some(123), "restore replays closed_at");
        assert!(!reborn.is_open());
    }

    #[test]
    fn bullet_closed_at_round_trips_through_outline_snapshot() {
        let mut oc = OutlineCell::new(typeface());
        let bid = oc.bullets()[0].id();
        oc.set_bullet_closed_at(bid, Some(42));

        let snap = oc.snapshot();
        assert_eq!(
            snap.bullets[0].closed_at, Some(42),
            "BulletSnapshot carries closed_at"
        );

        let mut reborn = OutlineCell::new(typeface());
        reborn.restore(snap);
        assert_eq!(
            reborn.bullets()[0].closed_at(),
            Some(42),
            "outline restore round-trips bullet closed_at"
        );
    }

    #[test]
    fn effective_open_cascades_through_outline_ancestors() {
        // Build an outline at depths [0, 1, 2, 1, 2]. Close the FIRST
        // depth-1 bullet; its depth-2 child should report
        // effectively-closed even though its own flag is open. The
        // next depth-1 sibling and its depth-2 child stay open —
        // cascade only reaches descendants of the closed ancestor.
        let mut oc = OutlineCell::new(typeface());
        // Replace the seed bullet by snapshot construction (cleanest
        // path that keeps Bullet ids stable across builds).
        let snap_bullets = vec![
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "root".to_string()).snapshot(),
                depth: 0,
                closed_at: None,
                resurface_after: None,
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "child A".to_string()).snapshot(),
                depth: 1,
                closed_at: Some(1), // ← this is the closed sub-outline root
                resurface_after: None,
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "grandchild A".to_string()).snapshot(),
                depth: 2,
                closed_at: None,
                resurface_after: None,
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "child B".to_string()).snapshot(),
                depth: 1,
                closed_at: None,
                resurface_after: None,
            },
            BulletSnapshot {
                id: Uuid::now_v7(),
                textbox: TextBox::new(typeface(), "grandchild B".to_string()).snapshot(),
                depth: 2,
                closed_at: None,
                resurface_after: None,
            },
        ];
        let snap = OutlineSnapshot {
            bullets: snap_bullets,
            focused_bullet: Uuid::now_v7(),
            reference_header: None,
        };
        oc.restore(snap);

        let eff = oc.compute_effective_open();
        assert_eq!(eff.len(), 5);
        assert!(eff[0], "root open");
        assert!(!eff[1], "self-closed child A");
        assert!(!eff[2], "grandchild A closed via ancestor cascade");
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
        let tag_links: Vec<&LinkSpan> =
            tb.links().iter().filter(|l| is_kept_tag_url(&l.url)).collect();
        assert_eq!(tag_links.len(), 1);
        assert_eq!(tag_links[0].url, "kept-tag://urgent");
        assert_eq!(tb.heading_tag_names(), vec!["urgent".to_string()]);
    }

    #[test]
    fn textbox_replace_with_tag_visible_immediately() {
        // Render-time tag styling reads kept-tag link spans directly
        // (no layout cache), so a span pushed by replace_with_tag is
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
        assert!(tb.links().iter().all(|l| !is_kept_tag_url(&l.url)));
        tb.replace_with_tag(6..8, "#urgent".to_string());
        let tag_links: Vec<&LinkSpan> =
            tb.links().iter().filter(|l| is_kept_tag_url(&l.url)).collect();
        assert_eq!(tag_links.len(), 1);
        assert_eq!(tag_links[0].range, 6..13);
    }

    #[test]
    fn textbox_migrate_tags_from_text_seeds_legacy_spans() {
        // Round-trip simulator for v6→v7 backfill: a freshly-loaded
        // TextBox has no spans; migrate scans trailing/inline tokens
        // and adds them as kept-tag link spans. Idempotent — second
        // call is a no-op.
        let mut tb = TextBox::new(typeface(), "Notes #urgent".to_string());
        tb.set_force_heading(true);
        tb.migrate_tags_from_text();
        let tag_count = |tb: &TextBox| -> usize {
            tb.links().iter().filter(|l| is_kept_tag_url(&l.url)).count()
        };
        assert_eq!(tag_count(&tb), 1);
        assert_eq!(tb.heading_tag_names(), vec!["urgent".to_string()]);
        tb.migrate_tags_from_text();
        assert_eq!(tag_count(&tb), 1, "migration is idempotent");
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
        if let CellKind::Plain(pc) = &mut cell.kind {
            pc.body_mut().migrate_tags_from_text();
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
        if let CellKind::Plain(pc) = &mut cell.kind {
            pc.body_mut().replace_text("note #foo".to_string());
            pc.body_mut().migrate_tags_from_text();
            pc.body_mut().set_caret_at(9); // end-of-text, inside `#foo` (5..9)
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
        if let CellKind::Plain(pc) = &mut cell.kind {
            pc.body_mut().replace_text("note #foo ".to_string());
            pc.body_mut().set_caret_at(10); // after the trailing space
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
