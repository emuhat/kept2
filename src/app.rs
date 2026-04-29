use std::collections::HashSet;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use uuid::{Uuid, uuid};
use skia_safe::{
    BlurStyle, Canvas, Color, Font, FontMgr, MaskFilter, Paint, PaintStyle, Point, Rect, Typeface,
};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

use crate::cell::{Cell, CellSnapshot, now_epoch_ms};
use crate::persist::{ContextRef, Db, db_path};

const FONT_BYTES: &[u8] = include_bytes!("../resources/fonts/Figtree.ttf");

const MARGIN_X: f32 = 40.0;
const MARGIN_TOP: f32 = 60.0;
const CELL_GAP: f32 = 20.0;
const FOCUS_PAD: f32 = 10.0;
const FOCUS_RADIUS: f32 = 10.0;
const FOCUS_STROKE: f32 = 1.0;
const FOCUS_STROKE_EDIT: f32 = 2.0;
const FOCUS_RING_ALPHA: u8 = 0x60;
const FOCUS_RING_ALPHA_EDIT: u8 = 0xff;
const FOCUS_SHADOW_ALPHA: u8 = 0x30;
const FOCUS_SHADOW_BLUR: f32 = 12.0;
const FOCUS_SHADOW_DY: f32 = 3.0;
const DOC_BOTTOM_PAD: f32 = 24.0;
const SCROLLBAR_INSET: f32 = 4.0;
const SCROLLBAR_WIDTH: f32 = 4.0;
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
const SCROLLBAR_HOLD: Duration = Duration::from_millis(800);
const SCROLLBAR_FADE: Duration = Duration::from_millis(700);
const ZOOM_STEP: f32 = 1.1;
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 3.0;
const COALESCE_INTERVAL: Duration = Duration::from_millis(600);

const MENTION_POPUP_WIDTH: f32 = 220.0;
const MENTION_POPUP_ROW_H: f32 = 28.0;
const MENTION_POPUP_PAD: f32 = 6.0;
const MENTION_POPUP_RADIUS: f32 = 6.0;
const MENTION_POPUP_MAX_VISIBLE: usize = 6;
const MENTION_BODY_FONT_SIZE: f32 = 16.0;

const FAKE_MENTIONS: &[&str] = &[
    "alice", "alex", "alfred", "anna", "bob", "carol", "dave", "eve",
    "frank", "grace", "heidi", "ivan", "judy", "karl", "linda",
];

/// Subsequence fuzzy match. Returns `(score, matched_byte_positions)` if every
/// query char appears in `candidate` (case-insensitive) in order; None otherwise.
/// Bonuses: start-of-string, post-separator, contiguous-with-previous-match.
/// Length penalty so shorter candidates win ties.
fn fuzzy_score(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let q_lower = query.to_lowercase();
    let c_lower = candidate.to_lowercase();
    let q = q_lower.as_bytes();
    let c = c_lower.as_bytes();

    let mut matches: Vec<usize> = Vec::with_capacity(q.len());
    let mut qi = 0;
    let mut score: i32 = 0;
    let mut prev_match: Option<usize> = None;

    for i in 0..c.len() {
        if qi >= q.len() {
            break;
        }
        if c[i] == q[qi] {
            matches.push(i);
            if i == 0 {
                score += 8;
            } else if !c[i - 1].is_ascii_alphanumeric() {
                score += 4;
            }
            if let Some(prev) = prev_match {
                if i == prev + 1 {
                    score += 5;
                }
            }
            score += 1;
            prev_match = Some(i);
            qi += 1;
        }
    }

    if qi < q.len() {
        return None;
    }
    score -= (c.len() as i32) / 4;
    Some((score, matches))
}

fn filter_mentions(query: &str) -> Vec<(&'static str, Vec<usize>)> {
    if query.is_empty() {
        return FAKE_MENTIONS.iter().map(|&n| (n, Vec::new())).collect();
    }
    let mut scored: Vec<(i32, &'static str, Vec<usize>)> = FAKE_MENTIONS
        .iter()
        .filter_map(|&name| fuzzy_score(query, name).map(|(s, m)| (s, name, m)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, n, m)| (n, m)).collect()
}

struct MentionPopup {
    /// Cell the popup is anchored to.
    cell_id: Uuid,
    /// For outline cells, the specific bullet's id. None for plain cells.
    bullet_id: Option<Uuid>,
    /// Byte position of the '@' in the active textbox.
    anchor_byte: usize,
    /// Currently typed query (text after the '@', no whitespace).
    query: String,
    /// Index of the highlighted item in the filtered list.
    selected: usize,
}

/// What the user is viewing in the doc area / highlighting in the sidebar.
/// Decoupled from the writable-target context (which is always the most
/// recent open one).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ViewSelection {
    /// A specific context's window (open or closed).
    Context(Uuid),
    /// All cells whose local date matches `d`, with context-section headers
    /// rendered between groups.
    Date(chrono::NaiveDate),
}

/// Context-level consequence of a cell deletion that emptied a context's
/// window. Stored on `UndoOp::DeleteCell` so undo/redo restore atomically.
#[derive(Clone)]
enum ContextSideEffect {
    /// A closed context became empty and was removed. Undo restores it.
    ContextRemoved {
        context: Context,
        prev_view: ViewSelection,
        new_view: ViewSelection,
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
        prev_view: ViewSelection,
        new_view: ViewSelection,
        pre_focused: Option<Uuid>,
        pre_scroll_y: f32,
    },
    /// Rotation on an already-empty active context: bumps the context's
    /// `start_time` to "now" instead of creating another empty context.
    ResetContextStart {
        context_id: Uuid,
        prev_start: i64,
        new_start: i64,
        prev_view: ViewSelection,
        new_view: ViewSelection,
    },
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

#[derive(Clone)]
pub struct Context {
    pub id: Uuid,
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub title: Option<String>,
}

const SEED_CONTEXT_ID: Uuid = uuid!("01900000-0000-7000-8000-000000000001");
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Idle threshold after which the next cell creation rotates to a fresh
/// context. Edits to existing cells don't reset this — only new cells do.
const IDLE_CONTEXT_THRESHOLD: Duration = Duration::from_secs(15 * 60);

const SIDEBAR_WIDTH: f32 = 180.0;
const SIDEBAR_PAD_X: f32 = 12.0;
const SIDEBAR_PAD_TOP: f32 = 18.0;
const SIDEBAR_HEADER_H: f32 = 28.0;
const SIDEBAR_DATE_H: f32 = 28.0;
const SIDEBAR_ITEM_H: f32 = 26.0;
const SIDEBAR_ITEM_GAP: f32 = 2.0;
const SIDEBAR_DATE_GAP: f32 = 6.0;
const SIDEBAR_INDENT: f32 = 14.0;
const SIDEBAR_ITEM_RADIUS: f32 = 6.0;
const SIDEBAR_HEADER_FONT_SIZE: f32 = 11.0;
const SIDEBAR_DATE_FONT_SIZE: f32 = 13.0;
const SIDEBAR_ITEM_FONT_SIZE: f32 = 12.0;

/// Context-section header drawn in Date view between cell groups from
/// different contexts. `H` covers `PAD_TOP + text + PAD_BOTTOM`. The bottom
/// pad must clear `FOCUS_PAD` on the cell that follows, plus breathing room.
const CONTEXT_HEADER_H: f32 = 50.0;
const CONTEXT_HEADER_PAD_TOP: f32 = 14.0;
const CONTEXT_HEADER_FONT_SIZE: f32 = 11.0;

const KEBAB_SIZE: f32 = 22.0;
const KEBAB_INSET_X: f32 = 4.0;
const KEBAB_INSET_Y: f32 = 2.0;
const KEBAB_DOT_RADIUS: f32 = 1.6;
/// Width reserved on the right of each cell so the kebab sits *outside* the
/// focused-cell card. Must accommodate `FOCUS_PAD` (card outset) + gap +
/// `KEBAB_SIZE` + `KEBAB_INSET_X`.
const KEBAB_RESERVE: f32 = FOCUS_PAD + 4.0 + KEBAB_SIZE + KEBAB_INSET_X;
const CELL_MENU_WIDTH: f32 = 280.0;
const CELL_MENU_HEIGHT: f32 = 64.0;

pub struct KeptApp {
    typeface: Typeface,
    /// Global, append-only stream of cells. Source of truth.
    /// Always sorted ascending by `Cell.timestamp`.
    cells: Vec<Cell>,
    /// Time-window overlays. Membership is derived (timestamp-based), not stored.
    contexts: Vec<Context>,
    /// What the user is currently viewing (single context or a whole date).
    /// The "writable target" is always the most recent open context, derived
    /// on demand via `writable_context_id` — independent of this field.
    view: ViewSelection,
    focused: Option<Uuid>,
    /// Modal state. `false` = view (cell is selected but not accepting text);
    /// `true` = edit (caret visible, text input forwarded). Toggled by Enter
    /// (view → edit) and Esc (edit → view). Any focus change drops to view
    /// mode except creation (Ctrl+Enter) and clicks, which enter edit mode.
    editing: bool,
    dragging_cell: Option<Uuid>,
    scroll_y: f32,
    max_scroll: f32,
    doc_height: f32,
    viewport_height: f32,
    last_scroll_time: Option<Instant>,
    font_scale: f32,
    pending_caret_scroll: bool,
    undo_stack: Vec<UndoOp>,
    redo_stack: Vec<UndoOp>,
    last_edit_time: Option<Instant>,
    coalesce_break: bool,
    mention_popup: Option<MentionPopup>,
    clipboard: Option<Clipboard>,
    db: Option<Db>,
    dirty_cells: HashSet<Uuid>,
    pending_deletes: HashSet<Uuid>,
    dirty_contexts: HashSet<Uuid>,
    pending_context_deletes: HashSet<Uuid>,
    cell_menu_open: Option<Uuid>,
    last_kebab_rects: Vec<(Uuid, Rect)>,
    /// Sidebar context-row rects (window coords) from last frame, for hit-testing.
    last_sidebar_rects: Vec<(Uuid, Rect)>,
    /// Sidebar date-header rects from last frame, for hit-testing.
    last_sidebar_date_rects: Vec<(chrono::NaiveDate, Rect)>,
    /// Most recent cursor position in window (logical) coords, used for hover.
    mouse_pos: (f32, f32),
}

impl KeptApp {
    pub fn new() -> Self {
        let typeface = FontMgr::new()
            .new_from_data(FONT_BYTES, None)
            .expect("failed to load embedded TTF");

        let path = db_path();
        let mut db = match Db::open(&path) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("kept: failed to open DB at {:?}: {e}", path);
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
        // Pick the most recent context. Prefer an open one; among ties, the
        // one with the latest start. `(end_time.is_none(), start_time)` works
        // because bool sorts false < true.
        let initial_context = contexts
            .iter()
            .max_by_key(|c| (c.end_time.is_none(), c.start_time))
            .map(|c| c.id)
            .expect("at least one context exists after seeding");
        let view = ViewSelection::Context(initial_context);

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
            typeface,
            cells,
            contexts,
            view,
            focused,
            editing: false,
            dragging_cell: None,
            scroll_y: 0.0,
            max_scroll: 0.0,
            doc_height: 0.0,
            viewport_height: 0.0,
            last_scroll_time: None,
            font_scale: 1.0,
            pending_caret_scroll: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit_time: None,
            coalesce_break: false,
            mention_popup: None,
            clipboard: Clipboard::new().ok(),
            db,
            dirty_cells: HashSet::new(),
            pending_deletes: HashSet::new(),
            dirty_contexts: HashSet::new(),
            pending_context_deletes: HashSet::new(),
            cell_menu_open: None,
            last_kebab_rects: Vec::new(),
            last_sidebar_rects: Vec::new(),
            last_sidebar_date_rects: Vec::new(),
            mouse_pos: (-1.0, -1.0),
        }
    }

    pub fn cursor_moved(&mut self, x: f32, y: f32) {
        self.mouse_pos = (x, y);
    }

    // ----- cell access helpers -----

    fn cell_idx(&self, id: Uuid) -> Option<usize> {
        self.cells.iter().position(|c| c.id == id)
    }

    fn cell(&self, id: Uuid) -> Option<&Cell> {
        self.cells.iter().find(|c| c.id == id)
    }

    fn cell_mut(&mut self, id: Uuid) -> Option<&mut Cell> {
        self.cells.iter_mut().find(|c| c.id == id)
    }

    /// Most recent open context's id — the writable target. Always Some in
    /// normal operation (the rotation/seed logic preserves the invariant).
    fn writable_context_id(&self) -> Option<Uuid> {
        self.contexts
            .iter()
            .filter(|c| c.end_time.is_none())
            .max_by_key(|c| c.start_time)
            .map(|c| c.id)
    }

    /// Test whether a cell with `cell_ts` is visible under the current view.
    fn is_visible_for_view(&self, cell_ts: i64) -> bool {
        match self.view {
            ViewSelection::Context(id) => {
                self.contexts.iter().find(|c| c.id == id).map_or(false, |c| {
                    cell_ts >= c.start_time && c.end_time.map_or(true, |e| cell_ts < e)
                })
            }
            ViewSelection::Date(d) => local_date_for_ms(cell_ts) == d,
        }
    }

    /// Find the context whose window contains `cell_ts`. Used for rendering
    /// per-context section headers in Date view.
    fn context_for_timestamp(&self, cell_ts: i64) -> Option<&Context> {
        self.contexts.iter().find(|c| {
            cell_ts >= c.start_time && c.end_time.map_or(true, |e| cell_ts < e)
        })
    }

    /// Timestamp (epoch ms) of the most recently created cell anywhere in the
    /// stream, used for idle detection. None if no cells exist.
    fn last_cell_create_ms(&self) -> Option<i64> {
        self.cells.iter().map(|c| c.timestamp).max()
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
        let prev_view = self.view;

        // Empty writable: bump its start_time instead of creating a new context.
        if !self.writable_has_cells() {
            let prev_start = self
                .contexts
                .iter()
                .find(|c| c.id == writable)
                .map(|c| c.start_time)
                .unwrap_or(now);
            if prev_start == now {
                return;
            }
            if let Some(ctx) = self.contexts.iter_mut().find(|c| c.id == writable) {
                ctx.start_time = now;
            }
            self.dirty_contexts.insert(writable);
            // View update: Date view stays. Context view focuses the bumped one.
            let new_view = match prev_view {
                ViewSelection::Date(d) => ViewSelection::Date(d),
                ViewSelection::Context(_) => ViewSelection::Context(writable),
            };
            self.view = new_view;
            self.undo_stack.push(UndoOp::ResetContextStart {
                context_id: writable,
                prev_start,
                new_start: now,
                prev_view,
                new_view,
            });
            self.redo_stack.clear();
            self.coalesce_break = true;
            return;
        }

        let prev_end_time = self
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
        let pre_focused = self.focused;
        let pre_scroll_y = self.scroll_y;
        let new_view = match prev_view {
            ViewSelection::Date(d) => ViewSelection::Date(d),
            ViewSelection::Context(_) => ViewSelection::Context(new_context.id),
        };

        self.apply_rotation(writable, now, &new_context, new_view);

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
        self.coalesce_break = true;
    }

    /// Does the writable (most-recent-open) context have any cells in its window?
    fn writable_has_cells(&self) -> bool {
        let Some(id) = self.writable_context_id() else {
            return false;
        };
        let Some(ctx) = self.contexts.iter().find(|c| c.id == id) else {
            return false;
        };
        let start = ctx.start_time;
        let end = ctx.end_time;
        self.cells
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
        new_view: ViewSelection,
    ) {
        if let Some(ctx) = self.contexts.iter_mut().find(|c| c.id == closed_id) {
            ctx.end_time = Some(new_end_time);
        }
        self.dirty_contexts.insert(closed_id);
        let new_id = new_context.id;
        if !self.contexts.iter().any(|c| c.id == new_id) {
            self.contexts.push(new_context.clone());
        }
        self.dirty_contexts.insert(new_id);
        self.pending_context_deletes.remove(&new_id);
        self.view = new_view;
        self.focused = None;
        self.editing = false;
        self.dragging_cell = None;
        self.cell_menu_open = None;
        self.scroll_y = 0.0;
    }

    /// Inverse of `apply_rotation`: restore the closed context's `end_time`,
    /// remove the new context (queue for DB deletion), restore prior view.
    fn inverse_rotation(
        &mut self,
        closed_id: Uuid,
        prev_end_time: Option<i64>,
        new_context_id: Uuid,
        prev_view: ViewSelection,
        pre_focused: Option<Uuid>,
        pre_scroll_y: f32,
    ) {
        if let Some(ctx) = self.contexts.iter_mut().find(|c| c.id == closed_id) {
            ctx.end_time = prev_end_time;
        }
        self.dirty_contexts.insert(closed_id);
        self.contexts.retain(|c| c.id != new_context_id);
        self.dirty_contexts.remove(&new_context_id);
        self.pending_context_deletes.insert(new_context_id);
        self.view = prev_view;
        self.focused = pre_focused;
        self.editing = false;
        self.dragging_cell = None;
        self.cell_menu_open = None;
        self.scroll_y = pre_scroll_y;
    }

    /// Previous context (older `start_time`) relative to the currently
    /// viewed one. None when in Date view (use date-arrow nav for that).
    fn prev_context(&self) -> Option<Uuid> {
        let current = match self.view {
            ViewSelection::Context(id) => id,
            ViewSelection::Date(_) => return None,
        };
        let mut sorted: Vec<&Context> = self.contexts.iter().collect();
        sorted.sort_by_key(|c| c.start_time);
        let pos = sorted.iter().position(|c| c.id == current)?;
        if pos == 0 {
            None
        } else {
            Some(sorted[pos - 1].id)
        }
    }

    /// Next context (newer `start_time`). None when in Date view.
    fn next_context(&self) -> Option<Uuid> {
        let current = match self.view {
            ViewSelection::Context(id) => id,
            ViewSelection::Date(_) => return None,
        };
        let mut sorted: Vec<&Context> = self.contexts.iter().collect();
        sorted.sort_by_key(|c| c.start_time);
        let pos = sorted.iter().position(|c| c.id == current)?;
        sorted.get(pos + 1).map(|c| c.id)
    }

    /// In Context view: if the viewed context is closed, switch to the most
    /// recent open one so writes land in "today." In Date view: no-op (writes
    /// still go to the writable context, which is found at insertion time).
    /// Returns true if the view changed.
    fn ensure_writable_context(&mut self) -> bool {
        let id = match self.view {
            ViewSelection::Context(id) => id,
            ViewSelection::Date(_) => return false,
        };
        let active_is_open = self
            .contexts
            .iter()
            .find(|c| c.id == id)
            .map_or(false, |c| c.end_time.is_none());
        if active_is_open {
            return false;
        }
        let target = self.writable_context_id();
        match target {
            Some(target_id) => self.set_active_context(target_id),
            None => false,
        }
    }

    /// Switch the view to a single existing context.
    fn set_active_context(&mut self, id: Uuid) -> bool {
        if self.view == ViewSelection::Context(id) {
            return false;
        }
        if !self.contexts.iter().any(|c| c.id == id) {
            return false;
        }
        self.view = ViewSelection::Context(id);
        // Focus the first visible cell in the new window (if any).
        self.focused = self.visible_cell_ids().first().copied();
        self.editing = false;
        self.dragging_cell = None;
        self.cell_menu_open = None;
        self.scroll_y = 0.0;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    /// Switch the view to "everything from this local date" mode.
    fn set_active_date(&mut self, d: chrono::NaiveDate) -> bool {
        if self.view == ViewSelection::Date(d) {
            return false;
        }
        self.view = ViewSelection::Date(d);
        self.focused = self.visible_cell_ids().first().copied();
        self.editing = false;
        self.dragging_cell = None;
        self.cell_menu_open = None;
        self.scroll_y = 0.0;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    /// IDs of cells visible under the active view, in timestamp order.
    fn visible_cell_ids(&self) -> Vec<Uuid> {
        self.cells
            .iter()
            .filter(|c| self.is_visible_for_view(c.timestamp))
            .map(|c| c.id)
            .collect()
    }

    /// Insert a cell into the stream maintaining ascending timestamp order.
    fn insert_cell_sorted(&mut self, cell: Cell) {
        let pos = self
            .cells
            .binary_search_by(|c| c.timestamp.cmp(&cell.timestamp).then(c.id.cmp(&cell.id)));
        let at = match pos {
            Ok(i) => i,
            Err(i) => i,
        };
        self.cells.insert(at, cell);
    }

    fn mark_cell_dirty(&mut self, id: Uuid) {
        self.dirty_cells.insert(id);
    }

    fn touch_cell(&mut self, id: Uuid) {
        let now = now_epoch_ms();
        if let Some(cell) = self.cell_mut(id) {
            cell.edited_at = now;
        }
        self.mark_cell_dirty(id);
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
        let Some(db) = self.db.as_mut() else {
            self.dirty_cells.clear();
            self.pending_deletes.clear();
            self.dirty_contexts.clear();
            self.pending_context_deletes.clear();
            return;
        };
        for id in self.pending_deletes.drain() {
            if let Err(e) = db.delete_cell(id) {
                eprintln!("kept: delete_cell failed for {id}: {e}");
            }
        }
        let dirty: Vec<Uuid> = self.dirty_cells.drain().collect();
        for id in dirty {
            if let Some(cell) = self.cells.iter().find(|c| c.id == id) {
                if let Err(e) = db.save_cell(cell) {
                    eprintln!("kept: save_cell failed for {id}: {e}");
                }
            }
        }
        for id in self.pending_context_deletes.drain() {
            if let Err(e) = db.delete_context(id) {
                eprintln!("kept: delete_context failed for {id}: {e}");
            }
        }
        let ctx_dirty: Vec<Uuid> = self.dirty_contexts.drain().collect();
        for id in ctx_dirty {
            if let Some(ctx) = self.contexts.iter().find(|c| c.id == id) {
                if let Err(e) = db.save_context(&context_ref(ctx)) {
                    eprintln!("kept: save_context failed for {id}: {e}");
                }
            }
        }
    }

    fn set_font_scale(&mut self, scale: f32) -> bool {
        let s = scale.clamp(ZOOM_MIN, ZOOM_MAX);
        if (s - self.font_scale).abs() < f32::EPSILON {
            return false;
        }
        self.font_scale = s;
        for cell in &mut self.cells {
            cell.set_font_scale(s);
        }
        self.pending_caret_scroll = true;
        true
    }

    fn zoom_in(&mut self) -> bool {
        self.set_font_scale(self.font_scale * ZOOM_STEP)
    }

    fn zoom_out(&mut self) -> bool {
        self.set_font_scale(self.font_scale / ZOOM_STEP)
    }

    pub fn scroll_by(&mut self, dy: f32) -> bool {
        let new_y = (self.scroll_y + dy).clamp(0.0, self.max_scroll);
        if new_y == self.scroll_y {
            return false;
        }
        self.scroll_y = new_y;
        self.last_scroll_time = Some(Instant::now());
        // Scrolling dismisses the per-cell menu (anchored in doc coords; would
        // visually decouple from its kebab if left open during a scroll).
        self.cell_menu_open = None;
        true
    }

    pub fn tick(&mut self, canvas: &Canvas, width: f32, height: f32) {
        canvas.clear(Color::from_rgb(0xfa, 0xf7, 0xf2));

        // Clamp scroll using last frame's max_scroll before drawing this frame.
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll);

        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        // Document space — translate so doc y=0 lands at window y = -scroll_y.
        canvas.save();
        canvas.translate((0.0, -self.scroll_y));

        // Capture focused-cell geometry up front. The card backdrop (drawn
        // *before* cell content) and the focus ring (drawn after) both use
        // this so they stay in lockstep — at most one frame of lag when the
        // cell grows from typing, but they always match each other.
        let focused_geom = self
            .focused
            .and_then(|id| self.cell(id))
            .filter(|c| c.height() > 0.0)
            .map(|c| (c.x_origin(), c.y_origin(), c.width(), c.height()));

        if let Some((cx, cy, cw, ch)) = focused_geom {
            let card_rect = Rect::new(
                cx - FOCUS_PAD,
                cy - FOCUS_PAD,
                cx + cw + FOCUS_PAD,
                cy + ch + FOCUS_PAD,
            );
            // Drop shadow: blurred dark rect, offset down a few px.
            let mut shadow_paint = Paint::default();
            shadow_paint.set_anti_alias(true);
            shadow_paint.set_color(Color::from_argb(FOCUS_SHADOW_ALPHA, 0, 0, 0));
            shadow_paint.set_mask_filter(MaskFilter::blur(
                BlurStyle::Normal,
                FOCUS_SHADOW_BLUR,
                false,
            ));
            let shadow_rect = Rect::new(
                card_rect.left,
                card_rect.top + FOCUS_SHADOW_DY,
                card_rect.right,
                card_rect.bottom + FOCUS_SHADOW_DY,
            );
            canvas.draw_round_rect(shadow_rect, FOCUS_RADIUS, FOCUS_RADIUS, &shadow_paint);
            // White card fill.
            let mut fill_paint = Paint::default();
            fill_paint.set_anti_alias(true);
            fill_paint.set_color(Color::WHITE);
            canvas.draw_round_rect(card_rect, FOCUS_RADIUS, FOCUS_RADIUS, &fill_paint);
        }

        let mut y = MARGIN_TOP;
        let cells_left = SIDEBAR_WIDTH * self.font_scale + MARGIN_X;
        let outer_cell_width = (width - cells_left - MARGIN_X).max(80.0);
        let content_width = (outer_cell_width - KEBAB_RESERVE).max(60.0);
        self.last_kebab_rects.clear();
        let mouse_doc_x = self.mouse_pos.0;
        let mouse_doc_y = self.mouse_pos.1 + self.scroll_y;
        let focused_id = self.focused;

        // Precompute per-cell visibility and section headers (Date view only)
        // so the mutable cell loop below doesn't have to re-borrow self.
        let visible: Vec<bool> = self
            .cells
            .iter()
            .map(|c| self.is_visible_for_view(c.timestamp))
            .collect();
        let in_date_view = matches!(self.view, ViewSelection::Date(_));
        let headers: Vec<Option<String>> = if in_date_view {
            let mut hs: Vec<Option<String>> = Vec::with_capacity(self.cells.len());
            let mut last_id: Option<Uuid> = None;
            for (i, cell) in self.cells.iter().enumerate() {
                if !visible[i] {
                    hs.push(None);
                    continue;
                }
                let ctx = self.context_for_timestamp(cell.timestamp);
                let ctx_id = ctx.map(|c| c.id);
                let label = if ctx_id != last_id {
                    last_id = ctx_id;
                    ctx.map(|c| format_context_time(c.start_time))
                } else {
                    None
                };
                hs.push(label);
            }
            hs
        } else {
            vec![None; self.cells.len()]
        };

        let scale = self.font_scale;
        let header_font =
            Font::from_typeface(&self.typeface, CONTEXT_HEADER_FONT_SIZE * scale);
        let (_, hm) = header_font.metrics();
        let header_h = CONTEXT_HEADER_H * scale;
        let header_pad_top = CONTEXT_HEADER_PAD_TOP * scale;

        for (i, cell) in self.cells.iter_mut().enumerate() {
            if !visible[i] {
                continue;
            }
            if let Some(label) = &headers[i] {
                let header_y = y + header_pad_top;
                let baseline = header_y + (-hm.ascent);
                let mut hp = Paint::default();
                hp.set_anti_alias(true);
                hp.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
                canvas.draw_str(
                    label,
                    Point::new(cells_left, baseline),
                    &header_font,
                    &hp,
                );
                let label_w = header_font.measure_str(label, Some(&hp)).0;
                let line_y = baseline - hm.ascent / 3.0;
                let mut lp = Paint::default();
                lp.set_anti_alias(true);
                lp.set_color(Color::from_argb(0x40, 0x90, 0x88, 0x7a));
                lp.set_stroke_width(1.0);
                canvas.draw_line(
                    Point::new(cells_left + label_w + 8.0 * scale, line_y),
                    Point::new(cells_left + outer_cell_width, line_y),
                    &lp,
                );
                y += header_h;
            }
            let cell_x = cells_left;
            let cell_y = y;
            let cell_is_focused = focused_id.map(|f| f == cell.id).unwrap_or(false);
            // Selection highlights are visible whenever the cell is focused
            // (so view-mode users can drag-select). Caret only renders in
            // edit mode.
            let render_focused = cell_is_focused;
            let show_caret = cell_is_focused && self.editing;
            let h = cell.tick(
                canvas,
                cell_x,
                cell_y,
                content_width,
                render_focused,
                show_caret,
            );
            let kebab_right = cell_x + outer_cell_width - KEBAB_INSET_X;
            let kebab_left = kebab_right - KEBAB_SIZE;
            let kebab_top = cell_y + KEBAB_INSET_Y;
            let kebab_bot = kebab_top + KEBAB_SIZE;
            let kebab_rect = Rect::new(kebab_left, kebab_top, kebab_right, kebab_bot);
            let hovered = mouse_doc_x >= kebab_rect.left
                && mouse_doc_x <= kebab_rect.right
                && mouse_doc_y >= kebab_rect.top
                && mouse_doc_y <= kebab_rect.bottom;
            draw_kebab(canvas, kebab_rect, hovered);
            self.last_kebab_rects.push((cell.id, kebab_rect));
            y += h + CELL_GAP;
        }

        // Focus ring — subtle when viewing, brighter and thicker when editing.
        if let Some((cx, cy, cw, ch)) = focused_geom {
            let (stroke, alpha) = if self.editing {
                (FOCUS_STROKE_EDIT, FOCUS_RING_ALPHA_EDIT)
            } else {
                (FOCUS_STROKE, FOCUS_RING_ALPHA)
            };
            let mut focus_paint = Paint::default();
            focus_paint.set_anti_alias(true);
            focus_paint.set_style(PaintStyle::Stroke);
            focus_paint.set_stroke_width(stroke);
            focus_paint.set_color(Color::from_argb(alpha, 0x4a, 0x90, 0xe2));
            let rect = Rect::new(
                cx - FOCUS_PAD,
                cy - FOCUS_PAD,
                cx + cw + FOCUS_PAD,
                cy + ch + FOCUS_PAD,
            );
            canvas.draw_round_rect(rect, FOCUS_RADIUS, FOCUS_RADIUS, &focus_paint);
        }

        if let Some(id) = self.cell_menu_open {
            self.render_cell_menu(canvas, id);
        }

        self.render_mention_popup(canvas);

        canvas.restore();

        // Update bookkeeping for scroll math + clamp again in case content shrank.
        self.doc_height = y - CELL_GAP + DOC_BOTTOM_PAD;
        self.viewport_height = height.max(0.0);
        self.max_scroll = (self.doc_height - self.viewport_height).max(0.0);
        self.scroll_y = self.scroll_y.min(self.max_scroll);

        // After cells are laid out (y_origin/height fresh), honor any caret-into-view
        // request from this tick's events. Effect lands on the next frame.
        if std::mem::take(&mut self.pending_caret_scroll) {
            self.scroll_caret_into_view();
        }

        // Debounced persistence: if anything dirty and user has been idle
        // for SAVE_DEBOUNCE, flush.
        let any_dirty = !self.dirty_cells.is_empty()
            || !self.pending_deletes.is_empty()
            || !self.dirty_contexts.is_empty()
            || !self.pending_context_deletes.is_empty();
        if any_dirty {
            let idle = self
                .last_edit_time
                .map(|t| t.elapsed() >= SAVE_DEBOUNCE)
                .unwrap_or(true);
            if idle {
                self.flush_persistence();
            }
        }

        // Scrollbar lives in window coords (no translate), so it doesn't scroll.
        if self.max_scroll > 0.0 {
            let alpha = scrollbar_alpha(self.last_scroll_time);
            if alpha > 0.0 {
                let track_top = 6.0_f32;
                let track_bot = self.viewport_height - 6.0;
                let track_len = (track_bot - track_top).max(1.0);
                let raw_thumb = (self.viewport_height / self.doc_height) * track_len;
                let thumb_h = raw_thumb.max(SCROLLBAR_MIN_THUMB).min(track_len);
                let thumb_top = track_top
                    + (self.scroll_y / self.max_scroll) * (track_len - thumb_h);
                let thumb_bot = thumb_top + thumb_h;
                let bar_x = width - SCROLLBAR_INSET - SCROLLBAR_WIDTH;

                let mut sb_paint = Paint::default();
                sb_paint.set_anti_alias(true);
                let alpha_byte = (alpha * 0xb0 as f32).round() as u8;
                sb_paint.set_color(Color::from_argb(alpha_byte, 0x1c, 0x1c, 0x1c));
                let r = SCROLLBAR_WIDTH * 0.5;
                canvas.draw_round_rect(
                    Rect::new(bar_x, thumb_top, bar_x + SCROLLBAR_WIDTH, thumb_bot),
                    r,
                    r,
                    &sb_paint,
                );
            }
        }

        // Sidebar (window space — does not scroll with content).
        self.render_sidebar(canvas, height);
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        // Esc closes the cell menu first if it's open.
        if event.state == ElementState::Pressed
            && self.cell_menu_open.is_some()
            && matches!(event.logical_key, Key::Named(NamedKey::Escape))
        {
            self.cell_menu_open = None;
            return true;
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
                    // Selection commit is non-functional for now — just dismiss.
                    self.mention_popup = None;
                    return true;
                }
                _ => {}
            }
        }

        if event.state == ElementState::Pressed && modifiers.state().control_key() {
            match &event.logical_key {
                Key::Named(NamedKey::Enter) => {
                    let outline = modifiers.state().shift_key();
                    return self.insert_cell_after_focused(outline);
                }
                Key::Named(NamedKey::Delete) => {
                    return self.delete_focused_cell();
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
                    if let Some(focused) = self.focused {
                        if let Some(prev) = self.prev_visible(focused) {
                            self.focused = Some(prev);
                            self.editing = false;
                            self.coalesce_break = true;
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
                    if let Some(focused) = self.focused {
                        if let Some(next) = self.next_visible(focused) {
                            self.focused = Some(next);
                            self.editing = false;
                            self.coalesce_break = true;
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
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("c") => {
                    return self.copy_to_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("x") => {
                    return self.cut_to_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("v") => {
                    return self.paste_from_clipboard();
                }
                Key::Character(s)
                    if modifiers.state().shift_key() && s.as_str().eq_ignore_ascii_case("n") =>
                {
                    self.rotate_context_now();
                    return true;
                }
                _ => {}
            }
        }

        // Modal mode switches: Esc exits edit, Enter enters edit.
        if event.state == ElementState::Pressed
            && !modifiers.state().control_key()
            && !modifiers.state().alt_key()
        {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) if self.editing => {
                    self.editing = false;
                    self.mention_popup = None;
                    self.coalesce_break = true;
                    return true;
                }
                Key::Named(NamedKey::Enter)
                    if !self.editing
                        && !modifiers.state().shift_key()
                        && self.focused.is_some() =>
                {
                    self.editing = true;
                    // Position caret at the end so typing appends to the cell.
                    if let Some(id) = self.focused {
                        if let Some(c) = self.cell_mut(id) {
                            c.place_caret_at_end();
                        }
                    }
                    self.pending_caret_scroll = true;
                    return true;
                }
                _ => {}
            }
        }

        // View mode: cell-level operations only. Text input is dropped —
        // Enter is the way back to edit; Backspace/Delete delete the cell.
        if !self.editing {
            if event.state == ElementState::Pressed
                && !modifiers.state().shift_key()
                && !modifiers.state().control_key()
                && !modifiers.state().alt_key()
            {
                match &event.logical_key {
                    Key::Named(NamedKey::ArrowUp) => {
                        if let Some(focused) = self.focused {
                            if let Some(prev) = self.prev_visible(focused) {
                                self.focused = Some(prev);
                                self.coalesce_break = true;
                                self.scroll_to_focused();
                                return true;
                            }
                        }
                        return false;
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        if let Some(focused) = self.focused {
                            if let Some(next) = self.next_visible(focused) {
                                self.focused = Some(next);
                                self.coalesce_break = true;
                                self.scroll_to_focused();
                                return true;
                            }
                        }
                        return false;
                    }
                    Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete) => {
                        return self.delete_focused_cell();
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
            && !modifiers.state().control_key()
            && !modifiers.state().alt_key()
        {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowUp) => {
                    if let Some(focused) = self.focused {
                        let at_top = self.cell(focused).map_or(false, |c| c.at_top_edge());
                        if at_top {
                            if let Some(prev) = self.prev_visible(focused) {
                                self.focused = Some(prev);
                                if let Some(c) = self.cell_mut(prev) {
                                    c.place_caret_at_end();
                                }
                                self.editing = false;
                                self.coalesce_break = true;
                                self.pending_caret_scroll = true;
                                return true;
                            }
                        }
                    }
                }
                Key::Named(NamedKey::ArrowDown) => {
                    if let Some(focused) = self.focused {
                        let at_bot = self.cell(focused).map_or(false, |c| c.at_bottom_edge());
                        if at_bot {
                            if let Some(next) = self.next_visible(focused) {
                                self.focused = Some(next);
                                if let Some(c) = self.cell_mut(next) {
                                    c.place_caret_at_start();
                                }
                                self.editing = false;
                                self.coalesce_break = true;
                                self.pending_caret_scroll = true;
                                return true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let focused_id = self.focused;
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
                        self.coalesce_break = true;
                    }
                }
            }
            self.pending_caret_scroll = true;

            // Maybe open the @-mention popup (if user just typed '@'), then
            // sync against the current text+caret state.
            if !popup_was_open && event.text.as_deref() == Some("@") {
                self.try_open_mention_popup();
            }
            self.sync_mention_popup();
        }
        handled
    }

    fn try_open_mention_popup(&mut self) {
        let Some(focused_id) = self.focused else {
            return;
        };
        let Some(cell) = self.cell(focused_id) else {
            return;
        };
        let Some((text, caret)) = cell.focused_text_and_caret() else {
            return;
        };
        if caret == 0 {
            return;
        }
        // Caret should be just past the '@'.
        if text.get(caret - 1..caret) != Some("@") {
            return;
        }
        self.mention_popup = Some(MentionPopup {
            cell_id: focused_id,
            bullet_id: cell.focused_bullet_id(),
            anchor_byte: caret - 1,
            query: String::new(),
            selected: 0,
        });
    }

    fn sync_mention_popup(&mut self) {
        let Some(popup) = self.mention_popup.as_ref() else {
            return;
        };
        // Cell focus must still match.
        if self.focused != Some(popup.cell_id) {
            self.mention_popup = None;
            return;
        }
        let cell_id = popup.cell_id;
        let bullet_id = popup.bullet_id;
        let anchor_byte = popup.anchor_byte;
        let query: String = {
            let Some(cell) = self.cell(cell_id) else {
                self.mention_popup = None;
                return;
            };
            // Bullet must still match (outline only).
            if cell.focused_bullet_id() != bullet_id {
                self.mention_popup = None;
                return;
            }
            let Some((text, caret)) = cell.focused_text_and_caret() else {
                self.mention_popup = None;
                return;
            };
            // The '@' must still be at anchor_byte.
            if text.get(anchor_byte..).map_or(true, |s| !s.starts_with('@')) {
                self.mention_popup = None;
                return;
            }
            // Caret must be at or past the '@' itself.
            if caret < anchor_byte + 1 {
                self.mention_popup = None;
                return;
            }
            // Query is everything between the '@' and the caret. Whitespace breaks it.
            let Some(q) = text.get(anchor_byte + 1..caret) else {
                self.mention_popup = None;
                return;
            };
            if q.chars().any(|c| c.is_whitespace()) {
                self.mention_popup = None;
                return;
            }
            q.to_string()
        };
        if let Some(p) = self.mention_popup.as_mut() {
            let count = filter_mentions(&query).len().min(MENTION_POPUP_MAX_VISIBLE);
            p.query = query;
            if count == 0 {
                p.selected = 0;
            } else if p.selected >= count {
                p.selected = count - 1;
            }
        }
    }

    fn copy_to_clipboard(&mut self) -> bool {
        let Some(id) = self.focused else { return false };
        let mut text = self.cell(id).map(|c| c.copy_text()).unwrap_or_default();
        // View mode + no selection → copy the whole cell. Edit mode keeps
        // the selection-or-nothing behavior so an accidental Ctrl+C with no
        // selection doesn't dump the whole cell.
        if text.is_empty() && !self.editing {
            text = self.cell(id).map(|c| c.full_text()).unwrap_or_default();
        }
        if text.is_empty() {
            return false;
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text);
        }
        true
    }

    fn cut_to_clipboard(&mut self) -> bool {
        let Some(id) = self.focused else { return false };
        let pre = self.cell(id).map(|c| c.snapshot());
        let cut = match self.cell_mut(id) {
            Some(c) => c.cut_text(),
            None => return false,
        };
        if cut.is_empty() {
            return false;
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(cut);
        }
        if let (Some(pre), Some(cell)) = (pre, self.cell(id)) {
            let post = cell.snapshot();
            if !pre.doc_eq(&post) {
                self.record_edit(pre, post);
            }
        }
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    fn paste_from_clipboard(&mut self) -> bool {
        let Some(id) = self.focused else { return false };
        let Some(cb) = self.clipboard.as_mut() else {
            return false;
        };
        let text = match cb.get_text() {
            Ok(t) => t,
            Err(_) => return false,
        };
        if text.is_empty() {
            return false;
        }
        let pre = self.cell(id).map(|c| c.snapshot());
        if let Some(c) = self.cell_mut(id) {
            c.paste_text(&text);
        } else {
            return false;
        }
        if let (Some(pre), Some(cell)) = (pre, self.cell(id)) {
            let post = cell.snapshot();
            if !pre.doc_eq(&post) {
                self.record_edit(pre, post);
            }
        }
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    fn render_cell_menu(&self, canvas: &Canvas, cell_id: Uuid) {
        let Some((_, anchor)) = self
            .last_kebab_rects
            .iter()
            .find(|(i, _)| *i == cell_id)
            .copied()
        else {
            return;
        };
        let Some(cell) = self.cell(cell_id) else {
            return;
        };
        let scale = self.font_scale;
        let menu_w = CELL_MENU_WIDTH * scale;
        let menu_h = CELL_MENU_HEIGHT * scale;
        let menu_x = anchor.right - menu_w;
        let menu_y = anchor.bottom + 4.0 * scale;
        let radius = 6.0 * scale;
        let rect = Rect::new(menu_x, menu_y, menu_x + menu_w, menu_y + menu_h);

        // Drop shadow.
        let mut shadow_paint = Paint::default();
        shadow_paint.set_anti_alias(true);
        shadow_paint.set_color(Color::from_argb(0x30, 0, 0, 0));
        shadow_paint.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 8.0, false));
        canvas.draw_round_rect(
            Rect::new(rect.left, rect.top + 2.0, rect.right, rect.bottom + 2.0),
            radius,
            radius,
            &shadow_paint,
        );

        // Background.
        let mut bg_paint = Paint::default();
        bg_paint.set_anti_alias(true);
        bg_paint.set_color(Color::WHITE);
        canvas.draw_round_rect(rect, radius, radius, &bg_paint);

        // Border.
        let mut border_paint = Paint::default();
        border_paint.set_anti_alias(true);
        border_paint.set_style(PaintStyle::Stroke);
        border_paint.set_stroke_width(1.0);
        border_paint.set_color(Color::from_rgb(0xc0, 0xc0, 0xc0));
        canvas.draw_round_rect(rect, radius, radius, &border_paint);

        // Two non-selectable timestamp lines.
        let font = Font::from_typeface(&self.typeface, 13.0 * scale);
        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x70, 0x70, 0x70));
        let (_, m) = font.metrics();
        let line_step = -m.ascent + m.descent + m.leading;
        let pad_y = 10.0 * scale;
        let line1_baseline = rect.top + pad_y + (-m.ascent);
        let line2_baseline = line1_baseline + line_step;
        let pad_x = 12.0 * scale;
        canvas.draw_str(
            format!("Created {}", format_timestamp(cell.timestamp)),
            Point::new(rect.left + pad_x, line1_baseline),
            &font,
            &text_paint,
        );
        canvas.draw_str(
            format!("Last edited {}", format_timestamp(cell.edited_at)),
            Point::new(rect.left + pad_x, line2_baseline),
            &font,
            &text_paint,
        );
    }

    fn render_sidebar(&mut self, canvas: &Canvas, height: f32) {
        let scale = self.font_scale;
        let sb_w = SIDEBAR_WIDTH * scale;
        let pad_x = SIDEBAR_PAD_X * scale;
        let pad_top = SIDEBAR_PAD_TOP * scale;
        let header_h = SIDEBAR_HEADER_H * scale;
        let date_h = SIDEBAR_DATE_H * scale;
        let item_h = SIDEBAR_ITEM_H * scale;
        let item_gap = SIDEBAR_ITEM_GAP * scale;
        let date_gap = SIDEBAR_DATE_GAP * scale;
        let indent = SIDEBAR_INDENT * scale;
        let radius = SIDEBAR_ITEM_RADIUS * scale;

        // Background panel.
        let mut bg_paint = Paint::default();
        bg_paint.set_anti_alias(true);
        bg_paint.set_color(Color::from_rgb(0xf2, 0xee, 0xe6));
        canvas.draw_rect(Rect::new(0.0, 0.0, sb_w, height.max(0.0)), &bg_paint);
        // Right-edge separator.
        let mut sep = Paint::default();
        sep.set_anti_alias(false);
        sep.set_color(Color::from_rgb(0xdc, 0xd4, 0xc6));
        canvas.draw_rect(
            Rect::new(sb_w - 1.0, 0.0, sb_w, height.max(0.0)),
            &sep,
        );

        // Header.
        let header_font =
            Font::from_typeface(&self.typeface, SIDEBAR_HEADER_FONT_SIZE * scale);
        let mut header_paint = Paint::default();
        header_paint.set_anti_alias(true);
        header_paint.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
        let (_, hm) = header_font.metrics();
        let header_baseline = pad_top + (-hm.ascent);
        canvas.draw_str(
            "CONTEXTS",
            Point::new(pad_x, header_baseline),
            &header_font,
            &header_paint,
        );

        // Group contexts by their local start-date. Render dates descending
        // (newest at top); within each date, contexts also descending so the
        // most recent context sits right under the date header.
        self.last_sidebar_rects.clear();
        self.last_sidebar_date_rects.clear();

        let mut groups: std::collections::BTreeMap<chrono::NaiveDate, Vec<&Context>> =
            std::collections::BTreeMap::new();
        for c in &self.contexts {
            groups
                .entry(local_date_for_ms(c.start_time))
                .or_default()
                .push(c);
        }
        let dates: Vec<chrono::NaiveDate> = groups.keys().rev().copied().collect();

        let date_font = Font::from_typeface(&self.typeface, SIDEBAR_DATE_FONT_SIZE * scale);
        let item_font = Font::from_typeface(&self.typeface, SIDEBAR_ITEM_FONT_SIZE * scale);
        let (_, dm) = date_font.metrics();
        let (_, im) = item_font.metrics();
        let mouse_x = self.mouse_pos.0;
        let mouse_y = self.mouse_pos.1;

        let mut y = pad_top + header_h;
        for d in dates {
            let mut day_contexts: Vec<&Context> = groups.get(&d).cloned().unwrap_or_default();
            day_contexts.sort_by_key(|c| std::cmp::Reverse(c.start_time));

            let date_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
            if date_rect.top > height {
                break;
            }
            let date_active = self.view == ViewSelection::Date(d);
            let date_hovered = mouse_x >= date_rect.left
                && mouse_x <= date_rect.right
                && mouse_y >= date_rect.top
                && mouse_y <= date_rect.bottom;

            if date_active {
                let mut p = Paint::default();
                p.set_anti_alias(true);
                p.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));
                canvas.draw_round_rect(date_rect, radius, radius, &p);
            } else if date_hovered {
                let mut p = Paint::default();
                p.set_anti_alias(true);
                p.set_color(Color::from_argb(0x18, 0x1c, 0x1c, 0x1c));
                canvas.draw_round_rect(date_rect, radius, radius, &p);
            }
            let mut date_paint = Paint::default();
            date_paint.set_anti_alias(true);
            date_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
            let date_baseline = y + (date_h + (-dm.ascent) - dm.descent) * 0.5;
            canvas.draw_str(
                format_date_label(d),
                Point::new(pad_x, date_baseline),
                &date_font,
                &date_paint,
            );
            self.last_sidebar_date_rects.push((d, date_rect));
            y += date_h + item_gap;

            for ctx in day_contexts {
                let item_rect = Rect::new(
                    pad_x * 0.5 + indent,
                    y,
                    sb_w - pad_x * 0.5,
                    y + item_h,
                );
                if item_rect.top > height {
                    break;
                }
                let is_active = self.view == ViewSelection::Context(ctx.id);
                let is_hovered = mouse_x >= item_rect.left
                    && mouse_x <= item_rect.right
                    && mouse_y >= item_rect.top
                    && mouse_y <= item_rect.bottom;

                if is_active {
                    let mut p = Paint::default();
                    p.set_anti_alias(true);
                    p.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));
                    canvas.draw_round_rect(item_rect, radius, radius, &p);
                } else if is_hovered {
                    let mut p = Paint::default();
                    p.set_anti_alias(true);
                    p.set_color(Color::from_argb(0x18, 0x1c, 0x1c, 0x1c));
                    canvas.draw_round_rect(item_rect, radius, radius, &p);
                }

                let mut text_paint = Paint::default();
                text_paint.set_anti_alias(true);
                let text_color = if is_active {
                    Color::from_rgb(0x1c, 0x1c, 0x1c)
                } else {
                    Color::from_rgb(0x55, 0x55, 0x55)
                };
                text_paint.set_color(text_color);
                let baseline = y + (item_h + (-im.ascent) - im.descent) * 0.5;
                let label = format_context_time(ctx.start_time);
                canvas.draw_str(
                    label,
                    Point::new(pad_x + indent, baseline),
                    &item_font,
                    &text_paint,
                );

                self.last_sidebar_rects.push((ctx.id, item_rect));
                y += item_h + item_gap;
            }
            y += date_gap;
        }
    }

    fn render_mention_popup(&self, canvas: &Canvas) {
        let Some(popup) = self.mention_popup.as_ref() else {
            return;
        };
        let Some(cell) = self.cell(popup.cell_id) else {
            return;
        };
        let Some((anchor_x, anchor_y_below)) =
            cell.anchor_doc_pos(popup.bullet_id, popup.anchor_byte)
        else {
            return;
        };

        let scale = self.font_scale;
        let popup_w = MENTION_POPUP_WIDTH * scale;
        let row_h = MENTION_POPUP_ROW_H * scale;
        let pad = MENTION_POPUP_PAD * scale;
        let radius = MENTION_POPUP_RADIUS * scale;

        let items = filter_mentions(&popup.query);
        let visible = items.len().min(MENTION_POPUP_MAX_VISIBLE);
        let popup_h = if visible == 0 {
            row_h + pad * 2.0
        } else {
            (visible as f32) * row_h + pad * 2.0
        };

        let popup_x = anchor_x;
        let popup_y = anchor_y_below + 4.0 * scale;

        // Drop shadow (drawn first, slightly offset).
        let mut shadow_paint = Paint::default();
        shadow_paint.set_anti_alias(true);
        shadow_paint.set_color(Color::from_argb(0x30, 0, 0, 0));
        canvas.draw_round_rect(
            Rect::new(
                popup_x + 1.0,
                popup_y + 2.0,
                popup_x + popup_w + 1.0,
                popup_y + popup_h + 2.0,
            ),
            radius,
            radius,
            &shadow_paint,
        );

        // Background.
        let mut bg_paint = Paint::default();
        bg_paint.set_anti_alias(true);
        bg_paint.set_color(Color::WHITE);
        let popup_rect = Rect::new(popup_x, popup_y, popup_x + popup_w, popup_y + popup_h);
        canvas.draw_round_rect(popup_rect, radius, radius, &bg_paint);

        // Border.
        let mut border_paint = Paint::default();
        border_paint.set_anti_alias(true);
        border_paint.set_style(PaintStyle::Stroke);
        border_paint.set_stroke_width(1.0);
        border_paint.set_color(Color::from_rgb(0xc0, 0xc0, 0xc0));
        canvas.draw_round_rect(popup_rect, radius, radius, &border_paint);

        let body_font = Font::from_typeface(&self.typeface, MENTION_BODY_FONT_SIZE * scale);
        let (_, m) = body_font.metrics();
        let row_text_height = -m.ascent + m.descent;
        let text_offset_in_row = (row_h - row_text_height) * 0.5 + (-m.ascent);

        if items.is_empty() {
            let mut hint_paint = Paint::default();
            hint_paint.set_anti_alias(true);
            hint_paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));
            let baseline = popup_y + pad + text_offset_in_row;
            let label = if popup.query.is_empty() {
                "Type to search…".to_string()
            } else {
                format!("No matches for \"{}\"", popup.query)
            };
            canvas.draw_str(
                label,
                Point::new(popup_x + 12.0 * scale, baseline),
                &body_font,
                &hint_paint,
            );
            return;
        }

        let mut dim_paint = Paint::default();
        dim_paint.set_anti_alias(true);
        dim_paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));

        let mut match_paint = Paint::default();
        match_paint.set_anti_alias(true);
        match_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        let mut hl_paint = Paint::default();
        hl_paint.set_anti_alias(true);
        hl_paint.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));

        let selected = popup.selected.min(visible - 1);
        let mut row_y = popup_y + pad;
        for (i, (item, matches)) in items.iter().take(visible).enumerate() {
            if i == selected {
                canvas.draw_round_rect(
                    Rect::new(
                        popup_x + 4.0 * scale,
                        row_y,
                        popup_x + popup_w - 4.0 * scale,
                        row_y + row_h,
                    ),
                    4.0 * scale,
                    4.0 * scale,
                    &hl_paint,
                );
            }
            let baseline = row_y + text_offset_in_row;
            let text_x = popup_x + 12.0 * scale;
            // Render '@' in dim, then alternate dim / match-paint runs.
            let at_w = body_font.measure_str("@", Some(&dim_paint)).0;
            canvas.draw_str("@", Point::new(text_x, baseline), &body_font, &dim_paint);
            draw_runs_with_matches(
                canvas,
                item,
                matches,
                Point::new(text_x + at_w, baseline),
                &body_font,
                &match_paint,
                &dim_paint,
            );
            row_y += row_h;
        }
    }

    fn mention_popup_move(&mut self, delta: i32) {
        let Some(p) = self.mention_popup.as_mut() else {
            return;
        };
        let count = filter_mentions(&p.query).len().min(MENTION_POPUP_MAX_VISIBLE);
        if count == 0 {
            return;
        }
        let cur = p.selected.min(count - 1) as i32;
        let new = ((cur + delta).rem_euclid(count as i32)) as usize;
        p.selected = new;
    }

    fn record_edit(&mut self, pre: CellSnapshot, post: CellSnapshot) {
        let Some(cell_id) = self.focused else { return };
        let now = Instant::now();

        let can_coalesce = !self.coalesce_break
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
        self.coalesce_break = false;

        self.touch_cell(cell_id);
    }

    fn undo(&mut self) -> bool {
        let Some(op) = self.undo_stack.pop() else {
            return false;
        };
        let mut bump_focused_edited = true;
        match &op {
            UndoOp::CellEdit { cell_id, pre, .. } => {
                self.focused = Some(*cell_id);
                if let Some(c) = self.cell_mut(*cell_id) {
                    c.restore(pre.clone());
                }
            }
            UndoOp::InsertCell {
                cell_id,
                pre_focused,
                ..
            } => {
                if let Some(idx) = self.cell_idx(*cell_id) {
                    self.cells.remove(idx);
                }
                self.pending_deletes.insert(*cell_id);
                self.dirty_cells.remove(cell_id);
                self.focused = *pre_focused;
            }
            UndoOp::DeleteCell {
                cell_id,
                snapshot,
                pre_focused,
                side_effect,
                ..
            } => {
                let cell = Cell::from_snapshot(*cell_id, snapshot.clone(), &self.typeface);
                self.insert_cell_sorted(cell);
                self.dirty_cells.insert(*cell_id);
                self.pending_deletes.remove(cell_id);
                if let Some(se) = side_effect {
                    self.reverse_context_side_effect(se);
                }
                self.focused = *pre_focused;
            }
            UndoOp::RotateContext {
                closed_id,
                prev_end_time,
                new_context,
                prev_view,
                pre_focused,
                pre_scroll_y,
                ..
            } => {
                self.inverse_rotation(
                    *closed_id,
                    *prev_end_time,
                    new_context.id,
                    *prev_view,
                    *pre_focused,
                    *pre_scroll_y,
                );
                bump_focused_edited = false;
            }
            UndoOp::ResetContextStart {
                context_id,
                prev_start,
                prev_view,
                ..
            } => {
                if let Some(c) = self.contexts.iter_mut().find(|c| c.id == *context_id) {
                    c.start_time = *prev_start;
                }
                self.dirty_contexts.insert(*context_id);
                self.view = *prev_view;
                bump_focused_edited = false;
            }
        }
        self.redo_stack.push(op);
        self.dragging_cell = None;
        self.pending_caret_scroll = true;
        self.coalesce_break = true;
        if bump_focused_edited {
            if let Some(id) = self.focused {
                self.touch_cell(id);
            }
        }
        true
    }

    fn redo(&mut self) -> bool {
        let Some(op) = self.redo_stack.pop() else {
            return false;
        };
        let mut bump_focused_edited = true;
        match &op {
            UndoOp::CellEdit {
                cell_id, post, ..
            } => {
                self.focused = Some(*cell_id);
                if let Some(c) = self.cell_mut(*cell_id) {
                    c.restore(post.clone());
                }
            }
            UndoOp::InsertCell {
                cell_id, snapshot, ..
            } => {
                let cell = Cell::from_snapshot(*cell_id, snapshot.clone(), &self.typeface);
                self.insert_cell_sorted(cell);
                self.dirty_cells.insert(*cell_id);
                self.pending_deletes.remove(cell_id);
                self.focused = Some(*cell_id);
            }
            UndoOp::DeleteCell {
                cell_id,
                post_focused,
                side_effect,
                ..
            } => {
                if let Some(idx) = self.cell_idx(*cell_id) {
                    self.cells.remove(idx);
                }
                self.pending_deletes.insert(*cell_id);
                self.dirty_cells.remove(cell_id);
                if let Some(se) = side_effect {
                    self.apply_context_side_effect(se);
                }
                self.focused = *post_focused;
            }
            UndoOp::RotateContext {
                closed_id,
                new_end_time,
                new_context,
                new_view,
                ..
            } => {
                self.apply_rotation(*closed_id, *new_end_time, new_context, *new_view);
                bump_focused_edited = false;
            }
            UndoOp::ResetContextStart {
                context_id,
                new_start,
                new_view,
                ..
            } => {
                if let Some(c) = self.contexts.iter_mut().find(|c| c.id == *context_id) {
                    c.start_time = *new_start;
                }
                self.dirty_contexts.insert(*context_id);
                self.view = *new_view;
                bump_focused_edited = false;
            }
        }
        self.undo_stack.push(op);
        self.dragging_cell = None;
        self.pending_caret_scroll = true;
        self.coalesce_break = true;
        if bump_focused_edited {
            if let Some(id) = self.focused {
                self.touch_cell(id);
            }
        }
        true
    }

    fn delete_focused_cell(&mut self) -> bool {
        let Some(id) = self.focused else { return false };
        let cell_ref = match self.cell(id) {
            Some(c) => c,
            None => return false,
        };
        let snapshot = cell_ref.snapshot();
        let cell_ts = cell_ref.timestamp;

        // Find the context whose window contains this cell (by timestamp).
        let containing_ctx: Option<Context> = self
            .contexts
            .iter()
            .find(|c| {
                cell_ts >= c.start_time && c.end_time.map_or(true, |e| cell_ts < e)
            })
            .cloned();

        // Will deleting this cell leave its containing context empty?
        let side_effect = if let Some(ctx) = containing_ctx {
            let others_in_ctx = self
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
                        .contexts
                        .iter()
                        .filter(|c| c.id != ctx.id && c.end_time.is_none())
                        .max_by_key(|c| c.start_time)
                        .map(|c| c.id);
                    new_active.map(|nid| {
                        // If user was viewing this closed context, follow to
                        // the new open one; otherwise leave the view alone
                        // (e.g., Date view stays).
                        let prev_view = self.view;
                        let new_view = match prev_view {
                            ViewSelection::Context(viewed) if viewed == ctx.id => {
                                ViewSelection::Context(nid)
                            }
                            other => other,
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
            self.cells.remove(idx);
        }
        self.pending_deletes.insert(id);
        self.dirty_cells.remove(&id);

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

        self.focused = new_focus;
        self.editing = false;
        self.dragging_cell = None;
        self.pending_caret_scroll = true;

        self.undo_stack.push(UndoOp::DeleteCell {
            cell_id: id,
            snapshot,
            pre_focused: Some(id),
            post_focused: new_focus,
            side_effect,
        });
        self.redo_stack.clear();
        self.coalesce_break = true;
        true
    }

    fn apply_context_side_effect(&mut self, se: &ContextSideEffect) {
        match se {
            ContextSideEffect::ContextRemoved {
                context, new_view, ..
            } => {
                self.contexts.retain(|c| c.id != context.id);
                self.dirty_contexts.remove(&context.id);
                self.pending_context_deletes.insert(context.id);
                self.view = *new_view;
            }
            ContextSideEffect::StartReset {
                context_id,
                new_start,
                ..
            } => {
                if let Some(c) = self.contexts.iter_mut().find(|c| c.id == *context_id) {
                    c.start_time = *new_start;
                }
                self.dirty_contexts.insert(*context_id);
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
                if !self.contexts.iter().any(|c| c.id == context.id) {
                    self.contexts.push(context.clone());
                }
                self.dirty_contexts.insert(context.id);
                self.pending_context_deletes.remove(&context.id);
                self.view = *prev_view;
            }
            ContextSideEffect::StartReset {
                context_id,
                prev_start,
                ..
            } => {
                if let Some(c) = self.contexts.iter_mut().find(|c| c.id == *context_id) {
                    c.start_time = *prev_start;
                }
                self.dirty_contexts.insert(*context_id);
            }
        }
    }

    fn insert_cell_after_focused(&mut self, outline: bool) -> bool {
        // If the user is viewing a closed context, jump to the current open
        // one before inserting. The note belongs in "today," not in history.
        let auto_switched = self.ensure_writable_context();
        // No-op if the focused cell is empty — Ctrl+Enter shouldn't pile up
        // empties. Skip when we just auto-switched: the destination's focused
        // cell is incidental, the user's intent was clearly to write.
        if !auto_switched {
            if let Some(id) = self.focused {
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
            .and_then(|id| self.contexts.iter().find(|c| c.id == id))
            .map(|c| c.start_time)
            .unwrap_or(i64::MIN);
        let baseline = self
            .last_cell_create_ms()
            .map(|t| t.max(writable_start))
            .unwrap_or(writable_start);
        if baseline > i64::MIN && now - baseline >= idle_ms {
            self.rotate_context_now();
        }
        let pre_focused = self.focused;
        let mut new_cell = if outline {
            Cell::new_outline(self.typeface.clone())
        } else {
            Cell::new(self.typeface.clone(), String::new())
        };
        new_cell.set_font_scale(self.font_scale);
        new_cell.context_hint_id = self.writable_context_id();
        let new_id = new_cell.id;
        let snapshot = new_cell.snapshot();
        self.insert_cell_sorted(new_cell);
        self.focused = Some(new_id);
        // Creating a cell is an explicit "I want to type" action.
        self.editing = true;
        self.dragging_cell = None;
        self.pending_caret_scroll = true;

        self.undo_stack.push(UndoOp::InsertCell {
            cell_id: new_id,
            snapshot,
            pre_focused,
        });
        self.redo_stack.clear();
        self.coalesce_break = true;
        self.touch_cell(new_id);
        true
    }

    /// Bring the primary caret of the focused cell into view if it's outside
    /// the viewport. Used after edits, caret movement, and zoom changes.
    fn scroll_caret_into_view(&mut self) {
        let Some(id) = self.focused else { return };
        let Some(cell) = self.cell(id) else { return };
        let Some((top, bot)) = cell.caret_doc_y_band() else {
            return;
        };
        let pad = 8.0_f32;
        let view_top = self.scroll_y;
        let view_bot = self.scroll_y + self.viewport_height;
        let new_scroll = if top < view_top + pad {
            (top - pad).max(0.0)
        } else if bot > view_bot - pad {
            (bot + pad - self.viewport_height).max(0.0)
        } else {
            return;
        };
        // Don't clamp to current max_scroll: a just-grown doc has a stale
        // max_scroll and the next tick will recompute it.
        self.scroll_y = new_scroll.max(0.0);
        self.last_scroll_time = Some(Instant::now());
    }

    /// Bring the focused cell into view if it's outside the current viewport.
    /// Uses last frame's cell geometry; on the first frame everything is at 0
    /// which results in scroll_y = 0, which is correct.
    fn scroll_to_focused(&mut self) {
        let Some(id) = self.focused else { return };
        let Some(cell) = self.cell(id) else { return };
        let pad = 8.0_f32;
        let cell_top = cell.y_origin();
        let cell_bot = cell.y_origin() + cell.height();
        let view_top = self.scroll_y;
        let view_bot = self.scroll_y + self.viewport_height;

        let new_scroll = if cell_top < view_top + pad {
            (cell_top - pad).max(0.0)
        } else if cell_bot > view_bot - pad {
            (cell_bot + pad - self.viewport_height).max(0.0)
        } else {
            return;
        };
        self.scroll_y = new_scroll.clamp(0.0, self.max_scroll);
        // Briefly show the scrollbar so the jump is visible.
        self.last_scroll_time = Some(Instant::now());
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, modifiers: &Modifiers) -> bool {
        // Any click dismisses an active @-mention popup.
        self.mention_popup = None;

        // Sidebar clicks switch the view. Sidebar lives in window (logical)
        // space, so use raw (x, y) — not doc_y.
        if x < SIDEBAR_WIDTH * self.font_scale {
            // Context rows first (they're indented inside dates so their bbox
            // overlaps date row gaps in some edge cases — context wins).
            for (id, rect) in self.last_sidebar_rects.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.cell_menu_open = None;
                    return self.set_active_context(id);
                }
            }
            for (date, rect) in self.last_sidebar_date_rects.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.cell_menu_open = None;
                    return self.set_active_date(date);
                }
            }
            self.cell_menu_open = None;
            return false;
        }

        let doc_y = y + self.scroll_y;

        // Per-cell kebab toggle wins over normal cell click routing.
        for (id, kebab) in &self.last_kebab_rects {
            if x >= kebab.left
                && x <= kebab.right
                && doc_y >= kebab.top
                && doc_y <= kebab.bottom
            {
                self.cell_menu_open = if self.cell_menu_open == Some(*id) {
                    None
                } else {
                    Some(*id)
                };
                return true;
            }
        }
        // Any other click closes the cell menu before falling through to
        // normal cell routing.
        self.cell_menu_open = None;

        let Some(target) = self.find_cell_at(x, doc_y) else {
            return false;
        };
        // Cross-cell click drops to view mode (matches keyboard nav). Same-cell
        // click preserves whatever mode the user was in. To start editing a new
        // cell, click it (selects), then hit Enter — or just keep typing once
        // already editing the same cell.
        if Some(target) != self.focused {
            self.focused = Some(target);
            self.editing = false;
        }
        // Any click moves/replaces the caret — break coalescing so the next
        // text edit starts a fresh undo entry.
        self.coalesce_break = true;
        self.dragging_cell = Some(target);
        let editing = self.editing;
        match self.cell_mut(target) {
            Some(cell) => cell.mouse_down(x, doc_y, modifiers, editing),
            None => false,
        }
    }

    pub fn mouse_drag_to(&mut self, x: f32, y: f32) -> bool {
        let doc_y = y + self.scroll_y;
        if let Some(id) = self.dragging_cell {
            match self.cell_mut(id) {
                Some(cell) => cell.mouse_drag_to(x, doc_y),
                None => false,
            }
        } else {
            false
        }
    }

    pub fn mouse_up(&mut self) -> bool {
        if let Some(id) = self.dragging_cell.take() {
            match self.cell_mut(id) {
                Some(cell) => cell.mouse_up(),
                None => false,
            }
        } else {
            false
        }
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

/// Draw `name` starting at `origin`, painting bytes in `match_indices` with
/// `match_paint` and the rest with `dim_paint`. ASCII-safe (matches use byte
/// indices); the FAKE_MENTIONS list is all ASCII so this is fine.
fn draw_runs_with_matches(
    canvas: &Canvas,
    name: &str,
    match_indices: &[usize],
    origin: Point,
    font: &Font,
    match_paint: &Paint,
    dim_paint: &Paint,
) {
    let bytes = name.as_bytes();
    let mut x = origin.x;
    let mut i = 0;
    while i < bytes.len() {
        let in_match = match_indices.contains(&i);
        let mut j = i + 1;
        while j < bytes.len() && match_indices.contains(&j) == in_match {
            j += 1;
        }
        let segment = &name[i..j];
        let paint = if in_match { match_paint } else { dim_paint };
        canvas.draw_str(segment, Point::new(x, origin.y), font, paint);
        x += font.measure_str(segment, Some(paint)).0;
        i = j;
    }
}

fn draw_kebab(canvas: &Canvas, rect: Rect, hovered: bool) {
    let cx = (rect.left + rect.right) * 0.5;
    let cy = (rect.top + rect.bottom) * 0.5;
    if hovered {
        let mut hover_paint = Paint::default();
        hover_paint.set_anti_alias(true);
        hover_paint.set_color(Color::from_argb(0x22, 0, 0, 0));
        let r = rect.width().min(rect.height()) * 0.5;
        canvas.draw_circle((cx, cy), r, &hover_paint);
    }
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));
    let h = rect.height();
    let cy0 = rect.top + h * 0.28;
    let cy1 = rect.top + h * 0.50;
    let cy2 = rect.top + h * 0.72;
    canvas.draw_circle((cx, cy0), KEBAB_DOT_RADIUS, &paint);
    canvas.draw_circle((cx, cy1), KEBAB_DOT_RADIUS, &paint);
    canvas.draw_circle((cx, cy2), KEBAB_DOT_RADIUS, &paint);
}

fn format_timestamp(epoch_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_millis_opt(epoch_ms)
        .single()
        .unwrap_or_else(|| Local.timestamp_millis_opt(0).single().unwrap());
    dt.format("%-d %B %Y, %-I:%M %p").to_string()
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

fn format_date_label(d: chrono::NaiveDate) -> String {
    let now = chrono::Local::now().date_naive();
    if d == now {
        format!("Today — {}", d.format("%b %-d"))
    } else if d.succ_opt() == Some(now) {
        format!("Yesterday — {}", d.format("%b %-d"))
    } else {
        d.format("%B %-d, %Y").to_string()
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
