use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use arboard::Clipboard;
use uuid::{Uuid, uuid};
use skia_safe::{
    Canvas, Font, FontMgr, Paint, PaintStyle, PathEffect, Point, Rect, Typeface,
};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

use crate::cell::{
    self, Cell, CellKind, CellSnapshot, ReferenceTarget, TextBox, now_epoch_ms, primary_mod,
};
use crate::document::{Context, Document};
use crate::entity_cache::EntityCache;
use crate::persist::{ContextRef, Db, db_path};
use crate::query;

mod context_menus;
mod mention_popup;
mod quick_add;
mod search;
mod sidebar;
mod pane;
use pane::{FocusedCellGeom, PANE_HEADER_H, PaneLayout};

use context_menus::{
    BarContextMenu, CellContextMenu, MenuRenderCtx, PeopleContextMenu, TagContextMenu,
};
use mention_popup::{MentionKind, MentionPopup};

const FONT_BYTES: &[u8] = include_bytes!("../../resources/fonts/Figtree.ttf");

const MARGIN_X: f32 = 20.0;
/// Top inset for body content inside a pane. Bakes in
/// `pane::PANE_HEADER_H` so cells start below the URL-bar header
/// (the header is drawn as a window-space overlay in `tick_pane`
/// and doesn't otherwise consume layout).
const MARGIN_TOP: f32 = PANE_HEADER_H + 20.0;
const CELL_GAP: f32 = 25.0;
/// Outer padding around the focused cell in focus mode (Ctrl+F). Smaller
/// than `MARGIN_X` so the cell really feels "kinda fullscreen."
const FOCUS_MODE_PAD: f32 = 16.0;
const FOCUS_PAD: f32 = 5.0;
const FOCUS_RADIUS: f32 = 10.0;
const FOCUS_STROKE: f32 = 2.0;
const FOCUS_STROKE_EDIT: f32 = 3.0;
const FOCUS_RING_ALPHA: u8 = 0x60;
const FOCUS_RING_ALPHA_EDIT: u8 = 0xff;
const FOCUS_SHADOW_ALPHA: u8 = 0x30;
const FOCUS_SHADOW_BLUR: f32 = 12.0;
const FOCUS_SHADOW_DY: f32 = 3.0;
const DOC_BOTTOM_PAD: f32 = 24.0;
const SCROLLBAR_INSET: f32 = 4.0;
const SCROLLBAR_WIDTH: f32 = 4.0;
/// Visual width while hovered or being dragged — wide enough to
/// reliably grab. The horizontal hit zone (`SCROLLBAR_HOVER_SLOP`) is
/// even wider so the cursor counts as "near" before reaching the bar
/// itself.
const SCROLLBAR_HOVER_WIDTH: f32 = 10.0;
/// Half-width of the "mouse is near the scrollbar" hit zone, measured
/// from the wide-bar centerline. Anything inside this zone wakes the
/// bar up (forces alpha to full + widens the thumb) so the user can
/// see what they're aiming at before clicking.
const SCROLLBAR_HOVER_SLOP: f32 = 14.0;
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
const SCROLLBAR_HOLD: Duration = Duration::from_millis(800);
const SCROLLBAR_FADE: Duration = Duration::from_millis(700);
const ZOOM_STEP: f32 = 1.1;
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 3.0;
const COALESCE_INTERVAL: Duration = Duration::from_millis(600);

/// Tag name used to mark a cell as a "person" — its heading title shows up
/// in the `@`-mention popup. Convention: `# Alice Smith #person`.
#[allow(dead_code)]
const PERSON_TAG: &str = "person";

/// Split a cell title's text into `(name_part, trailing_tags)` so a rename
/// can substitute the name without losing tags. Tags are recognized as a
/// trailing run of whitespace-delimited `#word` tokens (matching the
/// persistence layer's parse_trailing_tags). Both pieces have their
/// surrounding whitespace stripped at the boundary; `tags` retains its
/// `#` prefix and any internal spacing.
fn split_title_name_and_tags(text: &str) -> (String, String) {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    let mut tags_start = end;
    loop {
        while end > 0 && (bytes[end - 1] as char).is_whitespace() {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        let mut start = end;
        while start > 0 && !(bytes[start - 1] as char).is_whitespace() {
            start -= 1;
        }
        if start < end && bytes[start] == b'#' {
            tags_start = start;
            end = start;
        } else {
            break;
        }
    }
    let mut name_end = tags_start;
    while name_end > 0 && (bytes[name_end - 1] as char).is_whitespace() {
        name_end -= 1;
    }
    (text[..name_end].to_string(), text[tags_start..].to_string())
}

/// Last-frame hit-test rects, populated during render and consumed by
/// `mouse_down`. Grouped by UI surface; each substruct's contents are
/// only meaningful while that surface is on screen and are cleared (or
/// overwritten) at the start of every render that owns them.
#[derive(Default)]
struct HitTestState {
    sidebar: SidebarHits,
    cell_menu: CellMenuHits,
    bar_menu: BarMenuHits,
    tag_menu: TagMenuHits,
    people_menu: PeopleMenuHits,
    entity_page: EntityPageHits,
    people_page: PeoplePageHits,
    mention_popup: MentionPopupHits,
    /// Per-frame list of (cell_id, left-bar rect) populated by
    /// `render_cell_stream`. The bar is the "select whole cell"
    /// click target and the anchor for the bar context menu
    /// (Delete, info, cell-level Snooze).
    cell_bars: Vec<(Uuid, Rect)>,
    /// Per-pane URL-bar pill rect (window coords), populated by
    /// `render_pane_header`. Clicks on this rect focus the pane's
    /// `header.textbox` for editing.
    pane_headers: Vec<(usize, Rect)>,
    /// Per-pane result-row rects for the URL-bar dropdown.
    /// `header_results[i] = (pane_idx, rects)` where the index into
    /// `rects` matches `pane.header.cached_results[i]`.
    header_results: Vec<(usize, Vec<Rect>)>,
}

#[derive(Default)]
struct SidebarHits {
    /// Context-row rects (window coords).
    contexts: Vec<(Uuid, Rect)>,
    /// Date-header rects.
    dates: Vec<(chrono::NaiveDate, Rect)>,
    /// Tag-row rects.
    tags: Vec<(String, Rect)>,
    /// PAGES section row rects — dispatched to `push_view(...)`.
    pages: Vec<(PageKind, Rect)>,
    /// Relative-week rows (This Week, Last Week) above the date list.
    /// Each entry is the time filter the click should activate.
    weeks: Vec<(query::TimeFilter, Rect)>,
    /// "Show archived" toggle pill at the bottom of the sidebar.
    /// Clicks flip `KeptApp::show_inactive_cells`. None when the
    /// sidebar didn't render this frame.
    show_inactive_toggle: Option<Rect>,
}

/// Rolling window the Current view surfaces. Cells older than this
/// (by `cell.timestamp`) drop out — Current is a working-attention
/// surface, not an open-loop archive.
const CURRENT_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Width of the colored left-edge state bar on every cell. Narrow
/// enough to read as a slim accent strip without dominating the
/// cell, but combined with `CELL_BAR_GAP` (= FOCUS_PAD) the hit
/// rect is `CELL_BAR_W + CELL_BAR_GAP` wide — still clickable.
/// For the rare whole-cell menu access without aiming at the bar,
/// Ctrl+right-click anywhere on the cell opens the BarContextMenu.
const CELL_BAR_W: f32 = 8.0;
/// Gap between the bar and the cell card's chrome. Set to
/// `FOCUS_PAD` so the bar's right edge lands exactly on the
/// chrome's left edge (outline.left / wrapper.left / focus_ring.left
/// all equal `bar.right`), making the bar visually flush against
/// whichever chrome the cell uses.
const CELL_BAR_GAP: f32 = FOCUS_PAD;

/// Ordered list of snooze presets the cell / bullet right-click menu
/// surfaces. Index alignment matters: `CellMenuHits.snooze[i]` is the
/// rect for `SNOOZE_PRESETS[i]`.
const SNOOZE_PRESETS: [(crate::attention::SnoozePreset, &str); 6] = [
    (crate::attention::SnoozePreset::LaterToday, "Snooze: Later today"),
    (crate::attention::SnoozePreset::Tomorrow, "Snooze: Tomorrow"),
    (crate::attention::SnoozePreset::NextWeek, "Snooze: Next week"),
    (crate::attention::SnoozePreset::NextMonth, "Snooze: Next month"),
    (crate::attention::SnoozePreset::NextQuarter, "Snooze: Next quarter"),
    (crate::attention::SnoozePreset::Someday, "Snooze: Someday"),
];

#[derive(Default)]
struct CellMenuHits {
    /// Always present when the menu is open.
    surface: Option<Rect>,
    /// `None` when the right-click didn't hit a bullet (non-outline cell,
    /// or outline whitespace).
    surface_subtree: Option<Rect>,
    /// "Close" / "Reopen" — toggles `Cell.closed_at`. Always
    /// present.
    toggle_cell_active: Option<Rect>,
    /// "Close sub-outline" / "Reopen sub-outline" — toggles the
    /// clicked bullet's `closed_at`. Only present when the click
    /// landed on a bullet that lives in *this* cell's outline (not
    /// inside a nested embed).
    toggle_bullet_active: Option<Rect>,
    /// Six "Snooze: …" rows when the cell isn't currently snoozed;
    /// indices align with `SNOOZE_PRESETS` (LaterToday, Tomorrow,
    /// NextWeek, NextMonth, NextQuarter, Someday). The
    /// `snooze_targets_bullet` flag below tells the dispatch path
    /// whether these apply to the bullet (when the menu opened on
    /// one) or to the cell.
    snooze: [Option<Rect>; 6],
    /// "Unsnooze" — present iff the target (cell or bullet) is
    /// currently snoozed.
    unsnooze: Option<Rect>,
    /// When true, the snooze + unsnooze rects target the bullet at
    /// `cell_context_menu.bullet_id`; when false, they target the
    /// cell.
    snooze_targets_bullet: bool,
}

/// Right-click on a cell's left-edge bar opens this menu. Always
/// operates on the whole cell — never on a bullet — so unlike the
/// body's `CellMenuHits` there's no `*_bullet` variant.
#[derive(Default)]
struct BarMenuHits {
    /// "Surface as reference" — always present. Surfaces the
    /// whole cell (or the Reference's preserved target, for
    /// Reference cells) into the current writable context.
    surface: Option<Rect>,
    /// "Copy reference" — writes a `KeptPayload::Reference` to
    /// the OS clipboard. Default paste = inline `kept://` link;
    /// Ctrl+Shift+V paste = a fresh Reference cell.
    copy_reference: Option<Rect>,
    snooze: [Option<Rect>; 6],
    unsnooze: Option<Rect>,
    /// "Envelope" — transforms a Reference cell into an envelope
    /// outline (preserving id + timestamp). `Some` only when the
    /// bar menu opened on a Reference cell.
    envelope: Option<Rect>,
    /// "Unwrap envelope" — inverse of Envelope; turns an envelope
    /// back into a bare Reference. `Some` only when the bar menu
    /// opened on an envelope outline.
    unwrap: Option<Rect>,
    delete: Option<Rect>,
}

#[derive(Default)]
struct TagMenuHits {
    delete: Option<Rect>,
}

#[derive(Default)]
struct PeopleMenuHits {
    rename: Option<Rect>,
    /// `None` when the entity isn't deletable (the row still renders but
    /// click is suppressed).
    delete: Option<Rect>,
}


#[derive(Default)]
struct MentionPopupHits {
    /// Per-row rects (window coords) — index aligns with the popup's
    /// filtered candidate list. Mouse click dispatches to the matching
    /// candidate. Cleared each frame the popup renders.
    rows: Vec<Rect>,
    /// "Add @X" / "Add #X" row rect, populated only when the typed
    /// query produced no matches. Mouse-only — keyboard Enter never
    /// reaches it (Enter without a match dismisses without commit).
    add_row: Option<Rect>,
}

#[derive(Default)]
struct EntityPageHits {
    /// "+ Create backing cell" button rect (doc coords). `Some` only when
    /// the current view is `Entity(eid)` and the entity has no
    /// `primary_cell_id`.
    create_button: Option<Rect>,
    /// "REFERENCED IN" embed-card rects paired with the source cell ids
    /// they point at. Cleared on every entity-page render and repopulated.
    refs: Vec<(Uuid, Rect)>,
    /// Active/inactive toggle rect. `Some` only while in `ViewKind::Entity(_)`.
    active_toggle: Option<Rect>,
}

#[derive(Default)]
struct PeoplePageHits {
    /// Row rects (entity_id, doc-space). Used to route clicks into entity
    /// nav or rename.
    rows: Vec<(Uuid, Rect)>,
    /// "+ Add person…" footer-row rect (doc coords). `None` when the
    /// People page isn't active.
    add: Option<Rect>,
    /// "Show inactive" toggle rect.
    show_inactive_toggle: Option<Rect>,
}

/// Which kind of cell to spawn from a "new cell" hotkey.
#[derive(Clone, Copy)]
enum NewCellKind {
    Plain,
    Outline,
    PopPop,
}

/// Sidebar PAGES section row identity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PageKind {
    People,
    /// Non-chronological "what's on my plate right now" pile.
    /// Surfaces snooze-expired cells/bullets plus recently-inserted
    /// reference / envelope cells.
    Current,
}

/// In-progress inline rename of a People-page row. While `Some`, the row's
/// static label is replaced by `input.tick(...)` and Enter / Esc / clicks
/// outside drive commit / cancel.
struct PeopleRenameState {
    entity_id: Uuid,
    input: TextBox,
}

/// What the user is viewing in the doc area / highlighting in the sidebar.
///
/// - `Ast` — filter cells through `Query.ast` (the v0.1 query language).
///   Empty AST matches every cell.
/// - `Context(uuid)` — show that context's `[start, end)` window. Used by
///   the sidebar's context rows + rotation flow. Doesn't fit the spec's
///   time grammar cleanly so it's a dedicated escape hatch.
/// - `Entity(uuid)` — entity page for that entity (header + backing-cell
///   section). Cells loop is bypassed; the page is rendered bespoke.
/// - `People` — directory of `kind=person` entities. Bespoke render.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
enum ViewKind {
    #[default]
    Ast,
    Context(Uuid),
    Entity(Uuid),
    People,
    /// Non-chronological pile of items currently competing for
    /// attention. Filter and sort live in `render_current_stream`,
    /// not in `is_visible_for_view`.
    Current,
    /// Single-cell view: just `cell_id` in isolation, with no
    /// surrounding timeline context. Reached by clicking a
    /// `kept://<cell>` link, navigating from a Reference cell, or
    /// committing a cell-result from search. Renders through the
    /// standard cell-stream pipeline; `is_visible_for_view` reduces
    /// the cell list to one entry.
    Cell(Uuid),
}

/// What the user is viewing in the doc area / highlighting in the sidebar.
/// `view_kind` is the discriminator; `ast` is consulted only when
/// `view_kind == Ast`. Decoupled from the writable-target context (which
/// is always the most recent open one).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Query {
    view_kind: ViewKind,
    ast: query::Ast,
}

impl Query {
    #[allow(dead_code)]
    fn empty() -> Self {
        Self::default()
    }
    fn context(id: Uuid) -> Self {
        Self { view_kind: ViewKind::Context(id), ast: query::Ast::default() }
    }
    fn date(d: chrono::NaiveDate) -> Self {
        let mut ast = query::Ast::default();
        ast.include.time = Some(query::TimeFilter::Day(d));
        Self { view_kind: ViewKind::Ast, ast }
    }
    fn this_week() -> Self {
        let mut ast = query::Ast::default();
        ast.include.time = Some(query::TimeFilter::ThisWeek);
        Self { view_kind: ViewKind::Ast, ast }
    }
    fn last_week() -> Self {
        let mut ast = query::Ast::default();
        ast.include.time = Some(query::TimeFilter::LastWeek);
        Self { view_kind: ViewKind::Ast, ast }
    }
    fn tag(name: String) -> Self {
        let mut ast = query::Ast::default();
        ast.include.tags = vec![name.to_lowercase()];
        Self { view_kind: ViewKind::Ast, ast }
    }
    fn entity(id: Uuid) -> Self {
        Self { view_kind: ViewKind::Entity(id), ast: query::Ast::default() }
    }
    #[allow(dead_code)]
    fn people() -> Self {
        Self { view_kind: ViewKind::People, ast: query::Ast::default() }
    }
    fn current() -> Self {
        Self { view_kind: ViewKind::Current, ast: query::Ast::default() }
    }
    fn cell(id: Uuid) -> Self {
        Self { view_kind: ViewKind::Cell(id), ast: query::Ast::default() }
    }
    fn from_text(input: &str) -> Self {
        Self { view_kind: ViewKind::Ast, ast: query::parse(input) }
    }
    /// Context id when in Context view; None otherwise. Convenience for
    /// the prev/next-context navigation that only operates in that view.
    fn context_view(&self) -> Option<Uuid> {
        match self.view_kind {
            ViewKind::Context(id) => Some(id),
            _ => None,
        }
    }
    /// Round-trip back into a query-language string. None for any non-Ast
    /// view (no clean text representation).
    #[allow(dead_code)]
    fn to_text(&self) -> Option<String> {
        if !matches!(self.view_kind, ViewKind::Ast) {
            return None;
        }
        Some(query::to_text(&self.ast))
    }
    /// True when the active view is exactly `#name` (sidebar tag-row
    /// highlighting). Excludes non-Ast views, multi-filter queries, etc.
    fn is_solo_tag(&self, name: &str) -> bool {
        matches!(self.view_kind, ViewKind::Ast)
            && self.ast.exclude.tags.is_empty()
            && self.ast.exclude.entities.is_empty()
            && self.ast.include.entities.is_empty()
            && self.ast.include.time.is_none()
            && self.ast.text.is_empty()
            && self.ast.include.tags.len() == 1
            && self
                .ast
                .include
                .tags
                .first()
                .map(|t| t.eq_ignore_ascii_case(name))
                .unwrap_or(false)
    }
    /// Sidebar-date highlighting: true when the AST is exactly `Day(d)`
    /// with no other filters / text in an Ast view.
    fn is_solo_date(&self, d: chrono::NaiveDate) -> bool {
        self.is_solo_time(query::TimeFilter::Day(d))
    }
    /// Sidebar-time highlighting: true when the AST is exactly the
    /// given time filter with nothing else set. Used for both the
    /// per-day rows and the This Week / Last Week rows.
    fn is_solo_time(&self, filter: query::TimeFilter) -> bool {
        matches!(self.view_kind, ViewKind::Ast)
            && self.ast.exclude.tags.is_empty()
            && self.ast.exclude.entities.is_empty()
            && self.ast.include.tags.is_empty()
            && self.ast.include.entities.is_empty()
            && self.ast.text.is_empty()
            && self.ast.include.time.as_ref() == Some(&filter)
    }
}

/// View transform applied after a context rotation. If the user was viewing
/// the rotated-out context, follow them to the new one. Other views (AST,
/// entity, people) stay put — their content doesn't depend on the rotation
/// target.
fn rotate_view_to(prev: &Query, new_context_id: Uuid) -> Query {
    if matches!(prev.view_kind, ViewKind::Context(_)) {
        Query::context(new_context_id)
    } else {
        prev.clone()
    }
}

/// Context-level consequence of a cell deletion that emptied a context's
/// window. Stored on `UndoOp::DeleteCell` so undo/redo restore atomically.
#[derive(Clone)]
enum ContextSideEffect {
    /// A closed context became empty and was removed. Undo restores it.
    ContextRemoved {
        context: Context,
        prev_view: Query,
        new_view: Query,
    },
    /// The open context became empty; its `start_time` was reset to "now"
    /// so the sidebar label and visible window track the resumption.
    StartReset {
        context_id: Uuid,
        prev_start: i64,
        new_start: i64,
    },
}

enum UndoOp {
    CellEdit {
        cell_id: Uuid,
        pre: CellSnapshot,
        post: CellSnapshot,
    },
    InsertCell {
        cell_id: Uuid,
        snapshot: CellSnapshot,
        pre_focused: Option<Uuid>,
    },
    DeleteCell {
        cell_id: Uuid,
        snapshot: CellSnapshot,
        pre_focused: Option<Uuid>,
        post_focused: Option<Uuid>,
        /// If this delete emptied the cell's context, what to do with it.
        side_effect: Option<ContextSideEffect>,
    },
    RotateContext {
        /// The context that got closed (formerly the most recent open).
        closed_id: Uuid,
        /// Its `end_time` before rotation (usually `None`).
        prev_end_time: Option<i64>,
        /// The `end_time` written when rotation applied.
        new_end_time: i64,
        /// The freshly-created open context. Re-inserted on redo, removed on undo.
        new_context: Context,
        prev_view: Query,
        new_view: Query,
        pre_focused: Option<Uuid>,
        pre_scroll_y: f32,
    },
    /// Rotation on an already-empty active context: bumps the context's
    /// `start_time` to "now" instead of creating another empty context.
    ResetContextStart {
        context_id: Uuid,
        prev_start: i64,
        new_start: i64,
        prev_view: Query,
        new_view: Query,
    },
    /// People-page rename: swap `display_name` (entity row + alias rows)
    /// and, when the entity has a backing cell, swap the cell's title
    /// text. Captures both halves so undo / redo flip atomically; the
    /// backing cell is also marked dirty so the next flush persists the
    /// title change to disk.
    RenamePersonEntity {
        entity_id: Uuid,
        prev_name: String,
        new_name: String,
        /// `(cell_id, prev_title_text, new_title_text)` when a backing
        /// cell's title was rewritten as part of the rename. None for
        /// cell-less entities.
        cell_title_change: Option<(Uuid, String, String)>,
    },
    /// People-page "Add person" (cell-less entity creation). Undo drops
    /// the row by id; redo re-inserts with the same id + name +
    /// created_at so any `kept://<id>` mentions written between create
    /// and undo stay valid through the round-trip.
    CreateCelllessEntity {
        entity_id: Uuid,
        name: String,
        created_at: i64,
    },
    /// People-page "Delete person." Captures the row's identity so undo
    /// re-inserts it; redo deletes again. Pre-condition (enforced at
    /// menu open time) is no backing cell + zero `kept://` mentions —
    /// without that, a deleted-then-undone entity would still leave
    /// dangling references in the live DB. Also captures `is_active` so
    /// an inactive person comes back inactive on undo.
    DeleteCelllessEntity {
        entity_id: Uuid,
        name: String,
        is_active: bool,
        created_at: i64,
    },
    /// Entity-page active/inactive toggle. Undo flips `is_active` to
    /// `prev`; redo flips to `new`. No focus side-effects.
    SetEntityActive {
        entity_id: Uuid,
        prev: bool,
        new: bool,
    },
    /// Cell close/reopen (the "archive" gesture, reframed). Sets
    /// `Cell.closed_at` to `prev` or `new` (each `Some(t)` ↔ closed
    /// at epoch ms `t`, `None` ↔ open). Pure metadata — does NOT
    /// touch `edited_at`, so attention sort isn't perturbed by an
    /// archive action.
    SetCellClosed {
        cell_id: Uuid,
        prev: Option<i64>,
        new: Option<i64>,
    },
    /// Snooze / clear-snooze on a cell. Sets `Cell.resurface_after`
    /// to `prev` or `new`. Pure metadata; no `edited_at` bump.
    SetCellResurface {
        cell_id: Uuid,
        prev: Option<i64>,
        new: Option<i64>,
    },
    /// Bullet close/reopen (cascades to its sub-outline via
    /// `compute_effective_open`). Bullet ids are unique within a
    /// cell; the lookup is a linear scan.
    SetBulletClosed {
        cell_id: Uuid,
        bullet_id: Uuid,
        prev: Option<i64>,
        new: Option<i64>,
    },
    /// Bullet snooze / clear-snooze. Sets the bullet's
    /// `resurface_after`. Metadata-only — no `edited_at` bump.
    SetBulletResurface {
        cell_id: Uuid,
        bullet_id: Uuid,
        prev: Option<i64>,
        new: Option<i64>,
    },
    /// "Envelope" action: replaces a Reference cell in place with an
    /// Outline cell whose first slot is the original embed. Cell id /
    /// timestamp are preserved, but the variant changes — `Cell::restore`
    /// can't round-trip that via `CellEdit`, so envelope gets its own
    /// op with both snapshots and a kind-rebuild on apply.
    Envelope {
        cell_id: Uuid,
        pre: CellSnapshot,
        post: CellSnapshot,
        pre_focused: Option<Uuid>,
    },
    /// Inverse of `Envelope`: turn an envelope outline back into a
    /// bare Reference at the same id / timestamp. The user's bullet
    /// notes live in `pre`, so Ctrl+Z restores them verbatim. Kept
    /// distinct from `Envelope` so the redo arm replays the same
    /// direction the user originally intended.
    Unwrap {
        cell_id: Uuid,
        pre: CellSnapshot,
        post: CellSnapshot,
        pre_focused: Option<Uuid>,
    },
}

/// Which direction an `UndoOp::apply` is going. Undo restores the pre
/// state; Redo restores the post. For asymmetric ops (InsertCell vs
/// DeleteCell, Create vs Delete entity, rotate) the apply method
/// branches on this internally; for symmetric ops it just picks
/// between pre/post fields.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UndoDir {
    Undo,
    Redo,
}

impl UndoOp {
    /// Replay this op against `app` in `dir`. The single dispatch
    /// surface that replaces the previously-parallel `undo` and `redo`
    /// 13-arm match blocks (S6).
    ///
    /// Symmetric variants (CellEdit, ResetContextStart, Rename,
    /// SetEntityActive, SetCellClosed, SetCellResurface,
    /// SetBulletClosed, SetBulletResurface, Envelope, Unwrap) read
    /// `pre` vs `post` based on `dir`. Asymmetric
    /// variants (InsertCell ↔ DeleteCell, Create ↔ Delete cell-less
    /// entity, RotateContext) branch on `dir` inside the arm because
    /// the inverse direction calls a different helper rather than
    /// just swapping a snapshot.
    fn apply(&self, app: &mut KeptApp, dir: UndoDir) {
        match self {
            Self::CellEdit { cell_id, pre, post } => {
                let snap = match dir {
                    UndoDir::Undo => pre,
                    UndoDir::Redo => post,
                };
                app.pane_mut().focused = Some(*cell_id);
                if let Some(c) = app.cell_mut(*cell_id) {
                    c.restore(snap.clone());
                }
            }
            Self::InsertCell {
                cell_id,
                snapshot,
                pre_focused,
            } => match dir {
                UndoDir::Undo => {
                    app.document.queue_cell_delete(*cell_id);
                    app.pane_mut().focused = *pre_focused;
                }
                UndoDir::Redo => {
                    let cell = Cell::from_snapshot(*cell_id, snapshot.clone(), &app.typeface);
                    app.insert_cell_sorted(cell);
                    app.document.pending_deletes.remove(cell_id);
                    app.pane_mut().focused = Some(*cell_id);
                }
            },
            Self::DeleteCell {
                cell_id,
                snapshot,
                pre_focused,
                post_focused,
                side_effect,
            } => match dir {
                UndoDir::Undo => {
                    let cell = Cell::from_snapshot(*cell_id, snapshot.clone(), &app.typeface);
                    app.insert_cell_sorted(cell);
                    app.document.dirty_cells.insert(*cell_id);
                    app.document.pending_deletes.remove(cell_id);
                    if let Some(se) = side_effect {
                        app.reverse_context_side_effect(se);
                    }
                    app.pane_mut().focused = *pre_focused;
                }
                UndoDir::Redo => {
                    app.document.queue_cell_delete(*cell_id);
                    if let Some(se) = side_effect {
                        app.apply_context_side_effect(se);
                    }
                    app.pane_mut().focused = *post_focused;
                }
            },
            Self::RotateContext {
                closed_id,
                prev_end_time,
                new_end_time,
                new_context,
                prev_view,
                new_view,
                pre_focused,
                pre_scroll_y,
            } => match dir {
                UndoDir::Undo => {
                    app.inverse_rotation(
                        *closed_id,
                        *prev_end_time,
                        new_context.id,
                        prev_view.clone(),
                        *pre_focused,
                        *pre_scroll_y,
                    );
                }
                UndoDir::Redo => {
                    app.apply_rotation(
                        *closed_id,
                        *new_end_time,
                        new_context,
                        new_view.clone(),
                    );
                }
            },
            Self::ResetContextStart {
                context_id,
                prev_start,
                new_start,
                prev_view,
                new_view,
            } => {
                let (start, view) = match dir {
                    UndoDir::Undo => (*prev_start, prev_view),
                    UndoDir::Redo => (*new_start, new_view),
                };
                if let Some(c) = app.document.contexts.iter_mut().find(|c| c.id == *context_id) {
                    c.start_time = start;
                }
                app.document.mark_context_dirty(*context_id);
                app.pane_mut().view = view.clone();
            }
            Self::RenamePersonEntity {
                entity_id,
                prev_name,
                new_name,
                cell_title_change,
            } => {
                let name = match dir {
                    UndoDir::Undo => prev_name,
                    UndoDir::Redo => new_name,
                };
                if let Some(db) = app.db.as_mut() {
                    if let Err(e) = db.rename_person_entity(*entity_id, name) {
                        eprintln!("kept: {dir:?} rename_person_entity failed: {e}");
                    }
                }
                if let Some((cell_id, prev_title, new_title)) = cell_title_change {
                    let title_text = match dir {
                        UndoDir::Undo => prev_title,
                        UndoDir::Redo => new_title,
                    };
                    if let Some(cell) = app.cell_mut(*cell_id) {
                        if let Some(title) = cell.title_mut() {
                            title.replace_text(title_text.clone());
                        }
                    }
                    app.mark_cell_dirty(*cell_id);
                }
                app.refresh_entities();
            }
            Self::CreateCelllessEntity {
                entity_id,
                name,
                created_at,
            } => {
                match dir {
                    UndoDir::Undo => {
                        if let Some(db) = app.db.as_mut() {
                            if let Err(e) = db.delete_entity(*entity_id) {
                                eprintln!("kept: undo create-entity (delete) failed: {e}");
                            }
                        }
                    }
                    UndoDir::Redo => {
                        if let Some(db) = app.db.as_mut() {
                            // Add Person always creates an active entity, so a
                            // redo restores it active. (If the user toggled it
                            // inactive between create and undo, that's a
                            // separate SetEntityActive op on the stack with
                            // its own redo.)
                            if let Err(e) = db.insert_person_entity_with_id(
                                *entity_id,
                                name,
                                true,
                                *created_at,
                            ) {
                                eprintln!("kept: redo create-entity failed: {e}");
                            }
                        }
                    }
                }
                app.refresh_entities();
            }
            Self::DeleteCelllessEntity {
                entity_id,
                name,
                is_active,
                created_at,
            } => {
                match dir {
                    UndoDir::Undo => {
                        if let Some(db) = app.db.as_mut() {
                            if let Err(e) = db.insert_person_entity_with_id(
                                *entity_id,
                                name,
                                *is_active,
                                *created_at,
                            ) {
                                eprintln!("kept: undo delete-entity (insert) failed: {e}");
                            }
                        }
                    }
                    UndoDir::Redo => {
                        if let Some(db) = app.db.as_mut() {
                            if let Err(e) = db.delete_entity(*entity_id) {
                                eprintln!("kept: redo delete-entity failed: {e}");
                            }
                        }
                    }
                }
                app.refresh_entities();
            }
            Self::SetEntityActive { entity_id, prev, new } => {
                let target = match dir {
                    UndoDir::Undo => *prev,
                    UndoDir::Redo => *new,
                };
                if let Some(db) = app.db.as_mut() {
                    let _ = db.set_entity_active(*entity_id, target);
                }
                app.refresh_entities();
            }
            Self::SetCellClosed { cell_id, prev, new } => {
                let target = match dir {
                    UndoDir::Undo => *prev,
                    UndoDir::Redo => *new,
                };
                if let Some(idx) = app.cell_idx(*cell_id) {
                    app.document.cells[idx].closed_at = target;
                    app.mark_cell_dirty(*cell_id);
                    // No touch_cell: metadata-only op must not bump
                    // edited_at (would distort attention sort).
                }
            }
            Self::SetCellResurface { cell_id, prev, new } => {
                let target = match dir {
                    UndoDir::Undo => *prev,
                    UndoDir::Redo => *new,
                };
                if let Some(idx) = app.cell_idx(*cell_id) {
                    app.document.cells[idx].resurface_after = target;
                    app.mark_cell_dirty(*cell_id);
                }
            }
            Self::SetBulletClosed {
                cell_id,
                bullet_id,
                prev,
                new,
            } => {
                let target = match dir {
                    UndoDir::Undo => *prev,
                    UndoDir::Redo => *new,
                };
                if let Some(idx) = app.cell_idx(*cell_id) {
                    if let CellKind::Outline(oc) = &mut app.document.cells[idx].kind {
                        oc.set_bullet_closed_at(*bullet_id, target);
                    }
                    app.mark_cell_dirty(*cell_id);
                }
            }
            Self::SetBulletResurface {
                cell_id,
                bullet_id,
                prev,
                new,
            } => {
                let target = match dir {
                    UndoDir::Undo => *prev,
                    UndoDir::Redo => *new,
                };
                if let Some(idx) = app.cell_idx(*cell_id) {
                    if let CellKind::Outline(oc) = &mut app.document.cells[idx].kind {
                        oc.set_bullet_resurface_after(*bullet_id, target);
                    }
                    app.mark_cell_dirty(*cell_id);
                }
            }
            Self::Envelope {
                cell_id,
                pre,
                post,
                pre_focused,
            }
            | Self::Unwrap {
                cell_id,
                pre,
                post,
                pre_focused,
            } => {
                // Kind-changing ops: Cell::restore can't round-trip a
                // variant change, so rebuild from snapshot at the same
                // id. Both directions share the same shape; only the
                // snapshot and focus target swap.
                let (snap, focused) = match dir {
                    UndoDir::Undo => (pre, *pre_focused),
                    UndoDir::Redo => (post, Some(*cell_id)),
                };
                if let Some(idx) = app.cell_idx(*cell_id) {
                    let cell = Cell::from_snapshot(*cell_id, snap.clone(), &app.typeface);
                    app.document.cells[idx] = cell;
                }
                app.document.dirty_cells.insert(*cell_id);
                app.pane_mut().focused = focused;
            }
        }
    }

    /// True when applying this op should bump the focused cell's
    /// `edited_at` afterward — i.e., the op is a content edit, not
    /// metadata. The post-apply hook in `undo` / `redo` uses this to
    /// gate the `touch_cell(focused)` call (so e.g. an undo of "mark
    /// inactive" doesn't make the cell look freshly edited).
    fn bumps_focused_edited(&self) -> bool {
        matches!(
            self,
            Self::CellEdit { .. }
                | Self::InsertCell { .. }
                | Self::DeleteCell { .. }
                | Self::Envelope { .. }
                | Self::Unwrap { .. }
        )
    }
}

impl std::fmt::Debug for UndoDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoDir::Undo => f.write_str("undo"),
            UndoDir::Redo => f.write_str("redo"),
        }
    }
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

const SEED_CONTEXT_ID: Uuid = uuid!("01900000-0000-7000-8000-000000000001");
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Idle threshold after which the next cell creation rotates to a fresh
/// context. Edits to existing cells don't reset this — only new cells do.
const IDLE_CONTEXT_THRESHOLD: Duration = Duration::from_secs(15 * 60);

const SIDEBAR_WIDTH: f32 = 180.0;
const SIDEBAR_HEADER_FONT_SIZE: f32 = 13.0;

/// Context-section header drawn in Date view between cell groups from
/// different contexts. `H` covers `PAD_TOP + text + PAD_BOTTOM`. The bottom
/// pad must clear `FOCUS_PAD` on the cell that follows, plus breathing room.
const CONTEXT_HEADER_H: f32 = 50.0;
const CONTEXT_HEADER_PAD_TOP: f32 = 14.0;
const CONTEXT_HEADER_FONT_SIZE: f32 = 12.0;
/// Faint outline drawn around non-focused cells so each one reads as a
/// distinct unit on the page. The focused cell's blue ring + card backdrop
/// supersede this — only non-focused cells get the outline.
const CELL_OUTLINE_ALPHA: u8 = 0x28;
const CELL_OUTLINE_STROKE: f32 = 1.0;

/// Entity page layout. Title is a large heading with `display_name`;
/// metadata line below it carries kind + alias; section headers borrow
/// the sidebar header styling.
const ENTITY_TITLE_FONT_SIZE: f32 = 26.0;
const ENTITY_META_FONT_SIZE: f32 = 13.0;
const ENTITY_SECTION_GAP: f32 = 32.0;
const ENTITY_SECTION_HEADER_GAP: f32 = 14.0;
const ENTITY_CREATE_BTN_H: f32 = 36.0;
const ENTITY_CREATE_BTN_W: f32 = 220.0;

/// People-page layout. Single-column rows with hairline dividers; a
/// muted "Add person…" footer pinned at the bottom of the list. Row
/// font size matches `cell.rs::BODY_FONT_SIZE` so the embedded rename /
/// add `TextBox` (whose font scale is the app's) renders at exactly
/// the same size as the static row text — same glyphs, same baseline.
const PEOPLE_ROW_H: f32 = 36.0;
const PEOPLE_ROW_PAD_X: f32 = 12.0;
const PEOPLE_ROW_FONT_SIZE: f32 = 18.0;

/// Reference-cell embed wrapper. Warm-tan dashed border with a faint warm
/// background tint, plus a muted footer line ("↗ originally <date>") so the
/// embed reads as "not the original; click for the source."
const EMBED_INSET: f32 = 8.0;
const EMBED_PAD: f32 = 6.0;
const EMBED_FOOTER_H: f32 = 18.0;
const EMBED_FOOTER_FONT_SIZE: f32 = 12.0;
/// Gap between the envelope outline's header embed and the bullet
/// body underneath. Same shape as `TITLE_BODY_GAP` in `cell/common.rs`
/// (kept here to avoid re-exporting that constant — they happen to
/// share a value but their roles are distinct).
const ENVELOPE_HEADER_GAP: f32 = 6.0;

/// Cursor displacement (logical px) at which an Alt-down click
/// promotes from a tentative click to a committed pan-drag. Below
/// this, the gesture stays a regular Alt+click (multi-cursor add /
/// link-open-in-other-pane); past it, the cell-level drag-state is
/// aborted and pan takes over. Sized to comfortably swallow trackpad
/// jitter — a few px of accidental motion shouldn't yank the doc.
const ALT_PAN_THRESHOLD: f32 = 6.0;

/// Total visible time before a transient toast pill starts fading.
/// Sized to read at a glance without lingering. See `Toast`.
const TOAST_HOLD: Duration = Duration::from_millis(1800);
/// Fade-out duration following `TOAST_HOLD`. The toast is fully gone
/// once `shown_at + TOAST_HOLD + TOAST_FADE` has elapsed.
const TOAST_FADE: Duration = Duration::from_millis(500);

/// Brief on-screen confirmation message ("Surfaced", etc.), rendered
/// as a pill at the bottom-center of the window with a fade-out
/// tail. Lives on `KeptApp::toast`; set via `show_toast`. Replaces
/// any pre-existing toast (no queue — the latest action wins).
#[derive(Clone)]
struct Toast {
    message: String,
    shown_at: Instant,
}

/// Maximum nesting depth for embed previews. When an envelope outline
/// is itself the target of a reference, its header (the inner embed)
/// is rendered recursively up to this many levels deep. Beyond the
/// cap, the deepest level shows an "embed depth limit" placeholder
/// instead of recursing further. Counts the user-facing reference as
/// level 0, so MAX = 4 means up to four nested dashed-border embeds
/// can stack before the placeholder appears.
const MAX_EMBED_DEPTH: usize = 4;

/// Pane divider (gutter between left and right panes). 6 px wide, painted
/// in the same separator tone as the sidebar's right edge. Hover within
/// ±DIVIDER_HIT_SLOP px in x grabs it for drag.
const DIVIDER_THICKNESS: f32 = 6.0;
const DIVIDER_HIT_SLOP: f32 = 4.0;
/// Active-pane indicator border thickness.
const PANE_BORDER_STROKE: f32 = 2.0;
/// Min/max for `split_ratio` so a pane can't shrink below ~15% of width.
const SPLIT_MIN: f32 = 0.15;
const SPLIT_MAX: f32 = 0.85;

/// How long the Ctrl+W pane chord stays armed waiting for a follow-up key
/// before auto-cancelling. Long enough to be forgiving of stray presses,
/// short enough not to swallow real keystrokes much later.
const PANE_CHORD_TIMEOUT: Duration = Duration::from_secs(2);

// ----- Kinetic scrolling -----
//
// Wheel events apply their dy directly (instant feedback) AND blend
// into a running velocity. Each frame we additionally integrate
// `velocity * dt_since_last_scroll_apply` — when a wheel event just
// fired, dt is ~0 so no double-apply; when the wheel goes quiet, dt
// climbs to a frame's worth and the page coasts on its accumulated
// velocity. No engage window, no visible pause.

/// Constant deceleration applied each frame: velocity_magnitude -=
/// KINETIC_FRICTION * dt. Linear stopping (vs. exponential decay) gives
/// a predictable, finite coast time: a 2000 px/s coast stops in ~0.7 s,
/// a 1000 px/s coast in ~0.3 s. Important on platforms where
/// finger-rest-without-motion isn't an observable event (most Linux
/// libinput configs) — the user can't manually interrupt, so the coast
/// just needs to be short enough that they don't want to.
const KINETIC_FRICTION: f32 = 3000.0;
/// Stop the kinetic decay when speed drops below this (logical px/sec).
const KINETIC_MIN_VELOCITY: f32 = 8.0;
/// Cap on velocity in either direction. Trackpad bursts can produce
/// pathological dy/dt readings (two events 10 µs apart with 30 px →
/// huge velocities); clamping here keeps a single bad sample from
/// teleporting the page.
const KINETIC_MAX_VELOCITY: f32 = 2500.0;
/// Window over which recent wheel events are averaged to estimate
/// velocity. Smooths out trackpad event jitter — short enough to track
/// finger motion, long enough to not be dominated by one noisy sample.
const KINETIC_VELOCITY_WINDOW: Duration = Duration::from_millis(80);
/// Largest gap between consecutive wheel events that still counts as the
/// same gesture. Beyond this, a new event is treated as a fresh gesture
/// — and if a coast is in progress, that fresh gesture interrupts it.
const KINETIC_BURST_GAP: Duration = Duration::from_millis(100);

/// Forward-compat: the orientation of the (eventual) split. Always `Horiz`
/// in v1 (left/right). When vertical splits land, this enum gains meaning
/// without renaming.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitDir {
    Horiz,
    Vert,
}

/// One scrollable region's complete state: position, bounds, kinetic
/// coast, and the fade timer that drives its scrollbar. Owned by panes
/// (one per pane) and by the app (one for the sidebar). All scrolling
/// surfaces use this — keeps wheel handling, kinetic decay, scrollbar
/// rendering, and interrupt rules identical everywhere.
pub struct Scroller {
    /// Vertical scroll offset in logical px.
    scroll_y: f32,
    /// Upper bound for `scroll_y`. Caller updates each frame via
    /// `set_max_scroll(content_h - viewport_h)`.
    max_scroll: f32,
    /// Last time `scroll_y` changed — drives the scrollbar fade
    /// (`scrollbar_alpha`).
    last_scroll_time: Option<Instant>,
    /// Kinetic velocity in px/sec. Estimated from the recent wheel
    /// burst; bled off by `step_kinetic`. Sign: positive = scrolling
    /// toward higher `scroll_y` (visually downward).
    scroll_velocity_y: f32,
    /// Recent scroll-delta samples within `KINETIC_VELOCITY_WINDOW`,
    /// shared by every input that contributes to kinetic state — wheel
    /// events and scrollbar-thumb drags. Smooths the velocity estimate
    /// against trackpad jitter (microsecond-gap events would otherwise
    /// produce runaway dy/dt). Pruned at the head whenever a sample is
    /// pushed or velocity is recomputed.
    recent_samples: VecDeque<(Instant, f32)>,
    /// Wall-clock time the most recent scroll change was applied —
    /// either by `apply_wheel` or by `step_kinetic`. Anchors the dt
    /// for kinetic integration so a frame triggered by the wheel
    /// itself doesn't double-apply velocity.
    last_scroll_apply_at: Option<Instant>,
    /// Last thumb rect (window coords) drawn by `draw_bar`. Used by
    /// mouse_down for hit-testing — empty rect when the bar wasn't
    /// drawn this frame (no content overflow, or fully faded).
    last_thumb_rect: Rect,
    /// Last track band: (top_y, bot_y, viewport_h, content_h, bar_center_x)
    /// from the most recent draw. Needed to translate a drag in
    /// window-y back into a scroll position. None when the bar wasn't
    /// drawn (no overflow / fully faded — neither hover nor drag is
    /// meaningful then).
    last_bar_geom: Option<BarGeom>,
    /// True while the cursor is inside the wide "near the scrollbar"
    /// hit zone. Drives both the bar's visual widening and forcing
    /// alpha to full in `draw_bar`. Set externally by `set_hover`
    /// from the cursor-move handler.
    hover: bool,
    /// `Some(grab_offset_y)` while the user is dragging the thumb.
    /// `grab_offset_y` is the y-distance from the thumb's top to the
    /// initial click point, kept constant through the drag so the
    /// thumb doesn't jump under the cursor on the first move.
    dragging: Option<f32>,
}

#[derive(Clone, Copy)]
struct BarGeom {
    track_top: f32,
    track_bot: f32,
    viewport_h: f32,
    content_h: f32,
    /// X-coordinate of the wide-bar centerline (constant whether the
    /// bar is currently drawn thin or wide).
    bar_center_x: f32,
}

impl Scroller {
    fn new() -> Self {
        Self {
            scroll_y: 0.0,
            max_scroll: 0.0,
            last_scroll_time: None,
            scroll_velocity_y: 0.0,
            recent_samples: VecDeque::new(),
            last_scroll_apply_at: None,
            last_thumb_rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            last_bar_geom: None,
            hover: false,
            dragging: None,
        }
    }

    /// Update the scroll bound and clamp the current position. If the
    /// content shrank below the prior scroll, this snaps back into
    /// range and zeroes any in-flight velocity (no point coasting
    /// against a wall).
    fn set_max_scroll(&mut self, max: f32) {
        let max = max.max(0.0);
        self.max_scroll = max;
        if self.scroll_y > max {
            self.scroll_y = max;
            self.scroll_velocity_y = 0.0;
        }
    }

    /// Apply a wheel event. Direct-applies `dy` to `scroll_y` and
    /// updates the kinetic state from the sample window. Returns true
    /// if anything redraw-relevant happened (scroll moved OR a coast
    /// was interrupted by a fresh gesture).
    fn apply_wheel(&mut self, dy: f32, phase: winit::event::TouchPhase) -> bool {
        use winit::event::TouchPhase;
        let now = Instant::now();

        // Interrupt detection. Two signals, in priority order:
        //
        // 1) `phase == Started`: trackpad fingers just touched. On
        //    Linux libinput this fires with dy=0 the moment the user
        //    rests fingers on the trackpad — the canonical "tap to
        //    stop" gesture. Kill the coast immediately.
        //
        // 2) Fallback for wheel mice (which don't carry meaningful
        //    phase): a wheel event after a >KINETIC_BURST_GAP idle
        //    counts as a fresh gesture and interrupts.
        let last_sample_at = self.recent_samples.back().map(|&(t, _)| t);
        let burst_gap_elapsed = match last_sample_at {
            Some(t) => now.duration_since(t) > KINETIC_BURST_GAP,
            None => true,
        };
        let interrupt = phase == TouchPhase::Started
            || (burst_gap_elapsed && self.scroll_velocity_y.abs() >= KINETIC_MIN_VELOCITY);
        if interrupt && self.scroll_velocity_y.abs() >= KINETIC_MIN_VELOCITY {
            self.scroll_velocity_y = 0.0;
            self.recent_samples.clear();
        }
        // A `Started` event with dy=0 (pure touch, no scroll motion) is
        // ONLY for interruption — don't pollute the velocity window or
        // attempt a no-op scroll.
        if phase == TouchPhase::Started && dy == 0.0 {
            return interrupt;
        }

        // Append this sample and recompute velocity from the window.
        // dy=0 is also pushed — libinput sends a dy=0 sentinel on
        // touchpad lift, and counting it correctly drops velocity to
        // zero if the user paused before releasing. The averaging /
        // pruning math lives on `recompute_velocity_from_window` so
        // wheel and scrollbar-thumb drag agree on the formula.
        self.recent_samples.push_back((now, dy));
        self.recompute_velocity_from_window(now);

        // Direct apply. Kinetic integration in `step_kinetic` uses dt
        // since the last scroll application — when the wheel just fired
        // (this very moment), that dt is ~0 so kinetic doesn't
        // double-apply.
        let new_y = (self.scroll_y + dy).clamp(0.0, self.max_scroll);
        if dy != 0.0 && new_y == self.scroll_y {
            // True bound hit (we asked to move and couldn't). Kill
            // velocity so kinetic doesn't spin against the wall after
            // the user releases. dy==0 events (libinput stop sentinels)
            // never reach this branch.
            self.scroll_velocity_y = 0.0;
            return false;
        }
        if new_y != self.scroll_y {
            self.scroll_y = new_y;
            self.last_scroll_time = Some(now);
            self.last_scroll_apply_at = Some(now);
            return true;
        }
        // dy=0 sentinel that didn't move us — still counts as activity
        // for the interrupt return path above.
        interrupt
    }

    /// Advance kinetic decay one frame. Integrates `velocity * dt`
    /// since the last scroll application, decays velocity by constant
    /// friction, and stops when speed drops below the floor or a
    /// bound is hit. Returns true if `scroll_y` moved.
    fn step_kinetic(&mut self) -> bool {
        // No kinetic while a drag is in flight (scrollbar thumb or
        // Alt-drag pan). The drag is direct control — the user's
        // cursor sets `scroll_y` exactly via `apply_thumb_drag`. The
        // velocity field still accumulates from drag samples so a
        // flick-and-release coasts, but until release we'd be
        // integrating that velocity ON TOP of the user's drag,
        // which fights the gesture (and inverts when the user
        // reverses direction faster than the velocity window prunes).
        if self.dragging.is_some() {
            return false;
        }
        if self.scroll_velocity_y.abs() < KINETIC_MIN_VELOCITY {
            self.scroll_velocity_y = 0.0;
            return false;
        }
        let now = Instant::now();
        let dt = match self.last_scroll_apply_at {
            Some(prev) => now.duration_since(prev).as_secs_f32(),
            None => 1.0 / 60.0,
        };
        let dt = dt.clamp(0.0, 0.05);
        let displacement = self.scroll_velocity_y * dt;
        let new_y = (self.scroll_y + displacement).clamp(0.0, self.max_scroll);
        if new_y == self.scroll_y {
            // Hit a bound. Stop coasting.
            self.scroll_velocity_y = 0.0;
            return false;
        }
        self.scroll_y = new_y;
        self.last_scroll_time = Some(now);
        self.last_scroll_apply_at = Some(now);
        // Constant friction: bleed off `KINETIC_FRICTION * dt` each
        // frame. Stop crisply when we'd cross zero rather than oscillate.
        let drop = KINETIC_FRICTION * dt;
        self.scroll_velocity_y = if self.scroll_velocity_y.abs() <= drop {
            0.0
        } else {
            self.scroll_velocity_y - drop * self.scroll_velocity_y.signum()
        };
        true
    }

    /// Halt any in-flight kinetic decay. Called from input handlers
    /// (mouse_down, key press) so user input always wins over coast.
    fn kill_kinetic(&mut self) {
        self.scroll_velocity_y = 0.0;
        self.recent_samples.clear();
        self.last_scroll_apply_at = None;
    }

    /// True iff the kinetic coast still has enough velocity to need
    /// another frame. `is_animating` aggregates this across panes.
    fn has_velocity(&self) -> bool {
        self.scroll_velocity_y.abs() >= KINETIC_MIN_VELOCITY
    }

    /// Draw the right-edge thumb-only scrollbar. Tucks `SCROLLBAR_INSET`
    /// inside `right_edge_x`, vertical inset top & bottom. No-op when
    /// content fits. Hover or active drag forces full opacity and a
    /// wider thumb so the cursor can grab it; otherwise the thumb fades
    /// per `scrollbar_alpha`. Records the drawn thumb rect on `self`
    /// for `mouse_down` hit-testing.
    fn draw_bar(
        &mut self,
        canvas: &Canvas,
        right_edge_x: f32,
        viewport_h: f32,
        content_h: f32,
        track_top_inset: f32,
    ) {
        if self.max_scroll <= 0.0 || content_h <= 0.0 {
            self.last_thumb_rect = Rect::new(0.0, 0.0, 0.0, 0.0);
            self.last_bar_geom = None;
            return;
        }
        // Track lives inside the body area: `track_top_inset` shifts
        // it down past the pane's header band so the scrollbar
        // doesn't intrude on the URL pill. `viewport_h` is the body
        // viewport (pane height minus the header), so the bottom of
        // the track lands at `track_top_inset + viewport_h - 6`.
        let track_top = track_top_inset + 6.0_f32;
        let track_bot = track_top_inset + viewport_h - 6.0;
        let track_len = (track_bot - track_top).max(1.0);
        let raw_thumb = (viewport_h / content_h) * track_len;
        let thumb_h = raw_thumb.max(SCROLLBAR_MIN_THUMB).min(track_len);
        let thumb_top =
            track_top + (self.scroll_y / self.max_scroll) * (track_len - thumb_h);
        let thumb_bot = thumb_top + thumb_h;

        // Wide-bar centerline anchors hover detection (and thin-bar
        // drawing too — both are aligned so the bar doesn't shift x as
        // it widens). `bar_x` always reads from this center.
        let center_x = right_edge_x - SCROLLBAR_INSET - SCROLLBAR_HOVER_WIDTH * 0.5;

        // Always remember geometry so mouse_down + cursor_moved have
        // something to hit-test against, even if we're about to skip
        // the visual draw because the fade has decayed.
        self.last_bar_geom = Some(BarGeom {
            track_top,
            track_bot,
            viewport_h,
            content_h,
            bar_center_x: center_x,
        });

        let active = self.hover || self.dragging.is_some();
        let raw_alpha = scrollbar_alpha(self.last_scroll_time);
        let alpha = if active { 1.0 } else { raw_alpha };
        if alpha <= 0.0 {
            // Faded out and nobody's hovering — record empty hit rect
            // (mouse_down won't grab; cursor proximity will revive it
            // on the next move via `set_hover_for_point`).
            self.last_thumb_rect = Rect::new(0.0, 0.0, 0.0, 0.0);
            return;
        }

        let width = if active {
            SCROLLBAR_HOVER_WIDTH
        } else {
            SCROLLBAR_WIDTH
        };
        let bar_x = center_x - width * 0.5;
        let thumb_rect = Rect::new(bar_x, thumb_top, bar_x + width, thumb_bot);

        let mut sb_paint = Paint::default();
        sb_paint.set_anti_alias(true);
        let alpha_byte = (alpha * 0xb0 as f32).round() as u8;
        sb_paint.set_color(crate::color::dark_alpha(alpha_byte));
        let r = width * 0.5;
        canvas.draw_round_rect(thumb_rect, r, r, &sb_paint);
        self.last_thumb_rect = thumb_rect;
    }

    /// Update the hover flag from a window-coord cursor position. The
    /// hover band is `bar_center_x ± SCROLLBAR_HOVER_SLOP` over the
    /// track's vertical range. Returns true if hover changed (caller
    /// can use this to schedule a redraw, since the bar's visual
    /// widening / fade-revival depends on hover state).
    fn set_hover_for_point(&mut self, x: f32, y: f32) -> bool {
        let new_hover = match self.last_bar_geom {
            Some(g) => {
                (x - g.bar_center_x).abs() <= SCROLLBAR_HOVER_SLOP
                    && y >= g.track_top
                    && y <= g.track_bot
            }
            None => false,
        };
        let changed = new_hover != self.hover;
        self.hover = new_hover;
        changed
    }

    /// If `(x, y)` lands inside the thumb's hit zone (using the wide
    /// rect for ergonomics — hover state isn't a precondition),
    /// returns `Some(grab_offset)` where `grab_offset = y -
    /// thumb_top`. The caller stores this as `dragging` so subsequent
    /// drag-to events can hold the thumb under the cursor without
    /// jumping.
    fn thumb_hit(&self, x: f32, y: f32) -> Option<f32> {
        let g = self.last_bar_geom?;
        // Use the wide hit-zone width regardless of current visual
        // width so a click on a thin bar still grabs cleanly.
        let hit_left = g.bar_center_x - SCROLLBAR_HOVER_WIDTH * 0.5;
        let hit_right = g.bar_center_x + SCROLLBAR_HOVER_WIDTH * 0.5;
        if x < hit_left || x > hit_right {
            return None;
        }
        let track_len = (g.track_bot - g.track_top).max(1.0);
        let raw_thumb = (g.viewport_h / g.content_h.max(1.0)) * track_len;
        let thumb_h = raw_thumb.max(SCROLLBAR_MIN_THUMB).min(track_len);
        let thumb_top =
            g.track_top + (self.scroll_y / self.max_scroll.max(1.0)) * (track_len - thumb_h);
        let thumb_bot = thumb_top + thumb_h;
        if y < thumb_top || y > thumb_bot {
            return None;
        }
        Some(y - thumb_top)
    }

    /// Begin a thumb drag — `grab_offset` comes from `thumb_hit`. Kills
    /// any in-flight kinetic coast (user is taking direct control)
    /// and pins hover on so the bar stays wide for the duration.
    fn start_thumb_drag(&mut self, grab_offset: f32) {
        self.kill_kinetic();
        self.dragging = Some(grab_offset);
        self.hover = true;
        self.last_scroll_time = Some(Instant::now());
    }

    /// Apply a thumb-drag motion: translates the new mouse y back into
    /// a scroll position. Records the resulting `dy` as a kinetic
    /// sample so velocity is populated when the user releases — a fast
    /// fling-and-release leaves enough velocity for `step_kinetic` to
    /// keep coasting after `end_thumb_drag`. No-op if not dragging or
    /// geometry is stale. Returns true if `scroll_y` changed.
    fn apply_thumb_drag(&mut self, y: f32) -> bool {
        let (Some(grab_offset), Some(g)) = (self.dragging, self.last_bar_geom) else {
            return false;
        };
        let track_len = (g.track_bot - g.track_top).max(1.0);
        let raw_thumb = (g.viewport_h / g.content_h.max(1.0)) * track_len;
        let thumb_h = raw_thumb.max(SCROLLBAR_MIN_THUMB).min(track_len);
        let scroll_range = (track_len - thumb_h).max(1.0);
        let desired_thumb_top = (y - grab_offset).clamp(g.track_top, g.track_top + scroll_range);
        let new_scroll = ((desired_thumb_top - g.track_top) / scroll_range) * self.max_scroll;
        let clamped = new_scroll.clamp(0.0, self.max_scroll);
        let dy = clamped - self.scroll_y;
        if dy.abs() < f32::EPSILON {
            return false;
        }
        self.scroll_y = clamped;
        let now = Instant::now();
        self.recent_samples.push_back((now, dy));
        self.recompute_velocity_from_window(now);
        self.last_scroll_time = Some(now);
        self.last_scroll_apply_at = Some(now);
        true
    }

    /// End a thumb drag. Recomputes velocity at release so a "drag,
    /// pause, then release" gesture doesn't fling on motion that
    /// already finished — handled by the shared
    /// `finalize_drag_release`. Caller usually pairs this with
    /// `set_hover_for_point` so the bar's wide-hover state matches
    /// whatever the cursor is over post-drag.
    fn end_thumb_drag(&mut self) -> bool {
        let was_dragging = self.dragging.take().is_some();
        if was_dragging {
            self.finalize_drag_release(Instant::now());
        }
        was_dragging
    }

    fn is_dragging_thumb(&self) -> bool {
        self.dragging.is_some()
    }

    /// Compute the grab offset for an Alt-drag pan starting at
    /// cursor `y`. Same shape as the offset captured by `thumb_hit`,
    /// but accepts ANY cursor y (not just inside the visible thumb)
    /// — the user grabs the doc as if the thumb were under the
    /// cursor. Returns None when geometry isn't ready (no overflow
    /// to scroll, or the bar hasn't drawn yet).
    fn pan_grab_offset_at(&self, y: f32) -> Option<f32> {
        let g = self.last_bar_geom?;
        let track_len = (g.track_bot - g.track_top).max(1.0);
        let raw_thumb = (g.viewport_h / g.content_h.max(1.0)) * track_len;
        let thumb_h = raw_thumb.max(SCROLLBAR_MIN_THUMB).min(track_len);
        let scroll_range = (track_len - thumb_h).max(1.0);
        let thumb_top = g.track_top
            + (self.scroll_y / self.max_scroll.max(1.0)) * scroll_range;
        Some(y - thumb_top)
    }

    /// Begin an Alt-drag pan. Captures the cursor's offset from the
    /// (conceptual) thumb at `click_y`, then routes through the same
    /// `dragging` field the scrollbar-thumb drag uses — so per-frame
    /// motion, velocity sampling, and fling-on-release all share the
    /// same code path. The user effectively "grabs the thumb" at
    /// wherever the cursor is, regardless of where the actual thumb
    /// lives. No-op when geometry isn't ready (no overflow to
    /// scroll).
    fn start_pan_drag(&mut self, click_y: f32) -> bool {
        let Some(grab_offset) = self.pan_grab_offset_at(click_y) else {
            return false;
        };
        self.start_thumb_drag(grab_offset);
        true
    }

    /// Drag-release fling-finalization, shared by every drag path
    /// (scrollbar thumb, Space-drag pan). Prunes stale samples
    /// relative to `now` (so a "drag, pause, release" gesture
    /// doesn't fling on motion that already ended), recomputes
    /// velocity from whatever survives, and anchors
    /// `last_scroll_apply_at` to `now` so the first kinetic step
    /// after release doesn't double-apply velocity from the last
    /// drag sample.
    fn finalize_drag_release(&mut self, now: Instant) {
        self.recompute_velocity_from_window(now);
        self.last_scroll_apply_at = Some(now);
    }

    /// Prune `recent_samples` to the velocity window relative to `now`,
    /// then set `scroll_velocity_y` from whatever survives. Called
    /// during drag (every move) and at release (where stale samples
    /// from a paused gesture get pruned away). Same averaging shape as
    /// `apply_wheel`'s inline computation, factored out so both
    /// drag-mid and drag-release paths agree on the math.
    fn recompute_velocity_from_window(&mut self, now: Instant) {
        let cutoff = now - KINETIC_VELOCITY_WINDOW;
        while self
            .recent_samples
            .front()
            .map_or(false, |&(t, _)| t < cutoff)
        {
            self.recent_samples.pop_front();
        }
        if self.recent_samples.is_empty() {
            self.scroll_velocity_y = 0.0;
            return;
        }
        let total_dy: f32 = self.recent_samples.iter().map(|&(_, d)| d).sum();
        let span = self
            .recent_samples
            .front()
            .map(|&(t, _)| now.duration_since(t).as_secs_f32())
            .unwrap_or(0.0)
            .max(0.016);
        let raw_v = total_dy / span;
        self.scroll_velocity_y = raw_v.clamp(-KINETIC_MAX_VELOCITY, KINETIC_MAX_VELOCITY);
    }
}

/// A viewport into the shared cell stream with its own focus, scroll,
/// edit, and navigation state. v1 has one Pane (Stage 1) → two (Stage 2);
/// future i3-style nesting replaces `Vec<Pane>` with a Layout tree but
/// leaves Pane internals untouched.
pub struct Pane {
    /// What this pane is showing (date / context / entity / people).
    view: Query,
    /// Focused cell within this pane's view (None if empty).
    focused: Option<Uuid>,
    /// Edit mode for the focused cell (false = view mode).
    editing: bool,
    /// In-pane mouse drag binding — drags belong to their origin pane.
    dragging_cell: Option<Uuid>,
    /// Pane's scroll state: position, bounds, kinetic, fade timer.
    /// `Pane` derefs to this so existing `pane.scroll_y`,
    /// `pane.max_scroll`, etc. accesses keep working unchanged.
    scroller: Scroller,
    doc_height: f32,
    viewport_height: f32,
    /// "Request scroll caret into view next frame" — honored at end of
    /// this pane's tick.
    pending_caret_scroll: bool,
    /// Undo coalesce-break for this pane's edit stream. Cross-pane edits
    /// or focus changes set this so the next edit begins a new undo entry.
    coalesce_break: bool,
    /// Per-pane back/forward navigation history.
    nav_back: Vec<HistoryEntry>,
    nav_forward: Vec<HistoryEntry>,
    /// Window-coord rect this pane occupies, populated by `tick`. Used by
    /// input dispatch (which pane was clicked) and overlay anchoring.
    #[allow(dead_code)]
    last_rect: Rect,
    /// Browser-style URL bar at the top of the pane. Doubles as the
    /// search input: when focused, suggestions drop under the pill
    /// (replacing the standalone Ctrl+K popup). See `pane::PaneHeader`.
    header: pane::PaneHeader,
}

impl std::ops::Deref for Pane {
    type Target = Scroller;
    fn deref(&self) -> &Scroller {
        &self.scroller
    }
}
impl std::ops::DerefMut for Pane {
    fn deref_mut(&mut self) -> &mut Scroller {
        &mut self.scroller
    }
}

impl Pane {
    fn new(typeface: Typeface, view: Query, focused: Option<Uuid>) -> Self {
        Self {
            view,
            focused,
            editing: false,
            dragging_cell: None,
            scroller: Scroller::new(),
            doc_height: 0.0,
            viewport_height: 0.0,
            pending_caret_scroll: false,
            coalesce_break: false,
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            last_rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            header: pane::PaneHeader::new(typeface),
        }
    }
}

pub struct KeptApp {
    typeface: Typeface,
    /// Source-of-truth cell stream + contexts + dirty/pending sets.
    /// Every mutation that needs to round-trip to the DB goes through
    /// `Document`'s API (insert_cell_sorted, touch_cell,
    /// queue_cell_delete, etc.) — those methods are the single place
    /// the dirty / pending sets get touched (S4: centralized dirty
    /// discipline).
    document: Document,
    /// Viewports. Length 1 in Stage 1; length 2 starting Stage 2. Future
    /// i3-style nesting replaces this with a Layout tree.
    panes: Vec<Pane>,
    /// Index into `panes` for the pane that owns keyboard input.
    active_pane: usize,
    /// Fractional split position (0.0..1.0) for the divider between panes.
    /// Lives on KeptApp (not Pane) because the divider sits *between* panes.
    /// Default 0.5; clamped to [0.15, 0.85] when dragged.
    #[allow(dead_code)]
    split_ratio: f32,
    /// True while the user is mouse-dragging the divider.
    #[allow(dead_code)]
    dragging_divider: bool,
    /// Reserved for future vertical splits — always `Horiz` in v1.
    #[allow(dead_code)]
    split_dir: SplitDir,
    /// `Some(armed_at)` while the pane chord (Ctrl+W) is awaiting a
    /// follow-up key. Cleared by the follow-up, by Esc, or by timeout.
    pane_chord_armed: Option<Instant>,
    font_scale: f32,
    undo_stack: Vec<UndoOp>,
    redo_stack: Vec<UndoOp>,
    last_edit_time: Option<Instant>,
    mention_popup: Option<MentionPopup>,
    /// Quick-Add modal (Ctrl+H / Ctrl+Shift+H). `Some` while open.
    /// See `app/quick_add.rs`.
    quick_add: Option<quick_add::QuickAddState>,
    clipboard: Option<Clipboard>,
    db: Option<Db>,
    /// Right-click context menu over a cell. While `Some`, render a
    /// floating card at the anchor; clicks inside dispatch the action,
    /// clicks elsewhere dismiss.
    cell_context_menu: Option<CellContextMenu>,
    /// Right-click on the cell's left-edge bar. Whole-cell-only
    /// operations (Delete, info, Snooze the entire cell). Mutually
    /// exclusive with `cell_context_menu` in practice — opening one
    /// dismisses the other.
    bar_context_menu: Option<BarContextMenu>,
    /// Active right-click context menu for a tag (only shown for tags
    /// with zero attached cells). When `Some`, render and hit-test the
    /// menu at the stored anchor.
    tag_context_menu: Option<TagContextMenu>,
    /// Sidebar's scroll state. Same `Scroller` that backs each pane —
    /// kinetic decay, scrollbar fade, interrupt rules all match. Wheel
    /// events whose mouse position falls in the sidebar column route
    /// here instead of to any pane's scroller.
    sidebar_scroll: Scroller,
    /// Inline rename in progress on the People page. While `Some`, that
    /// row renders an editable `TextBox` instead of static text.
    people_rename: Option<PeopleRenameState>,
    /// Inline "Add person" input. While `Some`, the footer row's "+ Add
    /// person…" prompt is replaced by this `TextBox`. On Enter, the
    /// trimmed text becomes a new cell-less entity; Esc cancels with no
    /// row created. Mutually exclusive with `people_rename`.
    people_add: Option<TextBox>,
    /// People-page "Show inactive" view filter. Default false: inactive
    /// entities are hidden from the list. Always-show in the @-mention
    /// popup (with downweight). Session-only — no persistence in v1.
    show_inactive: bool,
    /// Global "Show archived" toggle for cells/bullets. Default false:
    /// inactive cells are hidden from every view (timeline, sidebar
    /// dates, search, entity references) and inactive bullets are
    /// hidden inside their outlines. Toggle on → those items render
    /// dimmed (alpha-blended) instead of being filtered out.
    /// Session-only, mirroring `show_inactive`.
    show_inactive_cells: bool,
    /// Transient confirmation pill. `Some(t)` while a recent action
    /// (e.g. "Surface as reference") is showing its toast; cleared
    /// once `shown_at + TOAST_HOLD + TOAST_FADE` has elapsed.
    /// `is_animating` reports true while a toast is live so the
    /// fade re-renders without needing a mouse move.
    toast: Option<Toast>,
    /// Active right-click menu over a People-page row.
    people_context_menu: Option<PeopleContextMenu>,
    /// True while the user is mouse-dragging inside the URL-bar pill
    /// (selecting header text). Drives `mouse_drag_to` / `mouse_up`
    /// routing. Holds the pane index whose pill owns the drag.
    header_dragging_pane: Option<usize>,
    /// Frozen hit-test snapshot from the most recently completed frame.
    /// Input handlers (mouse_down, right_click, dispatch_*) read here
    /// and here only.
    hit_tests: HitTestState,
    /// Per-frame write surface. Render code accumulates rects into this
    /// while `tick()` runs; at end-of-frame `std::mem::take` atomically
    /// swaps it into `hit_tests` so the next mouse event reads a fresh,
    /// complete snapshot (never a partial one, never a stale one if the
    /// frame was skipped).
    hit_tests_builder: HitTestState,
    /// In-memory mirror of the DB's entity tables (entities + alias
    /// index + cell→entity index + title-fallback corpus). Repopulated
    /// in lockstep via `entities.refresh(db)` after any entity
    /// mutation — the single invalidation entry point. Invariants
    /// #1–#7 documented at the EntityCache definition site.
    entities: EntityCache,
    /// Most recent cursor position in window (logical) coords, used for hover.
    mouse_pos: (f32, f32),
    /// Tentative Alt-drag pan. Set at `mouse_down` when Alt is held +
    /// the click lands in a pane; until the cursor's accumulated
    /// y-displacement crosses `ALT_PAN_THRESHOLD` we don't commit to
    /// pan — the click might be a plain Alt+click (multi-cursor add /
    /// link-open). On threshold cross, the existing cell-level drag
    /// (if any) is aborted and `pan_drag` takes over.
    tentative_pan: Option<TentativePan>,
    /// True while an Alt-drag pan is committed (post-threshold).
    /// The actual scroll math runs through `Scroller::dragging` —
    /// same field the scrollbar-thumb drag uses, so a single
    /// `apply_thumb_drag` pass drives both kinds of gesture
    /// uniformly. This flag is just a marker so the cursor icon can
    /// switch to "grabbing" and `mouse_up` can tell that a pan was
    /// in flight (even when the scroller's dragging slot was
    /// already cleared by the end-pass).
    pan_drag: bool,
}

/// Which surface's `Scroller` a pan-drag is bound to. Sidebar gets
/// its own scroller (independent of any pane's), so the gesture
/// needs to track which surface it targets.
#[derive(Clone, Copy)]
enum PanTarget {
    Pane(usize),
    Sidebar,
}


/// Pre-commit state for an Alt-drag gesture — captured at mouse_down,
/// promoted to `PanDrag` once the cursor moves more than
/// `ALT_PAN_THRESHOLD` from the initial click point. The dispatch
/// (cell-level OR sidebar-level) is deferred while this is `Some`:
/// on threshold cross we drop the deferred click (no multi-cursor
/// add, no link navigation, no view change); on `mouse_up` without a
/// cross we replay it through the matching dispatcher so the
/// gesture commits as a plain Alt+click.
#[derive(Clone, Copy)]
struct TentativePan {
    target: PanTarget,
    click_x: f32,
    click_y: f32,
    /// Doc-space y at click time. For pane targets this is the
    /// pane-scroll-adjusted y; for the sidebar it's the sidebar-
    /// scroll-adjusted y. Captured here so a scroll change between
    /// click and replay (kinetic decay finishing, etc.) can't shift
    /// hit-test geometry out from under the replay.
    click_doc_y: f32,
    /// Modifier state at click time. Re-using `mouse_up`'s "now"
    /// modifiers would let "alt down, click, alt up, release"
    /// dispatch as a plain click — wrong; the user committed to
    /// alt-click semantics the moment they pressed.
    click_modifiers: Modifiers,
}

#[derive(Clone)]
struct HistoryEntry {
    query: Query,
    focused: Option<Uuid>,
    scroll_y: f32,
}

const NAV_HISTORY_CAP: usize = 100;

/// `KeptApp` derefs to its active `Pane` so existing call sites — which
/// say `self.pane_mut().view`, `self.pane_mut().focused`, `self.pane_mut().scroll_y`, etc. — keep working
/// without rewriting every one. New per-pane access (Stage 2+) goes
/// through `self.panes[i].field` directly.
impl KeptApp {
    /// Active pane (the pane that owns keyboard input). Replaces the
    /// previous `Deref<Target=Pane>` magic: every per-pane field access
    /// is now an explicit `self.pane().X` or `self.pane_mut().X`
    /// call. Sub-render passes inside `tick_pane` rely on the caller
    /// (`tick`) having set `active_pane = pane_idx` before invocation,
    /// so `pane()` resolves to the pane being rendered.
    fn pane(&self) -> &Pane {
        &self.panes[self.active_pane]
    }

    fn pane_mut(&mut self) -> &mut Pane {
        &mut self.panes[self.active_pane]
    }
}

impl KeptApp {
    pub fn new() -> Self {
        let typeface = FontMgr::new()
            .new_from_data(FONT_BYTES, None)
            .expect("failed to load embedded TTF");

        // Make sure the user's colors.yaml exists (write defaults
        // first time). After this, the per-frame poller in `tick`
        // picks up edits.
        crate::color::ensure_colors_file_exists();
        crate::color::maybe_reload();

        let path = db_path();
        eprintln!("kept: opening DB at {}", path.display());
        let mut db = match Db::open(&path) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("kept: failed to open DB at {}: {e}", path.display());
                None
            }
        };

        let mut cells: Vec<Cell> = match db.as_ref().map(|d| d.load_cells(&typeface)) {
            Some(Ok(rows)) => rows,
            Some(Err(e)) => {
                eprintln!("kept: failed to load cells: {e}");
                Vec::new()
            }
            None => Vec::new(),
        };

        let mut contexts: Vec<Context> = match db.as_ref().map(|d| d.load_contexts()) {
            Some(Ok(rows)) => rows
                .into_iter()
                .map(|r| Context {
                    id: r.id,
                    start_time: r.start_time,
                    end_time: r.end_time,
                    title: r.title,
                })
                .collect(),
            Some(Err(e)) => {
                eprintln!("kept: failed to load contexts: {e}");
                Vec::new()
            }
            None => Vec::new(),
        };

        // Initial entity load. The migration backfilled the entity
        // tables from `#person` cells in v4→v5, so this populates the
        // cache from the existing data.
        let entities = EntityCache::load(db.as_ref(), &cells);

        // Sweep: drop any closed context whose window contains no cells.
        // These are leftovers from earlier rotations on already-empty
        // contexts; the current code path no longer creates them.
        let stale: Vec<Uuid> = contexts
            .iter()
            .filter(|c| c.end_time.is_some())
            .filter(|c| {
                !cells.iter().any(|cell| {
                    cell.timestamp >= c.start_time
                        && c.end_time.map_or(true, |e| cell.timestamp < e)
                })
            })
            .map(|c| c.id)
            .collect();
        if !stale.is_empty() {
            contexts.retain(|c| !stale.contains(&c.id));
            if let Some(d) = db.as_mut() {
                for id in &stale {
                    let _ = d.delete_context(*id);
                }
            }
        }

        // First-run / empty-DB seed: one default open context, three plain
        // welcome cells.
        if contexts.is_empty() {
            let ctx = Context {
                id: SEED_CONTEXT_ID,
                start_time: now_epoch_ms(),
                end_time: None,
                title: None,
            };
            if let Some(d) = db.as_mut() {
                let _ = d.save_context(&context_ref(&ctx));
            }
            contexts.push(ctx);
        }
        // Pick the most recent context as the writable target — needed even
        // though we default the view to today, because seed cells claim it
        // as their context_hint_id below.
        let initial_context = contexts
            .iter()
            .max_by_key(|c| (c.end_time.is_none(), c.start_time))
            .map(|c| c.id)
            .expect("at least one context exists after seeding");
        // Default the view to today's date so the "Today" sidebar row is
        // highlighted on launch and new notes land where the user expects.
        let view = Query::date(local_date_for_ms(now_epoch_ms()));

        if cells.is_empty() {
            for (i, text) in SEED_TEXTS.iter().enumerate() {
                let mut cell = Cell::new(typeface.clone(), (*text).to_string());
                // Stagger seed timestamps by 1ms so order is stable.
                cell.timestamp += i as i64;
                cell.edited_at = cell.timestamp;
                cell.context_hint_id = Some(initial_context);
                cells.push(cell);
            }
            if let Some(c) = cells.get_mut(0) {
                c.add_link_to_first(19..27, "https://example.com/click".to_string());
            }
            if let Some(c) = cells.get_mut(1) {
                c.add_link_to_first(17..28, "https://example.com/intent".to_string());
            }
            if let Some(d) = db.as_mut() {
                for c in &cells {
                    let _ = d.save_cell(c);
                }
            }
        }

        let focused = cells.first().map(|c| c.id);

        Self {
            typeface: typeface.clone(),
            document: Document {
                cells,
                contexts,
                dirty_cells: HashSet::new(),
                pending_deletes: HashSet::new(),
                dirty_contexts: HashSet::new(),
                pending_context_deletes: HashSet::new(),
            },
            panes: vec![Pane::new(typeface, view, focused)],
            active_pane: 0,
            split_ratio: 0.5,
            dragging_divider: false,
            split_dir: SplitDir::Horiz,
            pane_chord_armed: None,
            font_scale: 1.0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit_time: None,
            mention_popup: None,
            quick_add: None,
            clipboard: Clipboard::new().ok(),
            db,
            cell_context_menu: None,
            bar_context_menu: None,
            tag_context_menu: None,
            sidebar_scroll: Scroller::new(),
            people_rename: None,
            people_add: None,
            show_inactive: false,
            show_inactive_cells: false,
            toast: None,
            people_context_menu: None,
            header_dragging_pane: None,
            hit_tests: HitTestState::default(),
            hit_tests_builder: HitTestState::default(),
            entities,
            mouse_pos: (-1.0, -1.0),
            tentative_pan: None,
            pan_drag: false,
        }
    }

    /// Returns true if anything changed that needs a redraw — currently
    /// either a scrollbar hover transition (thin → wide / faded → full)
    /// or an in-flight thumb drag updating `scroll_y`.
    pub fn cursor_moved(&mut self, x: f32, y: f32) -> bool {
        self.mouse_pos = (x, y);
        // Hover-state for every scrollbar: sidebar's bar uses raw (x, y)
        // because the sidebar lives in window coords; pane bars also
        // use window coords (pane scroller's geometry is recorded in
        // window space by `tick_pane`). Hover bands don't overlap, so
        // at most one bar will end up `hover=true` per move.
        let mut changed = self.sidebar_scroll.set_hover_for_point(x, y);
        for pane in &mut self.panes {
            changed |= pane.scroller.set_hover_for_point(x, y);
        }
        // If a thumb drag is in progress, apply it. mouse_drag_to also
        // routes drags, but cursor_moved fires for plain motion too —
        // so a moving cursor with the button held updates the scroll
        // even without a drag-specific delta.
        if self.sidebar_scroll.is_dragging_thumb() {
            changed |= self.sidebar_scroll.apply_thumb_drag(y);
        }
        for pane in &mut self.panes {
            if pane.scroller.is_dragging_thumb() {
                changed |= pane.scroller.apply_thumb_drag(y);
            }
        }
        // Alt-drag pan: first chance per move to promote a tentative
        // Alt+click into a committed pan once the cursor has moved
        // past the threshold. Promotion installs scroller.dragging
        // (via `start_pan_drag`); from there the thumb-drag pass
        // above picks up subsequent moves uniformly with real thumb
        // drags, so no separate per-frame pan apply is needed.
        changed |= self.maybe_promote_tentative_pan(y);
        changed
    }

    // ----- pane helpers -----

    /// Index of the pane whose `last_rect` contains `(x, y)`. None when
    /// the point is in the sidebar, on the divider, or out of bounds.
    fn pane_at(&self, x: f32, y: f32) -> Option<usize> {
        self.panes.iter().position(|p| {
            x >= p.last_rect.left
                && x < p.last_rect.right
                && y >= p.last_rect.top
                && y < p.last_rect.bottom
        })
    }

    /// Resolve a `PanTarget` to a mutable `Scroller` reference. Sidebar
    /// and per-pane scrollers share the same `Scroller` type, so the
    /// pan code can call `apply_pan_delta` / `kill_kinetic` /
    /// `finalize_drag_release` uniformly through this resolver.
    fn pan_scroller_mut(&mut self, target: PanTarget) -> &mut Scroller {
        match target {
            PanTarget::Pane(i) => &mut self.panes[i].scroller,
            PanTarget::Sidebar => &mut self.sidebar_scroll,
        }
    }

    /// Inspect a cursor sample against an active `tentative_pan`. If
    /// the cursor has moved more than `ALT_PAN_THRESHOLD` from the
    /// initial click y, promote the gesture: abort any cell-level
    /// drag started at mouse_down, capture the click-time grab
    /// offset on the captured scroller (`start_pan_drag`), and apply
    /// the current cursor y so the scroll position snaps to "thumb
    /// under the cursor." From there, every cursor_moved /
    /// mouse_drag_to flows through `apply_thumb_drag` exactly like a
    /// scrollbar-thumb drag — the user is dragging the (invisible)
    /// thumb from anywhere in the pane. Returns true if a promotion
    /// happened.
    fn maybe_promote_tentative_pan(&mut self, y: f32) -> bool {
        if self.pan_drag {
            return false;
        }
        let Some(tp) = self.tentative_pan else {
            return false;
        };
        let dy_from_click = y - tp.click_y;
        if dy_from_click.abs() <= ALT_PAN_THRESHOLD {
            return false;
        }
        // Abort whatever cell-level drag started at mouse_down (drag-
        // select extension, multi-cursor extend) so it doesn't keep
        // tracking under us. Cell.mouse_up cleanly closes its drag
        // state without committing additional side effects. Sidebar
        // targets never set `dragging_cell`, so the `take()` is a
        // no-op there.
        if let Some(id) = self.pane_mut().dragging_cell.take() {
            if let Some(cell) = self.cell_mut(id) {
                cell.mouse_up();
            }
        }
        let scroller = self.pan_scroller_mut(tp.target);
        // Capture grab offset relative to the thumb's *click-time*
        // position so subsequent `apply_thumb_drag(y)` calls maintain
        // the cursor's offset from the thumb, exactly like a real
        // thumb drag. Then snap the doc to the current cursor — same
        // sample feeds the velocity window so a fast threshold-cross
        // coasts on release.
        if !scroller.start_pan_drag(tp.click_y) {
            // Geometry not ready (no overflow / bar not drawn) —
            // there's nothing to scroll. Drop the tentative; the
            // gesture has nowhere to go.
            self.tentative_pan = None;
            return false;
        }
        let _ = scroller.apply_thumb_drag(y);
        self.pan_drag = true;
        self.tentative_pan = None;
        true
    }

    /// True iff some text input is currently consuming keystrokes —
    /// the active pane is in cell edit mode (which covers cell body
    /// and title editing, both of which set `editing` when entered),
    /// the search popup is open, or a People-page inline edit is
    /// running. The mention popup is always nested inside one of
    /// those, so it's covered transitively.
    ///
    /// Used to gate behavior that conflicts with typing — e.g.,
    /// Space-drag pan is suppressed while text inputs are focused
    /// because Space types a character there.
    fn is_text_input_focused(&self) -> bool {
        self.pane().editing
            || self.panes.iter().any(|p| p.header.focused)
            || self.people_rename.is_some()
            || self.people_add.is_some()
            || self.quick_add.is_some()
    }

    /// True if `x` falls inside the divider's hit slop. Only meaningful
    /// when there are 2+ panes.
    fn is_on_divider(&self, x: f32) -> bool {
        if self.panes.len() < 2 {
            return false;
        }
        // Divider sits between pane[0].right and pane[1].left.
        let div_x = (self.panes[0].last_rect.right + self.panes[1].last_rect.left) * 0.5;
        (x - div_x).abs() <= DIVIDER_HIT_SLOP
    }

    /// Dispatch the follow-up key in a Ctrl+W pane chord. Returns whether
    /// the action consumed the key. Unrecognized follow-ups consume the
    /// key (return true) so a stray keystroke after the leader doesn't
    /// also fire a normal app shortcut.
    fn dispatch_pane_chord(&mut self, event: &KeyEvent) -> bool {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => true,
            Key::Character(s) => {
                let c = s.as_str();
                if c.eq_ignore_ascii_case("h") {
                    if self.active_pane > 0 {
                        return self.set_active_pane(self.active_pane - 1);
                    }
                    return true;
                }
                if c.eq_ignore_ascii_case("l") {
                    if self.active_pane + 1 < self.panes.len() {
                        return self.set_active_pane(self.active_pane + 1);
                    }
                    return true;
                }
                if c == "=" {
                    self.split_ratio = 0.5;
                    return true;
                }
                if c.eq_ignore_ascii_case("v") {
                    return self.split_pane();
                }
                if c.eq_ignore_ascii_case("q") {
                    return self.close_active_pane();
                }
                // j / k reserved for future vertical splits; s for future
                // horizontal-split-of-existing. Consume so they don't fire
                // normal app shortcuts.
                true
            }
            _ => true,
        }
    }

    /// Ctrl+W v — duplicate the active pane to its right and make the new
    /// pane active. The new pane mirrors the active pane's view (same
    /// query, same focus, same scroll) so a "split" feels like cloning the
    /// current context, then the user navigates the new pane elsewhere.
    /// No-op when already at the 2-pane cap (nested splits are future work).
    fn split_pane(&mut self) -> bool {
        if self.panes.len() >= 2 {
            return true;
        }
        let src = &self.panes[self.active_pane];
        let mut new_scroller = Scroller::new();
        // Carry scroll position + bound from the source pane so the
        // split lands at the same place visually. Velocity, fade
        // timer, and wheel-history don't carry — those belong to the
        // gesture, not the view.
        new_scroller.scroll_y = src.scroll_y;
        new_scroller.max_scroll = src.max_scroll;
        let new_pane = Pane {
            view: src.view.clone(),
            focused: src.focused,
            editing: false,
            dragging_cell: None,
            scroller: new_scroller,
            doc_height: src.doc_height,
            viewport_height: src.viewport_height,
            pending_caret_scroll: false,
            coalesce_break: true,
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            last_rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            header: pane::PaneHeader::new(self.typeface.clone()),
        };
        // Insert to the right of the active pane and activate it. With v1
        // capped at 2 panes, this just means push + active = 1.
        self.panes.push(new_pane);
        self.split_ratio = 0.5;
        self.active_pane = self.panes.len() - 1;
        true
    }

    /// Open `q` in the *other* pane, splitting first if needed. The
    /// active pane is *preserved* across the call — Alt-open is a
    /// "show that there but keep my focus here" gesture, not a focus
    /// move. Returns the destination pane index unconditionally;
    /// callers always want to apply follow-up side effects (focus a
    /// cell, enter focus mode, scroll into view) even when
    /// `push_view` short-circuits because the destination was
    /// already on that view. `self.foo` deref-writes hit the
    /// (preserved) original active pane, so use the returned index
    /// for any pane-state writes that should land on the destination.
    fn open_in_other_pane(&mut self, q: Query) -> Option<usize> {
        let saved_active = self.active_pane;
        if self.panes.len() < 2 {
            // `split_pane` activates the new pane; we restore active
            // below so the user's keyboard focus doesn't jump.
            self.split_pane();
        }
        let other = (saved_active + 1) % self.panes.len();
        // Push regardless of whether the view changes — the no-op
        // case (destination already on `q`) still counts as a
        // successful "land here" for the caller's purposes.
        self.push_view_in_pane(other, q);
        self.active_pane = saved_active;
        Some(other)
    }

    /// Run `push_view(q)` against `pane_idx` regardless of the
    /// currently-active pane. Implemented by temporarily swapping
    /// `active_pane` (since `push_view` writes to the active pane via
    /// `Deref`), then restoring it. The swap doesn't go through
    /// `set_active_pane` — that has menu-closing side effects we
    /// don't want to fire on Alt-open.
    fn push_view_in_pane(&mut self, pane_idx: usize, q: Query) -> bool {
        let saved = self.active_pane;
        self.active_pane = pane_idx;
        let result = self.push_view(q);
        self.active_pane = saved;
        result
    }

    /// Open a specific cell in the other pane, in single-cell view —
    /// the cell fills the pane like a local Ctrl+F. Pushes
    /// `Query::cell(cell_id)` on the destination pane's nav stack and
    /// focuses the cell. (Active pane is preserved by
    /// `open_in_other_pane`, so deref-writes go to the wrong pane —
    /// write to the destination index directly.)
    /// Returns false when the cell isn't in `self.document.cells` or
    /// the destination pane was already on the same view.
    fn open_cell_in_other_pane(&mut self, cell_id: Uuid) -> bool {
        if self.cell(cell_id).is_none() {
            return false;
        }
        let Some(other) = self.open_in_other_pane(Query::cell(cell_id)) else {
            return false;
        };
        let pane = &mut self.panes[other];
        pane.focused = Some(cell_id);
        pane.editing = false;
        pane.pending_caret_scroll = true;
        true
    }

    /// Ctrl+W q — close the active pane. No-op when only one pane remains
    /// (refuse to leave the user with zero panes).
    fn close_active_pane(&mut self) -> bool {
        if self.panes.len() <= 1 {
            return true;
        }
        self.panes.remove(self.active_pane);
        // active_pane points at the (now-)next pane in array order; clamp
        // to the last index so closing the rightmost pane lands on what's
        // now the rightmost.
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len() - 1;
        }
        // Reset transient overlays anchored to the closed pane.
        self.cell_context_menu = None;
        self.mention_popup = None;
        true
    }

    /// Switch the active pane. Closes transient overlays that were
    /// anchored to the previously-active pane (mention popup, cell menu).
    /// Returns true if the active pane changed.
    fn set_active_pane(&mut self, i: usize) -> bool {
        if i >= self.panes.len() || self.active_pane == i {
            return false;
        }
        self.cell_context_menu = None;
        self.mention_popup = None;
        self.active_pane = i;
        true
    }

    /// Compute window-coord rects for each pane and write them into
    /// `pane.last_rect`. Sidebar occupies the leftmost `SIDEBAR_WIDTH *
    /// scale` pixels; remaining width is split among panes per
    /// `split_ratio`. Single-pane mode (panes.len() == 1) gives the lone
    /// pane the full content area.
    fn layout_panes(&mut self, width: f32, height: f32) {
        let scale = self.font_scale;
        let sb_w = SIDEBAR_WIDTH * scale;
        let pane_area_left = sb_w;
        let pane_area_w = (width - sb_w).max(120.0);
        match self.panes.len() {
            1 => {
                self.panes[0].last_rect =
                    Rect::new(pane_area_left, 0.0, pane_area_left + pane_area_w, height);
            }
            _ => {
                let div_x =
                    pane_area_left + pane_area_w * self.split_ratio.clamp(SPLIT_MIN, SPLIT_MAX);
                let half_t = DIVIDER_THICKNESS * 0.5;
                self.panes[0].last_rect =
                    Rect::new(pane_area_left, 0.0, (div_x - half_t).max(pane_area_left), height);
                self.panes[1].last_rect = Rect::new(
                    (div_x + half_t).min(pane_area_left + pane_area_w),
                    0.0,
                    pane_area_left + pane_area_w,
                    height,
                );
            }
        }
    }

    /// Paint the divider gutter between panes. No-op for single-pane.
    fn render_divider(&self, canvas: &Canvas, height: f32) {
        if self.panes.len() < 2 {
            return;
        }
        let div_x = (self.panes[0].last_rect.right + self.panes[1].last_rect.left) * 0.5;
        let half_t = DIVIDER_THICKNESS * 0.5;
        let hovered = (self.mouse_pos.0 - div_x).abs() <= DIVIDER_HIT_SLOP;
        let color = if hovered || self.dragging_divider {
            crate::color::divider_pane_hover()
        } else {
            crate::color::divider_pane()
        };
        let mut p = Paint::default();
        p.set_anti_alias(true);
        p.set_color(color);
        canvas.draw_rect(
            Rect::new(div_x - half_t, 0.0, div_x + half_t, height),
            &p,
        );
    }

    /// Stroke a 2 px accent border around the active pane. No-op for
    /// single-pane.
    fn render_active_pane_indicator(&self, canvas: &Canvas) {
        if self.panes.len() < 2 {
            return;
        }
        let r = self.panes[self.active_pane].last_rect;
        // Inset by half the stroke so the border sits inside the rect.
        let s = PANE_BORDER_STROKE * 0.5;
        let mut p = Paint::default();
        p.set_anti_alias(true);
        p.set_style(PaintStyle::Stroke);
        p.set_stroke_width(PANE_BORDER_STROKE);
        p.set_color(crate::color::accent_blue_pane_border());
        canvas.draw_rect(
            Rect::new(r.left + s, r.top + s, r.right - s, r.bottom - s),
            &p,
        );
    }

    /// Render a reference cell at `(x, y)` with `width`. Returns the height
    /// drawn. Looks up the target out of `self.document.cells`, dispatches to the
    /// appropriate body's `render_view`, and wraps it in the embed visual
    /// (warm-tan dashed border + faint background tint + footer line).
    /// Dangling targets (deleted source) render as a placeholder line.
    /// Records geometry on the reference cell so click-tests work.
    fn render_reference_cell(
        &mut self,
        canvas: &Canvas,
        ref_idx: usize,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
    ) -> f32 {
        let target = match &self.document.cells[ref_idx].kind {
            CellKind::Reference(rc) => rc.target(),
            _ => return 0.0,
        };
        let scale = self.font_scale;
        let inset = EMBED_INSET * scale;
        let pad = EMBED_PAD * scale;
        let body_x = x + inset;
        let body_y = y + pad;
        let body_w = (width - 2.0 * inset).max(40.0);

        let target_idx = self.document.cells.iter().position(|c| c.id == target.cell_id());

        // Decide what kind of preview to render and refresh the cache on
        // the reference cell if the source's edited_at has changed.
        enum PreviewKind {
            Cached,
            Placeholder(&'static str),
        }
        let preview = match target_idx {
            None => {
                // Target gone — clear any stale cache and show placeholder.
                if let CellKind::Reference(rc) = &mut self.document.cells[ref_idx].kind {
                    rc.install_cache(None, None);
                }
                PreviewKind::Placeholder("↗ [referenced cell deleted]")
            }
            Some(tidx) => {
                let source_edited_at = self.document.cells[tidx].edited_at;
                let is_stale = match &self.document.cells[ref_idx].kind {
                    CellKind::Reference(rc) => rc.cache_is_stale_for(Some(source_edited_at)),
                    _ => false,
                };
                if is_stale {
                    let new_cache = self.build_reference_cache(tidx, target, 0);
                    if let CellKind::Reference(rc) = &mut self.document.cells[ref_idx].kind {
                        rc.install_cache(new_cache, Some(source_edited_at));
                    }
                }
                // If the build returned None (e.g., subtree's bullet missing),
                // surface a placeholder. Otherwise tick the cache.
                let has_cache = matches!(
                    &self.document.cells[ref_idx].kind,
                    CellKind::Reference(rc) if rc.cache_ref().is_some()
                );
                if has_cache {
                    PreviewKind::Cached
                } else if matches!(target, ReferenceTarget::Subtree { .. }) {
                    PreviewKind::Placeholder("↗ [referenced bullet deleted]")
                } else {
                    PreviewKind::Placeholder("↗ [reference target unrenderable]")
                }
            }
        };

        let body_h = match preview {
            PreviewKind::Placeholder(msg) => self.render_embed_placeholder(
                canvas, msg, body_x, body_y, body_w, scale,
            ),
            PreviewKind::Cached => {
                // Detach the cache from the host so the &mut borrow on
                // `self.document.cells` ends, then route through
                // `tick_embedded_cell` (which needs &self for the
                // wrapper / placeholder helpers and recurses on
                // envelope-outline caches). Re-attach afterwards.
                let detached = if let CellKind::Reference(rc) =
                    &mut self.document.cells[ref_idx].kind
                {
                    rc.detach_cache()
                } else {
                    None
                };
                let mut h = 0.0;
                if let Some(mut cache) = detached {
                    h = self.tick_embedded_cell(
                        canvas, &mut cache, body_x, body_y, body_w, focused,
                    );
                    if let CellKind::Reference(rc) = &mut self.document.cells[ref_idx].kind {
                        rc.attach_cache(Some(cache));
                    }
                }
                h
            }
        };

        let footer_text = match target_idx {
            Some(tidx) => {
                let ts = self.document.cells[tidx].timestamp;
                format!("↗ originally {}", format_date_label(local_date_for_ms(ts)))
            }
            None => "↗ original deleted".to_string(),
        };
        // Timeline-level reference cell: extend the wrapper
        // FOCUS_PAD on every side so its outer geometry matches
        // the `outline_rect` used by non-reference cells. The
        // bar abuts wrapper.left identically to how it abuts
        // outline.left for plain cells, and the vertical extent
        // matches outline.top/bottom — no special-casing in
        // render_cell_stream needed.
        let total_h = self.draw_embed_wrapper(
            canvas,
            x,
            y,
            width,
            body_x,
            body_h,
            &footer_text,
            scale,
            [FOCUS_PAD, FOCUS_PAD, FOCUS_PAD, FOCUS_PAD],
            true,
        );

        // Record geometry on the embed: both on the inner ReferenceCell
        // (for symmetry / future use) and on the outer Cell (which is what
        // `find_cell_at` reads via Cell::x_origin/width/height).
        if let CellKind::Reference(rc) = &mut self.document.cells[ref_idx].kind {
            rc.set_view_geometry(x, y, width, total_h);
        }
        self.document.cells[ref_idx].set_view_geometry(x, y, width, total_h);

        total_h
    }

    /// Render an envelope outline: optional title slot at the top,
    /// then the read-only embed (the original reference target),
    /// then the editable bullet body underneath. Mirrors
    /// `Cell::tick`'s title handling because Cell::tick can't see the
    /// header (no access to `&[Cell]` for cache lookup), and dispatch
    /// already special-cases envelope outlines at the cell-render
    /// loop. Returns the total height consumed.
    fn render_envelope_outline_cell(
        &mut self,
        canvas: &Canvas,
        cell_idx: usize,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32 {
        // Mirror Cell::tick: drop an empty unfocused title.
        let title_focused = self.document.cells[cell_idx].title_focused;
        if !title_focused
            && self.document.cells[cell_idx]
                .title()
                .map(|t| t.is_empty())
                .unwrap_or(false)
        {
            self.document.cells[cell_idx].set_title(None);
        }

        let mut consumed = 0.0_f32;
        let mut body_y = y;
        if let Some(title) = self.document.cells[cell_idx].title_mut() {
            let scale = title.font_scale();
            let pad = cell::TITLE_BODY_GAP * scale;
            let title_h = title.tick(
                canvas,
                x,
                y,
                width,
                focused && title_focused,
                show_caret && title_focused,
            );
            let block = title_h + pad;
            consumed += block;
            body_y = y + block;
        }

        // Resolve the header target. Defensive — caller already
        // checked `has_reference_header` but tolerate the cell type
        // shifting under us between dispatch and render.
        let target = match &self.document.cells[cell_idx].kind {
            CellKind::Outline(oc) => oc.reference_header().map(|h| h.target()),
            _ => None,
        };
        let Some(target) = target else {
            return self.document.cells[cell_idx]
                .tick(canvas, x, y, width, focused, show_caret);
        };

        let scale = self.font_scale;
        let inset = EMBED_INSET * scale;
        let pad = EMBED_PAD * scale;
        let body_x_inner = x + inset;
        let body_y_inner = body_y + pad;
        let body_w_inner = (width - 2.0 * inset).max(40.0);

        let target_idx = self.document.cells.iter().position(|c| c.id == target.cell_id());

        enum PreviewKind {
            Cached,
            Placeholder(&'static str),
        }
        let preview = match target_idx {
            None => {
                if let CellKind::Outline(oc) = &mut self.document.cells[cell_idx].kind {
                    if let Some(h) = oc.reference_header_mut() {
                        h.install_cache(None, None);
                    }
                }
                PreviewKind::Placeholder("↗ [referenced cell deleted]")
            }
            Some(tidx) => {
                let source_edited_at = self.document.cells[tidx].edited_at;
                let is_stale = match &self.document.cells[cell_idx].kind {
                    CellKind::Outline(oc) => oc
                        .reference_header()
                        .map(|h| h.cache_is_stale_for(Some(source_edited_at)))
                        .unwrap_or(false),
                    _ => false,
                };
                if is_stale {
                    let new_cache = self.build_reference_cache(tidx, target, 0);
                    if let CellKind::Outline(oc) = &mut self.document.cells[cell_idx].kind {
                        if let Some(h) = oc.reference_header_mut() {
                            h.install_cache(new_cache, Some(source_edited_at));
                        }
                    }
                }
                let has_cache = matches!(
                    &self.document.cells[cell_idx].kind,
                    CellKind::Outline(oc)
                        if oc.reference_header()
                            .and_then(|h| h.cache_ref())
                            .is_some()
                );
                if has_cache {
                    PreviewKind::Cached
                } else if matches!(target, ReferenceTarget::Subtree { .. }) {
                    PreviewKind::Placeholder("↗ [referenced bullet deleted]")
                } else {
                    PreviewKind::Placeholder("↗ [reference target unrenderable]")
                }
            }
        };

        let body_h = match preview {
            PreviewKind::Placeholder(msg) => self.render_embed_placeholder(
                canvas,
                msg,
                body_x_inner,
                body_y_inner,
                body_w_inner,
                scale,
            ),
            PreviewKind::Cached => {
                // Detach + route through `tick_embedded_cell` so a
                // nested envelope inside this header renders
                // recursively. Re-attach afterwards.
                let detached = if let CellKind::Outline(oc) =
                    &mut self.document.cells[cell_idx].kind
                {
                    oc.reference_header_mut()
                        .and_then(|h| h.detach_cache())
                } else {
                    None
                };
                let mut h = 0.0;
                if let Some(mut cache) = detached {
                    h = self.tick_embedded_cell(
                        canvas,
                        &mut cache,
                        body_x_inner,
                        body_y_inner,
                        body_w_inner,
                        focused,
                    );
                    if let CellKind::Outline(oc) = &mut self.document.cells[cell_idx].kind {
                        if let Some(href) = oc.reference_header_mut() {
                            href.attach_cache(Some(cache));
                        }
                    }
                }
                h
            }
        };

        let footer_text = match target_idx {
            Some(tidx) => {
                let ts = self.document.cells[tidx].timestamp;
                format!("↗ originally {}", format_date_label(local_date_for_ms(ts)))
            }
            None => "↗ original deleted".to_string(),
        };
        let header_total_h = self.draw_embed_wrapper(
            canvas,
            x,
            body_y,
            width,
            body_x_inner,
            body_h,
            &footer_text,
            scale,
            [0.0, 0.0, 0.0, 0.0],
            false,
        );

        // Record header band for hit-testing (clicks inside route to
        // the cache cell).
        if let CellKind::Outline(oc) = &mut self.document.cells[cell_idx].kind {
            oc.set_reference_header_geometry(body_y, header_total_h);
        }

        consumed += header_total_h + ENVELOPE_HEADER_GAP * scale;
        let after_header_y = y + consumed;

        // Bullet body.
        let body_focused = focused && !title_focused;
        let body_caret = show_caret && !title_focused;
        let show_inactive = self.show_inactive_cells;
        let bullets_h = if let CellKind::Outline(oc) = &mut self.document.cells[cell_idx].kind {
            oc.set_show_inactive(show_inactive);
            oc.tick(canvas, x, after_header_y, width, body_focused, body_caret)
        } else {
            0.0
        };

        let total_h = consumed + bullets_h;
        self.document.cells[cell_idx].set_view_geometry(x, y, width, total_h);
        total_h
    }

    /// Build a fresh cache `Cell` mirroring the source's content. Returns
    /// None when the target isn't renderable (e.g., Subtree whose bullet
    /// is gone, Subtree pointing at a non-Outline cell), or when the
    /// embed `depth` has reached `MAX_EMBED_DEPTH`. The cache is a real
    /// `Cell` so it owns selection state across frames and dispatches
    /// mouse events through the standard machinery.
    ///
    /// `depth` counts the *current* embed level — top-level callers
    /// (a reference cell, an envelope outline header, the entity-page
    /// references list) pass 0. When the resulting cache is itself an
    /// envelope outline, this fn recursively builds the nested header's
    /// cache at `depth + 1`, so `clone_for_scale`'s preserved-header
    /// gets populated before render time. None at the cap surfaces as a
    /// "depth limit" placeholder via the render path.
    fn build_reference_cache(
        &self,
        target_idx: usize,
        target: ReferenceTarget,
        depth: usize,
    ) -> Option<Cell> {
        if depth >= MAX_EMBED_DEPTH {
            return None;
        }
        let source = &self.document.cells[target_idx];
        let scale = self.font_scale;
        let typeface = &self.typeface;
        let mut cache = match target {
            ReferenceTarget::WholeCell(_) => {
                let kind = source.kind.clone_for_scale(typeface, scale)?;
                let title = source
                    .title()
                    .map(|t| t.clone_for_cache(typeface.clone(), scale));
                let mut cache = Cell::from_parts(
                    Uuid::now_v7(),
                    kind,
                    title,
                    source.timestamp,
                    source.edited_at,
                    source.context_hint_id,
                    // Cache cells are internal to the embed render; they
                    // never enter `self.document.cells`, so attention
                    // metadata is irrelevant. Use defaults (open, no
                    // snooze) so the embed renders normally regardless
                    // of source state.
                    None,
                    None,
                );
                cache.set_font_scale(scale);
                cache
            }
            ReferenceTarget::Subtree { bullet_id, .. } => {
                let oc = match &source.kind {
                    CellKind::Outline(oc) => oc,
                    _ => return None,
                };
                let range = oc.subtree_range(bullet_id)?;
                let root_depth = oc.bullets()[range.start].depth();
                let bullets: Vec<cell::Bullet> = oc.bullets()[range]
                    .iter()
                    .map(|b| {
                        let tb = b.textbox().clone_for_cache(typeface.clone(), scale);
                        let new_depth = b.depth().saturating_sub(root_depth);
                        cell::Bullet::new(b.id(), tb, new_depth)
                    })
                    .collect();
                // Subtree caches never carry an envelope header — the
                // subtree is bullet content only — so no recursive
                // resolution is needed here.
                let mut new_oc = cell::OutlineCell::from_bullets(typeface.clone(), bullets);
                new_oc.set_font_scale(scale);
                let mut cache = Cell::from_parts(
                    Uuid::now_v7(),
                    CellKind::Outline(new_oc),
                    None,
                    source.timestamp,
                    source.edited_at,
                    source.context_hint_id,
                    None,
                    None,
                );
                cache.set_font_scale(scale);
                cache
            }
        };

        // If the cache cell is an envelope outline, recursively build
        // the nested header's cache. `clone_for_scale` carried the
        // header's target across; the cache slot is currently empty.
        // Resolve the nested target from the live cells list and
        // install the result (which may itself be a deeper envelope or
        // a None at the depth cap).
        if let CellKind::Outline(oc) = &mut cache.kind {
            if let Some(h) = oc.reference_header_mut() {
                let nested_target = h.target();
                let nested_idx = self
                    .document
                    .cells
                    .iter()
                    .position(|c| c.id == nested_target.cell_id());
                if let Some(ni) = nested_idx {
                    let nested_edited_at = self.document.cells[ni].edited_at;
                    let nested_cache =
                        self.build_reference_cache(ni, nested_target, depth + 1);
                    h.install_cache(nested_cache, Some(nested_edited_at));
                } else {
                    h.install_cache(None, None);
                }
            }
        }
        Some(cache)
    }

    /// One-line muted placeholder for dangling / wrong-kind references.
    /// Returns the rendered height.
    /// Paint the embed's wrapper chrome: faint warm-tan background tint,
    /// dashed warm-tan border, muted footer line. Used by both the
    /// timeline reference cell render and the entity-page references
    /// list — the visual is identical because the meaning is identical.
    /// Returns `inner_h` (= body + footer + paddings) — i.e. the
    /// natural content height. The wrapper visual rect can extend
    /// past the content on any of the four sides via `extras` so
    /// it matches the outline_rect's chrome geometry used by
    /// other cell types (FOCUS_PAD all around), without affecting
    /// the returned height (so inter-cell spacing stays consistent).
    ///
    /// `extras = [left, top, right, bottom]` — extension in logical
    /// pixels per side. Top-level reference cells pass FOCUS_PAD on
    /// all four sides; internal embed wrappers (envelope headers,
    /// recursive nested embeds) pass 0 everywhere.
    fn draw_embed_wrapper(
        &self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        body_x: f32,
        body_h: f32,
        footer_text: &str,
        scale: f32,
        extras: [f32; 4],
        flat_left_corners: bool,
    ) -> f32 {
        let pad = EMBED_PAD * scale;
        let footer_h = EMBED_FOOTER_H * scale;
        let inner_h = pad + body_h + 4.0 * scale + footer_h;
        let [extra_left, extra_top, extra_right, extra_bottom] = extras;
        let wrapper = Rect::new(
            x - extra_left,
            y - extra_top,
            x + width + extra_right,
            y + inner_h + extra_bottom,
        );
        let total_h = inner_h;
        // When this wrapper plays the role of the cell's OUTER
        // chrome (timeline reference render), its TL/BL corners go
        // flat — the cell's left state bar supplies those outer
        // corners. Internal embed wrappers (envelope headers,
        // recursive nested embeds, entity-page refs) stay fully
        // rounded so they read as self-contained pills.
        let r = FOCUS_RADIUS;
        let flat = skia_safe::Vector::new(0.0, 0.0);
        let round = skia_safe::Vector::new(r, r);
        let radii: [skia_safe::Vector; 4] = if flat_left_corners {
            [flat, round, round, flat]
        } else {
            [round, round, round, round]
        };
        let wrapper_rr = skia_safe::RRect::new_rect_radii(wrapper, &radii);

        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(crate::color::embed_tint());
        canvas.draw_rrect(&wrapper_rr, &bg);

        let mut stroke = Paint::default();
        stroke.set_anti_alias(true);
        stroke.set_style(PaintStyle::Stroke);
        stroke.set_stroke_width(1.5);
        stroke.set_stroke_cap(skia_safe::PaintCap::Round);
        stroke.set_color(crate::color::embed_border());
        // Tight dots: a near-zero "on" segment with round caps draws as a
        // single round dot at each interval; "off" sets the spacing.
        if let Some(eff) = PathEffect::dash(&[0.0, 4.0], 0.0) {
            stroke.set_path_effect(eff);
        }
        if flat_left_corners {
            // Outer chrome (timeline reference): skip the left
            // edge so the dashed border doesn't overdraw the bar.
            let path = chrome_open_path(wrapper, FOCUS_RADIUS);
            canvas.draw_path(&path, &stroke);
        } else {
            // Internal embed wrapper: stroke the full rrect.
            canvas.draw_rrect(&wrapper_rr, &stroke);
        }

        let footer_font = Font::from_typeface(&self.typeface, EMBED_FOOTER_FONT_SIZE * scale);
        let (_, fm) = footer_font.metrics();
        let footer_baseline = y + total_h - pad - (-fm.ascent);
        let mut footer_paint = Paint::default();
        footer_paint.set_anti_alias(true);
        footer_paint.set_color(crate::color::text_muted_grey());
        canvas.draw_str(
            footer_text,
            Point::new(body_x, footer_baseline + 10.0*scale),
            &footer_font,
            &footer_paint,
        );

        total_h
    }

    fn render_embed_placeholder(
        &self,
        canvas: &Canvas,
        text: &str,
        x: f32,
        y: f32,
        _width: f32,
        scale: f32,
    ) -> f32 {
        // Slightly smaller than body text — placeholder is a notice, not
        // actual content.
        let font = Font::from_typeface(&self.typeface, 14.0 * scale);
        let (_, m) = font.metrics();
        let baseline = y + (-m.ascent);
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(crate::color::text_muted_grey());
        canvas.draw_str(text, Point::new(x, baseline), &font, &paint);
        -m.ascent + m.descent
    }

    /// Tick a cache `Cell` inside an embed wrapper, with envelope-aware
    /// recursion. For non-envelope caches this is a thin pass-through
    /// to `Cell::tick`. For envelope-outline caches it draws the
    /// nested embed header (recursively rendered through this same fn,
    /// or a "depth limit" placeholder when the nested cache is None)
    /// followed by the bullet body, mirroring the top-level
    /// `render_envelope_outline_cell` chrome.
    ///
    /// Caller owns the cache cell — typically detached from a
    /// `ReferenceCell` or `EmbeddedReference` to drop the host borrow,
    /// then re-attached after this call returns.
    fn tick_embedded_cell(
        &self,
        canvas: &Canvas,
        cell: &mut Cell,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
    ) -> f32 {
        let is_envelope = matches!(
            &cell.kind,
            CellKind::Outline(oc) if oc.has_reference_header()
        );
        if !is_envelope {
            return cell.tick(canvas, x, y, width, focused, false);
        }

        let scale = self.font_scale;
        let inset = EMBED_INSET * scale;
        let pad = EMBED_PAD * scale;
        let body_x_inner = x + inset;
        let body_y_inner = y + pad;
        let body_w_inner = (width - 2.0 * inset).max(40.0);

        // Render the nested embedded reference (the envelope's
        // header). If the nested cache is None — either depth-cap was
        // hit during the build pass, or the target was unrenderable —
        // surface a placeholder line.
        let header_body_h = if let CellKind::Outline(oc) = &mut cell.kind {
            let detached = oc
                .reference_header_mut()
                .and_then(|h| h.detach_cache());
            match detached {
                Some(mut nested) => {
                    let h = self.tick_embedded_cell(
                        canvas,
                        &mut nested,
                        body_x_inner,
                        body_y_inner,
                        body_w_inner,
                        focused,
                    );
                    if let Some(href) = oc.reference_header_mut() {
                        href.attach_cache(Some(nested));
                    }
                    h
                }
                None => self.render_embed_placeholder(
                    canvas,
                    "↗ [embed depth limit]",
                    body_x_inner,
                    body_y_inner,
                    body_w_inner,
                    scale,
                ),
            }
        } else {
            0.0
        };

        // Cache cells carry the source's timestamp from
        // `build_reference_cache`, so this surfaces the right date for
        // every nested level without needing the source index.
        let footer_text = format!(
            "↗ originally {}",
            format_date_label(local_date_for_ms(cell.timestamp))
        );
        let header_total_h = self.draw_embed_wrapper(
            canvas,
            x,
            y,
            width,
            body_x_inner,
            header_body_h,
            &footer_text,
            scale,
            [0.0, 0.0, 0.0, 0.0],
            false,
        );

        // Record the header band on the cache outline so hit-testing
        // inside the cache (drag-select, link clicks) routes correctly
        // even when this fn is rendering a nested cache cell. Geometry
        // is in document space (same convention as the top-level
        // `render_envelope_outline_cell`).
        if let CellKind::Outline(oc) = &mut cell.kind {
            oc.set_reference_header_geometry(y, header_total_h);
        }

        let after_header_y = y + header_total_h + ENVELOPE_HEADER_GAP * scale;
        let bullets_h = if let CellKind::Outline(oc) = &mut cell.kind {
            oc.tick(canvas, x, after_header_y, width, focused, false)
        } else {
            0.0
        };

        let total_h = header_total_h + ENVELOPE_HEADER_GAP * scale + bullets_h;
        cell.set_view_geometry(x, y, width, total_h);
        total_h
    }

    /// Debounced persistence flush. Called once per frame from `tick`,
    /// outside the per-pane loop (dirty cells are global, not per-pane).
    fn maybe_flush_persistence(&mut self) {
        let any_dirty = !self.document.dirty_cells.is_empty()
            || !self.document.pending_deletes.is_empty()
            || !self.document.dirty_contexts.is_empty()
            || !self.document.pending_context_deletes.is_empty();
        if !any_dirty {
            return;
        }
        let idle = self
            .last_edit_time
            .map(|t| t.elapsed() >= SAVE_DEBOUNCE)
            .unwrap_or(true);
        if idle {
            self.flush_persistence();
        }
    }

    /// True while an Alt-drag pan is in flight (committed past the
    /// drag threshold). Used by the host (`main.rs`) to swap the
    /// system cursor to a closed-hand "grabbing" icon.
    pub fn is_panning(&self) -> bool {
        self.pan_drag
    }

    /// True if the mouse is currently over a link or an inline `#tag` in
    /// any visible cell. Used by the host (`main.rs`) to swap the system
    /// cursor to a hand pointer.
    pub fn is_hovering_link(&self) -> bool {
        let (x, y) = self.mouse_pos;
        // Sidebar columns / out-of-bounds have no links.
        if x < SIDEBAR_WIDTH * self.font_scale || x < 0.0 || y < 0.0 {
            return false;
        }
        let doc_y = y + self.pane().scroll_y;
        // Narrow to the single cell under the cursor before doing
        // any link / tag span walks. Iterating every visible cell
        // here used to be cheap when most views filtered down to a
        // few cells, but views like Current expose hundreds of
        // cells at once — `link_at_doc_pos` per cell walks each
        // TextBox's spans, and this runs on every cursor-move event
        // (way faster than 60 Hz). Short-circuit by hitting the
        // one cell whose y-band covers the cursor.
        let Some(target) = self.find_cell_at(x, doc_y) else {
            return false;
        };
        let Some(cell) = self.cell(target) else {
            return false;
        };
        cell.link_at_doc_pos(x, doc_y)
    }

    // ----- cell access helpers (thin proxies to `Document`) -----

    fn cell_idx(&self, id: Uuid) -> Option<usize> {
        self.document.cell_idx(id)
    }

    fn cell(&self, id: Uuid) -> Option<&Cell> {
        self.document.cell(id)
    }

    fn cell_mut(&mut self, id: Uuid) -> Option<&mut Cell> {
        self.document.cell_mut(id)
    }

    /// Reload the entity caches from the DB + recompute mention
    /// counts from the in-memory cells. Called after every
    /// `save_cell` / `delete_cell` so the in-memory state stays in
    /// lockstep with persistence + the cell mutations that just
    /// happened. Thin proxy — see `EntityCache::refresh`.
    fn refresh_entities(&mut self) {
        self.entities
            .refresh(self.db.as_ref(), &self.document.cells);
    }

    fn writable_context_id(&self) -> Option<Uuid> {
        self.document.contexts
            .iter()
            .filter(|c| c.end_time.is_none())
            .max_by_key(|c| c.start_time)
            .map(|c| c.id)
    }

    /// Test whether `cell` is visible under the current query.
    ///
    /// - `Context(id)` → the cell's timestamp must fall in that context's
    ///   `[start, end)` window (legacy context-window filter; bypasses
    ///   the AST executor).
    /// - `Entity(eid)` → the cell must be that entity's `primary_cell_id`.
    ///   Cell-less entity pages have no visible cells; the embedded
    ///   backing cell is drawn directly by `render_entity_page`.
    /// - `People` → no cells visible (the page is bespoke).
    /// - `Ast` → delegate to `query::matches` against `view.ast`.
    fn is_visible_for_view(&self, cell: &Cell, ctx: &query::MatchContext) -> bool {
        // Closed cells drop out of every view by default; the global
        // "Show archived" toggle in the sidebar surfaces them again
        // (rendered dim — see `INACTIVE_ALPHA`). Checked first so the
        // rest of the predicate doesn't have to re-filter on each
        // view kind.
        if !cell.is_open() && !self.show_inactive_cells {
            return false;
        }
        // A focused cell whose caret is mid-edit inside a `#tag` token
        // stays visible regardless of filter match. Otherwise the in-
        // memory tag set shifts on every keystroke and the cell yanks
        // out from under the user the instant they edit the tag they're
        // filtering by. Gated on edit mode: outside editing no
        // keystrokes can change the text, so the filter should re-
        // evaluate normally (matches the persistence-flush gate).
        if self.pane().editing
            && self.pane().focused == Some(cell.id)
            && cell.caret_in_in_progress_tag()
        {
            return true;
        }
        match self.pane().view.view_kind {
            ViewKind::Context(id) => {
                let cell_ts = cell.timestamp;
                self.document.contexts.iter().find(|c| c.id == id).map_or(false, |c| {
                    cell_ts >= c.start_time && c.end_time.map_or(true, |e| cell_ts < e)
                })
            }
            ViewKind::Entity(eid) => self
                .entities
                .entities
                .iter()
                .find(|e| e.id == eid)
                .and_then(|e| e.primary_cell_id)
                .map_or(false, |pid| pid == cell.id),
            ViewKind::People => false,
            // Current is just another filter: cells from the last
            // CURRENT_WINDOW_DAYS whose snooze (if set) has already
            // elapsed. Open-ness is already enforced by the
            // is_open() guard above. Goes through the standard
            // cell-stream render path like Ast/Context.
            ViewKind::Current => {
                let now_ms = crate::cell::now_epoch_ms();
                let cutoff = now_ms - CURRENT_WINDOW_MS;
                if cell.timestamp < cutoff {
                    return false;
                }
                cell.resurface_after.map_or(true, |t| now_ms >= t)
            }
            ViewKind::Ast => query::matches(&self.pane().view.ast, cell, ctx),
            ViewKind::Cell(target) => cell.id == target,
        }
    }

    /// Distinct tag names across every in-memory cell, alphabetical
    /// (case-insensitive). The sidebar and the `#`-autocomplete popup
    /// both source from this so a freshly-committed tag appears on
    /// the next frame instead of waiting for the debounced save to
    /// land in the DB. Reference cells are skipped (no editable text).
    /// Right-click "Delete tag" handler. Strips every `TagSpan`
    /// covering `#name` from any in-memory cell, marks those cells
    /// dirty + bumps their `edited_at` so the next persistence
    /// flush rewrites their JSON without the spans, and deletes the
    /// row from the DB's `tags` table. The underlying text is left
    /// alone — the tag styling/semantic disappears, but the bytes
    /// stay where they were so the user can decide whether to clean
    /// them up. Cell-side strip happens in memory immediately so
    /// the sidebar drops the tag on the next frame, even before the
    /// debounced save fires.
    fn delete_tag_globally(&mut self, name: &str) {
        let mut affected: Vec<Uuid> = Vec::new();
        for cell in &mut self.document.cells {
            if cell.remove_tags_named(name) {
                affected.push(cell.id);
            }
        }
        for id in affected {
            self.mark_cell_dirty(id);
            self.touch_cell(id);
        }
        if let Some(db) = self.db.as_mut() {
            if let Err(e) = db.delete_tag(name) {
                eprintln!("kept: delete_tag failed for {name}: {e}");
            }
        }
        self.pane_mut().coalesce_break = true;
    }

    fn all_tag_names_in_memory(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for cell in &self.document.cells {
            for name in cell.all_tag_names() {
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        out
    }

    /// Build the per-render `MatchContext`: today's date plus the resolved
    /// entity-id sets for any `@id` refs in the active AST. Both the alias
    /// index and the title-fallback corpus are entity-derived (invariants
    /// #1, #2). Cheap — both inputs are already cached on `self`.
    fn match_context(&self) -> query::MatchContext {
        let today = local_date_for_ms(now_epoch_ms());
        let person_targets = query::resolve_persons(
            &self.pane().view.ast.include.entities,
            &self.entities.alias_index,
            &self.entities.title_fallback,
        );
        let person_excludes = query::resolve_persons(
            &self.pane().view.ast.exclude.entities,
            &self.entities.alias_index,
            &self.entities.title_fallback,
        );
        query::MatchContext {
            today,
            person_targets,
            person_excludes,
        }
    }

    /// Find the context whose window contains `cell_ts`. Used for rendering
    /// per-context section headers in Date view.
    fn context_for_timestamp(&self, cell_ts: i64) -> Option<&Context> {
        self.document.contexts.iter().find(|c| {
            cell_ts >= c.start_time && c.end_time.map_or(true, |e| cell_ts < e)
        })
    }

    /// Timestamp (epoch ms) of the most recently created cell anywhere in the
    /// stream, used for idle detection. None if no cells exist.
    fn last_cell_create_ms(&self) -> Option<i64> {
        self.document.cells.iter().map(|c| c.timestamp).max()
    }

    /// Close the writable (open) context and open a new one whose window
    /// starts at `now`. View follows: in Date view, stays in Date view; in
    /// Context view, switches to Context(new_id). Empty-writable case bumps
    /// `start_time` instead of creating a new context. Recorded as an
    /// undoable op.
    fn rotate_context_now(&mut self) {
        let now = now_epoch_ms();
        let writable = match self.writable_context_id() {
            Some(id) => id,
            None => return, // Invariant violated; bail safely.
        };
        let prev_view = self.pane_mut().view.clone();

        // Empty writable: bump its start_time instead of creating a new context.
        if !self.writable_has_cells() {
            let prev_start = self
                .document
                .contexts
                .iter()
                .find(|c| c.id == writable)
                .map(|c| c.start_time)
                .unwrap_or(now);
            if prev_start == now {
                return;
            }
            if let Some(ctx) = self.document.contexts.iter_mut().find(|c| c.id == writable) {
                ctx.start_time = now;
            }
            self.document.mark_context_dirty(writable);
            // View update: Context view follows to the bumped one; Date and
            // tag views keep their filters/time-bound unchanged.
            let new_view = rotate_view_to(&prev_view, writable);
            self.pane_mut().view = new_view.clone();
            self.undo_stack.push(UndoOp::ResetContextStart {
                context_id: writable,
                prev_start,
                new_start: now,
                prev_view,
                new_view,
            });
            self.redo_stack.clear();
            self.pane_mut().coalesce_break = true;
            return;
        }

        let prev_end_time = self
            .document
            .contexts
            .iter()
            .find(|c| c.id == writable)
            .and_then(|c| c.end_time);
        let new_context = Context {
            id: Uuid::now_v7(),
            start_time: now,
            end_time: None,
            title: None,
        };
        let pre_focused = self.pane_mut().focused;
        let pre_scroll_y = self.pane_mut().scroll_y;
        let new_view = rotate_view_to(&prev_view, new_context.id);

        self.apply_rotation(writable, now, &new_context, new_view.clone());

        self.undo_stack.push(UndoOp::RotateContext {
            closed_id: writable,
            prev_end_time,
            new_end_time: now,
            new_context,
            prev_view,
            new_view,
            pre_focused,
            pre_scroll_y,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
    }

    /// Does the writable (most-recent-open) context have any cells in its window?
    fn writable_has_cells(&self) -> bool {
        let Some(id) = self.writable_context_id() else {
            return false;
        };
        let Some(ctx) = self.document.contexts.iter().find(|c| c.id == id) else {
            return false;
        };
        let start = ctx.start_time;
        let end = ctx.end_time;
        self.document.cells
            .iter()
            .any(|c| c.timestamp >= start && end.map(|e| c.timestamp < e).unwrap_or(true))
    }

    /// Apply rotation forward: close `closed_id`, insert `new_context`,
    /// switch view as specified. Used by initial rotation and redo.
    fn apply_rotation(
        &mut self,
        closed_id: Uuid,
        new_end_time: i64,
        new_context: &Context,
        new_view: Query,
    ) {
        if let Some(ctx) = self.document.contexts.iter_mut().find(|c| c.id == closed_id) {
            ctx.end_time = Some(new_end_time);
        }
        self.document.mark_context_dirty(closed_id);
        let new_id = new_context.id;
        if !self.document.contexts.iter().any(|c| c.id == new_id) {
            self.document.contexts.push(new_context.clone());
        }
        self.document.mark_context_dirty(new_id);
        self.document.pending_context_deletes.remove(&new_id);
        self.pane_mut().view = new_view;
        self.pane_mut().focused = None;
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.cell_context_menu = None;
        self.pane_mut().scroll_y = 0.0;
    }

    /// Inverse of `apply_rotation`: restore the closed context's `end_time`,
    /// remove the new context (queue for DB deletion), restore prior view.
    fn inverse_rotation(
        &mut self,
        closed_id: Uuid,
        prev_end_time: Option<i64>,
        new_context_id: Uuid,
        prev_view: Query,
        pre_focused: Option<Uuid>,
        pre_scroll_y: f32,
    ) {
        if let Some(ctx) = self.document.contexts.iter_mut().find(|c| c.id == closed_id) {
            ctx.end_time = prev_end_time;
        }
        self.document.mark_context_dirty(closed_id);
        self.document.queue_context_delete(new_context_id);
        self.pane_mut().view = prev_view;
        self.pane_mut().focused = pre_focused;
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.cell_context_menu = None;
        self.pane_mut().scroll_y = pre_scroll_y;
    }

    /// Previous context (older `start_time`) relative to the currently
    /// viewed one. None when not in Context view.
    fn prev_context(&self) -> Option<Uuid> {
        let current = self.pane().view.context_view()?;
        let mut sorted: Vec<&Context> = self.document.contexts.iter().collect();
        sorted.sort_by_key(|c| c.start_time);
        let pos = sorted.iter().position(|c| c.id == current)?;
        if pos == 0 {
            None
        } else {
            Some(sorted[pos - 1].id)
        }
    }

    /// Next context (newer `start_time`). None when not in Context view.
    fn next_context(&self) -> Option<Uuid> {
        let current = self.pane().view.context_view()?;
        let mut sorted: Vec<&Context> = self.document.contexts.iter().collect();
        sorted.sort_by_key(|c| c.start_time);
        let pos = sorted.iter().position(|c| c.id == current)?;
        sorted.get(pos + 1).map(|c| c.id)
    }

    fn context_has_cells(&self, ctx: &Context) -> bool {
        let start = ctx.start_time;
        let end = ctx.end_time;
        self.document.cells
            .iter()
            .any(|c| c.timestamp >= start && end.map(|e| c.timestamp < e).unwrap_or(true))
    }

    /// Walk contexts forward in time from the current view, skipping empties.
    /// Used for arrow-nav cross-context jumps so an empty newer context
    /// doesn't trap the cursor.
    fn next_context_with_cells(&self) -> Option<Uuid> {
        let current = self.pane().view.context_view()?;
        let mut sorted: Vec<&Context> = self.document.contexts.iter().collect();
        sorted.sort_by_key(|c| c.start_time);
        let pos = sorted.iter().position(|c| c.id == current)?;
        sorted
            .iter()
            .skip(pos + 1)
            .find(|c| self.context_has_cells(c))
            .map(|c| c.id)
    }

    /// Walk contexts backward in time, skipping empties.
    fn prev_context_with_cells(&self) -> Option<Uuid> {
        let current = self.pane().view.context_view()?;
        let mut sorted: Vec<&Context> = self.document.contexts.iter().collect();
        sorted.sort_by_key(|c| c.start_time);
        let pos = sorted.iter().position(|c| c.id == current)?;
        sorted
            .iter()
            .take(pos)
            .rev()
            .find(|c| self.context_has_cells(c))
            .map(|c| c.id)
    }

    /// Make sure the current view will actually contain the cell we're
    /// about to write at `now`. In context view this means promoting from a
    /// closed context to the writable one. In date view, if the user is
    /// looking at a past day, jump them forward to today. Any other AST
    /// (tag view, multi-filter query, free-text search active, etc.) also
    /// jumps to today — a freshly-empty cell carries no tags / links /
    /// matching text, so staying in the filter would hide it.
    /// Returns true if the view changed.
    fn ensure_writable_context(&mut self) -> bool {
        let today = local_date_for_ms(now_epoch_ms());
        if let Some(id) = self.pane_mut().view.context_view() {
            let active_is_open = self
                .document
                .contexts
                .iter()
                .find(|c| c.id == id)
                .map_or(false, |c| c.end_time.is_none());
            if active_is_open {
                return false;
            }
            return match self.writable_context_id() {
                Some(target_id) => self.set_active_context(target_id),
                None => false,
            };
        }
        // AST view. Jump to today unless the AST is already exactly Day(today).
        if self.pane_mut().view.is_solo_date(today) {
            return false;
        }
        self.set_active_date(today)
    }

    /// Switch the view to a single existing context.
    fn set_active_context(&mut self, id: Uuid) -> bool {
        let next = Query::context(id);
        if self.pane_mut().view == next {
            return false;
        }
        if !self.document.contexts.iter().any(|c| c.id == id) {
            return false;
        }
        self.pane_mut().view = next;
        // Focus the first visible cell in the new window (if any).
        self.pane_mut().focused = self.visible_cell_ids().first().copied();
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.cell_context_menu = None;
        self.pane_mut().scroll_y = 0.0;
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    /// Switch the view to "everything from this local date" mode.
    fn set_active_date(&mut self, d: chrono::NaiveDate) -> bool {
        let next = Query::date(d);
        if self.pane_mut().view == next {
            return false;
        }
        self.pane_mut().view = next;
        self.pane_mut().focused = self.visible_cell_ids().first().copied();
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.cell_context_menu = None;
        self.pane_mut().scroll_y = 0.0;
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    /// Switch the view to "all cells carrying this tag" mode (no time bound).
    #[allow(dead_code)]
    fn set_active_tag(&mut self, name: String) -> bool {
        let next = Query::tag(name);
        if self.pane_mut().view == next {
            return false;
        }
        self.pane_mut().view = next;
        self.pane_mut().focused = self.visible_cell_ids().first().copied();
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.cell_context_menu = None;
        self.pane_mut().scroll_y = 0.0;
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    // ----- View history (Cmd/Ctrl+[ / ]) -----

    /// Snapshot the current `(view, focused, scroll_y)` onto the back
    /// stack, clear the forward stack, and transition to `new`. Called by
    /// deliberate-nav sites (sidebar clicks, search commit). Auto-flows
    /// (rotation, ensure_writable_context, undo) bypass this and mutate
    /// the view directly.
    fn push_view(&mut self, new: Query) -> bool {
        if self.pane_mut().view == new {
            return false;
        }
        let entry = HistoryEntry {
            query: self.pane_mut().view.clone(),
            focused: self.pane_mut().focused,
            scroll_y: self.pane_mut().scroll_y,
        };
        self.pane_mut().nav_back.push(entry);
        if self.pane_mut().nav_back.len() > NAV_HISTORY_CAP {
            self.pane_mut().nav_back.remove(0);
        }
        self.pane_mut().nav_forward.clear();
        self.pane_mut().view = new;
        self.pane_mut().focused = self.visible_cell_ids().first().copied();
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        // Any open context menu is anchored to the previous view —
        // dismiss every kind on a view switch.
        self.dismiss_open_context_menu();
        self.pane_mut().scroll_y = 0.0;
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    /// Cmd/Ctrl+[: pop the back stack onto the active view, pushing the
    /// current view onto the forward stack first. No-op when the back
    /// stack is empty.
    fn nav_back(&mut self) -> bool {
        let Some(prev) = self.pane_mut().nav_back.pop() else { return false };
        let entry = HistoryEntry {
            query: self.pane_mut().view.clone(),
            focused: self.pane_mut().focused,
            scroll_y: self.pane_mut().scroll_y,
        };
        self.pane_mut().nav_forward.push(entry);
        if self.pane_mut().nav_forward.len() > NAV_HISTORY_CAP {
            self.pane_mut().nav_forward.remove(0);
        }
        self.restore_history_entry(prev);
        true
    }

    /// Cmd/Ctrl+]: mirror of `nav_back`. No-op when the forward stack is
    /// empty (which is the case until the user has gone back at least
    /// once and not yet pushed a new view).
    fn nav_forward(&mut self) -> bool {
        let Some(next) = self.pane_mut().nav_forward.pop() else { return false };
        let entry = HistoryEntry {
            query: self.pane_mut().view.clone(),
            focused: self.pane_mut().focused,
            scroll_y: self.pane_mut().scroll_y,
        };
        self.pane_mut().nav_back.push(entry);
        if self.pane_mut().nav_back.len() > NAV_HISTORY_CAP {
            self.pane_mut().nav_back.remove(0);
        }
        self.restore_history_entry(next);
        true
    }

    fn restore_history_entry(&mut self, e: HistoryEntry) {
        self.pane_mut().view = e.query;
        self.pane_mut().focused = e.focused;
        self.pane_mut().scroll_y = e.scroll_y;
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.cell_context_menu = None;
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
    }

    /// IDs of cells visible under the active view, in DISPLAY order — newest
    /// first. Index 0 is the topmost (most recent) cell. `prev_visible` /
    /// `next_visible` operate on this same order, so "prev" = visually above.
    fn visible_cell_ids(&self) -> Vec<Uuid> {
        let ctx = self.match_context();
        let mut ids: Vec<Uuid> = self
            .document
            .cells
            .iter()
            .filter(|c| self.is_visible_for_view(c, &ctx))
            .map(|c| c.id)
            .collect();
        ids.reverse();
        ids
    }

    /// Insert a cell into the stream maintaining ascending timestamp
    /// order. Thin proxy to `Document::insert_cell_sorted`, which
    /// auto-dirties the new cell (S4: a freshly inserted cell needs
    /// to reach disk on the next flush).
    fn insert_cell_sorted(&mut self, cell: Cell) {
        self.document.insert_cell_sorted(cell);
    }

    fn mark_cell_dirty(&mut self, id: Uuid) {
        self.document.mark_cell_dirty(id);
    }

    fn touch_cell(&mut self, id: Uuid) {
        self.document.touch_cell(id);
    }

    /// Previous visible cell relative to `id` in timestamp order. None if
    /// `id` is unknown or already first.
    fn prev_visible(&self, id: Uuid) -> Option<Uuid> {
        let ids = self.visible_cell_ids();
        let pos = ids.iter().position(|x| *x == id)?;
        if pos == 0 {
            None
        } else {
            Some(ids[pos - 1])
        }
    }

    fn next_visible(&self, id: Uuid) -> Option<Uuid> {
        let ids = self.visible_cell_ids();
        let pos = ids.iter().position(|x| *x == id)?;
        ids.get(pos + 1).copied()
    }

    pub fn flush_persistence(&mut self) {
        // Snapshot before grabbing `&mut self.db` (else borrow conflict).
        // The deferral predicate only matters for the cell currently
        // being typed into; both flags get re-read fresh next flush.
        let editing_snapshot = self.pane_mut().editing;
        let focused_snapshot = self.pane_mut().focused;
        let Some(db) = self.db.as_mut() else {
            self.document.dirty_cells.clear();
            self.document.pending_deletes.clear();
            self.document.dirty_contexts.clear();
            self.document.pending_context_deletes.clear();
            return;
        };
        for id in self.document.pending_deletes.drain() {
            if let Err(e) = db.delete_cell(id) {
                eprintln!("kept: delete_cell failed for {id}: {e}");
            }
        }
        let dirty: Vec<Uuid> = self.document.dirty_cells.drain().collect();
        for id in dirty {
            // Defer this cell's save while a title `#tag` is mid-edit
            // (popup-driven typing OR plain in-place rename). Otherwise
            // each keystroke would persist a new partial-name tag and
            // pop the cell out of any tag-filtered view in real time.
            // The dirty mark is re-asserted so the next flush — once
            // the caret leaves the tag (whitespace, focus loss, or
            // moving outside the title) — saves the finalized name.
            //
            // Gated on `editing && focused`: the deferral exists for
            // in-flight typing, and typing only happens to the focused
            // cell in edit mode. Outside that (e.g., user hit Escape),
            // no keystrokes can change the text, so we save immediately
            // — otherwise the dirty cell would languish until something
            // else broke the predicate (re-focus, click elsewhere, …).
            let actively_editing = editing_snapshot && focused_snapshot == Some(id);
            if let Some(cell) = self.document.cells.iter().find(|c| c.id == id) {
                if actively_editing && cell.caret_in_in_progress_tag() {
                    self.document.dirty_cells.insert(id);
                    continue;
                }
                if let Err(e) = db.save_cell(cell) {
                    eprintln!("kept: save_cell failed for {id}: {e}");
                }
            }
        }
        for id in self.document.pending_context_deletes.drain() {
            if let Err(e) = db.delete_context(id) {
                eprintln!("kept: delete_context failed for {id}: {e}");
            }
        }
        let ctx_dirty: Vec<Uuid> = self.document.dirty_contexts.drain().collect();
        for id in ctx_dirty {
            if let Some(ctx) = self.document.contexts.iter().find(|c| c.id == id) {
                if let Err(e) = db.save_context(&context_ref(ctx)) {
                    eprintln!("kept: save_context failed for {id}: {e}");
                }
            }
        }
        // Entity caches may have shifted from save/delete cell hooks
        // (`#person`-tagged saves upsert; cell deletes detach). Reload.
        self.refresh_entities();
    }

    fn set_font_scale(&mut self, scale: f32) -> bool {
        let s = scale.clamp(ZOOM_MIN, ZOOM_MAX);
        if (s - self.font_scale).abs() < f32::EPSILON {
            return false;
        }
        self.font_scale = s;
        for cell in &mut self.document.cells {
            cell.set_font_scale(s);
        }
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    fn zoom_in(&mut self) -> bool {
        self.set_font_scale(self.font_scale * ZOOM_STEP)
    }

    fn zoom_out(&mut self) -> bool {
        self.set_font_scale(self.font_scale / ZOOM_STEP)
    }

    /// Mouse-wheel scroll. Routes to the pane under the cursor (per the
    /// multi-pane spec), not the active pane — letting the user scroll one
    /// pane while keyboard input goes to another. Falls back to the active
    /// pane when the cursor isn't over any pane.
    pub fn scroll_by(&mut self, dy: f32, phase: winit::event::TouchPhase) -> bool {
        // Wheel-over-sidebar scrolls the sidebar; otherwise the pane
        // under the mouse, falling back to the active pane. Both go
        // through the same `Scroller::apply_wheel` so kinetic decay,
        // fade timing, and interrupt rules behave identically.
        if self.mouse_pos.0 < SIDEBAR_WIDTH * self.font_scale {
            return self.sidebar_scroll.apply_wheel(dy, phase);
        }
        let target = self
            .pane_at(self.mouse_pos.0, self.mouse_pos.1)
            .unwrap_or(self.active_pane);
        let moved = self.panes[target].apply_wheel(dy, phase);
        if moved {
            // Any doc-anchored menu loses its anchor on a pane
            // scroll — dismiss them all so a stale menu doesn't
            // sit at the wrong y for a cell that's now scrolled
            // off / shifted.
            self.dismiss_open_context_menu();
        }
        moved
    }

    /// Advance kinetic decay for `pane_idx`. Thin wrapper that
    /// delegates to the pane's `Scroller::step_kinetic`.
    fn step_kinetic(&mut self, pane_idx: usize) {
        let _ = self.panes[pane_idx].step_kinetic();
    }

    /// True iff any scrollable surface has enough velocity to need
    /// another frame. main.rs checks this after `tick` to schedule a
    /// redraw. Sidebar coast counts too — it shares the kinetic path.
    pub fn is_animating(&self) -> bool {
        self.panes.iter().any(|p| p.has_velocity())
            || self.sidebar_scroll.has_velocity()
            || self.toast_alpha() > 0.0
    }

    /// Show a transient confirmation pill. Replaces any toast already
    /// on screen — the most recent action wins; no queue. Called from
    /// actions that succeed silently (e.g. "Surface as reference"
    /// inserts a cell into today's view but the user is browsing an
    /// old date, so the visible side-effect of zero would be
    /// confusing without a confirmation).
    fn show_toast(&mut self, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            shown_at: Instant::now(),
        });
    }

    /// Current toast alpha in [0.0, 1.0]. 1.0 during the hold window,
    /// linearly fades to 0.0 over `TOAST_FADE`, 0.0 once expired
    /// (caller drops the toast on its next render pass).
    fn toast_alpha(&self) -> f32 {
        let Some(t) = self.toast.as_ref() else {
            return 0.0;
        };
        let elapsed = t.shown_at.elapsed();
        if elapsed <= TOAST_HOLD {
            1.0
        } else if elapsed >= TOAST_HOLD + TOAST_FADE {
            0.0
        } else {
            let into_fade = elapsed - TOAST_HOLD;
            1.0 - (into_fade.as_secs_f32() / TOAST_FADE.as_secs_f32())
        }
    }

    /// Halt kinetic coast on every scrollable surface. Called from any
    /// user input other than the wheel itself — clicks and key presses
    /// always win.
    fn kill_all_kinetic(&mut self) {
        for p in &mut self.panes {
            p.kill_kinetic();
        }
        self.sidebar_scroll.kill_kinetic();
    }

    pub fn tick(&mut self, canvas: &Canvas, width: f32, height: f32) {
        // Hot-reload colors from disk (~250ms throttle inside).
        // Cheap when the file's mtime hasn't changed.
        crate::color::maybe_reload();
        canvas.clear(crate::color::bg_page());
        self.layout_panes(width, height);

        // Render each pane. We swap `active_pane` for the duration of each
        // pane's tick so Deref-based field access (self.pane_mut().scroll_y, self.pane_mut().view,
        // etc.) resolves to the pane currently being rendered. The truly
        // active pane (saved_active) is restored before global UI is drawn
        // so sidebar highlight / overlays anchor correctly.
        //
        // Render order: inactive panes first, active pane LAST. Cell
        // positions (cell.x_origin/y_origin) are stored on the Cell itself,
        // so whichever pane renders last "wins" them. Putting the active
        // pane last means in-pane hit-testing (find_cell_at) reads the
        // correct positions. Clicks on an inactive pane just activate it —
        // a second click is needed to focus a specific cell, since cell
        // positions for that pane don't update until the next frame.
        let saved_active = self.active_pane;
        let n = self.panes.len();
        for i in 0..n {
            if i == saved_active {
                continue;
            }
            self.active_pane = i;
            self.tick_pane(canvas, i, height);
        }
        if n > 0 {
            self.active_pane = saved_active;
            self.tick_pane(canvas, saved_active, height);
        }

        // Pane chrome (between and around).
        self.render_divider(canvas, height);
        self.render_active_pane_indicator(canvas);

        // Sidebar (window space, single global instance). Step its
        // kinetic decay before render so a coast advances each frame
        // — matches the per-pane `step_kinetic` call in `tick_pane`.
        let _ = self.sidebar_scroll.step_kinetic();
        self.render_sidebar(canvas, height);

        // Overlays (window space, drawn last so they layer on top).
        // The URL-bar pill + result dropdown live inside each pane's
        // `render_pane_header` (see `pane.rs`); they're already
        // painted before this point.
        let _ = width; // kept for symmetry with mention_popup signature
        // Quick-Add modal sits above the cell stream + panes but
        // *under* the mention popup, so a `@`/`#` autocomplete
        // triggered inside the modal renders on top of the modal
        // (not buried beneath it).
        self.render_quick_add(canvas, width, height);
        self.render_mention_popup(canvas, width, height);
        self.render_context_menus(canvas, width, height);
        self.render_toast(canvas, width, height);

        // Publish this frame's hit-test rects. Every render method writes
        // into `hit_tests_builder`; the swap below makes them visible to
        // mouse_down / right_click / dispatch_* atomically, with no
        // partial-frame state ever observable. If the next frame is
        // skipped (window unfocused etc.), `hit_tests` retains this
        // frame's snapshot — correct, not stale-from-N-frames-ago.
        self.hit_tests = std::mem::take(&mut self.hit_tests_builder);

        // Persistence flush is global (dirty cells aren't per-pane), so it
        // runs once per frame, after all panes have rendered.
        self.maybe_flush_persistence();
    }

    /// Render the three right-click context menus (tag / people /
    /// cell). Builds a `MenuRenderCtx` from the slim slice of self the
    /// menus actually need (S8: per-subsystem context instead of
    /// `&mut KeptApp`) and delegates to each menu's `render` method.
    fn render_context_menus(&mut self, canvas: &Canvas, width: f32, height: f32) {
        // Tag menu: stateless beyond the menu struct itself.
        if let Some(menu) = self.tag_context_menu.as_ref() {
            let mut ctx = MenuRenderCtx {
                font_scale: self.font_scale,
                typeface: &self.typeface,
                mouse_pos: self.mouse_pos,
                hit_tests: &mut self.hit_tests_builder,
            };
            menu.render(canvas, width, height, &mut ctx);
        }
        // People menu: same.
        if let Some(menu) = self.people_context_menu.as_ref() {
            let mut ctx = MenuRenderCtx {
                font_scale: self.font_scale,
                typeface: &self.typeface,
                mouse_pos: self.mouse_pos,
                hit_tests: &mut self.hit_tests_builder,
            };
            menu.render(canvas, width, height, &mut ctx);
        }
        // Cell menu: needs the cell the menu was opened on to format
        // its info rows + the active-toggle labels.
        if let Some(menu) = self.cell_context_menu.as_ref() {
            if let Some(cell) = self.document.cell(menu.cell_id) {
                let mut ctx = MenuRenderCtx {
                    font_scale: self.font_scale,
                    typeface: &self.typeface,
                    mouse_pos: self.mouse_pos,
                    hit_tests: &mut self.hit_tests_builder,
                };
                menu.render(canvas, width, height, cell, &mut ctx);
            }
        }
        // Bar menu: whole-cell operations. Same shape as cell menu —
        // needs the cell for timestamps + the Unsnooze visibility.
        if let Some(menu) = self.bar_context_menu.as_ref() {
            if let Some(cell) = self.document.cell(menu.cell_id) {
                let mut ctx = MenuRenderCtx {
                    font_scale: self.font_scale,
                    typeface: &self.typeface,
                    mouse_pos: self.mouse_pos,
                    hit_tests: &mut self.hit_tests_builder,
                };
                menu.render(canvas, width, height, cell, &mut ctx);
            }
        }
    }

    /// Draw the transient confirmation pill, if active. Bottom-center
    /// of the window with a 24px bottom margin; rounded rect with a
    /// semi-transparent dark bg, light-on-dark text. Fades out per
    /// `toast_alpha`; expired toasts are dropped so future renders
    /// stop touching the canvas.
    fn render_toast(&mut self, canvas: &Canvas, width: f32, height: f32) {
        let alpha = self.toast_alpha();
        if alpha <= 0.0 {
            self.toast = None;
            return;
        }
        let Some(t) = self.toast.as_ref() else {
            return;
        };
        let scale = self.font_scale;
        let font = Font::from_typeface(&self.typeface, 14.0 * scale);
        let (_, fm) = font.metrics();
        let text_w = font
            .measure_str(&t.message, None)
            .0;
        let pad_x = 16.0 * scale;
        let pad_y = 8.0 * scale;
        let pill_w = text_w + pad_x * 2.0;
        let pill_h = (-fm.ascent + fm.descent) + pad_y * 2.0;
        let pill_x = (width - pill_w) * 0.5;
        let pill_y = height - pill_h - 24.0 * scale;
        let pill_rect = Rect::new(pill_x, pill_y, pill_x + pill_w, pill_y + pill_h);
        let radius = pill_h * 0.5;

        // Drop shadow for a bit of depth — soft warm-tan-on-bg.
        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        let shadow_alpha = (alpha * 0x40 as f32).round() as u8;
        shadow.set_color(crate::color::dark_alpha(shadow_alpha));
        shadow.set_mask_filter(skia_safe::MaskFilter::blur(
            skia_safe::BlurStyle::Normal,
            FOCUS_SHADOW_BLUR * 0.5,
            None,
        ));
        canvas.draw_round_rect(pill_rect, radius, radius, &shadow);

        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        let bg_alpha = (alpha * 0xe0 as f32).round() as u8;
        bg.set_color(crate::color::dark_alpha(bg_alpha));
        canvas.draw_round_rect(pill_rect, radius, radius, &bg);

        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        let text_alpha = (alpha * 0xff as f32).round() as u8;
        text_paint.set_color(skia_safe::Color::from_argb(text_alpha, 0xff, 0xff, 0xff));
        let baseline = pill_y + pad_y + (-fm.ascent);
        canvas.draw_str(
            &t.message,
            Point::new(pill_x + pad_x, baseline),
            &font,
            &text_paint,
        );
    }


    /// The cell-stream view body (Ast / Context). Pre-computes per-cell
    /// visibility + section headers, then loops visible cells
    /// newest-first, drawing context/date headers and delegating each
    /// cell to `cell.tick()` / `render_reference_cell()` /
    /// `render_envelope_outline_cell()`. Returns the final y cursor.
    fn render_cell_stream(&mut self, canvas: &Canvas, layout: &PaneLayout) -> f32 {
        let cells_left = layout.cells_left;
        let outer_cell_width = layout.outer_cell_width;
        let content_width = layout.content_width;
        let scale = self.font_scale;
        let focused_id = self.pane_mut().focused;
        let editing_local = self.pane_mut().editing;
        let mut y = MARGIN_TOP;

        // Principled layout: the cell column splits horizontally
        // into a bar slice on the left and a body slice on the right.
        // The bar lives in `[cells_left, cells_left + bar_full_w]`.
        // Everything else (headers, cell content, the cell's chrome
        // — outline / wrapper / focus ring) lives inside the body
        // slice at `[body_x, body_x + body_w]` where
        // `body_x = cells_left + bar_full_w + bar_gap`. The
        // CELL_BAR_GAP-equals-FOCUS_PAD invariant means the cell's
        // chrome left edge (= body_x - FOCUS_PAD) lands exactly on
        // the bar's right edge — no overlap, no visible gap.
        let bar_full_w = CELL_BAR_W * scale;
        let bar_gap = CELL_BAR_GAP * scale;
        let bar_left_x = cells_left;
        let bar_right_x = bar_left_x + bar_full_w;
        let body_x = cells_left + bar_full_w + bar_gap;
        let body_w = (content_width - bar_full_w - bar_gap).max(40.0);
        // `hit_tests_builder` is reset to default between frames by
        // the mem::take in `tick`; no per-frame clear needed here
        // (and clearing inside this fn would erase the other pane's
        // bars in a multi-pane layout).
        let now_ms_for_bars = crate::cell::now_epoch_ms();

        // Two-phase render: record cells into a Picture so we can sandwich
        // them between the focus-card backdrop (under) and focus ring
        // (over) using a pane-local FocusedCellGeom captured during the
        // recording pass. Doing it post-render with `cell.y_origin()`
        // was broken in multi-pane layouts because that field is a single
        // shared cell-level value overwritten by whichever pane rendered
        // most recently.
        let mut recorder = skia_safe::PictureRecorder::new();
        let rec_bounds = Rect::new(-1.0e6, -1.0e6, 1.0e6, 1.0e6);
        let rec_canvas = recorder.begin_recording(rec_bounds, None);
        let mut focused_geom: Option<FocusedCellGeom> = None;

        // Precompute per-cell visibility and section headers in a
        // single descending walk. Cells are stored ascending in
        // `document.cells`; rendering iterates descending (newest
        // first), so a header lands above the first cell of each
        // group as the user scrolls top-down.
        //
        // Date view groups by context (multiple contexts can land in
        // one day). Any other AST view (tag, search query,
        // multi-filter, free-text) groups by local date. Context view
        // and the single-cell view have no inter-group headers.
        #[derive(PartialEq, Eq)]
        enum HeaderMode {
            ByContext,
            ByDate,
            None,
        }
        let view_ref = &self.pane().view;
        let header_mode = if !matches!(view_ref.view_kind, ViewKind::Ast) {
            HeaderMode::None
        } else if matches!(
            view_ref.ast.include.time,
            Some(query::TimeFilter::Day(_))
        ) && view_ref.ast.include.tags.is_empty()
            && view_ref.ast.include.entities.is_empty()
            && view_ref.ast.exclude.tags.is_empty()
            && view_ref.ast.exclude.entities.is_empty()
            && view_ref.ast.text.is_empty()
        {
            HeaderMode::ByContext
        } else {
            HeaderMode::ByDate
        };
        let total_cells = self.document.cells.len();
        let match_ctx = self.match_context();
        let mut visible: Vec<bool> = vec![false; total_cells];
        let mut headers: Vec<Option<String>> = vec![None; total_cells];
        let mut last_label: Option<String> = None;
        for i in (0..total_cells).rev() {
            let cell = &self.document.cells[i];
            let v = self.is_visible_for_view(cell, &match_ctx);
            visible[i] = v;
            if !v || header_mode == HeaderMode::None {
                continue;
            }
            let label: String = match header_mode {
                HeaderMode::ByContext => self
                    .context_for_timestamp(cell.timestamp)
                    .map(|c| format_context_time(c.start_time))
                    .unwrap_or_default(),
                HeaderMode::ByDate => format_date_label(local_date_for_ms(cell.timestamp)),
                HeaderMode::None => unreachable!(),
            };
            if last_label.as_deref() != Some(label.as_str()) {
                last_label = Some(label.clone());
                headers[i] = Some(label);
            }
        }

        let header_font = Font::from_typeface(
            &self.typeface,
            CONTEXT_HEADER_FONT_SIZE * scale,
        );
        let (_, hm) = header_font.metrics();
        let header_h = CONTEXT_HEADER_H * scale;
        let header_pad_top = CONTEXT_HEADER_PAD_TOP * scale;

        // Viewport culling: only cells whose vertical band overlaps
        // `[viewport_top, viewport_bot]` get fully rendered. Cells
        // outside that band re-use their previously-cached
        // `cell.height()` to advance `y` and to refresh
        // `cell.set_view_geometry`, but skip the expensive `cell.tick`
        // / `render_reference_cell` / bar+outline drawing. The slack
        // half-viewport above and below absorbs a fast scroll without
        // exposing blank space for a frame. Cells with no cached
        // height (just inserted, view just switched) bypass the cull
        // and render eagerly; the focused cell also bypasses so its
        // `FocusedCellGeom` is always captured.
        let scroll_y = self.pane().scroll_y;
        let body_viewport_h = (layout.pane_h - PANE_HEADER_H).max(0.0);
        let cull_slack = body_viewport_h * 0.5;
        let viewport_top = scroll_y - cull_slack;
        let viewport_bot = scroll_y + body_viewport_h + cull_slack;

        // Render cells newest-first (descending) — index walked in reverse so
        // self.document.cells (asc) iterates from end to start.
        for i in (0..total_cells).rev() {
            if !visible[i] {
                continue;
            }
            let cached_h = self.document.cells[i].height();
            let header_advance = if headers[i].is_some() { header_h } else { 0.0 };
            let cell_top_doc = y + header_advance;
            let cell_bot_doc = cell_top_doc + cached_h;
            let in_viewport = cell_bot_doc >= viewport_top && cell_top_doc <= viewport_bot;
            let cell_is_focused_now = focused_id == Some(self.document.cells[i].id);
            let can_skip = cached_h > 0.0 && !in_viewport && !cell_is_focused_now;
            if can_skip {
                // Refresh the cell's recorded geometry (y may have
                // shifted from prior frames as content above grew /
                // shrank) so hit-tests and embed lookups stay
                // accurate even without a full tick.
                let cell_y_skip = y + header_advance;
                self.document.cells[i].set_view_geometry(
                    body_x,
                    cell_y_skip,
                    body_w,
                    cached_h,
                );
                y += header_advance + cached_h + CELL_GAP;
                continue;
            }
            if let Some(label) = &headers[i] {
                let header_y = y + header_pad_top;
                let baseline = header_y + (-hm.ascent);
                let mut hp = Paint::default();
                hp.set_anti_alias(true);
                hp.set_color(crate::color::text_muted_warm());
                // Headers align with the cell content (post-bar
                // shift) so the label sits flush with cell text
                // below it — not floating into the bar column.
                rec_canvas.draw_str(
                    label,
                    Point::new(body_x, baseline),
                    &header_font,
                    &hp,
                );
                let label_w = header_font.measure_str(label, Some(&hp)).0;
                let line_y = baseline - hm.ascent / 3.0;
                let mut lp = Paint::default();
                lp.set_anti_alias(true);
                lp.set_color(crate::color::heading_rule());
                lp.set_stroke_width(1.5);
                rec_canvas.draw_line(
                    Point::new(body_x + label_w + 8.0 * scale, line_y),
                    Point::new(cells_left + outer_cell_width, line_y),
                    &lp,
                );
                y += header_h;
            }
            let cell_y = y;
            let cell_id = self.document.cells[i].id;
            let is_reference = matches!(self.document.cells[i].kind, CellKind::Reference(_));
            let is_envelope_outline = matches!(
                &self.document.cells[i].kind,
                CellKind::Outline(oc) if oc.has_reference_header()
            );
            let cell_is_focused =
                focused_id.map(|f| f == cell_id).unwrap_or(false);

            // Selection highlights are visible whenever the cell is focused
            // (so view-mode users can drag-select). Caret only renders in
            // edit mode.
            let render_focused = cell_is_focused;
            let show_caret = cell_is_focused && editing_local;
            // Inactive ("archived") cells reach this point only
            // when the global toggle is on (otherwise the
            // visibility filter dropped them earlier). Wrap
            // their entire render — body, post-tick outline,
            // and any embed chrome — in an alpha layer so the
            // dim treatment composites uniformly without
            // threading a paint color through every primitive.
            let cell_inactive = !self.document.cells[i].is_open();
            if cell_inactive {
                rec_canvas.save_layer_alpha(None, cell::INACTIVE_ALPHA as u32);
            }
            let h = if is_reference {
                // Reference cells render via the app layer (which can
                // see the full cell list to look up the target).
                self.render_reference_cell(
                    rec_canvas,
                    i,
                    body_x,
                    cell_y,
                    body_w,
                    render_focused,
                )
            } else if is_envelope_outline {
                // Envelope outlines: read-only embed at the
                // top + editable bullet body. Same reason as
                // Reference — needs the cell list to refresh
                // the embed cache.
                self.render_envelope_outline_cell(
                    rec_canvas,
                    i,
                    body_x,
                    cell_y,
                    body_w,
                    render_focused,
                    show_caret,
                )
            } else {
                // Bullet-granular tag filter for non-focused
                // outline cells: when the active view filters by
                // `#tag` and this outline matches via body
                // bullets only (title doesn't carry the tag),
                // restrict the render to matching bullets +
                // their subtree descendants. Cleared (None) for
                // focused cells so interaction always sees the
                // full outline.
                // Clone the include-tag list so we can take a
                // mutable borrow on `self.document.cells` without keeping
                // an outstanding immutable borrow on `self.pane_mut().view`.
                let include_tags = self.pane_mut().view.ast.include.tags.clone();
                let show_inactive = self.show_inactive_cells;
                let cell = &mut self.document.cells[i];
                let filter = compute_outline_bullet_filter(
                    cell,
                    &include_tags,
                    cell_is_focused,
                );
                if let CellKind::Outline(oc) = &mut cell.kind {
                    oc.set_bullet_filter(filter);
                    oc.set_show_inactive(show_inactive);
                }
                cell.tick(
                    rec_canvas,
                    body_x,
                    cell_y,
                    body_w,
                    render_focused,
                    show_caret,
                )
            };

            // Capture the focused cell's pane-local geometry. The
            // backdrop and ring (drawn outside the recording onto
            // the real canvas) read from this snapshot, so they
            // always track *this* pane regardless of which pane
            // rendered last. Geometry = the cell's body region;
            // the focus ring extends FOCUS_PAD on each side, which
            // lands its left edge exactly on `bar.right_x` (since
            // CELL_BAR_GAP == FOCUS_PAD).
            if cell_is_focused {
                focused_geom = Some(FocusedCellGeom {
                    x: body_x,
                    y: cell_y,
                    w: body_w,
                    h,
                    bar_left_dx: bar_full_w + bar_gap,
                });
            }

            // Outline rect — wraps the cell BODY with FOCUS_PAD
            // padding on each side (same as the focus ring). With
            // CELL_BAR_GAP == FOCUS_PAD, the outline's left edge
            // sits at `bar.right_x`, so the bar appears flush
            // against the outline without any merging logic.
            let outline_rect = Rect::new(
                body_x - FOCUS_PAD,
                cell_y - FOCUS_PAD,
                body_x + body_w + FOCUS_PAD,
                cell_y + h + FOCUS_PAD,
            );

            // Bar visual + hit rect. Every cell type's chrome
            // now has the same outer geometry — FOCUS_PAD on each
            // side around the body — so the bar's vertical bounds
            // are uniform too. Per-corner radii: TL/BL rounded
            // (match the chrome's corner radius), TR/BR flat
            // (against the cell card).
            let bar_top = cell_y - FOCUS_PAD;
            let bar_bottom = cell_y + h + FOCUS_PAD;
            let bar_color = bar_color_for_cell(&self.document.cells[i], now_ms_for_bars);
            let bar_visual_rect = Rect::new(
                bar_left_x,
                bar_top,
                bar_right_x,
                bar_bottom,
            );
            let mut bar_paint = Paint::default();
            bar_paint.set_anti_alias(true);
            bar_paint.set_color(bar_color);
            let r = FOCUS_RADIUS;
            let zero = skia_safe::Vector::new(0.0, 0.0);
            let corner = skia_safe::Vector::new(r, r);
            let bar_rr = skia_safe::RRect::new_rect_radii(
                bar_visual_rect,
                &[corner, zero, zero, corner],
            );
            rec_canvas.draw_rrect(&bar_rr, &bar_paint);

            // Hit rect extends across the visible bar AND the
            // bar_gap up to the chrome's left edge, so a click
            // anywhere in the left column registers.
            let bar_rect = Rect::new(
                bar_left_x,
                bar_top,
                body_x,
                bar_bottom,
            );

            // Faint outline around non-focused cells so each one
            // reads as a distinct unit. Drawn in the same position
            // the focus ring would occupy so cells don't visually
            // shift when focus moves. Reference cells have their
            // own dashed warm-tan border — skip the standard
            // outline so the two don't compete.
            //
            // TL/BL corners are FLAT — the bar provides those
            // outer corners. TR/BR rounded matches the bar's
            // rounded BL/TL on its side, giving the combined card
            // a uniform rounded outer shape and a clean vertical
            // seam at `bar.right`.
            if !cell_is_focused && !is_reference {
                let mut outline = Paint::default();
                outline.set_anti_alias(true);
                outline.set_style(PaintStyle::Stroke);
                outline.set_stroke_width(CELL_OUTLINE_STROKE);
                outline.set_color(crate::color::dark_alpha(CELL_OUTLINE_ALPHA));
                let path = chrome_open_path(outline_rect, FOCUS_RADIUS);
                rec_canvas.draw_path(&path, &outline);
            }
            if cell_inactive {
                rec_canvas.restore();
            }

            // Record the bar's hit rect for click dispatch
            // (left-click → focus cell view-mode, right-click →
            // BarContextMenu).
            self.hit_tests_builder
                .cell_bars
                .push((cell_id, bar_rect));

            y += h + CELL_GAP;
        }

        // Phase 2: finalize the recording and composite onto the real
        // canvas in z-order: focus backdrop (under) → cell stream replay
        // → focus ring (over).
        let picture = recorder.finish_recording_as_picture(None);
        self.render_focus_card_backdrop(canvas, focused_geom);
        if let Some(pic) = picture {
            canvas.draw_picture(&pic, None, None);
        }
        self.render_focus_ring(canvas, focused_geom);

        y
    }


    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        // Any *fresh* key press cancels in-flight kinetic coast on
        // every pane. Releases don't (so a kinetic scroll doesn't get
        // killed by the user releasing a modifier they were holding
        // while wheeling). OS auto-repeat events also don't — those
        // are the system reasserting the held state, not new user
        // input, and killing kinetic on every repeat would nuke a
        // post-mouse-up Space-drag fling while the user is still
        // holding Space.
        if event.state == ElementState::Pressed && !event.repeat {
            self.kill_all_kinetic();
        }

        // Pane chord follow-up: when armed (by Ctrl+W), intercept the very
        // next "real" key press as a pane command. Modifier-only press
        // events (Shift/Ctrl/Alt by themselves) and key releases are
        // ignored so holding Shift while picking a follow-up doesn't break
        // the chord. Auto-cancels after PANE_CHORD_TIMEOUT.
        if let Some(armed_at) = self.pane_chord_armed {
            if armed_at.elapsed() > PANE_CHORD_TIMEOUT {
                self.pane_chord_armed = None;
            } else if event.state == ElementState::Pressed {
                let is_mod_only = matches!(
                    event.logical_key,
                    Key::Named(
                        NamedKey::Shift
                            | NamedKey::Control
                            | NamedKey::Alt
                            | NamedKey::Super
                            | NamedKey::Meta
                    )
                );
                if !is_mod_only {
                    self.pane_chord_armed = None;
                    return self.dispatch_pane_chord(event);
                }
            }
        }

        // Esc closes any open right-click context menu (cell, bar,
        // tag, people) before falling through to other Esc bindings.
        if event.state == ElementState::Pressed
            && matches!(event.logical_key, Key::Named(NamedKey::Escape))
            && self.dismiss_open_context_menu()
        {
            return true;
        }

        // Cmd/Ctrl+H — toggle the Quick-Add modal. Shift adds a
        // title slot. Same key while open commits + closes (the
        // "yeet" gesture). The toggle short-circuits BEFORE the
        // generic Quick-Add key forwarder below so the H itself
        // doesn't land as a typed character in the modal.
        if event.state == ElementState::Pressed
            && primary_mod(modifiers.state())
            && matches!(&event.logical_key, Key::Character(s) if s.as_str().eq_ignore_ascii_case("h"))
        {
            let with_title = modifiers.state().shift_key();
            self.toggle_quick_add(with_title);
            return true;
        }
        // While the Quick-Add modal is open it owns the keyboard.
        // Routes Esc to commit + close, everything else to the
        // modal's cell.
        if self.quick_add.is_some() {
            return self.handle_quick_add_key(event, modifiers);
        }

        // Cmd/Ctrl+L — toggle focus on the active pane's header
        // pill (the URL bar). When focused with synced text we
        // select-all so the next keystroke starts fresh, matching
        // browser URL-bar feel.
        if event.state == ElementState::Pressed
            && primary_mod(modifiers.state())
            && matches!(&event.logical_key, Key::Character(s) if s.as_str().eq_ignore_ascii_case("l"))
        {
            let idx = self.active_pane;
            if self.panes[idx].header.focused {
                self.panes[idx].header.blur();
            } else {
                // Blur the other pane just in case.
                for (i, p) in self.panes.iter_mut().enumerate() {
                    if i != idx {
                        p.header.blur();
                    }
                }
                self.panes[idx].header.focused = true;
                self.panes[idx].header.selected = None;
                self.panes[idx].header.textbox.select_all();
                // Other transient overlays would compete for input.
                self.mention_popup = None;
                self.cell_context_menu = None;
            }
            return true;
        }

        // Pane header URL-bar pill: when focused it doubles as the
        // search input. Arrow nav / Enter commit / Esc blur are
        // handled here; clipboard + select-all + undo / redo route
        // through the shared `apply_clipboard_shortcut` helper
        // (same path as every other inline input). Other keystrokes
        // edit the textbox.
        let header_focused_pane = self.panes.iter().position(|p| p.header.focused);
        if let Some(idx) = header_focused_pane {
            let mods = modifiers.state();
            // Mention popup over the pill (typed `@` or `#`):
            // owns Enter/Tab/Esc/Up/Down for that picker only.
            if self.mention_popup.is_some() && event.state == ElementState::Pressed {
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
                        self.commit_mention();
                        return true;
                    }
                    _ => {}
                }
            }
            if event.state == ElementState::Pressed {
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        self.panes[idx].header.blur();
                        return true;
                    }
                    Key::Named(NamedKey::Enter) => {
                        // Filter-first: with no row picked, commit
                        // the typed query as a view (browser URL-bar
                        // feel). With a row picked (Down arrow), jump
                        // to that cell instead. Alt+Enter routes into
                        // the other pane in both cases.
                        let alt = mods.alt_key();
                        match self.panes[idx].header.selected {
                            Some(row) => self.commit_header_result(idx, row, alt),
                            None => self.commit_header_filter(idx, alt),
                        }
                        return true;
                    }
                    Key::Named(NamedKey::ArrowUp) if !mods.shift_key() => {
                        self.panes[idx].header.move_selection(-1);
                        return true;
                    }
                    Key::Named(NamedKey::ArrowDown) if !mods.shift_key() => {
                        self.panes[idx].header.move_selection(1);
                        return true;
                    }
                    _ => {}
                }
            }
            // Cmd/Ctrl + letter: clipboard / undo / select-all
            // route through the shared helper. Swallow other
            // letter combos so app shortcuts don't fire behind
            // the focused pill. Named keys (arrows, Home/End,
            // Backspace) under primary_mod fall through to the
            // textbox so word-nav and line-edge work.
            if event.state == ElementState::Pressed && primary_mod(mods) {
                let clipboard = self.clipboard.as_mut();
                let pre = self.panes[idx].header.textbox.text().to_string();
                if apply_clipboard_shortcut(
                    &mut self.panes[idx].header.textbox,
                    clipboard,
                    event,
                    mods,
                    true,
                ) {
                    if self.panes[idx].header.textbox.text() != pre {
                        self.panes[idx].header.selected = None;
                        self.sync_mention_popup();
                    }
                    return true;
                }
                if let Key::Character(_) = &event.logical_key {
                    return true;
                }
                // Fall through for Named keys (arrows etc.) so
                // word-nav reaches the textbox.
            }
            // Forward to the textbox; on text change reset
            // `selected` so the result list stays in sync, and
            // hook the mention popup against new triggers.
            let pre = self.panes[idx].header.textbox.text().to_string();
            let popup_was_open = self.mention_popup.is_some();
            self.panes[idx].header.textbox.handle_key(event, modifiers);
            let post = self.panes[idx].header.textbox.text().to_string();
            if pre != post {
                self.panes[idx].header.selected = None;
            }
            if !popup_was_open {
                match event.text.as_deref() {
                    Some("@") => self.try_open_mention_popup(MentionKind::Person),
                    Some("#") => self.try_open_mention_popup(MentionKind::Tag),
                    _ => {}
                }
            }
            self.sync_mention_popup();
            return true;
        }

        // People-page rename input: Enter and Esc both commit (Esc is a
        // "blur" that keeps the typed text live, matching the cell
        // edit-vs-view modal elsewhere). Clipboard shortcuts route
        // through the shared helper so Cmd+V actually pastes instead
        // of typing a literal "v"; everything else flows into the
        // embedded TextBox.
        if event.state == ElementState::Pressed && self.people_rename.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter) => {
                    self.commit_people_rename();
                    return true;
                }
                _ => {}
            }
            let rename = self.people_rename.as_mut();
            let clipboard = self.clipboard.as_mut();
            if let Some(rs) = rename {
                if apply_clipboard_shortcut(
                    &mut rs.input,
                    clipboard,
                    event,
                    modifiers.state(),
                    true,
                ) {
                    return true;
                }
                return rs.input.handle_key(event, modifiers);
            }
        }

        // People-page Add input: same shape as rename. Esc / Enter both
        // commit; trimmed-empty input is still a no-op (no row created).
        if event.state == ElementState::Pressed && self.people_add.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter) => {
                    self.commit_people_add();
                    return true;
                }
                _ => {}
            }
            let input = self.people_add.as_mut();
            let clipboard = self.clipboard.as_mut();
            if let Some(input) = input {
                if apply_clipboard_shortcut(
                    input,
                    clipboard,
                    event,
                    modifiers.state(),
                    true,
                ) {
                    return true;
                }
                return input.handle_key(event, modifiers);
            }
        }

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
                    self.commit_mention();
                    return true;
                }
                _ => {}
            }
        }

        if event.state == ElementState::Pressed && primary_mod(modifiers.state()) {
            // Pane chord leader: Ctrl+W arms the chord. The next key (handled
            // by the intercept at the top of `handle_key`) is interpreted as
            // a pane command — switch active, split, close, reset divider.
            if !modifiers.state().shift_key() && !modifiers.state().alt_key() {
                if let Key::Character(s) = &event.logical_key {
                    if s.as_str().eq_ignore_ascii_case("w") {
                        self.pane_chord_armed = Some(Instant::now());
                        return true;
                    }
                }
            }
            match &event.logical_key {
                // View history: Cmd/Ctrl+[ = back, Cmd/Ctrl+] = forward.
                // No-op when the corresponding stack is empty.
                Key::Character(s) if s.as_str() == "[" => {
                    return self.nav_back();
                }
                Key::Character(s) if s.as_str() == "]" => {
                    return self.nav_forward();
                }
                Key::Named(NamedKey::ArrowUp) => {
                    if modifiers.state().shift_key() {
                        // Sidebar is rendered newest-first; "Up" should move
                        // visually upward — toward the newer context.
                        if let Some(id) = self.next_context() {
                            return self.set_active_context(id);
                        }
                        return false;
                    }
                    if let Some(focused) = self.pane_mut().focused {
                        if let Some(prev) = self.prev_visible(focused) {
                            self.pane_mut().focused = Some(prev);
                            self.pane_mut().editing = false;
                            self.pane_mut().coalesce_break = true;
                            self.scroll_to_focused();
                            return true;
                        }
                    }
                    return false;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    if modifiers.state().shift_key() {
                        // "Down" moves visually downward in the newest-first
                        // sidebar — toward the older context.
                        if let Some(id) = self.prev_context() {
                            return self.set_active_context(id);
                        }
                        return false;
                    }
                    if let Some(focused) = self.pane_mut().focused {
                        if let Some(next) = self.next_visible(focused) {
                            self.pane_mut().focused = Some(next);
                            self.pane_mut().editing = false;
                            self.pane_mut().coalesce_break = true;
                            self.scroll_to_focused();
                            return true;
                        }
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
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("a") => {
                    if let Some(id) = self.pane_mut().focused {
                        if let Some(cell) = self.cell_mut(id) {
                            cell.select_all_focused();
                        }
                        self.pane_mut().coalesce_break = true;
                        return true;
                    }
                    return false;
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("c") => {
                    return self.copy_to_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("x") => {
                    return self.cut_to_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("v") => {
                    // Ctrl+Shift+V → paste-alternate: a Reference
                    // payload becomes a fresh Reference cell rather
                    // than an inline link; any other payload pastes
                    // as plain text (strips formatting + links).
                    if modifiers.state().shift_key() {
                        return self.paste_from_clipboard_alternate();
                    }
                    return self.paste_from_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("n") => {
                    let with_title = modifiers.state().shift_key();
                    return self
                        .insert_cell_after_focused(NewCellKind::Plain, with_title);
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("o") => {
                    let with_title = modifiers.state().shift_key();
                    return self
                        .insert_cell_after_focused(NewCellKind::Outline, with_title);
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("p") => {
                    let with_title = modifiers.state().shift_key();
                    return self
                        .insert_cell_after_focused(NewCellKind::PopPop, with_title);
                }
                // Rotate context (start a new context "now"). Moved
                // off Ctrl+Shift+N so that combo can mirror its
                // siblings (Plain cell, title pre-focused).
                Key::Character(s)
                    if s.as_str().eq_ignore_ascii_case("r")
                        && modifiers.state().shift_key() =>
                {
                    self.rotate_context_now();
                    return true;
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("f") => {
                    // Ctrl+F: push a single-cell view of the focused cell
                    // onto the nav history. Back (Cmd+[) returns to the
                    // previous view. No-op if there's no focus, or the
                    // current view is already that same single cell.
                    let Some(id) = self.pane_mut().focused else {
                        return false;
                    };
                    let target = Query::cell(id);
                    if self.pane().view == target {
                        return false;
                    }
                    let pushed = self.push_view(target);
                    if pushed {
                        // `push_view` resets focus to the first visible
                        // cell; restore to the same id we came from
                        // (it's the only visible cell anyway, but be
                        // explicit so caret-scroll lands on it).
                        self.pane_mut().focused = Some(id);
                    }
                    return pushed;
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("e") => {
                    // Ctrl+E: envelope the focused Reference cell.
                    // No-op for any other cell kind — envelope only
                    // applies to references. Unwrap is menu-only on
                    // purpose (so the user can't accidentally drop
                    // notes by hitting the same combo twice).
                    let Some(id) = self.pane_mut().focused else {
                        return false;
                    };
                    let is_reference = matches!(
                        self.cell(id).map(|c| &c.kind),
                        Some(CellKind::Reference(_))
                    );
                    if is_reference {
                        return self.envelope_reference(id);
                    }
                    return false;
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("t") => {
                    // Ctrl/Cmd+T: create + focus the title slot on the
                    // focused cell. Idempotent — focuses an existing title.
                    // (Cmd+H is reserved by macOS for "hide app," so title
                    // gets T and tables move to J.)
                    let Some(id) = self.pane_mut().focused else { return false };
                    let changed = self
                        .cell_mut(id)
                        .map(|c| c.toggle_title_focus())
                        .unwrap_or(false);
                    if changed {
                        self.pane_mut().editing = true;
                        self.pane_mut().coalesce_break = true;
                        self.pane_mut().pending_caret_scroll = true;
                        self.mark_cell_dirty(id);
                    }
                    return changed;
                }
                Key::Character(s)
                    if s.as_str().eq_ignore_ascii_case("d")
                        && modifiers.state().shift_key() =>
                {
                    // Ctrl/Cmd+Shift+D: jump to today's date view.
                    let today = local_date_for_ms(now_epoch_ms());
                    return self.push_view(Query::date(today));
                }
                _ => {}
            }
        }

        // Modal mode switches: Esc exits edit, Enter enters edit.
        if event.state == ElementState::Pressed
            && !primary_mod(modifiers.state())
            && !modifiers.state().alt_key()
        {
            match &event.logical_key {
                // (Context-menu Esc dismissals are handled above
                // by `dismiss_open_context_menu`.)
                // Esc with an active text / bullet-range selection
                // on the focused cell → clear the selection
                // instead of exiting edit mode. One Esc cancels
                // the selection; a second Esc (now selection-less)
                // exits edit. Works in view mode too — Esc on a
                // view-mode drag-selection retires it.
                Key::Named(NamedKey::Escape) => {
                    let has_sel = self
                        .pane_mut()
                        .focused
                        .and_then(|id| self.cell(id))
                        .map(|c| c.has_any_selection())
                        .unwrap_or(false);
                    if has_sel {
                        if let Some(id) = self.pane_mut().focused {
                            if let Some(c) = self.cell_mut(id) {
                                c.clear_all_selections();
                            }
                        }
                        self.pane_mut().coalesce_break = true;
                        return true;
                    }
                    // No selection — fall through to edit-mode
                    // exit if applicable. A view-mode Esc with no
                    // selection is a no-op (the existing single-
                    // cell view stays put; press Cmd+[ to back).
                    if self.pane_mut().editing {
                        self.pane_mut().editing = false;
                        self.mention_popup = None;
                        self.pane_mut().coalesce_break = true;
                        return true;
                    }
                    return false;
                }
                Key::Named(NamedKey::Enter)
                    if !self.pane_mut().editing
                        && !modifiers.state().shift_key()
                        && self.pane_mut().focused.is_some() =>
                {
                    // Reference cells are read-only — Enter on a focused
                    // reference navigates to the original instead of
                    // entering edit mode.
                    if let Some(id) = self.pane_mut().focused {
                        let target = match self.cell(id) {
                            Some(c) => match &c.kind {
                                CellKind::Reference(rc) => Some(rc.target()),
                                _ => None,
                            },
                            None => None,
                        };
                        if let Some(t) = target {
                            self.navigate_to_reference(t);
                            return true;
                        }
                    }
                    self.pane_mut().editing = true;
                    // If the user has an existing selection (text
                    // drag, bullet-range, etc.) keep it on edit-mode
                    // entry — they probably want to act on it. With
                    // no selection, drop the caret at the end so
                    // typing appends.
                    if let Some(id) = self.pane_mut().focused {
                        let keep_selection = self
                            .cell(id)
                            .map(|c| c.has_any_selection())
                            .unwrap_or(false);
                        if !keep_selection {
                            if let Some(c) = self.cell_mut(id) {
                                c.place_caret_at_end();
                            }
                        }
                    }
                    self.pane_mut().pending_caret_scroll = true;
                    return true;
                }
                _ => {}
            }
        }

        // View mode: cell-level operations only. Text input is dropped —
        // Enter is the way back to edit; Backspace/Delete delete the cell.
        if !self.pane_mut().editing {
            if event.state == ElementState::Pressed
                && !modifiers.state().shift_key()
                && !primary_mod(modifiers.state())
                && !modifiers.state().alt_key()
            {
                match &event.logical_key {
                    Key::Named(NamedKey::ArrowUp) => {
                        if let Some(focused) = self.pane_mut().focused {
                            if let Some(prev) = self.prev_visible(focused) {
                                self.pane_mut().focused = Some(prev);
                                self.pane_mut().coalesce_break = true;
                                self.scroll_to_focused();
                                return true;
                            }
                            if let Some(next_ctx) = self.next_context_with_cells() {
                                if self.set_active_context(next_ctx) {
                                    self.pane_mut().focused = self.visible_cell_ids().last().copied();
                                    return true;
                                }
                            }
                        }
                        return false;
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        if let Some(focused) = self.pane_mut().focused {
                            if let Some(next) = self.next_visible(focused) {
                                self.pane_mut().focused = Some(next);
                                self.pane_mut().coalesce_break = true;
                                self.scroll_to_focused();
                                return true;
                            }
                            if let Some(prev_ctx) = self.prev_context_with_cells() {
                                if self.set_active_context(prev_ctx) {
                                    self.pane_mut().focused = self.visible_cell_ids().first().copied();
                                    return true;
                                }
                            }
                        }
                        return false;
                    }
                    _ => {}
                }
            }
            return false;
        }

        // Cross-cell arrow nav: a plain ArrowUp/Down at the focused cell's
        // top/bottom edge moves focus to the adjacent cell, dropping to view
        // mode (the new cell starts unedited; press Enter to continue).
        if event.state == ElementState::Pressed
            && !modifiers.state().shift_key()
            && !primary_mod(modifiers.state())
            && !modifiers.state().alt_key()
        {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowUp) => {
                    if let Some(focused) = self.pane_mut().focused {
                        let at_top = self.cell(focused).map_or(false, |c| c.at_top_edge());
                        if at_top {
                            if let Some(prev) = self.prev_visible(focused) {
                                self.pane_mut().focused = Some(prev);
                                if let Some(c) = self.cell_mut(prev) {
                                    c.place_caret_at_end();
                                }
                                self.pane_mut().editing = false;
                                self.pane_mut().coalesce_break = true;
                                self.pane_mut().pending_caret_scroll = true;
                                return true;
                            }
                            // No more cells in this view going up — cross to
                            // the newer context and land at its bottom (oldest)
                            // cell, caret at end so the chronological flow is
                            // continuous when arrowing further up. Skip empty
                            // contexts so the cursor doesn't get trapped.
                            if let Some(next_ctx) = self.next_context_with_cells() {
                                if self.set_active_context(next_ctx) {
                                    let landing = self.visible_cell_ids().last().copied();
                                    self.pane_mut().focused = landing;
                                    if let Some(id) = landing {
                                        if let Some(c) = self.cell_mut(id) {
                                            c.place_caret_at_end();
                                        }
                                    }
                                    self.pane_mut().editing = false;
                                    self.pane_mut().coalesce_break = true;
                                    self.pane_mut().pending_caret_scroll = true;
                                    return true;
                                }
                            }
                        }
                    }
                }
                Key::Named(NamedKey::ArrowDown) => {
                    if let Some(focused) = self.pane_mut().focused {
                        let at_bot = self.cell(focused).map_or(false, |c| c.at_bottom_edge());
                        if at_bot {
                            if let Some(next) = self.next_visible(focused) {
                                self.pane_mut().focused = Some(next);
                                if let Some(c) = self.cell_mut(next) {
                                    c.place_caret_at_start();
                                }
                                self.pane_mut().editing = false;
                                self.pane_mut().coalesce_break = true;
                                self.pane_mut().pending_caret_scroll = true;
                                return true;
                            }
                            // Bottom of the view — cross to the older context
                            // and land at its top (newest) cell, caret at start.
                            // Skip empties.
                            if let Some(prev_ctx) = self.prev_context_with_cells() {
                                if self.set_active_context(prev_ctx) {
                                    let landing = self.visible_cell_ids().first().copied();
                                    self.pane_mut().focused = landing;
                                    if let Some(id) = landing {
                                        if let Some(c) = self.cell_mut(id) {
                                            c.place_caret_at_start();
                                        }
                                    }
                                    self.pane_mut().editing = false;
                                    self.pane_mut().coalesce_break = true;
                                    self.pane_mut().pending_caret_scroll = true;
                                    return true;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let focused_id = self.pane_mut().focused;
        let pre = focused_id.and_then(|id| self.cell(id)).map(|c| c.snapshot());
        let popup_was_open = self.mention_popup.is_some();
        let handled = if let Some(id) = focused_id {
            if let Some(cell) = self.cell_mut(id) {
                cell.handle_key(event, modifiers)
            } else {
                false
            }
        } else {
            false
        };
        if handled {
            if let (Some(pre), Some(id)) = (pre, focused_id) {
                if let Some(cell) = self.cell(id) {
                    let post = cell.snapshot();
                    if !pre.doc_eq(&post) {
                        self.record_edit(pre, post);
                    } else {
                        // Cursor-only event: break coalescing so the next text edit
                        // starts a fresh undo entry.
                        self.pane_mut().coalesce_break = true;
                    }
                }
            }
            self.pane_mut().pending_caret_scroll = true;

            // Maybe open the mention popup (if user just typed a trigger
            // character: `@` for persons, `#` for tags), then sync against
            // the current text+caret state.
            if !popup_was_open {
                match event.text.as_deref() {
                    Some("@") => self.try_open_mention_popup(MentionKind::Person),
                    Some("#") => self.try_open_mention_popup(MentionKind::Tag),
                    _ => {}
                }
            }
            self.sync_mention_popup();
        }
        handled
    }

    /// Build a `KeptPayload` from the focused cell's current
    /// selection. Returns `None` when there's nothing to copy.
    /// View-mode + no selection falls back to a whole-cell payload
    /// (Outline for outline cells, Text otherwise) — that's the
    /// "Ctrl+C with no selection copies the whole cell" affordance.
    fn build_copy_payload(&self) -> Option<crate::clipboard::KeptPayload> {
        let id = self.pane().focused?;
        let cell = self.cell(id)?;
        build_copy_payload_for_cell(cell, self.pane().editing)
    }

    /// Write a `KeptPayload` to the OS clipboard as HTML (with the
    /// embedded round-trip marker) + plain-text fallback.
    fn write_payload_to_clipboard(&mut self, p: &crate::clipboard::KeptPayload) {
        let html = crate::clipboard::to_html(p);
        let plain = crate::clipboard::to_plain_text(p);
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_html(html, Some(plain));
        }
    }

    fn copy_to_clipboard(&mut self) -> bool {
        let Some(p) = self.build_copy_payload() else {
            return false;
        };
        self.write_payload_to_clipboard(&p);
        true
    }

    fn cut_to_clipboard(&mut self) -> bool {
        let Some(id) = self.pane_mut().focused else {
            return false;
        };
        let Some(payload) = self.build_copy_payload() else {
            return false;
        };
        self.write_payload_to_clipboard(&payload);
        let pre = self.cell(id).map(|c| c.snapshot());
        let cut = match self.cell_mut(id) {
            Some(c) => c.cut_text(),
            None => return false,
        };
        if cut.is_empty() {
            return true; // We still wrote the payload; nothing to delete.
        }
        if let (Some(pre), Some(cell)) = (pre, self.cell(id)) {
            let post = cell.snapshot();
            if !pre.doc_eq(&post) {
                self.record_edit(pre, post);
            }
        }
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    /// Read clipboard formats (HTML preferred — carries the
    /// embedded marker for byte-perfect Kept↔Kept round-trip;
    /// plain text fallback) and apply to the focused cell.
    ///
    /// `alternate` (Ctrl+Shift+V): on a Reference payload, insert
    /// a fresh Reference cell rather than an inline link; on any
    /// other payload, strip formatting (paste-as-plain-text).
    fn paste_from_clipboard_inner(&mut self, alternate: bool) -> bool {
        let Some(id) = self.pane_mut().focused else {
            return false;
        };
        let html = self
            .clipboard
            .as_mut()
            .and_then(|cb| cb.get().html().ok());
        let text = self
            .clipboard
            .as_mut()
            .and_then(|cb| cb.get_text().ok())
            .unwrap_or_default();
        if html.is_none() && text.is_empty() {
            return false;
        }
        let payload =
            crate::clipboard::from_clipboard(html.as_deref(), &text);

        let pre = self.cell(id).map(|c| c.snapshot());

        if alternate {
            self.apply_paste_alternate(id, payload);
        } else {
            self.apply_paste_default(id, payload);
        }

        if let (Some(pre), Some(cell)) = (pre, self.cell(id)) {
            let post = cell.snapshot();
            if !pre.doc_eq(&post) {
                self.record_edit(pre, post);
            }
        }
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    fn paste_from_clipboard(&mut self) -> bool {
        self.paste_from_clipboard_inner(false)
    }

    fn paste_from_clipboard_alternate(&mut self) -> bool {
        self.paste_from_clipboard_inner(true)
    }

    /// Default paste dispatch — by payload kind. Delegates to the
    /// cell-local `apply_paste_into_cell` so the Quick-Add path
    /// can share the dispatch without going through document
    /// focus / dirty machinery.
    fn apply_paste_default(
        &mut self,
        cell_id: Uuid,
        payload: crate::clipboard::KeptPayload,
    ) {
        if let Some(cell) = self.cell_mut(cell_id) {
            apply_paste_into_cell(cell, payload);
        }
    }

    /// Alternate paste (Ctrl+Shift+V).
    fn apply_paste_alternate(
        &mut self,
        cell_id: Uuid,
        payload: crate::clipboard::KeptPayload,
    ) {
        use crate::clipboard::KeptPayload;
        match payload {
            KeptPayload::Reference { target, .. } => {
                // Materialize as a fresh Reference cell, sorted into
                // the timeline. Same machinery the "Surface as
                // reference" action uses.
                let _ = self.surface_as_reference(target.into_target());
            }
            KeptPayload::Text { text, .. } => {
                // Strip links — paste plain text only.
                self.paste_text_with_links(cell_id, &text, &[]);
            }
            KeptPayload::Outline { bullets } => {
                // Strip links, flatten to indented text.
                let (flat, _) = flatten_outline(&bullets);
                self.paste_text_with_links(cell_id, &flat, &[]);
            }
        }
    }

    /// Insert `text` + `links` at the focused caret in `cell_id`.
    /// Thin wrapper around `paste_text_with_links_into_cell`.
    fn paste_text_with_links(
        &mut self,
        cell_id: Uuid,
        text: &str,
        links: &[crate::cell::LinkSpan],
    ) {
        if let Some(cell) = self.cell_mut(cell_id) {
            paste_text_with_links_into_cell(cell, text, links);
        }
    }

    /// Render the entity page for `entity_id` into the doc area. Returns
    /// the total content height (so `tick` can update `doc_height` for the
    /// scrollbar). Layout, top to bottom: display_name heading, metadata
    /// line, "BACKING CELL" section, the embedded backing cell (drawn via
    /// `Cell::tick`) or a "+ Create backing cell" affordance for cell-less
    /// entities. The embedded cell uses the same focus/edit state as the
    /// doc loop, so editing inline Just Works.
    fn render_entity_page(
        &mut self,
        canvas: &Canvas,
        entity_id: Uuid,
        cells_left: f32,
        content_width: f32,
        scale: f32,
        mouse_doc_x: f32,
        mouse_doc_y: f32,
    ) -> f32 {
        let entity = match self.entities.entities.iter().find(|e| e.id == entity_id).cloned() {
            Some(e) => e,
            None => {
                let font = Font::from_typeface(&self.typeface, ENTITY_META_FONT_SIZE * scale);
                let (_, fm) = font.metrics();
                let mut paint = Paint::default();
                paint.set_anti_alias(true);
                paint.set_color(crate::color::text_section_header());
                canvas.draw_str(
                    "Entity not found",
                    Point::new(cells_left, MARGIN_TOP + (-fm.ascent)),
                    &font,
                    &paint,
                );
                return -fm.ascent + fm.descent + 60.0 * scale;
            }
        };

        let mut y = MARGIN_TOP;

        // Title (display_name).
        let title_font = Font::from_typeface(&self.typeface, ENTITY_TITLE_FONT_SIZE * scale);
        let (_, tm) = title_font.metrics();
        let mut title_paint = Paint::default();
        title_paint.set_anti_alias(true);
        title_paint.set_color(crate::color::text_primary());
        canvas.draw_str(
            &entity.display_name,
            Point::new(cells_left, y + (-tm.ascent)),
            &title_font,
            &title_paint,
        );
        y += -tm.ascent + tm.descent;

        // Metadata: "<kind> · @<alias>". Alias may be missing for very old
        // entity rows or fresh inserts; render just the kind in that case.
        let alias = self
            .entities
            .alias_index
            .iter()
            .find(|(_, eid, _)| *eid == entity.id)
            .map(|(a, _, _)| a.clone());
        let meta = match alias {
            Some(a) => format!("{} · @{a}", entity.kind),
            None => entity.kind.clone(),
        };
        let meta_font = Font::from_typeface(&self.typeface, ENTITY_META_FONT_SIZE * scale);
        let (_, mm) = meta_font.metrics();
        let mut meta_paint = Paint::default();
        meta_paint.set_anti_alias(true);
        meta_paint.set_color(crate::color::text_muted_warm());
        y += 4.0 * scale;
        let meta_baseline = y + (-mm.ascent);
        canvas.draw_str(
            &meta,
            Point::new(cells_left, meta_baseline),
            &meta_font,
            &meta_paint,
        );
        // Active/inactive toggle, right-aligned with the meta baseline.
        // Label sits to the left of the pill in the same font/color as
        // the meta text; mouse_down hit-tests the pill rect alone.
        let toggle_w = 34.0 * scale;
        let toggle_h = 18.0 * scale;
        let label = if entity.is_active { "active" } else { "inactive" };
        let label_w = meta_font.measure_str(label, Some(&meta_paint)).0;
        let toggle_right = cells_left + content_width;
        let toggle_left = toggle_right - toggle_w;
        let label_right = toggle_left - 8.0 * scale;
        let label_x = label_right - label_w;
        canvas.draw_str(
            label,
            Point::new(label_x, meta_baseline),
            &meta_font,
            &meta_paint,
        );
        // Vertically center the pill on the meta-text band (ascent..descent).
        let band_top = meta_baseline + mm.ascent;
        let band_bot = meta_baseline + mm.descent;
        let band_mid = (band_top + band_bot) * 0.5;
        let toggle_rect = Rect::new(
            toggle_left,
            band_mid - toggle_h * 0.5,
            toggle_left + toggle_w,
            band_mid + toggle_h * 0.5,
        );
        let toggle_hovered = mouse_doc_x >= toggle_rect.left
            && mouse_doc_x <= toggle_rect.right
            && mouse_doc_y >= toggle_rect.top
            && mouse_doc_y <= toggle_rect.bottom;
        draw_toggle(canvas, toggle_rect, entity.is_active, toggle_hovered);
        self.hit_tests_builder.entity_page.active_toggle = Some(toggle_rect);

        y += -mm.ascent + mm.descent;
        y += ENTITY_SECTION_GAP * scale;

        // BACKING CELL section header (sidebar-header styling).
        let header_font =
            Font::from_typeface(&self.typeface, SIDEBAR_HEADER_FONT_SIZE * scale);
        let (_, hm) = header_font.metrics();
        let mut header_paint = Paint::default();
        header_paint.set_anti_alias(true);
        header_paint.set_color(crate::color::text_section_header());
        canvas.draw_str(
            "BACKING CELL",
            Point::new(cells_left, y + (-hm.ascent)),
            &header_font,
            &header_paint,
        );
        y += -hm.ascent + hm.descent + ENTITY_SECTION_HEADER_GAP * scale;

        // Backing-cell body.
        if let Some(pid) = entity.primary_cell_id {
            if let Some(cell_idx) = self.document.cells.iter().position(|c| c.id == pid) {
                let focused_id = self.pane_mut().focused;
                let editing = self.pane_mut().editing;
                let cell = &mut self.document.cells[cell_idx];
                let cell_is_focused = focused_id.map(|f| f == cell.id).unwrap_or(false);
                let render_focused = cell_is_focused;
                let show_caret = cell_is_focused && editing;
                let h = cell.tick(
                    canvas,
                    cells_left,
                    y,
                    content_width,
                    render_focused,
                    show_caret,
                );
                if !cell_is_focused {
                    let mut outline = Paint::default();
                    outline.set_anti_alias(true);
                    outline.set_style(PaintStyle::Stroke);
                    outline.set_stroke_width(CELL_OUTLINE_STROKE);
                    outline.set_color(crate::color::dark_alpha(CELL_OUTLINE_ALPHA));
                    let rect = Rect::new(
                        cells_left - FOCUS_PAD,
                        y - FOCUS_PAD,
                        cells_left + content_width + FOCUS_PAD,
                        y + h + FOCUS_PAD,
                    );
                    canvas.draw_round_rect(rect, FOCUS_RADIUS, FOCUS_RADIUS, &outline);
                }
                y += h;
            } else {
                // primary_cell_id refers to a cell we don't have loaded —
                // shouldn't happen post-load but render a stub rather than
                // panicking.
                let mut paint = Paint::default();
                paint.set_anti_alias(true);
                paint.set_color(crate::color::text_section_header());
                canvas.draw_str(
                    "Backing cell missing",
                    Point::new(cells_left, y + (-mm.ascent)),
                    &meta_font,
                    &paint,
                );
                y += -mm.ascent + mm.descent;
            }
        } else {
            // Cell-less entity — show a "+ Create backing cell" affordance.
            // Wire-up of the click is deferred to Chunk 2; for now the
            // button just records its rect for hit-testing.
            let btn_h = ENTITY_CREATE_BTN_H * scale;
            let btn_w = (ENTITY_CREATE_BTN_W * scale).min(content_width);
            let btn_rect = Rect::new(
                cells_left,
                y,
                cells_left + btn_w,
                y + btn_h,
            );
            let hovered = mouse_doc_x >= btn_rect.left
                && mouse_doc_x <= btn_rect.right
                && mouse_doc_y >= btn_rect.top
                && mouse_doc_y <= btn_rect.bottom;
            let bg_alpha: u8 = if hovered { 0x18 } else { 0x08 };
            let mut bg = Paint::default();
            bg.set_anti_alias(true);
            bg.set_color(crate::color::dark_alpha(bg_alpha));
            canvas.draw_round_rect(btn_rect, 6.0 * scale, 6.0 * scale, &bg);
            let mut border = Paint::default();
            border.set_anti_alias(true);
            border.set_style(PaintStyle::Stroke);
            border.set_stroke_width(1.0);
            border.set_color(crate::color::button_border_faint());
            canvas.draw_round_rect(btn_rect, 6.0 * scale, 6.0 * scale, &border);

            let mut lp = Paint::default();
            lp.set_anti_alias(true);
            lp.set_color(crate::color::text_muted_warm_deep());
            let label = "+ Create backing cell";
            let lw = meta_font.measure_str(label, Some(&lp)).0;
            let label_baseline =
                btn_rect.top + (btn_h + (-mm.ascent) - mm.descent) * 0.5;
            canvas.draw_str(
                label,
                Point::new(btn_rect.left + (btn_w - lw) * 0.5, label_baseline),
                &meta_font,
                &lp,
            );
            self.hit_tests_builder.entity_page.create_button = Some(btn_rect);
            y += btn_h;
        }

        // REFERENCED IN — list of cells that link to this entity. Rendered
        // as embed previews (warm-tan dashed wrapper + cached body), sorted
        // newest-first by `edited_at`. The previews aren't real cells —
        // they live only as long as this page render. Click an embed →
        // navigate to the source cell; rect-tracked in
        // `hit_tests_builder.entity_page.refs` for hit-test in `mouse_down`.
        let kept_url = format!("kept://{}", entity_id);
        let primary = entity.primary_cell_id;
        let mut mentions: Vec<(usize, i64)> = self
            .document
            .cells
            .iter()
            .enumerate()
            .filter(|(_, c)| Some(c.id) != primary)
            .filter(|(_, c)| c.all_link_urls().iter().any(|u| u == &kept_url))
            .map(|(idx, c)| (idx, c.edited_at))
            .collect();
        mentions.sort_by_key(|&(_, t)| std::cmp::Reverse(t));

        if !mentions.is_empty() {
            y += ENTITY_SECTION_GAP * scale;
            let mut ref_header_paint = Paint::default();
            ref_header_paint.set_anti_alias(true);
            ref_header_paint.set_color(crate::color::text_section_header());
            canvas.draw_str(
                "REFERENCED IN",
                Point::new(cells_left, y + (-hm.ascent)),
                &header_font,
                &ref_header_paint,
            );
            y += -hm.ascent + hm.descent + ENTITY_SECTION_HEADER_GAP * scale;

            let inset = EMBED_INSET * scale;
            let pad = EMBED_PAD * scale;
            let body_x = cells_left + inset;
            let body_w = (content_width - 2.0 * inset).max(40.0);

            for (target_idx, _) in mentions {
                let target_cell_id = self.document.cells[target_idx].id;
                let target_ts = self.document.cells[target_idx].timestamp;
                // Fresh cache per frame — no selection persistence on the
                // entity page (acceptable for v1; click-to-navigate covers
                // the main interaction).
                let mut maybe_cache = self.build_reference_cache(
                    target_idx,
                    ReferenceTarget::WholeCell(target_cell_id),
                    0,
                );
                let body_h = match &mut maybe_cache {
                    Some(cache) => self.tick_embedded_cell(
                        canvas, cache, body_x, y + pad, body_w, false,
                    ),
                    None => self.render_embed_placeholder(
                        canvas,
                        "↗ [unrenderable]",
                        body_x,
                        y + pad,
                        body_w,
                        scale,
                    ),
                };
                let footer_text = format!(
                    "↗ originally {}",
                    format_date_label(local_date_for_ms(target_ts))
                );
                let total_h = self.draw_embed_wrapper(
                    canvas,
                    cells_left,
                    y,
                    content_width,
                    body_x,
                    body_h,
                    &footer_text,
                    scale,
                    [0.0, 0.0, 0.0, 0.0],
                    false,
                );
                self.hit_tests_builder.entity_page.refs.push((
                    target_cell_id,
                    Rect::new(cells_left, y, cells_left + content_width, y + total_h),
                ));
                y += total_h + CELL_GAP;
            }
        }

        y - MARGIN_TOP
    }

    /// Render the People page: alphabetical list of `kind=person`
    /// entities, each as a clickable row, with an "+ Add person…" footer.
    /// While `people_rename` is `Some`, that row's static label is
    /// replaced by an embedded `TextBox` for inline editing. Returns the
    /// total content height so `tick` can update `doc_height`.
    fn render_people_page(
        &mut self,
        canvas: &Canvas,
        cells_left: f32,
        content_width: f32,
        scale: f32,
        mouse_doc_x: f32,
        mouse_doc_y: f32,
    ) -> f32 {
        let mut y = MARGIN_TOP;

        // Title + "Show inactive" toggle, sharing a baseline.
        let title_font =
            Font::from_typeface(&self.typeface, ENTITY_TITLE_FONT_SIZE * scale);
        let (_, tm) = title_font.metrics();
        let mut title_paint = Paint::default();
        title_paint.set_anti_alias(true);
        title_paint.set_color(crate::color::text_primary());
        let title_baseline = y + (-tm.ascent);
        canvas.draw_str(
            "People",
            Point::new(cells_left, title_baseline),
            &title_font,
            &title_paint,
        );
        // Toggle: right-aligned, vertically centered on the title's
        // text band. Label "Show inactive" sits to its left in the
        // muted meta-text style.
        let toggle_w = 34.0 * scale;
        let toggle_h = 18.0 * scale;
        let label_font = Font::from_typeface(&self.typeface, ENTITY_META_FONT_SIZE * scale);
        let (_, lm) = label_font.metrics();
        let mut label_paint = Paint::default();
        label_paint.set_anti_alias(true);
        label_paint.set_color(crate::color::text_muted_warm());
        let label = "Show inactive";
        let label_w = label_font.measure_str(label, Some(&label_paint)).0;
        let toggle_right = cells_left + content_width;
        let toggle_left = toggle_right - toggle_w;
        let label_x = toggle_left - 8.0 * scale - label_w;
        // Vertically center toggle + label on the title's text band.
        let title_band_top = title_baseline + tm.ascent;
        let title_band_bot = title_baseline + tm.descent;
        let title_band_mid = (title_band_top + title_band_bot) * 0.5;
        let label_baseline = title_band_mid + (-lm.ascent + lm.descent) * 0.5 - lm.descent;
        canvas.draw_str(
            label,
            Point::new(label_x, label_baseline),
            &label_font,
            &label_paint,
        );
        let toggle_rect = Rect::new(
            toggle_left,
            title_band_mid - toggle_h * 0.5,
            toggle_left + toggle_w,
            title_band_mid + toggle_h * 0.5,
        );
        let toggle_hovered = mouse_doc_x >= toggle_rect.left
            && mouse_doc_x <= toggle_rect.right
            && mouse_doc_y >= toggle_rect.top
            && mouse_doc_y <= toggle_rect.bottom;
        draw_toggle(canvas, toggle_rect, self.show_inactive, toggle_hovered);
        self.hit_tests_builder.people_page.show_inactive_toggle = Some(toggle_rect);

        y += -tm.ascent + tm.descent + 24.0 * scale;

        // Sorted snapshot — case-insensitive by display_name. When
        // `show_inactive` is off, hide inactive rows entirely; when on,
        // they stay in alphabetical order but render in muted color.
        let show_inactive = self.show_inactive;
        let mut people: Vec<(String, Uuid, bool)> = self
            .entities
            .entities
            .iter()
            .filter(|e| e.kind == PERSON_TAG)
            .filter(|e| show_inactive || e.is_active)
            .map(|e| (e.display_name.clone(), e.id, e.is_active))
            .collect();
        people.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

        let row_h = PEOPLE_ROW_H * scale;
        let row_pad_x = PEOPLE_ROW_PAD_X * scale;
        let row_w = content_width;
        let row_font =
            Font::from_typeface(&self.typeface, PEOPLE_ROW_FONT_SIZE * scale);
        let (_, rm) = row_font.metrics();
        let text_baseline_offset = (row_h + (-rm.ascent) - rm.descent) * 0.5;

        let renaming_id = self.people_rename.as_ref().map(|s| s.entity_id);
        let adding = self.people_add.is_some();
        // Suppress doc-area hover paints while a menu is open — the menu
        // renders in window space, so the cursor is "over the menu," not
        // the row underneath. Showing both highlights at once is
        // disorienting.
        let menu_open = self.people_context_menu.is_some();

        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(crate::color::text_primary());
        let mut inactive_paint = Paint::default();
        inactive_paint.set_anti_alias(true);
        inactive_paint.set_color(crate::color::text_section_header());
        let mut divider_paint = Paint::default();
        divider_paint.set_anti_alias(true);
        divider_paint.set_color(crate::color::hover_faint());
        divider_paint.set_stroke_width(1.0);
        let mut hover_paint = Paint::default();
        hover_paint.set_anti_alias(true);
        hover_paint.set_color(crate::color::dark_alpha(0x14));

        for (display_name, entity_id, is_active) in &people {
            let row_rect = Rect::new(cells_left, y, cells_left + row_w, y + row_h);
            let hovered = mouse_doc_x >= row_rect.left
                && mouse_doc_x <= row_rect.right
                && mouse_doc_y >= row_rect.top
                && mouse_doc_y <= row_rect.bottom;
            let is_renaming = renaming_id == Some(*entity_id);
            if hovered && !is_renaming && !menu_open {
                canvas.draw_rect(row_rect, &hover_paint);
            }

            if is_renaming {
                if let Some(rs) = self.people_rename.as_mut() {
                    // Align TextBox baseline with the static row baseline:
                    // baseline = top + text_baseline_offset, and TextBox
                    // draws its baseline at `tb_y + (-ascent)`, so
                    // tb_y = top + text_baseline_offset + ascent (negative).
                    let tb_y = row_rect.top + text_baseline_offset + rm.ascent;
                    rs.input.tick(
                        canvas,
                        row_rect.left + row_pad_x,
                        tb_y,
                        row_w - row_pad_x * 2.0,
                        true,
                        true,
                    );
                }
            } else {
                let row_text_paint = if *is_active {
                    &text_paint
                } else {
                    &inactive_paint
                };
                canvas.draw_str(
                    display_name,
                    Point::new(
                        row_rect.left + row_pad_x,
                        row_rect.top + text_baseline_offset,
                    ),
                    &row_font,
                    row_text_paint,
                );
                // Right-justified mention count when > 0 — surfaces
                // who you actually interact with. Painted in the
                // muted inactive color regardless of `is_active` so
                // it reads as metadata, not the row's primary text.
                let mention_count = self.entities.mention_count(*entity_id);
                if mention_count > 0 {
                    let count_text = format!("{}", mention_count);
                    let count_w =
                        row_font.measure_str(&count_text, Some(&inactive_paint)).0;
                    canvas.draw_str(
                        &count_text,
                        Point::new(
                            row_rect.right - row_pad_x - count_w,
                            row_rect.top + text_baseline_offset,
                        ),
                        &row_font,
                        &inactive_paint,
                    );
                }
            }

            // Hairline divider at the bottom of each row.
            canvas.draw_line(
                Point::new(row_rect.left, row_rect.bottom),
                Point::new(row_rect.right, row_rect.bottom),
                &divider_paint,
            );

            self.hit_tests_builder.people_page.rows.push((*entity_id, row_rect));
            y += row_h;
        }

        // "+ Add person…" footer row. When `people_add` is Some, this
        // row hosts the inline input; otherwise it shows muted prompt
        // text. Living at the bottom of the list (rather than wherever
        // the new name would sort) keeps the user visually anchored.
        let add_rect = Rect::new(cells_left, y, cells_left + row_w, y + row_h);
        let add_hovered = mouse_doc_x >= add_rect.left
            && mouse_doc_x <= add_rect.right
            && mouse_doc_y >= add_rect.top
            && mouse_doc_y <= add_rect.bottom;
        if add_hovered && !adding && !menu_open {
            canvas.draw_rect(add_rect, &hover_paint);
        }
        if adding {
            if let Some(input) = self.people_add.as_mut() {
                let tb_y = add_rect.top + text_baseline_offset + rm.ascent;
                input.tick(
                    canvas,
                    add_rect.left + row_pad_x,
                    tb_y,
                    row_w - row_pad_x * 2.0,
                    true,
                    true,
                );
            }
        } else {
            let mut muted = Paint::default();
            muted.set_anti_alias(true);
            muted.set_color(crate::color::text_section_header());
            canvas.draw_str(
                "+ Add person…",
                Point::new(
                    add_rect.left + row_pad_x,
                    add_rect.top + text_baseline_offset,
                ),
                &row_font,
                &muted,
            );
        }
        self.hit_tests_builder.people_page.add = Some(add_rect);
        y += row_h;

        y - MARGIN_TOP
    }

    /// Begin inline rename for a People-page row. Pre-fills the input
    /// with the entity's current `display_name` and selects all so the
    /// next keystroke replaces it. Cancels any in-progress Add input.
    fn start_people_rename(&mut self, entity_id: Uuid) {
        self.people_add = None;
        let display_name = self
            .entities
            .entities
            .iter()
            .find(|e| e.id == entity_id)
            .map(|e| e.display_name.clone())
            .unwrap_or_default();
        let mut input = TextBox::new(self.typeface.clone(), display_name);
        input.set_font_scale(self.font_scale);
        input.select_all();
        self.people_rename = Some(PeopleRenameState { entity_id, input });
    }

    /// Commit the in-progress rename. Writes the new display_name to the
    /// entity row and replaces alias rows. If the entity has a backing
    /// cell, the cell's title text is rewritten (preserving trailing tags)
    /// and the cell is marked dirty so `flush_persistence` saves it on
    /// the next idle window. The whole operation lands as a single
    /// `UndoOp::RenamePersonEntity` on the undo stack.
    fn commit_people_rename(&mut self) {
        let Some(rs) = self.people_rename.take() else { return };
        let new_text = rs.input.text().trim().to_string();
        if new_text.is_empty() {
            return;
        }
        let entity_id = rs.entity_id;

        // Snapshot pre-rename state for undo.
        let entity_pre = self.entities.entities.iter().find(|e| e.id == entity_id).cloned();
        let Some(entity_pre) = entity_pre else { return };
        let prev_name = entity_pre.display_name.clone();
        if prev_name == new_text {
            // No-op rename — don't pollute the undo stack.
            return;
        }
        let primary_cell_id = entity_pre.primary_cell_id;

        if let Some(db) = self.db.as_mut() {
            if let Err(e) = db.rename_person_entity(entity_id, &new_text) {
                eprintln!("kept: rename_person_entity failed: {e}");
                return;
            }
        }

        let mut cell_title_change: Option<(Uuid, String, String)> = None;
        if let Some(cell_id) = primary_cell_id {
            if let Some(cell) = self.cell_mut(cell_id) {
                if let Some(title) = cell.title_mut() {
                    let prev_title = title.text().to_string();
                    let (_, tags) = split_title_name_and_tags(&prev_title);
                    let new_title = if tags.is_empty() {
                        new_text.clone()
                    } else {
                        format!("{} {}", new_text, tags)
                    };
                    title.replace_text(new_title.clone());
                    cell_title_change = Some((cell_id, prev_title, new_title));
                }
            }
            self.mark_cell_dirty(cell_id);
        }

        self.refresh_entities();
        self.undo_stack.push(UndoOp::RenamePersonEntity {
            entity_id,
            prev_name,
            new_name: new_text,
            cell_title_change,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
    }

    /// Count `kept://<entity_id>` mentions across every cell.
    /// Thin proxy over `EntityCache::mention_count` (the indexed
    /// HashMap rebuilt by `refresh`); the walk happens once per
    /// entity-mutating event, not once per lookup.
    fn count_entity_references(&self, entity_id: Uuid) -> usize {
        self.entities.mention_count(entity_id)
    }

    /// Open the People right-click context menu for `entity_id`,
    /// Close whichever right-click context menu is currently open
    /// (cell / bar / tag / people). Returns true if any was open and
    /// got dismissed. Single point of truth for "dismiss any menu" —
    /// called by Esc keypress, by right-click before opening a new
    /// menu, and by any flow that wants to be sure no menu is
    /// stuck on screen.
    fn dismiss_open_context_menu(&mut self) -> bool {
        // `|` (not `||`) so all four `take()`s evaluate — that
        // way no menu lingers even if multiple were somehow open.
        self.cell_context_menu.take().is_some()
            | self.bar_context_menu.take().is_some()
            | self.tag_context_menu.take().is_some()
            | self.people_context_menu.take().is_some()
    }

    /// anchored at window-space `(x, y)`. Precomputes deletability
    /// (no backing cell + zero references). The menu closes on any
    /// subsequent click or Esc.
    fn open_people_context_menu(&mut self, entity_id: Uuid, x: f32, y: f32) {
        let primary = self
            .entities
            .entities
            .iter()
            .find(|e| e.id == entity_id)
            .and_then(|e| e.primary_cell_id);
        let refs = self.count_entity_references(entity_id);
        let deletable = primary.is_none() && refs == 0;
        let ref_count = if deletable { None } else { Some(refs) };
        self.people_context_menu = Some(PeopleContextMenu {
            entity_id,
            anchor_x: x,
            anchor_y: y,
            deletable,
            ref_count,
        });
    }

    /// Flip an entity's `is_active` flag. Recorded as a single
    /// `UndoOp::SetEntityActive` so Ctrl+Z reverses it. No-ops cleanly
    /// when the entity is missing.
    fn toggle_entity_active(&mut self, entity_id: Uuid) {
        let prev = self
            .entities
            .entities
            .iter()
            .find(|e| e.id == entity_id)
            .map(|e| e.is_active);
        let Some(prev) = prev else { return };
        let new = !prev;
        if let Some(db) = self.db.as_mut() {
            if let Err(e) = db.set_entity_active(entity_id, new) {
                eprintln!("kept: set_entity_active failed: {e}");
                return;
            }
        }
        self.refresh_entities();
        self.undo_stack.push(UndoOp::SetEntityActive {
            entity_id,
            prev,
            new,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
    }

    /// Flip a cell's `closed_at` (the close/reopen gesture from the
    /// cell context menu). Pushes a dedicated undo op and marks the
    /// cell dirty for persistence. Metadata-only — does NOT touch
    /// `edited_at` (would distort attention sort). Returns whether
    /// the toggle landed (false when the cell is missing).
    fn toggle_cell_closed(&mut self, cell_id: Uuid) -> bool {
        let Some(idx) = self.cell_idx(cell_id) else {
            return false;
        };
        let prev = self.document.cells[idx].closed_at;
        let new = if prev.is_none() {
            Some(now_epoch_ms())
        } else {
            None
        };
        self.document.cells[idx].closed_at = new;
        self.mark_cell_dirty(cell_id);
        self.undo_stack.push(UndoOp::SetCellClosed {
            cell_id,
            prev,
            new,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    /// Set a cell's `resurface_after`. Pass `None` to clear an
    /// existing snooze, `Some(t)` to schedule. Metadata-only.
    fn set_cell_resurface(&mut self, cell_id: Uuid, when: Option<i64>) -> bool {
        let Some(idx) = self.cell_idx(cell_id) else {
            return false;
        };
        let prev = self.document.cells[idx].resurface_after;
        if prev == when {
            return false;
        }
        self.document.cells[idx].resurface_after = when;
        self.mark_cell_dirty(cell_id);
        self.undo_stack.push(UndoOp::SetCellResurface {
            cell_id,
            prev,
            new: when,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        true
    }

    /// Flip a single bullet's `closed_at` (the "close sub-outline"
    /// gesture). Cascade is read at render time via
    /// `compute_effective_open`; nothing is mutated on descendants
    /// here. Returns false when the cell isn't an outline or the
    /// bullet id isn't present (defensive — the menu only offers the
    /// row when both hold).
    fn toggle_bullet_closed(&mut self, cell_id: Uuid, bullet_id: Uuid) -> bool {
        let Some(idx) = self.cell_idx(cell_id) else {
            return false;
        };
        let prev = match &self.document.cells[idx].kind {
            CellKind::Outline(oc) => oc
                .bullets()
                .iter()
                .find(|b| b.id() == bullet_id)
                .map(|b| b.closed_at()),
            _ => None,
        };
        let Some(prev) = prev else {
            return false;
        };
        let new = if prev.is_none() {
            Some(now_epoch_ms())
        } else {
            None
        };
        let mutated = match &mut self.document.cells[idx].kind {
            CellKind::Outline(oc) => oc.set_bullet_closed_at(bullet_id, new),
            _ => false,
        };
        if !mutated {
            return false;
        }
        self.mark_cell_dirty(cell_id);
        self.undo_stack.push(UndoOp::SetBulletClosed {
            cell_id,
            bullet_id,
            prev,
            new,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        self.pane_mut().pending_caret_scroll = true;
        true
    }

    /// Set a bullet's `resurface_after`. Metadata-only.
    fn set_bullet_resurface(
        &mut self,
        cell_id: Uuid,
        bullet_id: Uuid,
        when: Option<i64>,
    ) -> bool {
        let Some(idx) = self.cell_idx(cell_id) else {
            return false;
        };
        let prev = match &self.document.cells[idx].kind {
            CellKind::Outline(oc) => oc
                .bullets()
                .iter()
                .find(|b| b.id() == bullet_id)
                .map(|b| b.resurface_after()),
            _ => None,
        };
        let Some(prev) = prev else {
            return false;
        };
        if prev == when {
            return false;
        }
        let mutated = match &mut self.document.cells[idx].kind {
            CellKind::Outline(oc) => oc.set_bullet_resurface_after(bullet_id, when),
            _ => false,
        };
        if !mutated {
            return false;
        }
        self.mark_cell_dirty(cell_id);
        self.undo_stack.push(UndoOp::SetBulletResurface {
            cell_id,
            bullet_id,
            prev,
            new: when,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        true
    }

    /// Hard-delete an entity that has no backing cell and no references.
    /// Caller must have verified those conditions (the menu does that at
    /// open time). Refreshes entity caches and pushes a
    /// `DeleteCelllessEntity` undo entry that captures the row's
    /// identity (id + name + created_at) so undo can recreate it
    /// faithfully.
    fn delete_person_entity(&mut self, entity_id: Uuid) {
        let snapshot = self
            .entities
            .entities
            .iter()
            .find(|e| e.id == entity_id)
            .cloned();
        if let Some(db) = self.db.as_mut() {
            if let Err(e) = db.delete_entity(entity_id) {
                eprintln!("kept: delete_entity failed: {e}");
                return;
            }
        }
        self.refresh_entities();
        if let Some(e) = snapshot {
            self.undo_stack.push(UndoOp::DeleteCelllessEntity {
                entity_id: e.id,
                name: e.display_name,
                is_active: e.is_active,
                created_at: e.created_at,
            });
            self.redo_stack.clear();
        }
        self.pane_mut().coalesce_break = true;
    }

    /// Begin inline "Add person" mode. The footer row's prompt is
    /// replaced by an empty editable `TextBox`; on Enter, the typed
    /// name is committed as a fresh cell-less entity. Cancels any
    /// in-progress rename so only one input is active at a time.
    fn start_people_add(&mut self) {
        self.people_rename = None;
        let mut input = TextBox::new(self.typeface.clone(), String::new());
        input.set_font_scale(self.font_scale);
        self.people_add = Some(input);
    }

    /// Commit the inline Add-person input. Trimmed-empty input cancels
    /// silently (no entity created); otherwise inserts a cell-less
    /// person entity with the typed name and pushes a
    /// `CreateCelllessEntity` undo entry.
    fn commit_people_add(&mut self) {
        let Some(input) = self.people_add.take() else { return };
        let name = input.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        let new_id = match self.db.as_mut() {
            Some(db) => match db.create_cell_less_person_entity(&name) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("kept: create_cell_less_person_entity failed: {e}");
                    return;
                }
            },
            None => return,
        };
        self.refresh_entities();
        // Pull the just-inserted row's `created_at` so undo→redo
        // round-trips preserve the stable timestamp.
        let created_at = self
            .entities
            .entities
            .iter()
            .find(|e| e.id == new_id)
            .map(|e| e.created_at)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        self.undo_stack.push(UndoOp::CreateCelllessEntity {
            entity_id: new_id,
            name,
            created_at,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
    }

    fn record_edit(&mut self, pre: CellSnapshot, post: CellSnapshot) {
        let Some(cell_id) = self.pane_mut().focused else { return };
        let now = Instant::now();

        let can_coalesce = !self.pane_mut().coalesce_break
            && self
                .last_edit_time
                .map(|t| now.duration_since(t) < COALESCE_INTERVAL)
                .unwrap_or(false)
            && matches!(
                self.undo_stack.last(),
                Some(UndoOp::CellEdit { cell_id: prev, .. }) if *prev == cell_id
            );

        if can_coalesce {
            if let Some(UndoOp::CellEdit { post: prev_post, .. }) = self.undo_stack.last_mut() {
                *prev_post = post;
            }
        } else {
            self.undo_stack.push(UndoOp::CellEdit {
                cell_id,
                pre,
                post,
            });
        }

        self.last_edit_time = Some(now);
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = false;

        self.touch_cell(cell_id);
    }

    fn undo(&mut self) -> bool {
        let Some(op) = self.undo_stack.pop() else {
            return false;
        };
        op.apply(self, UndoDir::Undo);
        let bumps = op.bumps_focused_edited();
        self.redo_stack.push(op);
        self.pane_mut().dragging_cell = None;
        self.pane_mut().pending_caret_scroll = true;
        self.pane_mut().coalesce_break = true;
        if bumps {
            if let Some(id) = self.pane_mut().focused {
                self.touch_cell(id);
            }
        }
        true
    }

    fn redo(&mut self) -> bool {
        let Some(op) = self.redo_stack.pop() else {
            return false;
        };
        op.apply(self, UndoDir::Redo);
        let bumps = op.bumps_focused_edited();
        self.undo_stack.push(op);
        self.pane_mut().dragging_cell = None;
        self.pane_mut().pending_caret_scroll = true;
        self.pane_mut().coalesce_break = true;
        if bumps {
            if let Some(id) = self.pane_mut().focused {
                self.touch_cell(id);
            }
        }
        true
    }

    /// Delete a cell by id. Used by the right-click "Delete cell" menu.
    /// The cell does not need to be focused — focus is repicked from the
    /// visible neighbors as part of the operation.
    fn delete_cell_by_id(&mut self, id: Uuid) -> bool {
        let cell_ref = match self.cell(id) {
            Some(c) => c,
            None => return false,
        };
        let snapshot = cell_ref.snapshot();
        let cell_ts = cell_ref.timestamp;

        // Find the context whose window contains this cell (by timestamp).
        let containing_ctx: Option<Context> = self
            .document
            .contexts
            .iter()
            .find(|c| {
                cell_ts >= c.start_time && c.end_time.map_or(true, |e| cell_ts < e)
            })
            .cloned();

        // Will deleting this cell leave its containing context empty?
        let side_effect = if let Some(ctx) = containing_ctx {
            let others_in_ctx = self
                .document
                .cells
                .iter()
                .filter(|c| c.id != id)
                .filter(|c| {
                    c.timestamp >= ctx.start_time
                        && ctx.end_time.map_or(true, |e| c.timestamp < e)
                })
                .count();
            if others_in_ctx == 0 {
                if ctx.end_time.is_some() {
                    // Closed context — remove it. Need an open context to
                    // become active; if none exists, skip the side effect to
                    // preserve the "always one open context" invariant.
                    let new_active = self
                        .document
                        .contexts
                        .iter()
                        .filter(|c| c.id != ctx.id && c.end_time.is_none())
                        .max_by_key(|c| c.start_time)
                        .map(|c| c.id);
                    new_active.map(|nid| {
                        // If user was viewing this closed context, follow to
                        // the new open one; otherwise leave the view alone
                        // (e.g., AST views stay put).
                        let prev_view = self.pane_mut().view.clone();
                        let new_view = if prev_view.context_view() == Some(ctx.id) {
                            Query::context(nid)
                        } else {
                            prev_view.clone()
                        };
                        ContextSideEffect::ContextRemoved {
                            context: ctx.clone(),
                            prev_view,
                            new_view,
                        }
                    })
                } else {
                    Some(ContextSideEffect::StartReset {
                        context_id: ctx.id,
                        prev_start: ctx.start_time,
                        new_start: now_epoch_ms(),
                    })
                }
            } else {
                None
            }
        } else {
            None
        };

        // Pick neighbor focus from the visible list (under the *current*
        // active context — may be overridden below if active changes).
        let visible = self.visible_cell_ids();
        let pos_in_visible = visible.iter().position(|x| *x == id);
        let mut new_focus = match pos_in_visible {
            Some(i) if i + 1 < visible.len() => Some(visible[i + 1]),
            Some(i) if i > 0 => Some(visible[i - 1]),
            _ => None,
        };

        // Remove the cell.
        if let Some(idx) = self.cell_idx(id) {
            self.document.cells.remove(idx);
        }
        self.document.pending_deletes.insert(id);
        self.document.dirty_cells.remove(&id);

        // Apply the context-level side effect.
        if let Some(se) = &side_effect {
            self.apply_context_side_effect(se);
            // Re-pick focus after the active context may have changed.
            new_focus = match se {
                ContextSideEffect::ContextRemoved { .. } => {
                    self.visible_cell_ids().first().copied()
                }
                ContextSideEffect::StartReset { .. } => None,
            };
        }

        self.pane_mut().focused = new_focus;
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.pane_mut().pending_caret_scroll = true;

        self.undo_stack.push(UndoOp::DeleteCell {
            cell_id: id,
            snapshot,
            pre_focused: Some(id),
            post_focused: new_focus,
            side_effect,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        true
    }

    fn apply_context_side_effect(&mut self, se: &ContextSideEffect) {
        match se {
            ContextSideEffect::ContextRemoved {
                context, new_view, ..
            } => {
                self.document.contexts.retain(|c| c.id != context.id);
                self.document.dirty_contexts.remove(&context.id);
                self.document.pending_context_deletes.insert(context.id);
                self.pane_mut().view = new_view.clone();
            }
            ContextSideEffect::StartReset {
                context_id,
                new_start,
                ..
            } => {
                if let Some(c) = self.document.contexts.iter_mut().find(|c| c.id == *context_id) {
                    c.start_time = *new_start;
                }
                self.document.mark_context_dirty(*context_id);
            }
        }
    }

    fn reverse_context_side_effect(&mut self, se: &ContextSideEffect) {
        match se {
            ContextSideEffect::ContextRemoved {
                context,
                prev_view,
                ..
            } => {
                if !self.document.contexts.iter().any(|c| c.id == context.id) {
                    self.document.contexts.push(context.clone());
                }
                self.document.mark_context_dirty(context.id);
                self.document.pending_context_deletes.remove(&context.id);
                self.pane_mut().view = prev_view.clone();
            }
            ContextSideEffect::StartReset {
                context_id,
                prev_start,
                ..
            } => {
                if let Some(c) = self.document.contexts.iter_mut().find(|c| c.id == *context_id) {
                    c.start_time = *prev_start;
                }
                self.document.mark_context_dirty(*context_id);
            }
        }
    }

    fn insert_cell_after_focused(&mut self, kind: NewCellKind, with_title: bool) -> bool {
        // If the user is viewing a closed context, jump to the current open
        // one before inserting. The note belongs in "today," not in history.
        let auto_switched = self.ensure_writable_context();
        // No-op if the focused cell is empty — the new-cell shortcut shouldn't
        // pile up empties. Skip when we just auto-switched: the destination's focused
        // cell is incidental, the user's intent was clearly to write.
        if !auto_switched {
            if let Some(id) = self.pane_mut().focused {
                if let Some(cell) = self.cell(id) {
                    if cell.is_empty() {
                        return false;
                    }
                }
            }
        }
        // Idle rotation: if the user has been quiet (no new cells) for the
        // threshold, this write opens a fresh context instead of extending the
        // current one. Pre-existing cells stay where they were — context
        // membership is purely time-derived. The baseline is the later of the
        // last cell creation and the active context's start, so an empty fresh
        // context can't rotate again on its very first write.
        let now = now_epoch_ms();
        let idle_ms = IDLE_CONTEXT_THRESHOLD.as_millis() as i64;
        // Idle baseline tracks the writable (most recent open) context, not
        // the user's view selection — date-view doesn't affect rotation.
        let writable_start = self
            .writable_context_id()
            .and_then(|id| self.document.contexts.iter().find(|c| c.id == id))
            .map(|c| c.start_time)
            .unwrap_or(i64::MIN);
        let baseline = self
            .last_cell_create_ms()
            .map(|t| t.max(writable_start))
            .unwrap_or(writable_start);
        if baseline > i64::MIN && now - baseline >= idle_ms {
            self.rotate_context_now();
        }
        let pre_focused = self.pane_mut().focused;
        let mut new_cell = match kind {
            NewCellKind::Plain => Cell::new(self.typeface.clone(), String::new()),
            NewCellKind::Outline => Cell::new_outline(self.typeface.clone()),
            NewCellKind::PopPop => Cell::new_poppop(self.typeface.clone()),
        };
        new_cell.set_font_scale(self.font_scale);
        new_cell.context_hint_id = self.writable_context_id();
        // When the user asked for "create + title" in one keystroke,
        // pre-attach an empty title and aim the cursor at it. Done
        // before the snapshot so undo/redo round-trips the
        // title-focused state — redoing the InsertCell op restores
        // the same caret position the user expected.
        if with_title {
            new_cell.toggle_title_focus();
        }
        let new_id = new_cell.id;
        let snapshot = new_cell.snapshot();
        self.insert_cell_sorted(new_cell);
        self.pane_mut().focused = Some(new_id);
        // Creating a cell is an explicit "I want to type" action.
        self.pane_mut().editing = true;
        self.pane_mut().dragging_cell = None;
        self.pane_mut().pending_caret_scroll = true;

        self.undo_stack.push(UndoOp::InsertCell {
            cell_id: new_id,
            snapshot,
            pre_focused,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        self.touch_cell(new_id);
        true
    }

    /// "Surface as reference" — create a new Reference cell pointing at
    /// `target` and insert it where any new cell would land: at "now," in
    /// the current writable context (auto-rotating to a fresh context when
    /// the user has been idle, just like `insert_cell_after_focused`).
    /// Focuses the new reference. Returns true on insert.
    fn surface_as_reference(&mut self, target: ReferenceTarget) -> bool {
        // Surface = drop a reference into "today" *without* yanking
        // the user's view. The cell goes into the current writable
        // context if one exists (None is fine — the cell just has no
        // context_hint and shows up in today's date view anyway).
        // The user gets a toast as confirmation since the visible
        // side effect on their current view is zero.
        let pre_focused = self.pane_mut().focused;
        let mut new_cell = Cell::new_reference(self.typeface.clone(), target);
        new_cell.set_font_scale(self.font_scale);
        new_cell.context_hint_id = self.writable_context_id();
        let new_id = new_cell.id;
        let snapshot = new_cell.snapshot();
        self.insert_cell_sorted(new_cell);
        // Don't change focus or scroll — the user is browsing
        // somewhere else and we want them to stay there.
        self.undo_stack.push(UndoOp::InsertCell {
            cell_id: new_id,
            snapshot,
            pre_focused,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        self.touch_cell(new_id);
        self.show_toast("Surfaced to today");
        true
    }

    /// Replace a Reference cell with an Outline whose first slot is the
    /// original embed (read-only) and whose body is one empty bullet
    /// for the user to start typing notes. Cell id and timestamp are
    /// preserved — any other Reference cells pointing at this one keep
    /// resolving. Records `UndoOp::Envelope` so Cmd+Z restores the
    /// Reference exactly.
    fn envelope_reference(&mut self, cell_id: Uuid) -> bool {
        let Some(idx) = self.cell_idx(cell_id) else {
            return false;
        };
        let target = match &self.document.cells[idx].kind {
            CellKind::Reference(rc) => rc.target(),
            _ => return false,
        };
        let pre_focused = self.pane_mut().focused;
        let pre = self.document.cells[idx].snapshot();
        let timestamp = self.document.cells[idx].timestamp;
        let edited_at = self.document.cells[idx].edited_at;
        let context_hint = self.document.cells[idx].context_hint_id;
        let closed_at = self.document.cells[idx].closed_at;
        let resurface_after = self.document.cells[idx].resurface_after;

        // Build the new Outline cell directly so we can hand-pick the
        // id / timestamp (replace-in-place semantics). Cell::from_parts
        // takes everything we need.
        let mut outline = cell::OutlineCell::with_envelope(self.typeface.clone(), target);
        outline.set_font_scale(self.font_scale);
        let mut new_cell = Cell::from_parts(
            cell_id,
            CellKind::Outline(outline),
            None,
            timestamp,
            edited_at,
            context_hint,
            closed_at,
            resurface_after,
        );
        new_cell.set_font_scale(self.font_scale);
        let post = new_cell.snapshot();

        self.document.cells[idx] = new_cell;
        self.pane_mut().focused = Some(cell_id);
        self.pane_mut().editing = true;
        self.pane_mut().dragging_cell = None;
        self.pane_mut().pending_caret_scroll = true;
        self.mark_cell_dirty(cell_id);
        self.touch_cell(cell_id);

        self.undo_stack.push(UndoOp::Envelope {
            cell_id,
            pre,
            post,
            pre_focused,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        true
    }

    /// Reverse of `envelope_reference`: turn an envelope outline back
    /// into a bare Reference at the same id / timestamp. Bullet notes
    /// the user typed into the envelope are dropped from the live
    /// cell — the pre-snapshot in the undo entry preserves them so
    /// Ctrl+Z restores everything if the unwrap was a mistake.
    fn unwrap_envelope(&mut self, cell_id: Uuid) -> bool {
        let Some(idx) = self.cell_idx(cell_id) else {
            return false;
        };
        let target = match &self.document.cells[idx].kind {
            CellKind::Outline(oc) => match oc.reference_header() {
                Some(h) => h.target(),
                None => return false,
            },
            _ => return false,
        };
        let pre_focused = self.pane_mut().focused;
        let pre = self.document.cells[idx].snapshot();
        let timestamp = self.document.cells[idx].timestamp;
        let edited_at = self.document.cells[idx].edited_at;
        let context_hint = self.document.cells[idx].context_hint_id;
        let closed_at = self.document.cells[idx].closed_at;
        let resurface_after = self.document.cells[idx].resurface_after;

        let mut new_cell = Cell::from_parts(
            cell_id,
            CellKind::Reference(cell::ReferenceCell::new(
                self.typeface.clone(),
                target,
            )),
            None,
            timestamp,
            edited_at,
            context_hint,
            closed_at,
            resurface_after,
        );
        new_cell.set_font_scale(self.font_scale);
        let post = new_cell.snapshot();

        self.document.cells[idx] = new_cell;
        self.pane_mut().focused = Some(cell_id);
        // References don't have an edit mode.
        self.pane_mut().editing = false;
        self.pane_mut().dragging_cell = None;
        self.pane_mut().pending_caret_scroll = true;
        self.mark_cell_dirty(cell_id);
        self.touch_cell(cell_id);

        self.undo_stack.push(UndoOp::Unwrap {
            cell_id,
            pre,
            post,
            pre_focused,
        });
        self.redo_stack.clear();
        self.pane_mut().coalesce_break = true;
        true
    }

    /// Bring the primary caret of the focused cell into view if it's outside
    /// the viewport. Used after edits, caret movement, and zoom changes.
    fn scroll_caret_into_view(&mut self) {
        let Some(id) = self.pane_mut().focused else { return };
        let Some(cell) = self.cell(id) else { return };
        let Some((top, bot)) = cell.caret_doc_y_band() else {
            return;
        };
        let pad = 8.0_f32;
        let view_top = self.pane_mut().scroll_y;
        let view_bot = self.pane_mut().scroll_y + self.pane_mut().viewport_height;
        let new_scroll = if top < view_top + pad {
            (top - pad).max(0.0)
        } else if bot > view_bot - pad {
            (bot + pad - self.pane_mut().viewport_height).max(0.0)
        } else {
            return;
        };
        // Don't clamp to current max_scroll: a just-grown doc has a stale
        // max_scroll and the next tick will recompute it.
        self.pane_mut().scroll_y = new_scroll.max(0.0);
        self.pane_mut().last_scroll_time = Some(Instant::now());
    }

    /// Bring the focused cell into view if it's outside the current viewport.
    /// Uses last frame's cell geometry; on the first frame everything is at 0
    /// which results in scroll_y = 0, which is correct.
    fn scroll_to_focused(&mut self) {
        let Some(id) = self.pane_mut().focused else { return };
        let Some(cell) = self.cell(id) else { return };
        let pad = 8.0_f32;
        let cell_top = cell.y_origin();
        let cell_bot = cell.y_origin() + cell.height();
        let view_top = self.pane_mut().scroll_y;
        let view_bot = self.pane_mut().scroll_y + self.pane_mut().viewport_height;

        let new_scroll = if cell_top < view_top + pad {
            (cell_top - pad).max(0.0)
        } else if cell_bot > view_bot - pad {
            (cell_bot + pad - self.pane_mut().viewport_height).max(0.0)
        } else {
            return;
        };
        self.pane_mut().scroll_y = new_scroll.clamp(0.0, self.pane_mut().max_scroll);
        // Briefly show the scrollbar so the jump is visible.
        self.pane_mut().last_scroll_time = Some(Instant::now());
    }

    /// Right-click handler. Sidebar tag rows offer a "Delete tag"
    /// context menu (strips the tag from any cells still carrying
    /// it AND drops the DB row). Doc-area right-clicks open the
    /// per-cell or per-row context menu. Returns true if the click
    /// was consumed.
    pub fn right_click(&mut self, x: f32, y: f32, modifiers: &Modifiers) -> bool {
        // Right-clicking anywhere first closes any open menu.
        let was_open = self.dismiss_open_context_menu();
        // Sidebar: any tag row offers the delete menu. The previous
        // gate (`count == 0` against `db.cells_with_tag`) was unreachable
        // — every visible tag has at least one in-memory cell carrying
        // its span (otherwise it wouldn't appear in the sidebar via
        // `all_tag_names_in_memory`). The delete operation now strips
        // tag spans from those cells in addition to dropping the DB
        // row, so there's nothing safety-critical to gate on.
        if x < SIDEBAR_WIDTH * self.font_scale {
            // Sidebar rects are in content-space; map mouse to match.
            let y = y + self.sidebar_scroll.scroll_y;
            for (name, rect) in self.hit_tests.sidebar.tags.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.tag_context_menu = Some(TagContextMenu {
                        name,
                        anchor_x: x,
                        anchor_y: y,
                    });
                    return true;
                }
            }
            return was_open;
        }
        // Doc-area right-clicks. People-page rows → rename/delete menu.
        // AST/Context cell-loop → per-cell context menu (timestamps +
        // Delete cell). First, make the clicked pane active so the menu
        // anchors correctly and operates on that pane's view.
        if let Some(idx) = self.pane_at(x, y) {
            self.set_active_pane(idx);
        }
        if matches!(self.pane_mut().view.view_kind, ViewKind::People) {
            let doc_y = y + self.pane_mut().scroll_y;
            for (entity_id, rect) in self.hit_tests.people_page.rows.clone() {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    self.open_people_context_menu(entity_id, x, y);
                    return true;
                }
            }
            return was_open;
        }
        if matches!(
            self.pane_mut().view.view_kind,
            ViewKind::Ast
                | ViewKind::Context(_)
                | ViewKind::Entity(_)
                | ViewKind::Current
                | ViewKind::Cell(_)
        ) {
            let doc_y = y + self.pane_mut().scroll_y;
            // Bar hit-test wins over the body — a right-click on the
            // left-edge bar always opens the whole-cell BarContextMenu,
            // never the body's CellContextMenu. Ctrl+right-click
            // anywhere on the cell behaves the same way: bypass the
            // body's bullet-subtree selection and treat the click as
            // if it had landed on the bar.
            let force_bar_menu = cell::primary_mod(modifiers.state());
            for (cell_id, rect) in self.hit_tests.cell_bars.clone() {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    self.pane_mut().focused = Some(cell_id);
                    self.pane_mut().editing = false;
                    self.bar_context_menu = Some(BarContextMenu {
                        cell_id,
                        anchor_x: x,
                        anchor_y: y,
                    });
                    return true;
                }
            }
            if force_bar_menu {
                if let Some(cell_id) = self.find_cell_at(x, doc_y) {
                    self.pane_mut().focused = Some(cell_id);
                    self.pane_mut().editing = false;
                    self.bar_context_menu = Some(BarContextMenu {
                        cell_id,
                        anchor_x: x,
                        anchor_y: y,
                    });
                    return true;
                }
            }
            if let Some(cell_id) = self.find_cell_at(x, doc_y) {
                // Right-click on a bullet captures (id, snippet) AND
                // visually highlights its sub-tree, descending through
                // any embed wrappers under the cursor (Reference cells,
                // envelope outline headers, recursive nested embeds)
                // until it reaches the actual outline. Cached outlines
                // carry the source bullet ids unchanged (see
                // `build_reference_cache`), so the captured id remains
                // meaningful against the source.
                let hit = self
                    .cell_mut(cell_id)
                    .and_then(|c| select_subtree_at_doc_y(c, cell_id, doc_y));
                if hit.is_some() {
                    self.pane_mut().focused = Some(cell_id);
                }
                let (bullet_origin_cell_id, bullet_id, bullet_snippet) = match hit.as_ref() {
                    Some((origin, id, text)) => (Some(*origin), Some(*id), Some(snippet(text))),
                    None => (None, None, None),
                };

                // Reference origin: always resolve through embeds to the
                // source. Surface-as-reference (whole-cell or subtree)
                // points to the original cell, never to another
                // reference — no chained-reference creation.
                let reference_origin_cell_id = self
                    .cell(cell_id)
                    .map(|c| match &c.kind {
                        CellKind::Reference(rc) => rc.target().cell_id(),
                        _ => cell_id,
                    })
                    .unwrap_or(cell_id);
                // For a Reference source, capture its full target so
                // the whole-cell surface row can preserve a Subtree
                // pointer when the user re-surfaces it.
                let source_reference_target: Option<ReferenceTarget> = self
                    .cell(cell_id)
                    .and_then(|c| match &c.kind {
                        CellKind::Reference(rc) => Some(rc.target()),
                        _ => None,
                    });
                self.cell_context_menu = Some(CellContextMenu {
                    cell_id,
                    anchor_x: x,
                    anchor_y: y,
                    bullet_id,
                    bullet_snippet,
                    reference_origin_cell_id,
                    bullet_origin_cell_id,
                    source_reference_target,
                });
                return true;
            }
        }
        was_open
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, modifiers: &Modifiers) -> bool {
        // Any click cancels in-flight kinetic coast on every pane.
        self.kill_all_kinetic();

        // Mention popup wins z-order over EVERYTHING — including
        // the Quick-Add modal — so a click on its "Add @X" /
        // "Add #X" row or one of its candidate rows commits the
        // pick instead of falling through to whatever sits behind
        // it (including the Quick-Add "click outside the card to
        // dismiss" path).
        if self.mention_popup.is_some() {
            let in_rect =
                |r: Rect| x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
            if let Some(rect) = self.hit_tests.mention_popup.add_row {
                if in_rect(rect) {
                    self.commit_add_mention();
                    return true;
                }
            }
            let row_hit = self
                .hit_tests
                .mention_popup
                .rows
                .iter()
                .position(|r| in_rect(*r));
            if let Some(idx) = row_hit {
                self.commit_mention_row(idx);
                return true;
            }
            // Miss: dismiss popup and keep routing the click.
            self.mention_popup = None;
        }

        // Quick-Add modal is exclusive while open: clicks inside
        // its card route to the modal's cell, clicks outside
        // commit + close. Either way the dispatch below doesn't
        // run.
        if self.quick_add.is_some() {
            return self.handle_quick_add_mouse_down(x, y, modifiers);
        }

        // Pane header URL-pill / dropdown click. Wins over body /
        // sidebar dispatch so the user can interact with the pill
        // and its result rows. A click outside any pill or its
        // suggestions blurs all headers.
        let in_rect = |r: Rect| x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
        // Click on a result row → commit it.
        let result_click = self.hit_tests.header_results.iter().find_map(|(idx, rects)| {
            rects.iter().position(|r| in_rect(*r)).map(|i| (*idx, i))
        });
        if let Some((pane_idx, row_idx)) = result_click {
            let alt = modifiers.state().alt_key();
            self.commit_header_result(pane_idx, row_idx, alt);
            return true;
        }
        // Click on the pill itself → focus the textbox at the
        // clicked position.
        let header_hit = self
            .hit_tests
            .pane_headers
            .iter()
            .find(|(_, r)| in_rect(*r))
            .map(|(idx, _)| *idx);
        if let Some(pane_idx) = header_hit {
            // Blur any other pane's header.
            for (i, p) in self.panes.iter_mut().enumerate() {
                if i != pane_idx {
                    p.header.blur();
                }
            }
            let pane = &mut self.panes[pane_idx];
            pane.header.focused = true;
            pane.header.selected = None;
            pane.header.textbox.mouse_down(x, y, modifiers, true);
            // Track the drag so cursor motion before mouse_up
            // extends the selection inside the pill.
            self.header_dragging_pane = Some(pane_idx);
            return true;
        }
        // Click outside any pill / dropdown blurs all focused headers.
        if self.panes.iter().any(|p| p.header.focused) {
            for p in &mut self.panes {
                if p.header.focused {
                    p.header.blur();
                }
            }
            self.mention_popup = None;
            // Don't return — fall through so the click also lands
            // wherever it normally would (cell focus, sidebar, etc.).
        }

        // (Mention-popup hit-tests are handled near the top of
        // `mouse_down` so they win even when the Quick-Add modal
        // is open — see the block above the modal early return.)

        // Tag context menu intercepts left-clicks: clicking the "Delete
        // tag" row deletes; clicking anywhere else closes the menu and
        // falls through to normal click routing.
        if self.tag_context_menu.is_some() {
            if let Some(rect) = self.hit_tests.tag_menu.delete {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.tag_context_menu.take() {
                        self.delete_tag_globally(&menu.name);
                    }
                    return true;
                }
            }
            self.tag_context_menu = None;
            // Fall through to normal click handling below.
        }

        // People context menu: same pattern. Rename starts inline edit;
        // Delete drops the entity; click outside dismisses.
        if self.people_context_menu.is_some() {
            if let Some(rect) = self.hit_tests.people_menu.rename {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.people_context_menu.take() {
                        self.start_people_rename(menu.entity_id);
                    }
                    return true;
                }
            }
            if let Some(rect) = self.hit_tests.people_menu.delete {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.people_context_menu.take() {
                        self.delete_person_entity(menu.entity_id);
                    }
                    return true;
                }
            }
            self.people_context_menu = None;
            // Fall through to normal click handling.
        }

        // Any click dismisses an active @-mention popup.
        self.mention_popup = None;

        // Scrollbar thumb grab. Wins over divider / sidebar / cell
        // dispatch because the bar is a visible UI element directly
        // under the cursor — clicking on it should never fall through
        // to whatever's behind. Sidebar's bar is checked first; pane
        // bars next. The hit zones don't overlap (bars are anchored
        // to different right-edges), so at most one match.
        if let Some(grab) = self.sidebar_scroll.thumb_hit(x, y) {
            self.sidebar_scroll.start_thumb_drag(grab);
            return true;
        }
        for pane in &mut self.panes {
            if let Some(grab) = pane.scroller.thumb_hit(x, y) {
                pane.scroller.start_thumb_drag(grab);
                return true;
            }
        }

        // Divider drag: a click within ±DIVIDER_HIT_SLOP of the divider
        // starts a drag. Tracks until mouse_up. Doesn't change active pane
        // or fire any other action.
        if self.is_on_divider(x) {
            self.dragging_divider = true;
            return true;
        }

        // Sidebar clicks switch the view. Sidebar lives in window (logical)
        // space, so use raw (x, y) — not doc_y. Any sidebar interaction
        // also exits focus mode so the new selection lands in the normal
        // multi-cell layout.
        if x < SIDEBAR_WIDTH * self.font_scale {
            // Alt-drag pan deferral, parallel to the doc-area branch
            // below: capture the click as tentative; threshold-cross
            // promotes to pan (sidebar's own scroller); release without
            // a cross replays through `dispatch_sidebar_click`. Letting
            // the click dispatch fire here would push a new view (a
            // sidebar nav) before the user has a chance to drag.
            if modifiers.state().alt_key() && !self.is_text_input_focused() {
                let click_doc_y = y + self.sidebar_scroll.scroll_y;
                self.tentative_pan = Some(TentativePan {
                    target: PanTarget::Sidebar,
                    click_x: x,
                    click_y: y,
                    click_doc_y,
                    click_modifiers: *modifiers,
                });
                return true;
            }
            return self.dispatch_sidebar_click(x, y, modifiers);
        }

        // Pane-area click. Activate the clicked pane up front so any
        // doc-space math sees the right scroll, and so an Alt-drag pan
        // captured below moves focus consistently with a non-Alt click
        // would have. EXCEPT for plain-Alt clicks (no Shift, no
        // primary mod): those are "look at the other pane without
        // committing focus there" gestures (open-in-other-pane,
        // Alt-drag pan), and the user expects their active pane to
        // stay put. Shift+Alt+click (multi-cursor) and any non-Alt
        // click still activate.
        let pane_idx = self.pane_at(x, y);
        let m = modifiers.state();
        let alt_no_switch =
            m.alt_key() && !m.shift_key() && !cell::primary_mod(m);
        if let Some(pi) = pane_idx {
            if !alt_no_switch {
                self.set_active_pane(pi);
            }
        }
        let doc_y = y + self.pane_mut().scroll_y;

        // Alt-drag pan deferral. Held Alt + click in any pane area
        // *might* be the start of a pan, but the click might also be
        // a plain Alt+click on a People-page row, an entity-page
        // reference, a cell, etc. We can't tell yet. Capture the
        // click and skip the doc-area dispatch entirely for now —
        // `cursor_moved` promotes to a committed pan if the cursor
        // moves past `ALT_PAN_THRESHOLD`; otherwise `mouse_up`
        // replays the click via `dispatch_doc_click` so the gesture
        // commits as the alt-click semantics it would have had.
        if let Some(pi) = pane_idx {
            if modifiers.state().alt_key() && !self.is_text_input_focused() {
                self.tentative_pan = Some(TentativePan {
                    target: PanTarget::Pane(pi),
                    click_x: x,
                    click_y: y,
                    click_doc_y: doc_y,
                    click_modifiers: *modifiers,
                });
                return true;
            }
        }

        self.dispatch_doc_click(x, y, doc_y, modifiers)
    }

    /// Sidebar-click dispatch — the body of the `mouse_down` sidebar
    /// branch, factored out so the Alt-drag-pan deferral path can
    /// replay it on `mouse_up` if the gesture didn't cross the pan
    /// threshold. Returns whether the click was consumed.
    fn dispatch_sidebar_click(
        &mut self,
        x: f32,
        y: f32,
        modifiers: &Modifiers,
    ) -> bool {
        // Sidebar rects are stored in content-space; map mouse to
        // match (sidebar can scroll independently of the doc area).
        let y = y + self.sidebar_scroll.scroll_y;
        // "Show archived" toggle pill at the bottom: clicks flip the
        // global flag and don't navigate anywhere. Checked first so
        // a stray hit on the toggle rect doesn't fall through to a
        // tag/date row that happens to overlap.
        if let Some(rect) = self.hit_tests.sidebar.show_inactive_toggle {
            if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                self.show_inactive_cells = !self.show_inactive_cells;
                self.cell_context_menu = None;
                return true;
            }
        }
        // Any sidebar interaction commits an in-progress People
        // rename or add (don't lose typed input on nav).
        if self.people_rename.is_some() {
            self.commit_people_rename();
        }
        if self.people_add.is_some() {
            self.commit_people_add();
        }
        // Alt+click on a sidebar entry opens the target in the *other*
        // pane (splitting if there's only one). Plain click replaces the
        // active pane's view as before.
        let alt = modifiers.state().alt_key();
        let open = |app: &mut Self, q: Query| -> bool {
            if alt {
                app.open_in_other_pane(q).is_some()
            } else {
                app.push_view(q)
            }
        };
        for (kind, rect) in self.hit_tests.sidebar.pages.clone() {
            if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                self.cell_context_menu = None;
                return match kind {
                    PageKind::People => open(self, Query::people()),
                    PageKind::Current => open(self, Query::current()),
                };
            }
        }
        // Context rows first (they're indented inside dates so their bbox
        // overlaps date row gaps in some edge cases — context wins).
        for (id, rect) in self.hit_tests.sidebar.contexts.clone() {
            if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                self.cell_context_menu = None;
                return open(self, Query::context(id));
            }
        }
        for (filter, rect) in self.hit_tests.sidebar.weeks.clone() {
            if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                self.cell_context_menu = None;
                let q = match filter {
                    query::TimeFilter::ThisWeek => Query::this_week(),
                    query::TimeFilter::LastWeek => Query::last_week(),
                    // Anything else would mean we shipped a row
                    // we don't know how to dispatch — fall back
                    // to a no-op rather than guessing.
                    _ => return false,
                };
                return open(self, q);
            }
        }
        for (date, rect) in self.hit_tests.sidebar.dates.clone() {
            if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                self.cell_context_menu = None;
                return open(self, Query::date(date));
            }
        }
        for (name, rect) in self.hit_tests.sidebar.tags.clone() {
            if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                self.cell_context_menu = None;
                return open(self, Query::tag(name));
            }
        }
        self.cell_context_menu = None;
        false
    }

    /// Doc-area click dispatch — the part of `mouse_down` that runs
    /// once we've decided the click isn't grabbed by a popup, the
    /// scrollbar thumb, the divider, the sidebar, or an Alt-drag
    /// pan. Factored out so the `mouse_up` replay path can run the
    /// same dispatch a click-without-drag would have triggered at
    /// `mouse_down` time. Caller is responsible for setting the
    /// active pane and computing `doc_y` (those depend on the
    /// click's coordinates and need to match between dispatch and
    /// any deferred replay).
    fn dispatch_doc_click(
        &mut self,
        x: f32,
        y: f32,
        doc_y: f32,
        modifiers: &Modifiers,
    ) -> bool {
        // Bar context menu dispatch: whole-cell operations (Surface,
        // Snooze, Envelope/Unwrap, Delete). Click anywhere else
        // dismisses and falls through to normal cell routing.
        if self.bar_context_menu.is_some() {
            if let Some(rect) = self.hit_tests.bar_menu.delete {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.bar_context_menu.take() {
                        self.delete_cell_by_id(menu.cell_id);
                    }
                    return true;
                }
            }
            // "Surface as reference" — surfaces the whole cell. For
            // Reference cells, preserve the original target shape
            // (Subtree stays a Subtree) so re-surfacing doesn't
            // degrade to WholeCell of the source.
            if let Some(rect) = self.hit_tests.bar_menu.surface {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.bar_context_menu.take() {
                        let target = self
                            .cell(menu.cell_id)
                            .and_then(|c| match &c.kind {
                                CellKind::Reference(rc) => Some(rc.target()),
                                _ => None,
                            })
                            .unwrap_or(ReferenceTarget::WholeCell(menu.cell_id));
                        self.surface_as_reference(target);
                    }
                    return true;
                }
            }
            // "Copy reference" — same target resolution as Surface,
            // but writes a `KeptPayload::Reference` to the
            // clipboard instead of inserting a Reference cell.
            // Pastes elsewhere as an inline kept:// link
            // (Ctrl+Shift+V to materialize as a Reference cell).
            if let Some(rect) = self.hit_tests.bar_menu.copy_reference {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.bar_context_menu.take() {
                        let target = self
                            .cell(menu.cell_id)
                            .and_then(|c| match &c.kind {
                                CellKind::Reference(rc) => Some(rc.target()),
                                _ => None,
                            })
                            .unwrap_or(ReferenceTarget::WholeCell(menu.cell_id));
                        let snippet = self
                            .cell(menu.cell_id)
                            .map(|c| snippet_for_cell(c))
                            .unwrap_or_default();
                        let payload = crate::clipboard::KeptPayload::Reference {
                            target: crate::clipboard::SerTarget::from_target(target),
                            snippet,
                        };
                        self.write_payload_to_clipboard(&payload);
                        self.show_toast("Reference copied");
                    }
                    return true;
                }
            }
            let bar_snooze = self.hit_tests.bar_menu.snooze;
            for (i, rect_opt) in bar_snooze.iter().enumerate() {
                if let Some(rect) = rect_opt {
                    if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                        let preset = SNOOZE_PRESETS[i].0;
                        let when = crate::attention::resurface_at(chrono::Local::now(), preset);
                        if let Some(menu) = self.bar_context_menu.take() {
                            self.set_cell_resurface(menu.cell_id, Some(when));
                        }
                        return true;
                    }
                }
            }
            if let Some(rect) = self.hit_tests.bar_menu.unsnooze {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.bar_context_menu.take() {
                        self.set_cell_resurface(menu.cell_id, None);
                    }
                    return true;
                }
            }
            // Envelope (Reference → envelope outline).
            if let Some(rect) = self.hit_tests.bar_menu.envelope {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.bar_context_menu.take() {
                        self.envelope_reference(menu.cell_id);
                    }
                    return true;
                }
            }
            // Unwrap envelope (envelope outline → Reference).
            if let Some(rect) = self.hit_tests.bar_menu.unwrap {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.bar_context_menu.take() {
                        self.unwrap_envelope(menu.cell_id);
                    }
                    return true;
                }
            }
            self.bar_context_menu = None;
        }
        // Cell context menu dispatch: click row dispatches, miss
        // dismisses and falls through.
        if self.cell_context_menu.is_some() {
            // "Surface as reference" — create a new reference cell at "now"
            // pointing at the source. For a Reference source, copy its
            // target verbatim: re-surfacing a Subtree reference yields
            // another Subtree pointing at the same bullet, not a
            // WholeCell of the original (the user wants the same chunk,
            // not the whole note). For a non-Reference source, fall
            // back to a WholeCell pointer at the cell itself.
            if let Some(rect) = self.hit_tests.cell_menu.surface {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        let target = menu
                            .source_reference_target
                            .unwrap_or(ReferenceTarget::WholeCell(
                                menu.reference_origin_cell_id,
                            ));
                        self.surface_as_reference(target);
                    }
                    return true;
                }
            }
            // "Surface '<snippet>' as reference" — sub-tree target. Only
            // present when the menu was opened over a specific bullet.
            // Same source-resolution rule via `reference_origin_cell_id`
            // so subtree references from inside embeds point at the
            // original outline, not at the embed.
            if let Some(rect) = self.hit_tests.cell_menu.surface_subtree {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        // Subtree origin is the cell that *owns* the
                        // bullet — for clicks inside an envelope
                        // header (or a deeper nested embed) that's
                        // the embed's source, not the outer cell.
                        // `bullet_origin_cell_id` tracks this; it
                        // diverges from `reference_origin_cell_id`
                        // for envelope-header bullet hits.
                        if let (Some(origin), Some(bid)) =
                            (menu.bullet_origin_cell_id, menu.bullet_id)
                        {
                            self.surface_as_reference(ReferenceTarget::Subtree {
                                cell_id: origin,
                                bullet_id: bid,
                            });
                        }
                    }
                    return true;
                }
            }
            // "Close" / "Reopen" — flips Cell.closed_at. The
            // visibility filter / dim render react on the next frame;
            // if the toggle closes the cell, it vanishes from the
            // current view (still recoverable via Ctrl+Z).
            if let Some(rect) = self.hit_tests.cell_menu.toggle_cell_active {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        self.toggle_cell_closed(menu.cell_id);
                    }
                    return true;
                }
            }
            // "Close sub-outline" / "Reopen sub-outline" — flips the
            // clicked bullet's closed_at (cascade applies via
            // `compute_effective_open` at render time).
            if let Some(rect) = self.hit_tests.cell_menu.toggle_bullet_active {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        if let Some(bid) = menu.bullet_id {
                            self.toggle_bullet_closed(menu.cell_id, bid);
                        }
                    }
                    return true;
                }
            }
            // Snooze rows — 6 fuzzy presets that target the bullet
            // when the menu opened on one, else the cell. Each one
            // computes its target epoch ms via `attention::resurface_at`
            // and writes through `set_cell_resurface` /
            // `set_bullet_resurface` (which record undo).
            let snooze_rects = self.hit_tests.cell_menu.snooze;
            let snooze_targets_bullet = self.hit_tests.cell_menu.snooze_targets_bullet;
            for (i, rect_opt) in snooze_rects.iter().enumerate() {
                if let Some(rect) = rect_opt {
                    if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                        let preset = SNOOZE_PRESETS[i].0;
                        let when = crate::attention::resurface_at(chrono::Local::now(), preset);
                        if let Some(menu) = self.cell_context_menu.take() {
                            if snooze_targets_bullet {
                                if let Some(bid) = menu.bullet_id {
                                    self.set_bullet_resurface(menu.cell_id, bid, Some(when));
                                }
                            } else {
                                self.set_cell_resurface(menu.cell_id, Some(when));
                            }
                        }
                        return true;
                    }
                }
            }
            // "Unsnooze" — clears `resurface_after` on the same
            // target the snooze rows operate on.
            if let Some(rect) = self.hit_tests.cell_menu.unsnooze {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        if snooze_targets_bullet {
                            if let Some(bid) = menu.bullet_id {
                                self.set_bullet_resurface(menu.cell_id, bid, None);
                            }
                        } else {
                            self.set_cell_resurface(menu.cell_id, None);
                        }
                    }
                    return true;
                }
            }
            self.cell_context_menu = None;
        }

        // Entity-page active/inactive toggle (always present in entity
        // view; rect is None outside it).
        if let ViewKind::Entity(eid) = self.pane_mut().view.view_kind {
            if let Some(rect) = self.hit_tests.entity_page.active_toggle {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    self.toggle_entity_active(eid);
                    return true;
                }
            }
        }

        // Entity-page "+ Create backing cell" button (only present when
        // viewing a cell-less entity). Wire-up of the actual create flow
        // is deferred to Chunk 2; for now, swallow the click so it doesn't
        // fall through.
        if let Some(rect) = self.hit_tests.entity_page.create_button {
            if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                // TODO(chunk-2): trigger create_backing_cell_for_entity.
                return true;
            }
        }

        // Entity-page "REFERENCED IN" embed cards: clicking any of them
        // navigates to the source cell at its real timeline location.
        // Snapshotted into a local first to avoid the &self borrow on
        // `hit_tests.entity_page.refs` outliving the &mut self call to
        // `navigate_to_reference`.
        if matches!(self.pane_mut().view.view_kind, ViewKind::Entity(_)) {
            let hit = self
                .hit_tests.entity_page.refs
                .iter()
                .find(|(_, rect)| {
                    x >= rect.left
                        && x <= rect.right
                        && doc_y >= rect.top
                        && doc_y <= rect.bottom
                })
                .map(|(id, _)| *id);
            if let Some(target_cell_id) = hit {
                self.navigate_to_reference(ReferenceTarget::WholeCell(target_cell_id));
                return true;
            }
        }

        // People-page click flow. Click inside the active rename or add
        // input forwards to that input; click outside commits whichever
        // is active, then continues processing the click. Clicking the
        // "Add person" footer (when no input is active) starts an Add.
        // Plain row click navigates to that entity's page.
        if matches!(self.pane_mut().view.view_kind, ViewKind::People) {
            // "Show inactive" header toggle wins over everything else
            // on the People page, including any in-progress rename
            // / add input — toggling the filter shouldn't lose typed
            // text but shouldn't get masked by the input rects either.
            if let Some(rect) = self.hit_tests.people_page.show_inactive_toggle {
                if x >= rect.left
                    && x <= rect.right
                    && doc_y >= rect.top
                    && doc_y <= rect.bottom
                {
                    if self.people_rename.is_some() {
                        self.commit_people_rename();
                    }
                    if self.people_add.is_some() {
                        self.commit_people_add();
                    }
                    self.show_inactive = !self.show_inactive;
                    return true;
                }
            }
            // Forward-into-input checks come first so caret moves work
            // when the user clicks within their own input.
            let renaming_id = self.people_rename.as_ref().map(|s| s.entity_id);
            if let Some(rid) = renaming_id {
                let rename_rect = self
                    .hit_tests.people_page.rows
                    .iter()
                    .find(|(eid, _)| *eid == rid)
                    .map(|(_, r)| *r);
                if let Some(rr) = rename_rect {
                    if x >= rr.left
                        && x <= rr.right
                        && doc_y >= rr.top
                        && doc_y <= rr.bottom
                    {
                        if let Some(rs) = self.people_rename.as_mut() {
                            rs.input.mouse_down(x, doc_y, modifiers, true);
                        }
                        return true;
                    }
                }
                // Click outside the renaming row → commit, then keep
                // processing.
                self.commit_people_rename();
            }
            if self.people_add.is_some() {
                if let Some(ar) = self.hit_tests.people_page.add {
                    if x >= ar.left
                        && x <= ar.right
                        && doc_y >= ar.top
                        && doc_y <= ar.bottom
                    {
                        if let Some(input) = self.people_add.as_mut() {
                            input.mouse_down(x, doc_y, modifiers, true);
                        }
                        return true;
                    }
                }
                self.commit_people_add();
            }

            // No input was inside the click → "Add person" footer starts
            // an Add input; a row click navigates to its entity page.
            if let Some(rect) = self.hit_tests.people_page.add {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    self.start_people_add();
                    return true;
                }
            }
            for (entity_id, rect) in self.hit_tests.people_page.rows.clone() {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    return self.push_view(Query::entity(entity_id));
                }
            }
            return false;
        }

        // Bar hit-test before falling through to the cell body. A
        // left-click on the bar selects the whole cell in view-mode
        // (focus only; no caret placement, no editing). Distinct
        // from clicking the body, which can drop into edit mode on
        // the same-cell case.
        for (cell_id, rect) in self.hit_tests.cell_bars.clone() {
            if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                self.pane_mut().focused = Some(cell_id);
                self.pane_mut().editing = false;
                self.pane_mut().dragging_cell = None;
                self.pane_mut().pending_caret_scroll = true;
                self.pane_mut().coalesce_break = true;
                // Retire any visible selection on every other cell
                // (matches `dispatch_cell_click` behaviour).
                for other in &mut self.document.cells {
                    if other.id != cell_id {
                        other.clear_all_selections();
                    }
                }
                return true;
            }
        }

        self.dispatch_cell_click(x, doc_y, modifiers)
    }

    /// Cell-area click dispatch, factored out so it can run from
    /// `mouse_down` directly (the common path) or from `mouse_up` as
    /// a replay when an Alt-drag gesture didn't cross the pan
    /// threshold (i.e., the user really did mean a plain Alt+click).
    /// Owns: pick the target cell, retire other cells' selections,
    /// shift focus, set up the cell drag binding, dispatch
    /// `cell.mouse_down`, and drain any link/tag the click stashed.
    fn dispatch_cell_click(
        &mut self,
        x: f32,
        doc_y: f32,
        modifiers: &Modifiers,
    ) -> bool {
        let Some(target) = self.find_cell_at(x, doc_y) else {
            return false;
        };
        // Plain Alt+click on a cell → open *the cell* in the other
        // pane. Sibling of Alt+click on a sidebar entry / search
        // result. Suppressed when the click landed on a link or
        // `#tag` — those have their own Alt+click semantic (open
        // the *link/tag* in the other pane), kept by falling through
        // so `cell.mouse_down` + the pending-link/tag drain below
        // run as before. Suppressed under Shift (Shift+Alt+click is
        // the multi-cursor add) and under the primary modifier
        // (which is already taken on links for "open link while
        // editing").
        let m = modifiers.state();
        let plain_alt = m.alt_key() && !m.shift_key() && !cell::primary_mod(m);
        if plain_alt {
            let on_link_or_tag = self
                .cell(target)
                .map(|c| c.link_at_doc_pos(x, doc_y))
                .unwrap_or(false);
            if !on_link_or_tag {
                return self.open_cell_in_other_pane(target);
            }
        }
        // Cross-cell click drops to view mode (matches keyboard nav). Same-cell
        // click preserves whatever mode the user was in. To start editing a new
        // cell, click it (selects), then hit Enter — or just keep typing once
        // already editing the same cell.
        if Some(target) != self.pane_mut().focused {
            self.pane_mut().focused = Some(target);
            self.pane_mut().editing = false;
        }
        // Retire any visible selection on cells that aren't the click
        // target. Includes embedded-reference caches (recursively), so
        // a stale highlight inside a Reference cell or envelope header
        // doesn't linger when focus moves elsewhere. The target cell's
        // own selection state is reset by its `mouse_down` below.
        for other in &mut self.document.cells {
            if other.id != target {
                other.clear_all_selections();
            }
        }
        // Any click moves/replaces the caret — break coalescing so the next
        // text edit starts a fresh undo entry.
        self.pane_mut().coalesce_break = true;
        self.pane_mut().dragging_cell = Some(target);
        let editing = self.pane_mut().editing;
        let result = match self.cell_mut(target) {
            Some(cell) => cell.mouse_down(x, doc_y, modifiers, editing),
            None => false,
        };
        // The cell's mouse_down may have stashed a URL because the click
        // landed on a link. Drain it here and route through the
        // navigation policy that lives on `KeptApp`. `kept://...` jumps
        // entity / cell pages; `kept-tag://<name>` opens the tag's
        // filter view; external URLs shell out via `open_url`.
        // Alt+click opens kept:// / kept-tag:// in the other pane to
        // match sidebar alt-click semantics.
        let alt = modifiers.state().alt_key();
        let pending = self
            .cell_mut(target)
            .and_then(|c| c.take_pending_link_url());
        if let Some(url) = pending {
            self.handle_link_click(&url, alt);
        }
        result
    }

    /// Click-on-embed → land on the original. Mirrors `close_search_commit`
    /// (app.rs:close_search_commit) for the cell-level case, plus an extra
    /// step for `Subtree` targets that focuses the specific bullet inside
    /// the target outline. No-op when the target cell is gone (the embed
    /// already showed a "[deleted]" placeholder).
    /// Filter-first commit from the URL bar: parse the typed text
    /// as a query AST and push it as a new view on `pane_idx` (or
    /// the other pane when `alt=true`). Matches browser URL-bar
    /// feel — Enter on `today` lands you in today's date timeline;
    /// Enter on `#urgent` lands you in the tag filter view; Enter
    /// on free-text lands you in a substring-match view. Blurs
    /// the header on commit. No-op when the typed text is empty.
    fn commit_header_filter(&mut self, pane_idx: usize, alt: bool) {
        let text = self.panes[pane_idx].header.textbox.text().trim().to_string();
        if text.is_empty() {
            self.panes[pane_idx].header.blur();
            return;
        }
        let q = Query::from_text(&text);
        let saved_active = self.active_pane;
        self.active_pane = pane_idx;
        if alt {
            self.open_in_other_pane(q);
        } else {
            self.push_view(q);
        }
        self.active_pane = saved_active;
        self.panes[pane_idx].header.blur();
        self.mention_popup = None;
    }

    /// Commit a result row from the focused pane's URL-bar
    /// dropdown. `pane_idx` is the pane the dropdown belongs to;
    /// `row_idx` is the index into that pane's
    /// `header.cached_results`. With `alt=true` the destination is
    /// the other pane (matches the old Ctrl+K Alt+Enter behavior).
    /// Dispatches on entry kind: entity-page shortcut → entity
    /// view; cell row → single-cell view. Blurs the header on
    /// commit. No-op if the row is out of range or the target
    /// vanished between cache and dispatch.
    fn commit_header_result(&mut self, pane_idx: usize, row_idx: usize, alt: bool) {
        let entry = match self.panes[pane_idx].header.cached_results.get(row_idx) {
            Some(&e) => e,
            None => return,
        };
        let (query, focus_cell_id): (Query, Option<Uuid>) = match entry {
            pane::HeaderResultEntry::EntityPage(eid) => {
                let known = self.entities.entities.iter().any(|e| e.id == eid);
                if !known {
                    self.panes[pane_idx].header.blur();
                    return;
                }
                (Query::entity(eid), None)
            }
            pane::HeaderResultEntry::Cell(cid) => {
                if self.cell(cid).is_none() {
                    self.panes[pane_idx].header.blur();
                    return;
                }
                (Query::cell(cid), Some(cid))
            }
        };
        // Hop active focus to the source pane so `push_view`'s
        // deref-writes land there.
        let saved_active = self.active_pane;
        self.active_pane = pane_idx;
        let dest_pane = if alt {
            self.open_in_other_pane(query)
        } else if self.push_view(query) {
            Some(self.active_pane)
        } else {
            Some(self.active_pane)
        };
        self.active_pane = saved_active;
        if let (Some(idx), Some(cell_id)) = (dest_pane, focus_cell_id) {
            let pane = &mut self.panes[idx];
            pane.focused = Some(cell_id);
            pane.editing = false;
            pane.coalesce_break = true;
            pane.pending_caret_scroll = true;
        }
        // Blur the source pane's header now that nav happened.
        self.panes[pane_idx].header.blur();
        self.mention_popup = None;
    }

    fn navigate_to_reference(&mut self, target: ReferenceTarget) {
        let cell_id = target.cell_id();
        if self.cell(cell_id).is_none() {
            return;
        }
        self.push_view(Query::cell(cell_id));
        self.pane_mut().focused = Some(cell_id);
        self.pane_mut().editing = false;
        self.pane_mut().pending_caret_scroll = true;
        // Subtree target: drill into the outline cell, focus the
        // specific bullet, AND select its subtree (bullet + descendants)
        // so the original chunk the embed pointed at is visually
        // highlighted on arrival — matches the right-click selection
        // behavior. Cell-level focus alone is the fallback if the
        // bullet is missing or the cell isn't an outline anymore.
        if let ReferenceTarget::Subtree { bullet_id, .. } = target {
            if let Some(c) = self.cell_mut(cell_id) {
                if let CellKind::Outline(oc) = &mut c.kind {
                    let _ = oc.set_focused_bullet(bullet_id);
                    oc.select_subtree(bullet_id);
                }
            }
        }
    }

    /// Resolve a clicked link URL. `kept://<uuid>` routes by uuid kind:
    /// entity match → entity page; cell match → date view + focus the
    /// cell; neither → drop (don't shell out, that produces a useless
    /// OS error). Other URLs hand off to `cell::open_url` (xdg-open).
    fn handle_link_click(&mut self, url: &str, alt: bool) {
        // `kept-tag://<name>` → navigate to the tag's filter view, same
        // destination as a sidebar tag-row click. Alt+click opens it
        // in the other pane.
        if let Some(name) = cell::tag_name_from_url(url) {
            if name.is_empty() {
                return;
            }
            let q = Query::tag(name.to_string());
            if alt {
                self.open_in_other_pane(q);
            } else {
                self.push_view(q);
            }
            return;
        }
        if let Some(rest) = url.strip_prefix("kept://") {
            if let Ok(uuid) = Uuid::parse_str(rest) {
                let q = if self.entities.entities.iter().any(|e| e.id == uuid) {
                    Some((Query::entity(uuid), None))
                } else if self.cell(uuid).is_some() {
                    Some((Query::cell(uuid), Some(uuid)))
                } else {
                    eprintln!("kept: dangling kept:// link: {url}");
                    return;
                };
                if let Some((q, focus_cell)) = q {
                    // Track which pane just received the view so the
                    // optional cell-focus step writes there, not on
                    // the (possibly unchanged) active pane.
                    let target_pane = if alt {
                        self.open_in_other_pane(q)
                    } else {
                        if self.push_view(q) {
                            Some(self.active_pane)
                        } else {
                            None
                        }
                    };
                    if let (Some(cell_id), Some(idx)) = (focus_cell, target_pane) {
                        // Cell-target link: focus the cell + drop edit
                        // mode + scroll it into view on the pane that
                        // just received the view. Active pane is
                        // preserved by `open_in_other_pane`, so write
                        // through `self.panes[idx]` directly.
                        let pane = &mut self.panes[idx];
                        pane.focused = Some(cell_id);
                        pane.editing = false;
                        pane.pending_caret_scroll = true;
                    }
                }
                return;
            }
        }
        cell::open_url(url);
    }

    pub fn mouse_drag_to(&mut self, x: f32, y: f32) -> bool {
        // Scrollbar drag wins — translates the y-motion directly into
        // a scroll position. `cursor_moved` also routes drag motion,
        // but mouse_drag_to is the fallback path some hosts use for
        // explicit drag deltas; honor both. Same applies to Space-drag
        // pan below.
        if self.sidebar_scroll.is_dragging_thumb() {
            return self.sidebar_scroll.apply_thumb_drag(y);
        }
        for pane in &mut self.panes {
            if pane.scroller.is_dragging_thumb() {
                return pane.scroller.apply_thumb_drag(y);
            }
        }
        // Promote a tentative Alt-drag the same way `cursor_moved`
        // does — promotion installs `scroller.dragging`, after which
        // the thumb-drag short-circuit at the top of this fn handles
        // every subsequent move uniformly with real thumb drags.
        let promoted = self.maybe_promote_tentative_pan(y);
        if promoted {
            return true;
        }
        // Divider drag wins — recompute split_ratio relative to the pane
        // area (sidebar's right edge → window's right edge).
        if self.dragging_divider && self.panes.len() >= 2 {
            let pane_area_left = self.panes[0].last_rect.left;
            let pane_area_right = self.panes[self.panes.len() - 1].last_rect.right;
            let pane_area_w = (pane_area_right - pane_area_left).max(1.0);
            self.split_ratio = ((x - pane_area_left) / pane_area_w).clamp(SPLIT_MIN, SPLIT_MAX);
            return true;
        }
        if let Some(idx) = self.header_dragging_pane {
            if let Some(pane) = self.panes.get_mut(idx) {
                return pane.header.textbox.mouse_drag_to(x, y);
            }
        }
        // Quick-Add modal is a window-space overlay — its cell
        // expects window-coords directly (no scroll translate).
        if let Some(state) = self.quick_add.as_mut() {
            return state.cell.mouse_drag_to(x, y);
        }
        let doc_y = y + self.pane_mut().scroll_y;
        if let Some(id) = self.pane_mut().dragging_cell {
            match self.cell_mut(id) {
                Some(cell) => cell.mouse_drag_to(x, doc_y),
                None => false,
            }
        } else {
            false
        }
    }

    pub fn mouse_up(&mut self) -> bool {
        // Tentative Alt-drag that never crossed the threshold —
        // commit it now as a plain Alt+click by replaying the
        // deferred dispatch on the matching surface. Sidebar
        // gestures replay through `dispatch_sidebar_click`; doc-area
        // gestures replay through `dispatch_doc_click` (covers cell
        // clicks, People-page rows, entity-page references, the cell
        // context menu, etc.). The cell-level `mouse_up` further
        // down then commits any drag state set up by the replay. (A
        // committed pan lives on `pan_drag` and is finalized further
        // down instead.)
        if let Some(tp) = self.tentative_pan.take() {
            match tp.target {
                PanTarget::Sidebar => {
                    let _ = self.dispatch_sidebar_click(
                        tp.click_x,
                        tp.click_y,
                        &tp.click_modifiers,
                    );
                }
                PanTarget::Pane(_) => {
                    let _ = self.dispatch_doc_click(
                        tp.click_x,
                        tp.click_y,
                        tp.click_doc_y,
                        &tp.click_modifiers,
                    );
                }
            }
        }

        // End any thumb drag — covers both real scrollbar thumb
        // drags AND Alt-drag pans (both route through
        // `scroller.dragging`, so a single end_thumb_drag pass
        // prunes the velocity window, anchors kinetic dt, and
        // releases the binding for either kind). Velocity is left in
        // place — a flick coasts via the per-frame `step_kinetic`
        // loop, identical to wheel-burst release.
        let mut ended = self.sidebar_scroll.end_thumb_drag();
        for pane in &mut self.panes {
            ended |= pane.scroller.end_thumb_drag();
        }
        // Pan-drag marker (drives the "grabbing" cursor) — clear
        // regardless. The actual scroll cleanup already happened in
        // end_thumb_drag.
        let was_panning = std::mem::replace(&mut self.pan_drag, false);
        if ended || was_panning {
            // Refresh hover from the current cursor — if the user
            // released far from any scrollbar, the bar should drop
            // back to thin immediately.
            let (mx, my) = self.mouse_pos;
            self.sidebar_scroll.set_hover_for_point(mx, my);
            for pane in &mut self.panes {
                pane.scroller.set_hover_for_point(mx, my);
            }
            return true;
        }
        if self.dragging_divider {
            self.dragging_divider = false;
            return true;
        }
        if let Some(state) = self.quick_add.as_mut() {
            return state.cell.mouse_up();
        }
        if let Some(idx) = self.header_dragging_pane.take() {
            if let Some(pane) = self.panes.get_mut(idx) {
                return pane.header.textbox.mouse_up();
            }
        }
        if let Some(id) = self.pane_mut().dragging_cell.take() {
            match self.cell_mut(id) {
                Some(cell) => cell.mouse_up(),
                None => false,
            }
        } else {
            false
        }
    }

    /// True when the mouse is currently over the pane divider — main.rs
    /// uses this to swap to a column-resize cursor.
    pub fn is_hovering_divider(&self) -> bool {
        self.is_on_divider(self.mouse_pos.0)
    }

    /// Pick the visible cell that contains `(x, doc_y)` — `doc_y` must already
    /// include any scroll offset. Each cell's clickable region is its
    /// rendered rect plus half of `CELL_GAP` on each interior side (so clicks in
    /// the gap snap to whichever cell owns that half). Returns `None` for clicks
    /// above the first cell, below the last cell, or outside the cell width.
    fn find_cell_at(&self, x: f32, y: f32) -> Option<Uuid> {
        let visible: Vec<Uuid> = self.visible_cell_ids();
        if visible.is_empty() {
            return None;
        }
        let half_gap = CELL_GAP * 0.5;
        let last = visible.len() - 1;
        for (i, id) in visible.iter().enumerate() {
            let cell = match self.cell(*id) {
                Some(c) => c,
                None => continue,
            };
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
                return Some(*id);
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


/// Pill-shaped on/off switch. `on=true` paints the track in the
/// active-blue used elsewhere with the knob on the right; `on=false` is
/// muted gray with the knob on the left. `hovered=true` adds a subtle
/// dark overlay so the affordance reads as clickable. Caller records
/// `rect` for hit-testing.
fn draw_toggle(canvas: &Canvas, rect: Rect, on: bool, hovered: bool) {
    let h = rect.height();
    let radius = h * 0.5;
    // Track.
    let mut track = Paint::default();
    track.set_anti_alias(true);
    if on {
        track.set_color(crate::color::accent_blue_focus_edit());
    } else {
        track.set_color(crate::color::toggle_off_bg());
    }
    canvas.draw_round_rect(rect, radius, radius, &track);
    if hovered {
        let mut overlay = Paint::default();
        overlay.set_anti_alias(true);
        overlay.set_color(crate::color::hover_faint());
        canvas.draw_round_rect(rect, radius, radius, &overlay);
    }
    // Knob.
    let inset = 2.0_f32.max(h * 0.1);
    let knob_r = h * 0.5 - inset;
    let cy = (rect.top + rect.bottom) * 0.5;
    let cx = if on {
        rect.right - inset - knob_r
    } else {
        rect.left + inset + knob_r
    };
    let mut knob = Paint::default();
    knob.set_anti_alias(true);
    knob.set_color(crate::color::bg_card());
    canvas.draw_circle((cx, cy), knob_r, &knob);
}

fn local_date_for_ms(epoch_ms: i64) -> chrono::NaiveDate {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(epoch_ms)
        .single()
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| {
            Local
                .timestamp_millis_opt(0)
                .single()
                .unwrap()
                .date_naive()
        })
}

/// Trim a cell's full text to a single-line snippet centered around the
/// match. If `query`'s residual text appears, show ~40 chars before + the
/// match + ~40 after. Falls back to the leading window for queries that are
/// entirely structured (`#tag`, `today`, etc. — no text to find).

/// Per-frame bullet filter for an outline cell rendered under a tag-
/// filtered view. Returns Some(set of bullet IDs) iff:
/// - the cell is NOT the focused cell (focused cells render full so
///   navigation / hit-testing operate on the complete outline);
/// - the active view has at least one tag-include filter; AND
/// - the cell's title doesn't carry any of the include tags (so the
///   cell matched via body bullets only — title-tagged cells are
///   whole-cell matches).
/// Otherwise None — render all bullets.
/// Pick the color the cell's left-edge state bar should paint with.
/// Priority order matters: closed wins over snoozed (closed cells
/// only show at all when the show-inactive toggle is on, and the
/// dim treatment should read first); snoozed wins over the
/// reference/envelope accent (a snoozed reference is "waiting" more
/// than "surfaced"); reference/envelope wins over plain open
/// (resurfacing origin is the distinguishing signal).
/// Build an OPEN path tracing the cell-chrome rect's top, right
/// side, and bottom — but NOT the left edge. The bar already
/// occupies the chrome's left edge visually (its rounded TL/BL
/// supply the card's outer-left corners), so any stroke painted
/// along chrome.left would overdraw the bar's right edge as a
/// visible line. Stroke this path instead of the full rrect to
/// hide that line.
///
/// The path's TL and BL are at `(rect.left, rect.top)` and
/// `(rect.left, rect.bottom)` — flat against the bar. TR and BR
/// curve at `radius`. Filling this path gives the same area as a
/// flat-left rrect (the implicit close is the missing left edge),
/// but stroking only traces the visible three sides.
fn chrome_open_path(rect: skia_safe::Rect, radius: f32) -> skia_safe::Path {
    let mut p = skia_safe::Path::new();
    p.move_to((rect.left, rect.top));
    p.line_to((rect.right - radius, rect.top));
    p.arc_to_tangent(
        (rect.right, rect.top),
        (rect.right, rect.top + radius),
        radius,
    );
    p.line_to((rect.right, rect.bottom - radius));
    p.arc_to_tangent(
        (rect.right, rect.bottom),
        (rect.right - radius, rect.bottom),
        radius,
    );
    p.line_to((rect.left, rect.bottom));
    p
}

/// Short display snippet for a cell — used as the human-readable
/// label on a reference link. Takes the cell's first ~40 chars,
/// stripping newlines and trailing whitespace. Empty for empty
/// cells (caller falls back to a generic label).
fn snippet_for_cell(cell: &Cell) -> String {
    const MAX: usize = 40;
    let text = cell.full_text();
    let mut one_line = String::new();
    for c in text.chars() {
        if c == '\n' || c == '\r' {
            if !one_line.is_empty() && !one_line.ends_with(' ') {
                one_line.push(' ');
            }
        } else {
            one_line.push(c);
        }
        if one_line.chars().count() >= MAX {
            break;
        }
    }
    let trimmed = one_line.trim();
    if trimmed.chars().count() > MAX {
        let truncated: String = trimmed.chars().take(MAX).collect();
        format!("{}…", truncated.trim_end())
    } else {
        trimmed.to_string()
    }
}

/// Flatten an Outline payload to a single indented text string +
/// rebased link spans, for pasting into a non-outline target.
/// Each bullet becomes a line prefixed by 4 spaces per depth.
fn flatten_outline(
    bullets: &[crate::clipboard::BulletPayload],
) -> (String, Vec<crate::clipboard::SerLink>) {
    use crate::clipboard::SerLink;
    let mut text = String::new();
    let mut links: Vec<SerLink> = Vec::new();
    for (i, b) in bullets.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        let indent = "    ".repeat(b.depth as usize);
        text.push_str(&indent);
        let bullet_start = text.len();
        text.push_str(&b.text);
        for l in &b.links {
            let s = bullet_start + l.start;
            let e = bullet_start + l.end;
            if e <= text.len() && s < e {
                links.push(SerLink {
                    start: s,
                    end: e,
                    url: l.url.clone(),
                });
            }
        }
    }
    (text, links)
}

/// Cell-local payload builder. Same logic as
/// `KeptApp::build_copy_payload` but parameterized on the cell ref
/// + editing flag, so the Quick-Add modal can build its own copy
/// payload from its in-flight cell (which isn't in `document.cells`).
fn build_copy_payload_for_cell(
    cell: &Cell,
    editing: bool,
) -> Option<crate::clipboard::KeptPayload> {
    use crate::clipboard::{BulletPayload, KeptPayload, SerLink};
    // 1. Outline with active multi-bullet selection → Outline payload.
    if !cell.title_focused {
        if let CellKind::Outline(oc) = &cell.kind {
            if let Some(rows) = oc.copy_bullet_selection_with_links() {
                if !rows.is_empty() {
                    return Some(KeptPayload::Outline {
                        bullets: rows
                            .into_iter()
                            .map(|(d, t, ls)| BulletPayload {
                                depth: d,
                                text: t,
                                links: SerLink::spans_to_ser(&ls),
                            })
                            .collect(),
                    });
                }
            }
        }
    }
    // 2. Whichever textbox in the cell currently holds a selection
    //    → Text payload. The single-selection invariant means
    //    there's at most one.
    if let Some((text, links)) = cell.copy_primary_selection_with_links() {
        return Some(KeptPayload::Text {
            text,
            links: SerLink::spans_to_ser(&links),
        });
    }
    // 3. View-mode whole-cell fallback. In edit mode a stray Ctrl+C
    //    with no selection shouldn't dump the whole cell.
    if editing {
        return None;
    }
    let whole_text = cell.full_text();
    if whole_text.is_empty() {
        return None;
    }
    Some(KeptPayload::Text {
        text: whole_text,
        links: Vec::new(),
    })
}

/// Insert `text` + `links` at the focused caret in `cell`. Title
/// or body, depending on `cell.title_focused`. Cell-local primitive
/// shared by `KeptApp::paste_text_with_links` and the Quick-Add
/// modal's paste path.
fn paste_text_with_links_into_cell(
    cell: &mut Cell,
    text: &str,
    links: &[crate::cell::LinkSpan],
) {
    if cell.title_focused {
        if let Some(title) = cell.title_mut() {
            title.paste_with_links(text, links);
            return;
        }
    }
    cell.paste_into_focused_with_links(text, links);
}

/// Apply a `KeptPayload` to `cell` as a default paste — same
/// dispatch as `KeptApp::apply_paste_default` but the
/// Reference→Reference-cell creation path is left to the caller
/// (the modal handles References as inline links only; the
/// document path uses `surface_as_reference` for the alternate
/// variant). Cell-local so the Quick-Add modal can paste without
/// going through the focus-and-dirty machinery.
fn apply_paste_into_cell(cell: &mut Cell, payload: crate::clipboard::KeptPayload) {
    use crate::clipboard::{KeptPayload, SerLink};
    match payload {
        KeptPayload::Text { text, links } => {
            paste_text_with_links_into_cell(cell, &text, &SerLink::ser_to_spans(links));
        }
        KeptPayload::Outline { bullets } => {
            let is_outline = matches!(cell.kind, CellKind::Outline(_));
            if is_outline && !cell.title_focused {
                if let CellKind::Outline(oc) = &mut cell.kind {
                    let raw: Vec<(u32, String, Vec<crate::cell::LinkSpan>)> = bullets
                        .into_iter()
                        .map(|b| (b.depth, b.text, SerLink::ser_to_spans(b.links)))
                        .collect();
                    oc.insert_bullets_after_focused(raw);
                }
            } else {
                let (text, links) = flatten_outline(&bullets);
                paste_text_with_links_into_cell(
                    cell,
                    &text,
                    &SerLink::ser_to_spans(links),
                );
            }
        }
        KeptPayload::Reference { target, snippet } => {
            let url = target.to_url();
            let display = if snippet.trim().is_empty() {
                "↗ reference".to_string()
            } else {
                format!("↗ {}", snippet)
            };
            let links = vec![crate::cell::LinkSpan {
                range: 0..display.len(),
                url,
            }];
            paste_text_with_links_into_cell(cell, &display, &links);
        }
    }
}

fn bar_color_for_cell(cell: &Cell, now_ms: i64) -> skia_safe::Color {
    if !cell.is_open() {
        return crate::color::cell_bar_closed();
    }
    let snoozed = cell.resurface_after.map_or(false, |t| now_ms < t);
    if snoozed {
        return crate::color::cell_bar_snoozed();
    }
    let is_reference_or_envelope = match &cell.kind {
        CellKind::Reference(_) => true,
        CellKind::Outline(oc) => oc.has_reference_header(),
        _ => false,
    };
    if is_reference_or_envelope {
        return crate::color::cell_bar_resurfaced();
    }
    crate::color::cell_bar_default()
}

fn compute_outline_bullet_filter(
    cell: &Cell,
    include_tags: &[String],
    cell_is_focused: bool,
) -> Option<HashSet<Uuid>> {
    if cell_is_focused || include_tags.is_empty() {
        return None;
    }
    let CellKind::Outline(oc) = &cell.kind else {
        return None;
    };
    let title_tags = cell.heading_tag_names();
    let title_carries = include_tags
        .iter()
        .any(|t| title_tags.iter().any(|n| n.eq_ignore_ascii_case(t)));
    if title_carries {
        return None;
    }
    let m = oc.bullets_matching_any_tag(include_tags);
    if m.is_empty() {
        None
    } else {
        Some(m)
    }
}

fn format_date_label(d: chrono::NaiveDate) -> String {
    let now = chrono::Local::now().date_naive();
    if d == now {
        format!("Today – {}", d.format("%b %-d"))
    } else if d.succ_opt() == Some(now) {
        format!("Yesterday – {}", d.format("%b %-d"))
    } else {
        // Explicit dates get a short weekday up front (Mon / Tue / …) so
        // a glance at the sidebar conveys "what day of the week" without
        // doing the math from the calendar date.
        d.format("%a %B %-d, %Y").to_string()
    }
}

/// Time-only label for sidebar context rows nested under a date header.
fn format_context_time(epoch_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_millis_opt(epoch_ms)
        .single()
        .unwrap_or_else(|| Local.timestamp_millis_opt(0).single().unwrap());
    dt.format("%-I:%M %p").to_string()
}

fn context_ref<'a>(c: &'a Context) -> ContextRef<'a> {
    ContextRef {
        id: c.id,
        start_time: c.start_time,
        end_time: c.end_time,
        title: c.title.as_deref(),
    }
}

/// Pixel-aware truncation: returns the longest prefix of `text` whose
/// rendered width (in `font` with `paint`) fits in `max_width`,
/// followed by `…` if any chars were dropped. Returns the whole input
/// untouched when it already fits, and an empty string when even the
/// ellipsis doesn't fit. O(n) measure_str calls — fine for short
/// menu/popup labels.
fn fit_text_ellipsized(text: &str, max_width: f32, font: &Font, paint: &Paint) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    if font.measure_str(text, Some(paint)).0 <= max_width {
        return text.to_string();
    }
    const ELLIPSIS: &str = "…";
    let ell_w = font.measure_str(ELLIPSIS, Some(paint)).0;
    let avail = max_width - ell_w;
    if avail <= 0.0 {
        return String::new();
    }
    let mut out = String::new();
    let mut acc_w = 0.0_f32;
    let mut buf = [0u8; 4];
    for ch in text.chars() {
        let s = ch.encode_utf8(&mut buf);
        let w = font.measure_str(s, Some(paint)).0;
        if acc_w + w > avail {
            break;
        }
        out.push(ch);
        acc_w += w;
    }
    out.push_str(ELLIPSIS);
    out
}

/// Shift `rect` so it stays inside the `[margin, view-margin]` window
/// in both axes. Used by floating overlays (context menus, mention
/// popup) so they don't draw past the right/bottom edges when their
/// anchor falls near a corner. If the rect is taller/wider than the
/// available space, the top-left wins (anchored at `margin`).
fn clamp_rect_to_viewport(rect: Rect, view_w: f32, view_h: f32, margin: f32) -> Rect {
    let w = rect.width();
    let h = rect.height();
    let mut left = rect.left;
    if left + w + margin > view_w {
        left = view_w - w - margin;
    }
    if left < margin {
        left = margin;
    }
    let mut top = rect.top;
    if top + h + margin > view_h {
        top = view_h - h - margin;
    }
    if top < margin {
        top = margin;
    }
    Rect::new(left, top, left + w, top + h)
}

/// Cmd/Ctrl+C/X/V/A and Cmd+Z / Cmd+Shift+Z / Cmd+Y on a `TextBox`,
/// with the system clipboard wired up and undo routed through the
/// textbox's local stack. Returns true iff the key was handled —
/// the caller should `return true` from its key handler. Pass
/// `single_line=true` for inline inputs (search, rename, add) so paste
/// flattens newlines into spaces. The cell-level paste/undo path stays
/// separate because it has snapshot/cell-undo concerns this helper
/// doesn't know about; cells intercept these shortcuts at the app
/// level before forwarding to the focused `TextBox`.
fn apply_clipboard_shortcut(
    input: &mut TextBox,
    clipboard: Option<&mut Clipboard>,
    event: &KeyEvent,
    mods: winit::keyboard::ModifiersState,
    single_line: bool,
) -> bool {
    if event.state != ElementState::Pressed || !primary_mod(mods) {
        return false;
    }
    let Key::Character(s) = &event.logical_key else {
        return false;
    };
    let s = s.as_str();
    if s.eq_ignore_ascii_case("c") {
        let text = input.copy_primary_selection();
        if !text.is_empty() {
            if let Some(cb) = clipboard {
                let _ = cb.set_text(text);
            }
        }
        return true;
    }
    if s.eq_ignore_ascii_case("x") {
        let text = input.cut_primary_selection();
        if !text.is_empty() {
            if let Some(cb) = clipboard {
                let _ = cb.set_text(text);
            }
        }
        return true;
    }
    if s.eq_ignore_ascii_case("v") {
        let Some(cb) = clipboard else {
            return true;
        };
        let text = match cb.get_text() {
            Ok(t) => t,
            Err(_) => return true,
        };
        if text.is_empty() {
            return true;
        }
        let cleaned: String = if single_line {
            text.chars()
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect()
        } else {
            text
        };
        input.paste(&cleaned);
        return true;
    }
    if s.eq_ignore_ascii_case("a") {
        input.select_all();
        return true;
    }
    if s.eq_ignore_ascii_case("z") {
        if mods.shift_key() {
            input.redo();
        } else {
            input.undo();
        }
        return true;
    }
    if s.eq_ignore_ascii_case("y") {
        // Windows-flavored redo. Cmd+Shift+Z above covers the Mac form.
        input.redo();
        return true;
    }
    false
}

/// same font as everything else in the app.
/// Trim arbitrary text to a single-line preview suitable for menu labels.
/// Collapses internal whitespace, truncates with ellipsis past 30 chars,
/// returns "[empty]" for blank input so the menu row reads sensibly.
fn snippet(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "[empty]".to_string();
    }
    if collapsed.chars().count() <= 30 {
        return collapsed;
    }
    let truncated: String = collapsed.chars().take(29).collect();
    format!("{}…", truncated)
}

/// Walk into `cell` looking for an outline whose bullets cover `doc_y`,
/// descending through embed wrappers (Reference cells, envelope outline
/// headers, recursive nested embeds) until either the deepest matching
/// outline is found or no outline matches. When a match is found,
/// highlights the bullet's sub-tree on that outline (`select_subtree`)
/// and returns `(origin_cell_id, bullet_id, bullet_text)`.
///
/// `origin_cell_id` is the id of the source cell whose outline owns
/// the bullet — *not* `cell.id` when the descent crossed an embed
/// boundary. Cache outlines preserve source bullet ids verbatim
/// (see `clone_for_scale` and `build_reference_cache`), so a Subtree
/// reference built from the returned `(origin, bullet)` resolves
/// against the live source. Without this, surfacing a bullet from
/// inside an envelope outline's header would mis-attribute the
/// bullet to the envelope itself and immediately render as
/// "[referenced bullet deleted]".
///
/// `top_cell_id` is the starting origin — the cell `cell` belongs to.
/// Embed boundaries (`Reference` cell or `Outline` header band)
/// update the origin to the embed's `target.cell_id()` for the
/// recursive call.
///
/// The descent is mutating because the caller wants the visual
/// highlight to land in the right place — the outline that contains
/// the click, not the outermost wrapper. Each layer is mutually
/// exclusive: a click in an envelope's header band routes into the
/// header cache and ignores the outer outline's bullets, so a single
/// hit can't double-highlight.
fn select_subtree_at_doc_y(
    cell: &mut Cell,
    top_cell_id: Uuid,
    doc_y: f32,
) -> Option<(Uuid, Uuid, String)> {
    fn descend(
        kind: &mut CellKind,
        origin: Uuid,
        doc_y: f32,
    ) -> Option<(Uuid, Uuid, String)> {
        match kind {
            CellKind::Outline(oc) => {
                // Envelope outline: a click inside the header band
                // descends into the embedded reference's cache rather
                // than treating it as a bullet hit. The new origin
                // is the header's target cell — that's the source
                // whose bullet ids the cache outline carries. If the
                // cache is missing (depth-cap, dangling target) the
                // click falls on the placeholder — return None and
                // don't fall through to bullets, since the click
                // wasn't in the bullet region.
                if let Some((top, bot)) = oc.header_y_band() {
                    if doc_y >= top && doc_y < bot {
                        let new_origin = oc
                            .reference_header()
                            .map(|h| h.target().cell_id())?;
                        return oc
                            .reference_header_mut()
                            .and_then(|h| h.cache_mut())
                            .and_then(|cache| descend(&mut cache.kind, new_origin, doc_y));
                    }
                }
                let (id, text) = oc.bullet_at_doc_y(doc_y)?;
                oc.select_subtree(id);
                Some((origin, id, text))
            }
            CellKind::Reference(rc) => {
                // Crossing into a Reference cell's cache: the cache
                // outline carries the target source's bullet ids, so
                // the origin shifts to that source.
                let new_origin = rc.target().cell_id();
                let cache = rc.cache_mut()?;
                descend(&mut cache.kind, new_origin, doc_y)
            }
            _ => None,
        }
    }
    descend(&mut cell.kind, top_cell_id, doc_y)
}


#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::{FontMgr, Typeface};

    fn tf() -> Typeface {
        FontMgr::new()
            .new_from_data(include_bytes!("../../resources/fonts/Figtree.ttf"), None)
            .expect("font loads")
    }

    #[test]
    fn closed_cell_still_visible_in_default_view_predicate() {
        // The visibility filter (is_visible_for_view) used to read
        // `cell.active`. Under the new model it must read
        // `cell.closed_at.is_none()` — closed cells are hidden by
        // default but reappear when `show_inactive_cells` is on.
        // This test exercises the Cell-level invariant: a closed
        // cell reports is_open() as false.
        let mut cell = Cell::new(tf(), "x".to_string());
        cell.closed_at = Some(123);
        assert!(!cell.is_open());
    }

    #[test]
    fn split_title_name_and_tags_basic() {
        let (name, tags) = split_title_name_and_tags("Patrick Foy #person");
        assert_eq!(name, "Patrick Foy");
        assert_eq!(tags, "#person");
    }

    #[test]
    fn split_title_name_and_tags_no_tags() {
        let (name, tags) = split_title_name_and_tags("PatrickFoy");
        assert_eq!(name, "PatrickFoy");
        assert_eq!(tags, "");
    }

    #[test]
    fn split_title_name_and_tags_multiple_tags() {
        let (name, tags) = split_title_name_and_tags("Big Idea #urgent #person");
        assert_eq!(name, "Big Idea");
        assert_eq!(tags, "#urgent #person");
    }

    #[test]
    fn split_title_name_and_tags_only_tags() {
        let (name, tags) = split_title_name_and_tags("#person");
        // No name remains; the whole string is tags.
        assert_eq!(name, "");
        assert_eq!(tags, "#person");
    }

    #[test]
    fn split_title_name_and_tags_internal_hash_is_not_a_tag() {
        // `#abc` mid-name (not at the trailing edge) doesn't count — the
        // walk-from-end logic stops at the first non-`#` whitespace-
        // delimited token.
        let (name, tags) = split_title_name_and_tags("Notes #abc Foo");
        assert_eq!(name, "Notes #abc Foo");
        assert_eq!(tags, "");
    }
}
