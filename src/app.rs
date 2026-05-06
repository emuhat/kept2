use std::collections::HashSet;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use uuid::{Uuid, uuid};
use skia_safe::{
    BlurStyle, Canvas, Color, Font, FontMgr, MaskFilter, Paint, PaintStyle, PathEffect, Point,
    Rect, Typeface,
};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

use crate::cell::{
    self, Cell, CellKind, CellSnapshot, ReferenceTarget, TextBox, now_epoch_ms, primary_mod,
};
use crate::persist::{ContextRef, Db, Entity, db_path};
use crate::query;

const FONT_BYTES: &[u8] = include_bytes!("../resources/fonts/Figtree.ttf");

const MARGIN_X: f32 = 40.0;
const MARGIN_TOP: f32 = 60.0;
const CELL_GAP: f32 = 32.0;
/// Outer padding around the focused cell in focus mode (Ctrl+F). Smaller
/// than `MARGIN_X` so the cell really feels "kinda fullscreen."
const FOCUS_MODE_PAD: f32 = 16.0;
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

/// Tag name used to mark a cell as a "person" — its heading title shows up
/// in the `@`-mention popup. Convention: `# Alice Smith #person`.
#[allow(dead_code)]
const PERSON_TAG: &str = "person";

/// Subsequence fuzzy match. Returns `(score, matched_byte_positions)` if every
/// query char appears in `candidate` (case-insensitive) in order; None otherwise.
/// Bonuses: start-of-string, post-separator (whitespace/punctuation OR a
/// camelCase boundary in the original candidate), contiguous-with-previous-match.
/// Length penalty so shorter candidates win ties.
fn fuzzy_score(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let q_lower = query.to_lowercase();
    let c_lower = candidate.to_lowercase();
    let q = q_lower.as_bytes();
    let c = c_lower.as_bytes();
    // CamelCase detection reads the original candidate to spot the
    // lower→upper transition that splits "PeterCarr" into "Peter|Carr".
    // Only valid when lowercased and original line up byte-for-byte
    // (true for ASCII names; false when `to_lowercase` reflowed bytes).
    let orig = candidate.as_bytes();
    let camel_aligned = orig.len() == c.len();

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
                // Post-separator (whitespace, punctuation): `Carr` after
                // a space in "Peter Carr".
                score += 6;
            } else if camel_aligned
                && orig[i].is_ascii_uppercase()
                && orig[i - 1].is_ascii_lowercase()
            {
                // CamelCase boundary inside an otherwise unbroken run —
                // e.g. the `C` in `PeterCarr` starts a new name component
                // even though there's no separator character.
                score += 6;
            }
            // Word-boundary bonuses (6) are intentionally larger than
            // contiguous (5) so initials-style matches like `th` →
            // `TrevorHickey` (T + camelCase H) outrank an inside-word
            // contiguous run like `th` → `ThomasOttaway` (T + adjacent
            // h inside "Thomas").
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

/// Rank `names` by fuzzy match against `query`. Empty query returns the
/// names in their input order.
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

/// Heavy penalty applied to inactive candidates in the @-mention popup.
/// Typical short-query fuzzy scores are in roughly `[0, 30]`, so an
/// inactive match always ranks below any active match — but the user can
/// still find an inactive person by typing enough of the name.
const INACTIVE_FUZZY_PENALTY: i32 = 50;

fn filter_mentions(
    candidates: &[(String, bool)],
    query: &str,
) -> Vec<(String, Vec<usize>)> {
    if query.is_empty() {
        return candidates
            .iter()
            .map(|(n, _)| (n.clone(), Vec::new()))
            .collect();
    }
    let mut scored: Vec<(i32, String, Vec<usize>)> = candidates
        .iter()
        .filter_map(|(name, is_active)| {
            fuzzy_score(query, name).map(|(s, m)| {
                let s = if *is_active {
                    s
                } else {
                    s - INACTIVE_FUZZY_PENALTY
                };
                (s, name.clone(), m)
            })
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, n, m)| (n, m)).collect()
}

struct TagContextMenu {
    name: String,
    anchor_x: f32,
    anchor_y: f32,
}

struct MentionPopup {
    /// What the popup is anchored to: a focused cell's text or the search
    /// bar's input. Drives sync, render-anchor, and commit behavior.
    source: MentionSource,
    /// Byte position of the '@' in the source's text.
    anchor_byte: usize,
    /// Currently typed query (text after the '@', no whitespace).
    query: String,
    /// Index of the highlighted item in the filtered list.
    selected: usize,
}

#[derive(Clone, Copy)]
enum MentionSource {
    Cell { cell_id: Uuid, bullet_id: Option<Uuid> },
    SearchBar,
}

/// Top-of-viewport search popup, opened with Ctrl/Cmd+K. The query is a
/// real `TextBox` so the input gets the same arrow / word-nav / selection /
/// line-edge / paste behavior the rest of the app has. Esc/Enter are
/// intercepted at the popup layer; everything else flows to the TextBox.
/// The popup parses input through the query language and shows the top N
/// matching cells in a list. Selecting a result jumps to it; the doc area
/// behind the popup is *not* live-filtered (use sidebar tag/date entries
/// for persistent filtered views).
struct SearchState {
    input: TextBox,
    /// Index of the highlighted result row. Reset to 0 on text change.
    selected: usize,
    /// Result list from the last render where the @-mention popup was
    /// closed. While the user is mid-pick (mention popup open), we keep
    /// showing these so the search-popup result list doesn't churn on
    /// every keystroke of the in-progress `@<query>` token.
    cached_results: Vec<Uuid>,
}

/// Which kind of cell to spawn from a "new cell" hotkey.
#[derive(Clone, Copy)]
enum NewCellKind {
    Plain,
    Outline,
    PopPop,
    Table,
}

/// Sidebar PAGES section row identity. v1 has just `People`; new entries
/// land here as the section grows (threads, saved queries, etc).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PageKind {
    People,
}

/// In-progress inline rename of a People-page row. While `Some`, the row's
/// static label is replaced by `input.tick(...)` and Enter / Esc / clicks
/// outside drive commit / cancel.
struct PeopleRenameState {
    entity_id: Uuid,
    input: TextBox,
}

/// Right-click menu anchored on a cell in the doc area. Replaces the
/// old kebab affordance: timestamps render as muted info rows, and a
/// "Delete cell" row is the only action.
struct CellContextMenu {
    cell_id: Uuid,
    anchor_x: f32,
    anchor_y: f32,
    /// When the right-click hit-tested onto a specific bullet inside an
    /// outline cell, the bullet's id + a short snippet of its text. Drives
    /// the "Copy '<snippet>' bullet sub-tree as embed" menu row. None for
    /// non-outline cells or right-clicks landing in outline whitespace.
    bullet_id: Option<Uuid>,
    bullet_snippet: Option<String>,
}

/// Right-click menu over a People-page row. `deletable` and `ref_count`
/// are precomputed at open time so the menu render doesn't have to walk
/// every cell's links each frame; if the user creates a new mention
/// while the menu is open, they'll see stale state — that's fine, the
/// menu is dismissed by any click anyway.
struct PeopleContextMenu {
    entity_id: Uuid,
    anchor_x: f32,
    anchor_y: f32,
    /// True when the entity has no `primary_cell_id` AND zero `kept://`
    /// references in any cell. Drives the Delete row's enabled state.
    deletable: bool,
    /// Reference count surfaced as muted text under "Delete" when the
    /// entity isn't deletable. `None` when deletable (zero, suppressed).
    ref_count: Option<usize>,
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
    #[allow(dead_code)]
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
        matches!(self.view_kind, ViewKind::Ast)
            && self.ast.exclude.tags.is_empty()
            && self.ast.exclude.entities.is_empty()
            && self.ast.include.tags.is_empty()
            && self.ast.include.entities.is_empty()
            && self.ast.text.is_empty()
            && self.ast.include.time == Some(query::TimeFilter::Day(d))
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
/// Cap on date rows in the sidebar so the TAGS section has room. Older
/// dates are reachable via Ctrl+Shift+Up/Down and search; the active date
/// is always pinned in even if it falls outside the most-recent N.
const SIDEBAR_DATE_LIMIT: usize = 10;
#[allow(dead_code)]
const SIDEBAR_ITEM_FONT_SIZE: f32 = 12.0;

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

/// Search popup (Ctrl/Cmd+K).
const SEARCH_WIDTH: f32 = 520.0;
const SEARCH_TOP: f32 = 48.0;
const SEARCH_PAD: f32 = 12.0;
const SEARCH_RADIUS: f32 = 8.0;
const SEARCH_INPUT_H: f32 = 36.0;
const SEARCH_INPUT_FONT_SIZE: f32 = 16.0;
const SEARCH_RESULT_H: f32 = 32.0;
const SEARCH_RESULT_FONT_SIZE: f32 = 13.0;
const SEARCH_DATE_FONT_SIZE: f32 = 12.0;
const SEARCH_MAX_VISIBLE: usize = 8;
const SEARCH_SNIPPET_LEN: usize = 80;

/// Cell context menu (right-click). Two muted timestamp lines + a
/// "Delete cell" action separated by a hairline.
const CELL_MENU_WIDTH: f32 = 220.0;
const CELL_MENU_INFO_H: f32 = 22.0;
const CELL_MENU_ACTION_H: f32 = 26.0;
const CELL_MENU_PAD: f32 = 6.0;

/// Reference-cell embed wrapper. Warm-tan dashed border with a faint warm
/// background tint, plus a muted footer line ("↗ originally <date>") so the
/// embed reads as "not the original; click for the source."
const EMBED_INSET: f32 = 8.0;
const EMBED_PAD: f32 = 6.0;
const EMBED_FOOTER_H: f32 = 18.0;
const EMBED_FOOTER_FONT_SIZE: f32 = 12.0;

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

/// Forward-compat: the orientation of the (eventual) split. Always `Horiz`
/// in v1 (left/right). When vertical splits land, this enum gains meaning
/// without renaming.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitDir {
    Horiz,
    Vert,
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
    /// Vertical scroll within this pane's content (doc coords).
    scroll_y: f32,
    max_scroll: f32,
    doc_height: f32,
    viewport_height: f32,
    /// Drives this pane's scrollbar fade.
    last_scroll_time: Option<Instant>,
    /// "Request scroll caret into view next frame" — honored at end of
    /// this pane's tick.
    pending_caret_scroll: bool,
    /// Undo coalesce-break for this pane's edit stream. Cross-pane edits
    /// or focus changes set this so the next edit begins a new undo entry.
    coalesce_break: bool,
    /// Ctrl+F enlarges the focused cell to fill this pane only.
    focus_mode: bool,
    /// Per-pane back/forward navigation history.
    nav_back: Vec<HistoryEntry>,
    nav_forward: Vec<HistoryEntry>,
    /// Window-coord rect this pane occupies, populated by `tick`. Used by
    /// input dispatch (which pane was clicked) and overlay anchoring.
    #[allow(dead_code)]
    last_rect: Rect,
}

impl Pane {
    fn new(view: Query, focused: Option<Uuid>) -> Self {
        Self {
            view,
            focused,
            editing: false,
            dragging_cell: None,
            scroll_y: 0.0,
            max_scroll: 0.0,
            doc_height: 0.0,
            viewport_height: 0.0,
            last_scroll_time: None,
            pending_caret_scroll: false,
            coalesce_break: false,
            focus_mode: false,
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            last_rect: Rect::new(0.0, 0.0, 0.0, 0.0),
        }
    }
}

pub struct KeptApp {
    typeface: Typeface,
    /// Global, append-only stream of cells. Source of truth.
    /// Always sorted ascending by `Cell.timestamp`.
    cells: Vec<Cell>,
    /// Time-window overlays. Membership is derived (timestamp-based), not stored.
    contexts: Vec<Context>,
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
    search: Option<SearchState>,
    clipboard: Option<Clipboard>,
    db: Option<Db>,
    dirty_cells: HashSet<Uuid>,
    pending_deletes: HashSet<Uuid>,
    dirty_contexts: HashSet<Uuid>,
    pending_context_deletes: HashSet<Uuid>,
    /// Right-click context menu over a cell. While `Some`, render a
    /// floating card at the anchor; clicks inside dispatch the action,
    /// clicks elsewhere dismiss.
    cell_context_menu: Option<CellContextMenu>,
    /// "Delete cell" row rect on the cell context menu, captured each
    /// render for hit-testing.
    last_cell_menu_delete_rect: Option<Rect>,
    /// "Surface as reference" row rect (always present when the menu is open).
    last_cell_menu_surface_rect: Option<Rect>,
    /// "Surface '<snippet>' as reference" (sub-tree) row rect. None when
    /// the right-click didn't hit a bullet (non-outline cell, or outline
    /// whitespace).
    last_cell_menu_surface_subtree_rect: Option<Rect>,
    /// Sidebar context-row rects (window coords) from last frame, for hit-testing.
    last_sidebar_rects: Vec<(Uuid, Rect)>,
    /// Sidebar date-header rects from last frame, for hit-testing.
    last_sidebar_date_rects: Vec<(chrono::NaiveDate, Rect)>,
    /// Sidebar tag-row rects from last frame, for hit-testing.
    last_sidebar_tag_rects: Vec<(String, Rect)>,
    /// Active right-click context menu for a tag (only shown for tags
    /// with zero attached cells). When `Some`, render and hit-test the
    /// menu at the stored anchor.
    tag_context_menu: Option<TagContextMenu>,
    /// "Delete tag" row rect from the last render — used by mouse_down to
    /// dispatch the click.
    last_tag_menu_delete_rect: Option<Rect>,
    /// Search-popup input rect (window coords) from last frame. Populated
    /// when the popup is open so `mouse_down` can route clicks into the
    /// search TextBox; None when the popup is closed.
    last_search_input_rect: Option<Rect>,
    /// "+ Create backing cell" button rect on the entity page from the
    /// last frame (doc coords). Some only when the current view is
    /// `Entity(eid)` and the entity has no `primary_cell_id`. Used by
    /// `mouse_down` to route a click into the create flow (Chunk 2).
    last_entity_create_button_rect: Option<Rect>,
    /// Doc-space rects of the entity page's "REFERENCED IN" embed cards
    /// from the last render, paired with the source cell ids they point
    /// at. Cleared on every entity-page render and repopulated; clicks
    /// that land in any rect navigate to that source cell.
    last_entity_page_ref_rects: Vec<(Uuid, Rect)>,
    /// Sidebar PAGES section row rects (window coords) from last frame.
    /// Hit-tested by `mouse_down` to dispatch to `push_view(Query::people())`.
    last_sidebar_pages_rects: Vec<(PageKind, Rect)>,
    /// People-page row rects (entity_id, doc-space rect) from last frame.
    /// Used by `mouse_down` to route clicks into entity nav or rename.
    last_people_row_rects: Vec<(Uuid, Rect)>,
    /// "+ Add person…" footer-row rect (doc coords) from the last People
    /// render. None when the People page isn't active.
    last_people_add_rect: Option<Rect>,
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
    /// Active/inactive toggle rect on the entity page from last frame.
    /// `Some` only while in `ViewKind::Entity(_)`.
    last_entity_active_toggle_rect: Option<Rect>,
    /// "Show inactive" toggle rect on the People page from last frame.
    /// `Some` only while in `ViewKind::People`.
    last_people_show_inactive_toggle_rect: Option<Rect>,
    /// Active right-click menu over a People-page row.
    people_context_menu: Option<PeopleContextMenu>,
    /// "Rename" row rect on the People context menu, captured each
    /// render for hit-testing.
    last_people_menu_rename_rect: Option<Rect>,
    /// "Delete person" row rect on the People context menu. None when
    /// the entity isn't deletable (the row still renders but click is
    /// suppressed).
    last_people_menu_delete_rect: Option<Rect>,
    /// True while the user is mouse-dragging inside the search input
    /// (selecting text). Drives `mouse_drag_to` / `mouse_up` routing.
    search_dragging: bool,
    // ---- Entity caches (invariants #1–#7) ----
    /// All entity rows from the DB. Source of identity (kind, display_name).
    entities: Vec<Entity>,
    /// `(alias, entity_id, kind)` index. Built from the DB; rebuilt on
    /// save/delete via `refresh_entities`.
    entity_alias_index: Vec<(String, Uuid, String)>,
    /// `cell_id → entity_id` for entities that have a backing cell. Gates
    /// the title fallback (invariant #2) and lets the @-popup speak in
    /// entity-id space without scanning entities each time.
    cell_to_entity: std::collections::HashMap<Uuid, Uuid>,
    /// `(entity_id, normalize(display_name))` for entities with a backing
    /// cell. The title-fallback corpus — entirely entity-derived. Cells
    /// without a corresponding entity are *not* here, even if their title
    /// matches (invariant #2). Rebuilt with the other entity caches.
    entity_title_fallback: Vec<(Uuid, String)>,
    /// Most recent cursor position in window (logical) coords, used for hover.
    mouse_pos: (f32, f32),
}

#[derive(Clone)]
struct HistoryEntry {
    query: Query,
    focused: Option<Uuid>,
    scroll_y: f32,
}

const NAV_HISTORY_CAP: usize = 100;

/// `KeptApp` derefs to its active `Pane` so existing call sites — which
/// say `self.view`, `self.focused`, `self.scroll_y`, etc. — keep working
/// without rewriting every one. New per-pane access (Stage 2+) goes
/// through `self.panes[i].field` directly.
impl std::ops::Deref for KeptApp {
    type Target = Pane;
    fn deref(&self) -> &Pane {
        &self.panes[self.active_pane]
    }
}
impl std::ops::DerefMut for KeptApp {
    fn deref_mut(&mut self) -> &mut Pane {
        &mut self.panes[self.active_pane]
    }
}

impl KeptApp {
    pub fn new() -> Self {
        let typeface = FontMgr::new()
            .new_from_data(FONT_BYTES, None)
            .expect("failed to load embedded TTF");

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

        // Initial entity load. The migration backfilled `entities` from
        // `#person` cells in v4→v5, so this should be populated.
        let entities: Vec<Entity> = match db.as_ref().map(|d| d.all_entities()) {
            Some(Ok(rows)) => rows,
            Some(Err(e)) => {
                eprintln!("kept: failed to load entities: {e}");
                Vec::new()
            }
            None => Vec::new(),
        };
        let entity_alias_index: Vec<(String, Uuid, String)> =
            match db.as_ref().map(|d| d.entity_alias_index()) {
                Some(Ok(rows)) => rows,
                Some(Err(e)) => {
                    eprintln!("kept: failed to load entity alias index: {e}");
                    Vec::new()
                }
                None => Vec::new(),
            };
        let cell_to_entity: std::collections::HashMap<Uuid, Uuid> =
            match db.as_ref().map(|d| d.cell_to_entity_index()) {
                Some(Ok(rows)) => rows.into_iter().collect(),
                Some(Err(e)) => {
                    eprintln!("kept: failed to load cell→entity index: {e}");
                    std::collections::HashMap::new()
                }
                None => std::collections::HashMap::new(),
            };
        let entity_title_fallback: Vec<(Uuid, String)> = entities
            .iter()
            .filter(|e| e.primary_cell_id.is_some())
            .map(|e| (e.id, normalize_title_for_fallback(&e.display_name)))
            .collect();

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
            typeface,
            cells,
            contexts,
            panes: vec![Pane::new(view, focused)],
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
            search: None,
            clipboard: Clipboard::new().ok(),
            db,
            dirty_cells: HashSet::new(),
            pending_deletes: HashSet::new(),
            dirty_contexts: HashSet::new(),
            pending_context_deletes: HashSet::new(),
            cell_context_menu: None,
            last_cell_menu_delete_rect: None,
            last_cell_menu_surface_rect: None,
            last_cell_menu_surface_subtree_rect: None,
            last_sidebar_rects: Vec::new(),
            last_sidebar_date_rects: Vec::new(),
            last_sidebar_tag_rects: Vec::new(),
            tag_context_menu: None,
            last_tag_menu_delete_rect: None,
            last_search_input_rect: None,
            last_entity_create_button_rect: None,
            last_entity_page_ref_rects: Vec::new(),
            last_sidebar_pages_rects: Vec::new(),
            last_people_row_rects: Vec::new(),
            last_people_add_rect: None,
            people_rename: None,
            people_add: None,
            show_inactive: false,
            last_entity_active_toggle_rect: None,
            last_people_show_inactive_toggle_rect: None,
            people_context_menu: None,
            last_people_menu_rename_rect: None,
            last_people_menu_delete_rect: None,
            search_dragging: false,
            entities,
            entity_alias_index,
            cell_to_entity,
            entity_title_fallback,
            mouse_pos: (-1.0, -1.0),
        }
    }

    pub fn cursor_moved(&mut self, x: f32, y: f32) {
        self.mouse_pos = (x, y);
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
        let new_pane = Pane {
            view: src.view.clone(),
            focused: src.focused,
            editing: false,
            dragging_cell: None,
            scroll_y: src.scroll_y,
            max_scroll: src.max_scroll,
            doc_height: src.doc_height,
            viewport_height: src.viewport_height,
            last_scroll_time: None,
            pending_caret_scroll: false,
            coalesce_break: true,
            focus_mode: false,
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            last_rect: Rect::new(0.0, 0.0, 0.0, 0.0),
        };
        // Insert to the right of the active pane and activate it. With v1
        // capped at 2 panes, this just means push + active = 1.
        self.panes.push(new_pane);
        self.split_ratio = 0.5;
        self.active_pane = self.panes.len() - 1;
        true
    }

    /// Open `q` in the *other* pane, splitting first if needed. The
    /// "other" pane becomes active, so subsequent keystrokes go there.
    /// Used by Alt+click on sidebar entries — the low-friction path for
    /// "I've got this open here, give me that over there."
    fn open_in_other_pane(&mut self, q: Query) -> bool {
        if self.panes.len() < 2 {
            self.split_pane();
        } else {
            let other = (self.active_pane + 1) % self.panes.len();
            self.set_active_pane(other);
        }
        self.push_view(q)
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
            Color::from_rgb(0xc8, 0xbf, 0xb0)
        } else {
            Color::from_rgb(0xdc, 0xd4, 0xc6)
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
        p.set_color(Color::from_argb(0x80, 0x4a, 0x90, 0xe2));
        canvas.draw_rect(
            Rect::new(r.left + s, r.top + s, r.right - s, r.bottom - s),
            &p,
        );
    }

    /// Render a reference cell at `(x, y)` with `width`. Returns the height
    /// drawn. Looks up the target out of `self.cells`, dispatches to the
    /// appropriate body's `render_view`, and wraps it in the embed visual
    /// (warm-tan dashed border + faint background tint + footer line).
    /// Handles dangling references and chained references via placeholder
    /// text. Records geometry on the reference cell so click-tests work.
    fn render_reference_cell(
        &mut self,
        canvas: &Canvas,
        ref_idx: usize,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
    ) -> f32 {
        let target = match &self.cells[ref_idx].kind {
            CellKind::Reference(rc) => rc.target,
            _ => return 0.0,
        };
        let scale = self.font_scale;
        let inset = EMBED_INSET * scale;
        let pad = EMBED_PAD * scale;
        let body_x = x + inset;
        let body_y = y + pad;
        let body_w = (width - 2.0 * inset).max(40.0);

        let target_idx = self.cells.iter().position(|c| c.id == target.cell_id());

        // Decide what kind of preview to render and refresh the cache on
        // the reference cell if the source's edited_at has changed.
        enum PreviewKind {
            Cached,
            Placeholder(&'static str),
        }
        let preview = match target_idx {
            None => {
                // Target gone — clear any stale cache and show placeholder.
                if let CellKind::Reference(rc) = &mut self.cells[ref_idx].kind {
                    rc.install_cache(None, None);
                }
                PreviewKind::Placeholder("↗ [referenced cell deleted]")
            }
            Some(tidx) if matches!(self.cells[tidx].kind, CellKind::Reference(_)) => {
                if let CellKind::Reference(rc) = &mut self.cells[ref_idx].kind {
                    rc.install_cache(None, None);
                }
                PreviewKind::Placeholder("↗ [chained reference]")
            }
            Some(tidx) => {
                let source_edited_at = self.cells[tidx].edited_at;
                let is_stale = match &self.cells[ref_idx].kind {
                    CellKind::Reference(rc) => rc.cache_is_stale_for(Some(source_edited_at)),
                    _ => false,
                };
                if is_stale {
                    let new_cache = self.build_reference_cache(tidx, target);
                    if let CellKind::Reference(rc) = &mut self.cells[ref_idx].kind {
                        rc.install_cache(new_cache, Some(source_edited_at));
                    }
                }
                // If the build returned None (e.g., subtree's bullet missing),
                // surface a placeholder. Otherwise tick the cache.
                let has_cache = matches!(
                    &self.cells[ref_idx].kind,
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
                if let CellKind::Reference(rc) = &mut self.cells[ref_idx].kind {
                    if let Some(cache) = rc.cache_mut() {
                        // Tick the cache: focused mirrors the outer cell's
                        // focus state (so selection highlights show only
                        // when this reference is the focused cell). Caret
                        // is always suppressed — references are read-only.
                        cache.tick(canvas, body_x, body_y, body_w, focused, false)
                    } else {
                        0.0
                    }
                } else {
                    0.0
                }
            }
        };

        let footer_text = match target_idx {
            Some(tidx) => {
                let ts = self.cells[tidx].timestamp;
                format!("↗ originally {}", format_date_label(local_date_for_ms(ts)))
            }
            None => "↗ original deleted".to_string(),
        };
        let total_h = self.draw_embed_wrapper(
            canvas, x, y, width, body_x, body_h, &footer_text, scale,
        );

        // Record geometry on the embed: both on the inner ReferenceCell
        // (for symmetry / future use) and on the outer Cell (which is what
        // `find_cell_at` reads via Cell::x_origin/width/height).
        if let CellKind::Reference(rc) = &mut self.cells[ref_idx].kind {
            rc.set_view_geometry(x, y, width, total_h);
        }
        self.cells[ref_idx].set_view_geometry(x, y, width, total_h);

        total_h
    }

    /// Build a fresh cache `Cell` mirroring the source's content. Returns
    /// None when the target isn't renderable (e.g., Subtree whose bullet
    /// is gone, Subtree pointing at a non-Outline cell). The cache is a
    /// real `Cell` so it owns selection state across frames and dispatches
    /// mouse events through the standard machinery.
    fn build_reference_cache(
        &self,
        target_idx: usize,
        target: ReferenceTarget,
    ) -> Option<Cell> {
        let source = &self.cells[target_idx];
        let scale = self.font_scale;
        let typeface = &self.typeface;
        match target {
            ReferenceTarget::WholeCell(_) => {
                let kind = source.kind.clone_for_scale(typeface, scale)?;
                let title = source.title().map(|t| {
                    let mut new_t = TextBox::new(typeface.clone(), t.text().to_string());
                    new_t.set_force_heading(true);
                    new_t.set_font_scale(scale);
                    for l in t.links() {
                        new_t.add_link(l.range.clone(), l.url.clone());
                    }
                    new_t
                });
                let mut cache = Cell::from_parts(
                    Uuid::now_v7(),
                    kind,
                    title,
                    source.timestamp,
                    source.edited_at,
                    source.context_hint_id,
                );
                cache.set_font_scale(scale);
                Some(cache)
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
                        let mut tb = TextBox::new(
                            typeface.clone(),
                            b.textbox().text().to_string(),
                        );
                        tb.set_font_scale(scale);
                        for l in b.textbox().links() {
                            tb.add_link(l.range.clone(), l.url.clone());
                        }
                        let new_depth = b.depth().saturating_sub(root_depth);
                        cell::Bullet::new(b.id(), tb, new_depth)
                    })
                    .collect();
                let mut new_oc = cell::OutlineCell::from_bullets(typeface.clone(), bullets);
                new_oc.set_font_scale(scale);
                let mut cache = Cell::from_parts(
                    Uuid::now_v7(),
                    CellKind::Outline(new_oc),
                    None,
                    source.timestamp,
                    source.edited_at,
                    source.context_hint_id,
                );
                cache.set_font_scale(scale);
                Some(cache)
            }
        }
    }

    /// One-line muted placeholder for dangling / chained / wrong-kind
    /// references. Returns the rendered height.
    /// Paint the embed's wrapper chrome: faint warm-tan background tint,
    /// dashed warm-tan border, muted footer line. Used by both the
    /// timeline reference cell render and the entity-page references
    /// list — the visual is identical because the meaning is identical.
    /// Returns the total height (body + footer + paddings).
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
    ) -> f32 {
        let pad = EMBED_PAD * scale;
        let footer_h = EMBED_FOOTER_H * scale;
        let total_h = pad + body_h + 4.0 * scale + footer_h;
        let wrapper = Rect::new(x, y, x + width, y + total_h);

        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(Color::from_argb(0x0c, 0xb3, 0x92, 0x60));
        canvas.draw_round_rect(wrapper, FOCUS_RADIUS, FOCUS_RADIUS, &bg);

        let mut stroke = Paint::default();
        stroke.set_anti_alias(true);
        stroke.set_style(PaintStyle::Stroke);
        stroke.set_stroke_width(1.0);
        stroke.set_color(Color::from_rgb(0xb3, 0x92, 0x60));
        if let Some(eff) = PathEffect::dash(&[4.0, 2.0], 0.0) {
            stroke.set_path_effect(eff);
        }
        canvas.draw_round_rect(wrapper, FOCUS_RADIUS, FOCUS_RADIUS, &stroke);

        let footer_font = Font::from_typeface(&self.typeface, EMBED_FOOTER_FONT_SIZE * scale);
        let (_, fm) = footer_font.metrics();
        let footer_baseline = y + total_h - pad - (-fm.ascent);
        let mut footer_paint = Paint::default();
        footer_paint.set_anti_alias(true);
        footer_paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));
        canvas.draw_str(
            footer_text,
            Point::new(body_x, footer_baseline),
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
        paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));
        canvas.draw_str(text, Point::new(x, baseline), &font, &paint);
        -m.ascent + m.descent
    }

    /// Debounced persistence flush. Called once per frame from `tick`,
    /// outside the per-pane loop (dirty cells are global, not per-pane).
    fn maybe_flush_persistence(&mut self) {
        let any_dirty = !self.dirty_cells.is_empty()
            || !self.pending_deletes.is_empty()
            || !self.dirty_contexts.is_empty()
            || !self.pending_context_deletes.is_empty();
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

    /// True if the mouse is currently over a link in any visible cell. Used
    /// by the host (`main.rs`) to swap the system cursor to a hand pointer.
    pub fn is_hovering_link(&self) -> bool {
        let (x, y) = self.mouse_pos;
        // Sidebar columns / out-of-bounds have no links.
        if x < SIDEBAR_WIDTH * self.font_scale || x < 0.0 || y < 0.0 {
            return false;
        }
        let doc_y = y + self.scroll_y;
        let ctx = self.match_context();
        for cell in &self.cells {
            if !self.is_visible_for_view(cell, &ctx) {
                continue;
            }
            if cell.link_at_doc_pos(x, doc_y) {
                return true;
            }
        }
        false
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
    /// `(display_name, entity_id)` for every person entity, sorted
    /// alphabetically. Thin view over `self.entities`. Drives the
    /// `@`-mention popup; commit inserts `kept://<entity_id>` (invariant
    /// #1 — the @-popup speaks entity-id space).
    fn person_entries(&self) -> Vec<(String, Uuid)> {
        let mut out: Vec<(String, Uuid)> = self
            .entities
            .iter()
            .filter(|e| e.kind == "person")
            .map(|e| (e.display_name.clone(), e.id))
            .collect();
        out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        out
    }

    /// `(display_name, is_active)` for every person entity, in the same
    /// alphabetical order as `person_entries`. Fed to `filter_mentions`
    /// so the popup can downweight inactive matches without losing them.
    fn person_mention_candidates(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self
            .entities
            .iter()
            .filter(|e| e.kind == "person")
            .map(|e| (e.display_name.clone(), e.is_active))
            .collect();
        out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        out
    }

    /// Reload the entity caches from the DB. Called after every
    /// `save_cell` / `delete_cell` so the in-memory state stays in lockstep
    /// with the persistence layer's authoritative entity table.
    fn refresh_entities(&mut self) {
        let Some(db) = self.db.as_ref() else { return };
        match db.all_entities() {
            Ok(rows) => self.entities = rows,
            Err(e) => eprintln!("kept: refresh_entities failed: {e}"),
        }
        match db.entity_alias_index() {
            Ok(rows) => self.entity_alias_index = rows,
            Err(e) => eprintln!("kept: entity_alias_index reload failed: {e}"),
        }
        match db.cell_to_entity_index() {
            Ok(rows) => self.cell_to_entity = rows.into_iter().collect(),
            Err(e) => eprintln!("kept: cell_to_entity_index reload failed: {e}"),
        }
        self.entity_title_fallback = self
            .entities
            .iter()
            .filter(|e| e.primary_cell_id.is_some())
            .map(|e| (e.id, normalize_title_for_fallback(&e.display_name)))
            .collect();
    }

    fn writable_context_id(&self) -> Option<Uuid> {
        self.contexts
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
        match self.view.view_kind {
            ViewKind::Context(id) => {
                let cell_ts = cell.timestamp;
                self.contexts.iter().find(|c| c.id == id).map_or(false, |c| {
                    cell_ts >= c.start_time && c.end_time.map_or(true, |e| cell_ts < e)
                })
            }
            ViewKind::Entity(eid) => self
                .entities
                .iter()
                .find(|e| e.id == eid)
                .and_then(|e| e.primary_cell_id)
                .map_or(false, |pid| pid == cell.id),
            ViewKind::People => false,
            ViewKind::Ast => query::matches(&self.view.ast, cell, ctx),
        }
    }

    /// Build the per-render `MatchContext`: today's date plus the resolved
    /// entity-id sets for any `@id` refs in the active AST. Both the alias
    /// index and the title-fallback corpus are entity-derived (invariants
    /// #1, #2). Cheap — both inputs are already cached on `self`.
    fn match_context(&self) -> query::MatchContext {
        let today = local_date_for_ms(now_epoch_ms());
        let person_targets = query::resolve_persons(
            &self.view.ast.include.entities,
            &self.entity_alias_index,
            &self.entity_title_fallback,
        );
        let person_excludes = query::resolve_persons(
            &self.view.ast.exclude.entities,
            &self.entity_alias_index,
            &self.entity_title_fallback,
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
        let prev_view = self.view.clone();

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
            // View update: Context view follows to the bumped one; Date and
            // tag views keep their filters/time-bound unchanged.
            let new_view = rotate_view_to(&prev_view, writable);
            self.view = new_view.clone();
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
        new_view: Query,
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
        self.cell_context_menu = None;
        self.scroll_y = 0.0;
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
        self.cell_context_menu = None;
        self.scroll_y = pre_scroll_y;
    }

    /// Previous context (older `start_time`) relative to the currently
    /// viewed one. None when not in Context view.
    fn prev_context(&self) -> Option<Uuid> {
        let current = self.view.context_view()?;
        let mut sorted: Vec<&Context> = self.contexts.iter().collect();
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
        let current = self.view.context_view()?;
        let mut sorted: Vec<&Context> = self.contexts.iter().collect();
        sorted.sort_by_key(|c| c.start_time);
        let pos = sorted.iter().position(|c| c.id == current)?;
        sorted.get(pos + 1).map(|c| c.id)
    }

    fn context_has_cells(&self, ctx: &Context) -> bool {
        let start = ctx.start_time;
        let end = ctx.end_time;
        self.cells
            .iter()
            .any(|c| c.timestamp >= start && end.map(|e| c.timestamp < e).unwrap_or(true))
    }

    /// Walk contexts forward in time from the current view, skipping empties.
    /// Used for arrow-nav cross-context jumps so an empty newer context
    /// doesn't trap the cursor.
    fn next_context_with_cells(&self) -> Option<Uuid> {
        let current = self.view.context_view()?;
        let mut sorted: Vec<&Context> = self.contexts.iter().collect();
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
        let current = self.view.context_view()?;
        let mut sorted: Vec<&Context> = self.contexts.iter().collect();
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
        if let Some(id) = self.view.context_view() {
            let active_is_open = self
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
        if self.view.is_solo_date(today) {
            return false;
        }
        self.set_active_date(today)
    }

    /// Switch the view to a single existing context.
    fn set_active_context(&mut self, id: Uuid) -> bool {
        let next = Query::context(id);
        if self.view == next {
            return false;
        }
        if !self.contexts.iter().any(|c| c.id == id) {
            return false;
        }
        self.view = next;
        // Focus the first visible cell in the new window (if any).
        self.focused = self.visible_cell_ids().first().copied();
        self.editing = false;
        self.dragging_cell = None;
        self.cell_context_menu = None;
        self.scroll_y = 0.0;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    /// Switch the view to "everything from this local date" mode.
    fn set_active_date(&mut self, d: chrono::NaiveDate) -> bool {
        let next = Query::date(d);
        if self.view == next {
            return false;
        }
        self.view = next;
        self.focused = self.visible_cell_ids().first().copied();
        self.editing = false;
        self.dragging_cell = None;
        self.cell_context_menu = None;
        self.scroll_y = 0.0;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    /// Switch the view to "all cells carrying this tag" mode (no time bound).
    #[allow(dead_code)]
    fn set_active_tag(&mut self, name: String) -> bool {
        let next = Query::tag(name);
        if self.view == next {
            return false;
        }
        self.view = next;
        self.focused = self.visible_cell_ids().first().copied();
        self.editing = false;
        self.dragging_cell = None;
        self.cell_context_menu = None;
        self.scroll_y = 0.0;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    // ----- View history (Cmd/Ctrl+[ / ]) -----

    /// Snapshot the current `(view, focused, scroll_y)` onto the back
    /// stack, clear the forward stack, and transition to `new`. Called by
    /// deliberate-nav sites (sidebar clicks, search commit). Auto-flows
    /// (rotation, ensure_writable_context, undo) bypass this and mutate
    /// the view directly.
    fn push_view(&mut self, new: Query) -> bool {
        if self.view == new {
            return false;
        }
        let entry = HistoryEntry {
            query: self.view.clone(),
            focused: self.focused,
            scroll_y: self.scroll_y,
        };
        self.nav_back.push(entry);
        if self.nav_back.len() > NAV_HISTORY_CAP {
            self.nav_back.remove(0);
        }
        self.nav_forward.clear();
        self.view = new;
        self.focused = self.visible_cell_ids().first().copied();
        self.editing = false;
        self.dragging_cell = None;
        self.cell_context_menu = None;
        self.scroll_y = 0.0;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        true
    }

    /// Cmd/Ctrl+[: pop the back stack onto the active view, pushing the
    /// current view onto the forward stack first. No-op when the back
    /// stack is empty.
    fn nav_back(&mut self) -> bool {
        let Some(prev) = self.nav_back.pop() else { return false };
        let entry = HistoryEntry {
            query: self.view.clone(),
            focused: self.focused,
            scroll_y: self.scroll_y,
        };
        self.nav_forward.push(entry);
        if self.nav_forward.len() > NAV_HISTORY_CAP {
            self.nav_forward.remove(0);
        }
        self.restore_history_entry(prev);
        true
    }

    /// Cmd/Ctrl+]: mirror of `nav_back`. No-op when the forward stack is
    /// empty (which is the case until the user has gone back at least
    /// once and not yet pushed a new view).
    fn nav_forward(&mut self) -> bool {
        let Some(next) = self.nav_forward.pop() else { return false };
        let entry = HistoryEntry {
            query: self.view.clone(),
            focused: self.focused,
            scroll_y: self.scroll_y,
        };
        self.nav_back.push(entry);
        if self.nav_back.len() > NAV_HISTORY_CAP {
            self.nav_back.remove(0);
        }
        self.restore_history_entry(next);
        true
    }

    fn restore_history_entry(&mut self, e: HistoryEntry) {
        self.view = e.query;
        self.focused = e.focused;
        self.scroll_y = e.scroll_y;
        self.editing = false;
        self.dragging_cell = None;
        self.cell_context_menu = None;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
        // Drop focus mode — the new view's focused cell may not be the
        // same; "fullscreen on a different cell" is jarring after a
        // back-nav. The user can re-enter focus mode with Ctrl+F.
        self.focus_mode = false;
    }

    /// IDs of cells visible under the active view, in DISPLAY order — newest
    /// first. Index 0 is the topmost (most recent) cell. `prev_visible` /
    /// `next_visible` operate on this same order, so "prev" = visually above.
    fn visible_cell_ids(&self) -> Vec<Uuid> {
        let ctx = self.match_context();
        let mut ids: Vec<Uuid> = self
            .cells
            .iter()
            .filter(|c| self.is_visible_for_view(c, &ctx))
            .map(|c| c.id)
            .collect();
        ids.reverse();
        ids
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

    /// Mouse-wheel scroll. Routes to the pane under the cursor (per the
    /// multi-pane spec), not the active pane — letting the user scroll one
    /// pane while keyboard input goes to another. Falls back to the active
    /// pane when the cursor isn't over any pane.
    pub fn scroll_by(&mut self, dy: f32) -> bool {
        let target = self
            .pane_at(self.mouse_pos.0, self.mouse_pos.1)
            .unwrap_or(self.active_pane);
        let pane = &mut self.panes[target];
        let new_y = (pane.scroll_y + dy).clamp(0.0, pane.max_scroll);
        if new_y == pane.scroll_y {
            return false;
        }
        pane.scroll_y = new_y;
        pane.last_scroll_time = Some(Instant::now());
        // Scrolling dismisses the per-cell menu (anchored in doc coords).
        self.cell_context_menu = None;
        true
    }

    pub fn tick(&mut self, canvas: &Canvas, width: f32, height: f32) {
        canvas.clear(Color::from_rgb(0xfa, 0xf7, 0xf2));
        self.layout_panes(width, height);

        // Render each pane. We swap `active_pane` for the duration of each
        // pane's tick so Deref-based field access (self.scroll_y, self.view,
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

        // Sidebar (window space, single global instance).
        self.render_sidebar(canvas, height);

        // Overlays (window space, drawn last so they layer on top).
        self.render_search_popup(canvas, width);
        self.render_mention_popup(canvas);
        self.render_tag_context_menu(canvas);
        self.render_people_context_menu(canvas);
        self.render_cell_context_menu(canvas);

        // Persistence flush is global (dirty cells aren't per-pane), so it
        // runs once per frame, after all panes have rendered.
        self.maybe_flush_persistence();
    }

    /// Render a single pane. With `active_pane` swapped to `pane_idx` by
    /// the caller, all `self.X` field accesses (Deref) resolve to this
    /// pane. Pane geometry comes from `self.panes[pane_idx].last_rect`,
    /// populated by `layout_panes`.
    fn tick_pane(&mut self, canvas: &Canvas, pane_idx: usize, _height: f32) {
        // Clamp scroll using last frame's max_scroll before drawing this frame.
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll);

        let pane_rect = self.panes[pane_idx].last_rect;
        let pane_left = pane_rect.left;
        let pane_right = pane_rect.right;
        let pane_h = pane_rect.height();

        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        // Clip to this pane's rect so over-wide content / focus shadows
        // can't bleed across the divider into the other pane.
        canvas.save();
        canvas.clip_rect(pane_rect, None, true);
        // Document space — translate so doc y=0 lands at window y = -scroll_y.
        canvas.translate((0.0, -self.scroll_y));

        let scale = self.font_scale;
        // Focus mode pulls the cell out near the pane's left edge with
        // smaller pad so it visually expands to fill the pane; normal mode
        // uses MARGIN_X on both sides.
        let (cells_left, outer_cell_width) = if self.focus_mode {
            let left = pane_left + FOCUS_MODE_PAD * scale;
            let outer = (pane_right - left - FOCUS_MODE_PAD * scale).max(80.0);
            (left, outer)
        } else {
            let left = pane_left + MARGIN_X;
            let outer = (pane_right - left - MARGIN_X).max(80.0);
            (left, outer)
        };
        let content_width = outer_cell_width.max(60.0);

        // Capture focused-cell geometry up front. The card backdrop (drawn
        // *before* cell content) and the focus ring (drawn after) both use
        // this so they stay in lockstep — at most one frame of lag when the
        // cell grows from typing, but they always match each other.
        // In focus mode we override the x/width to match the wider focus
        // layout (otherwise the card would draw at last frame's normal-mode
        // size); the ring is suppressed since there's nothing to compare to.
        let mut y = MARGIN_TOP;
        let mouse_doc_x = self.mouse_pos.0;
        let mouse_doc_y = self.mouse_pos.1 + self.scroll_y;
        let focused_id = self.focused;
        let editing_local = self.editing;

        // Cell-loop views (Ast / Context) draw the cell stream with the
        // focused-cell card backdrop + ring. Entity / People views draw
        // bespoke pages and bypass that entire path.
        let view_kind_local = self.view.view_kind.clone();
        if !matches!(view_kind_local, ViewKind::Ast | ViewKind::Context(_)) {
            self.last_entity_create_button_rect = None;
        }

        // Card backdrop and focus ring use this. We pull `cells_left` and
        // `content_width` from this frame's pane geometry (so they're always
        // correct for the pane being rendered, not stale from another pane's
        // last render). y/height come from the cell's last-rendered values
        // — at most one frame stale when content size changes.
        let focused_geom = if matches!(view_kind_local, ViewKind::Ast | ViewKind::Context(_)) {
            self.focused
                .and_then(|id| self.cell(id))
                .filter(|c| c.height() > 0.0)
                .map(|c| {
                    if self.focus_mode {
                        (cells_left, MARGIN_TOP, content_width, c.height())
                    } else {
                        (cells_left, c.y_origin(), content_width, c.height())
                    }
                })
        } else {
            None
        };

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

        match view_kind_local {
            ViewKind::Ast | ViewKind::Context(_) => {
                // Precompute per-cell visibility and section headers (Date view only)
                // so the mutable cell loop below doesn't have to re-borrow self.
                // In focus mode only the focused cell is visible — everything else
                // is suppressed regardless of the current view's filters.
                let visible: Vec<bool> = if self.focus_mode {
                    self.cells
                        .iter()
                        .map(|c| Some(c.id) == focused_id)
                        .collect()
                } else {
                    let match_ctx = self.match_context();
                    self.cells
                        .iter()
                        .map(|c| self.is_visible_for_view(c, &match_ctx))
                        .collect()
                };
                // Headers are aligned to self.cells indices but computed in DISPLAY
                // order (descending) so a header lands above the first cell of each
                // group as the user scrolls top-down.
                //
                // Date view groups by context (multiple contexts can land in one day).
                // Any other AST view (tag, search query, multi-filter, free-text)
                // groups by local date since cells can span days. Context view and
                // focus mode have no inter-group headers.
                #[derive(PartialEq, Eq)]
                enum HeaderMode {
                    ByContext,
                    ByDate,
                    None,
                }
                let header_mode = if self.focus_mode {
                    HeaderMode::None
                } else if !matches!(self.view.view_kind, ViewKind::Ast) {
                    HeaderMode::None
                } else if matches!(
                    self.view.ast.include.time,
                    Some(query::TimeFilter::Day(_))
                ) && self.view.ast.include.tags.is_empty()
                    && self.view.ast.include.entities.is_empty()
                    && self.view.ast.exclude.tags.is_empty()
                    && self.view.ast.exclude.entities.is_empty()
                    && self.view.ast.text.is_empty()
                {
                    // Pure date view — show context-section headers within the day.
                    HeaderMode::ByContext
                } else {
                    // Tag / search / multi-filter — group by date across the result set.
                    HeaderMode::ByDate
                };
                let headers: Vec<Option<String>> = if header_mode == HeaderMode::None {
                    vec![None; self.cells.len()]
                } else {
                    let mut hs: Vec<Option<String>> = vec![None; self.cells.len()];
                    let mut last_label: Option<String> = None;
                    for i in (0..self.cells.len()).rev() {
                        if !visible[i] {
                            continue;
                        }
                        let cell = &self.cells[i];
                        let label: String = match header_mode {
                            HeaderMode::ByContext => self
                                .context_for_timestamp(cell.timestamp)
                                .map(|c| format_context_time(c.start_time))
                                .unwrap_or_default(),
                            HeaderMode::ByDate => {
                                format_date_label(local_date_for_ms(cell.timestamp))
                            }
                            HeaderMode::None => unreachable!(),
                        };
                        if last_label.as_deref() != Some(label.as_str()) {
                            last_label = Some(label.clone());
                            hs[i] = Some(label);
                        }
                    }
                    hs
                };

                let header_font = Font::from_typeface(
                    &self.typeface,
                    CONTEXT_HEADER_FONT_SIZE * scale,
                );
                let (_, hm) = header_font.metrics();
                let header_h = CONTEXT_HEADER_H * scale;
                let header_pad_top = CONTEXT_HEADER_PAD_TOP * scale;

                // Render cells newest-first (descending) — index walked in reverse so
                // self.cells (asc) iterates from end to start.
                let total_cells = self.cells.len();
                for i in (0..total_cells).rev() {
                    if !visible[i] {
                        continue;
                    }
                    if let Some(label) = &headers[i] {
                        let header_y = y + header_pad_top;
                        let baseline = header_y + (-hm.ascent);
                        let mut hp = Paint::default();
                        hp.set_anti_alias(true);
                        hp.set_color(Color::from_rgb(0x70, 0x68, 0x58));
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
                        lp.set_color(Color::from_argb(0x80, 0x70, 0x68, 0x58));
                        lp.set_stroke_width(1.5);
                        canvas.draw_line(
                            Point::new(cells_left + label_w + 8.0 * scale, line_y),
                            Point::new(cells_left + outer_cell_width, line_y),
                            &lp,
                        );
                        y += header_h;
                    }
                    let cell_x = cells_left;
                    let cell_y = y;
                    let cell_id = self.cells[i].id;
                    let is_reference = matches!(self.cells[i].kind, CellKind::Reference(_));
                    let cell_is_focused =
                        focused_id.map(|f| f == cell_id).unwrap_or(false);

                    // Selection highlights are visible whenever the cell is focused
                    // (so view-mode users can drag-select). Caret only renders in
                    // edit mode.
                    let render_focused = cell_is_focused;
                    let show_caret = cell_is_focused && editing_local;
                    let h = if is_reference {
                        // Reference cells render via the app layer (which can
                        // see the full cell list to look up the target).
                        self.render_reference_cell(
                            canvas,
                            i,
                            cell_x,
                            cell_y,
                            content_width,
                            render_focused,
                        )
                    } else {
                        let cell = &mut self.cells[i];
                        cell.tick(
                            canvas,
                            cell_x,
                            cell_y,
                            content_width,
                            render_focused,
                            show_caret,
                        )
                    };

                    // Faint outline around non-focused cells so each one reads as a
                    // distinct unit. Drawn in the same position the focus ring would
                    // occupy so cells don't visually shift when focus moves.
                    // Reference cells have their own dashed warm-tan border —
                    // skip the standard outline so the two don't compete.
                    if !cell_is_focused && !is_reference {
                        let mut outline = Paint::default();
                        outline.set_anti_alias(true);
                        outline.set_style(PaintStyle::Stroke);
                        outline.set_stroke_width(CELL_OUTLINE_STROKE);
                        outline.set_color(Color::from_argb(
                            CELL_OUTLINE_ALPHA,
                            0x1c,
                            0x1c,
                            0x1c,
                        ));
                        let rect = Rect::new(
                            cell_x - FOCUS_PAD,
                            cell_y - FOCUS_PAD,
                            cell_x + content_width + FOCUS_PAD,
                            cell_y + h + FOCUS_PAD,
                        );
                        canvas.draw_round_rect(rect, FOCUS_RADIUS, FOCUS_RADIUS, &outline);
                    }

                    y += h + CELL_GAP;
                }
            }
            ViewKind::Entity(eid) => {
                let h = self.render_entity_page(
                    canvas,
                    eid,
                    cells_left,
                    content_width,
                    scale,
                    mouse_doc_x,
                    mouse_doc_y,
                );
                // +CELL_GAP so the doc_height formula (`y - CELL_GAP +
                // DOC_BOTTOM_PAD`) matches the cell-loop convention.
                y = MARGIN_TOP + h + CELL_GAP;
            }
            ViewKind::People => {
                let h = self.render_people_page(
                    canvas,
                    cells_left,
                    content_width,
                    scale,
                    mouse_doc_x,
                    mouse_doc_y,
                );
                y = MARGIN_TOP + h + CELL_GAP;
            }
        }

        // Focus ring — subtle when viewing, brighter and thicker when editing.
        // Suppressed in focus mode where the white card backdrop alone marks
        // the active area (no other cells to compete with).
        if let Some((cx, cy, cw, ch)) = focused_geom.filter(|_| !self.focus_mode) {
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

        canvas.restore();

        // Update bookkeeping for scroll math + clamp again in case content shrank.
        self.doc_height = y - CELL_GAP + DOC_BOTTOM_PAD;
        self.viewport_height = pane_h.max(0.0);
        self.max_scroll = (self.doc_height - self.viewport_height).max(0.0);
        self.scroll_y = self.scroll_y.min(self.max_scroll);

        // After cells are laid out (y_origin/height fresh), honor any caret-into-view
        // request from this tick's events. Effect lands on the next frame.
        if std::mem::take(&mut self.pending_caret_scroll) {
            self.scroll_caret_into_view();
        }

        // Per-pane scrollbar in window coords, anchored at the pane's right edge.
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
                let bar_x = pane_right - SCROLLBAR_INSET - SCROLLBAR_WIDTH;

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
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
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

        // Esc closes the cell context menu first if it's open.
        if event.state == ElementState::Pressed
            && self.cell_context_menu.is_some()
            && matches!(event.logical_key, Key::Named(NamedKey::Escape))
        {
            self.cell_context_menu = None;
            return true;
        }

        // Search popup. Ctrl/Cmd+K opens; while open, all keys go to it.
        if event.state == ElementState::Pressed
            && primary_mod(modifiers.state())
            && matches!(&event.logical_key, Key::Character(s) if s.as_str().eq_ignore_ascii_case("k"))
        {
            if self.search.is_some() {
                self.close_search_cancel();
            } else {
                self.open_search();
            }
            return true;
        }
        if event.state == ElementState::Pressed && self.search.is_some() {
            let mods = modifiers.state();
            // When the @-mention popup is open over the search input, it
            // owns Enter/Tab/Esc/Up/Down — those select / commit / dismiss
            // a person, not the search-popup result list.
            if self.mention_popup.is_some() {
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
            // Popup-specific keys take precedence over text editing.
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.close_search_cancel();
                    return true;
                }
                Key::Named(NamedKey::Enter) => {
                    // Alt+Enter routes the result into the *other* pane
                    // (splitting if needed). Plain Enter lands it in the
                    // active pane.
                    let other = mods.alt_key();
                    self.close_search_commit(other);
                    return true;
                }
                Key::Named(NamedKey::ArrowUp) if !mods.shift_key() => {
                    self.search_move(-1);
                    return true;
                }
                Key::Named(NamedKey::ArrowDown) if !mods.shift_key() => {
                    self.search_move(1);
                    return true;
                }
                _ => {}
            }
            // Cmd/Ctrl + letter combos: clipboard + select-all are routed
            // to the search input; other letter shortcuts (zoom, new cell,
            // undo, etc.) are swallowed so they don't fire behind the
            // popup. Named keys (arrows, Home/End, Backspace) under
            // primary_mod fall through to the TextBox so line-edge
            // (Cmd+Arrow on Mac), word-nav (Ctrl+Arrow on Linux/Win), and
            // word-Backspace all work.
            if primary_mod(mods) {
                if let Key::Character(s) = &event.logical_key {
                    let s = s.as_str();
                    if s.eq_ignore_ascii_case("c") {
                        self.search_copy_to_clipboard();
                        return true;
                    }
                    if s.eq_ignore_ascii_case("x") {
                        self.search_cut_to_clipboard();
                        if let Some(state) = self.search.as_mut() {
                            state.selected = 0;
                        }
                        return true;
                    }
                    if s.eq_ignore_ascii_case("v") {
                        self.search_paste_from_clipboard();
                        if let Some(state) = self.search.as_mut() {
                            state.selected = 0;
                        }
                        return true;
                    }
                    if s.eq_ignore_ascii_case("a") {
                        if let Some(state) = self.search.as_mut() {
                            state.input.select_all();
                        }
                        return true;
                    }
                    // Other letter combos: swallow so app shortcuts don't
                    // fire while the popup is up.
                    return true;
                }
                // Fall through for Named keys.
            }
            // Forward to the input. Reset selected on text change so the
            // result list always tracks the current query.
            let pre = self.search.as_ref().map(|s| s.input.text().to_string());
            let popup_was_open = self.mention_popup.is_some();
            if let Some(state) = self.search.as_mut() {
                state.input.handle_key(event, modifiers);
            }
            let post = self.search.as_ref().map(|s| s.input.text().to_string());
            if pre != post {
                if let Some(state) = self.search.as_mut() {
                    state.selected = 0;
                }
            }
            // @-mention popup hooks: maybe open if the user just typed '@';
            // sync against the new caret/text otherwise so a shrinking
            // query backs out the popup or updates its filter.
            if !popup_was_open && event.text.as_deref() == Some("@") {
                self.try_open_mention_popup();
            }
            self.sync_mention_popup();
            return true;
        }

        // People-page rename input: Enter and Esc both commit (Esc is a
        // "blur" that keeps the typed text live, matching the cell
        // edit-vs-view modal elsewhere). Everything else flows into the
        // embedded TextBox.
        if event.state == ElementState::Pressed && self.people_rename.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter) => {
                    self.commit_people_rename();
                    return true;
                }
                _ => {}
            }
            if let Some(rs) = self.people_rename.as_mut() {
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
            if let Some(input) = self.people_add.as_mut() {
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
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("a") => {
                    if let Some(id) = self.focused {
                        if let Some(cell) = self.cell_mut(id) {
                            cell.select_all_focused();
                        }
                        self.coalesce_break = true;
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
                    return self.paste_from_clipboard();
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("n") => {
                    if modifiers.state().shift_key() {
                        self.rotate_context_now();
                        return true;
                    }
                    return self.insert_cell_after_focused(NewCellKind::Plain);
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("o") => {
                    return self.insert_cell_after_focused(NewCellKind::Outline);
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("p") => {
                    return self.insert_cell_after_focused(NewCellKind::PopPop);
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("j") => {
                    return self.insert_cell_after_focused(NewCellKind::Table);
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("f") => {
                    // Ctrl+F: enter "focus mode" — render only the focused
                    // cell at full width. Esc or any sidebar click exits.
                    if self.focused.is_none() {
                        return false;
                    }
                    if self.focus_mode {
                        return false;
                    }
                    self.focus_mode = true;
                    self.scroll_y = 0.0;
                    self.coalesce_break = true;
                    return true;
                }
                Key::Character(s) if s.as_str().eq_ignore_ascii_case("t") => {
                    // Ctrl/Cmd+T: create + focus the title slot on the
                    // focused cell. Idempotent — focuses an existing title.
                    // (Cmd+H is reserved by macOS for "hide app," so title
                    // gets T and tables move to J.)
                    let Some(id) = self.focused else { return false };
                    let changed = self
                        .cell_mut(id)
                        .map(|c| c.toggle_title_focus())
                        .unwrap_or(false);
                    if changed {
                        self.editing = true;
                        self.coalesce_break = true;
                        self.pending_caret_scroll = true;
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
                // Esc closes the tag context menu first.
                Key::Named(NamedKey::Escape) if self.tag_context_menu.is_some() => {
                    self.tag_context_menu = None;
                    return true;
                }
                // Same for the People context menu.
                Key::Named(NamedKey::Escape) if self.people_context_menu.is_some() => {
                    self.people_context_menu = None;
                    return true;
                }
                // Esc exits focus mode first; if it wasn't on, fall through
                // to the edit→view exit below.
                Key::Named(NamedKey::Escape) if self.focus_mode => {
                    self.focus_mode = false;
                    self.coalesce_break = true;
                    self.pending_caret_scroll = true;
                    return true;
                }
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
                    // Reference cells are read-only — Enter on a focused
                    // reference navigates to the original instead of
                    // entering edit mode.
                    if let Some(id) = self.focused {
                        let target = match self.cell(id) {
                            Some(c) => match &c.kind {
                                CellKind::Reference(rc) => Some(rc.target),
                                _ => None,
                            },
                            None => None,
                        };
                        if let Some(t) = target {
                            self.navigate_to_reference(t);
                            return true;
                        }
                    }
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
                && !primary_mod(modifiers.state())
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
                            if let Some(next_ctx) = self.next_context_with_cells() {
                                if self.set_active_context(next_ctx) {
                                    self.focused = self.visible_cell_ids().last().copied();
                                    return true;
                                }
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
                            if let Some(prev_ctx) = self.prev_context_with_cells() {
                                if self.set_active_context(prev_ctx) {
                                    self.focused = self.visible_cell_ids().first().copied();
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
                            // No more cells in this view going up — cross to
                            // the newer context and land at its bottom (oldest)
                            // cell, caret at end so the chronological flow is
                            // continuous when arrowing further up. Skip empty
                            // contexts so the cursor doesn't get trapped.
                            if let Some(next_ctx) = self.next_context_with_cells() {
                                if self.set_active_context(next_ctx) {
                                    let landing = self.visible_cell_ids().last().copied();
                                    self.focused = landing;
                                    if let Some(id) = landing {
                                        if let Some(c) = self.cell_mut(id) {
                                            c.place_caret_at_end();
                                        }
                                    }
                                    self.editing = false;
                                    self.coalesce_break = true;
                                    self.pending_caret_scroll = true;
                                    return true;
                                }
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
                            // Bottom of the view — cross to the older context
                            // and land at its top (newest) cell, caret at start.
                            // Skip empties.
                            if let Some(prev_ctx) = self.prev_context_with_cells() {
                                if self.set_active_context(prev_ctx) {
                                    let landing = self.visible_cell_ids().first().copied();
                                    self.focused = landing;
                                    if let Some(id) = landing {
                                        if let Some(c) = self.cell_mut(id) {
                                            c.place_caret_at_start();
                                        }
                                    }
                                    self.editing = false;
                                    self.coalesce_break = true;
                                    self.pending_caret_scroll = true;
                                    return true;
                                }
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
        // Prefer the search bar when it has the keyboard focus — typing in
        // the popup never goes through cell.handle_key, so the cell-source
        // path would be a no-op here.
        if let Some(state) = self.search.as_ref() {
            let text = state.input.text();
            let caret = state
                .input
                .primary_caret()
                .map(|(_, h)| h)
                .unwrap_or(0);
            if caret == 0 {
                return;
            }
            if text.get(caret - 1..caret) != Some("@") {
                return;
            }
            self.mention_popup = Some(MentionPopup {
                source: MentionSource::SearchBar,
                anchor_byte: caret - 1,
                query: String::new(),
                selected: 0,
            });
            return;
        }
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
            source: MentionSource::Cell {
                cell_id: focused_id,
                bullet_id: cell.focused_bullet_id(),
            },
            anchor_byte: caret - 1,
            query: String::new(),
            selected: 0,
        });
    }

    fn sync_mention_popup(&mut self) {
        let Some(popup) = self.mention_popup.as_ref() else {
            return;
        };
        let anchor_byte = popup.anchor_byte;
        let source = popup.source;
        // Pull the current `(text, caret)` from whichever source is anchored.
        let cur: Option<(String, usize)> = match source {
            MentionSource::Cell { cell_id, bullet_id } => {
                if self.focused != Some(cell_id) {
                    None
                } else if let Some(cell) = self.cell(cell_id) {
                    if cell.focused_bullet_id() != bullet_id {
                        None
                    } else {
                        cell.focused_text_and_caret()
                            .map(|(t, c)| (t.to_string(), c))
                    }
                } else {
                    None
                }
            }
            MentionSource::SearchBar => self.search.as_ref().and_then(|s| {
                let caret = s.input.primary_caret().map(|(_, h)| h)?;
                Some((s.input.text().to_string(), caret))
            }),
        };
        let Some((text, caret)) = cur else {
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
        let query = q.to_string();
        let candidates = self.person_mention_candidates();
        if let Some(p) = self.mention_popup.as_mut() {
            let count = filter_mentions(&candidates, &query)
                .len()
                .min(MENTION_POPUP_MAX_VISIBLE);
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

    /// Right-click context menu over a cell. Two muted info lines
    /// (Created / Last edited timestamps) followed by a hairline and a
    /// red "Delete cell" action row. Anchored at the cursor position
    /// recorded when the menu opened.
    fn render_cell_context_menu(&mut self, canvas: &Canvas) {
        let Some(menu) = self.cell_context_menu.as_ref() else {
            self.last_cell_menu_delete_rect = None;
            self.last_cell_menu_surface_rect = None;
            self.last_cell_menu_surface_subtree_rect = None;
            return;
        };
        let Some(cell) = self.cell(menu.cell_id) else {
            self.last_cell_menu_delete_rect = None;
            self.last_cell_menu_surface_rect = None;
            self.last_cell_menu_surface_subtree_rect = None;
            return;
        };
        let scale = self.font_scale;
        let pad = CELL_MENU_PAD * scale;
        let info_h = CELL_MENU_INFO_H * scale;
        let action_h = CELL_MENU_ACTION_H * scale;
        let menu_w = CELL_MENU_WIDTH * scale;

        // Compute action rows. Order matches the visual stack.
        let has_subtree = menu.bullet_id.is_some();
        let mut action_count: usize = 1; // Delete cell
        action_count += 1; // Surface as reference (always)
        if has_subtree {
            action_count += 1;
        }
        let menu_h =
            pad + info_h * 2.0 + 1.0 + action_h * action_count as f32 + pad;
        let radius = 6.0 * scale;
        let rect = Rect::new(
            menu.anchor_x,
            menu.anchor_y,
            menu.anchor_x + menu_w,
            menu.anchor_y + menu_h,
        );

        // Drop shadow.
        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        shadow.set_color(Color::from_argb(0x40, 0, 0, 0));
        shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 8.0, false));
        canvas.draw_round_rect(
            Rect::new(rect.left, rect.top + 2.0, rect.right, rect.bottom + 2.0),
            radius,
            radius,
            &shadow,
        );

        // Background + border.
        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(Color::WHITE);
        canvas.draw_round_rect(rect, radius, radius, &bg);
        let mut border = Paint::default();
        border.set_anti_alias(true);
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(1.0);
        border.set_color(Color::from_rgb(0xc0, 0xc0, 0xc0));
        canvas.draw_round_rect(rect, radius, radius, &border);

        // Two muted info lines.
        let info_font = Font::from_typeface(&self.typeface, 12.0 * scale);
        let mut info_paint = Paint::default();
        info_paint.set_anti_alias(true);
        info_paint.set_color(Color::from_rgb(0x80, 0x80, 0x80));
        let (_, im) = info_font.metrics();
        let line1_baseline =
            rect.top + pad + (info_h + (-im.ascent) - im.descent) * 0.5;
        let line2_baseline = line1_baseline + info_h;
        canvas.draw_str(
            format!("Created {}", format_timestamp(cell.timestamp)),
            Point::new(rect.left + pad * 2.0, line1_baseline),
            &info_font,
            &info_paint,
        );
        canvas.draw_str(
            format!("Last edited {}", format_timestamp(cell.edited_at)),
            Point::new(rect.left + pad * 2.0, line2_baseline),
            &info_font,
            &info_paint,
        );

        // Hairline divider above the action rows.
        let divider_y = rect.top + pad + info_h * 2.0 + 0.5;
        let mut divider = Paint::default();
        divider.set_anti_alias(false);
        divider.set_color(Color::from_argb(0x28, 0x1c, 0x1c, 0x1c));
        canvas.draw_line(
            Point::new(rect.left + pad, divider_y),
            Point::new(rect.right - pad, divider_y),
            &divider,
        );

        let action_font = Font::from_typeface(&self.typeface, 13.0 * scale);
        let (_, am) = action_font.metrics();
        let mouse_x = self.mouse_pos.0;
        let mouse_y = self.mouse_pos.1;
        let mut row_top = rect.top + pad + info_h * 2.0 + 1.0;
        let mut draw_row = |label: &str,
                            color: Color,
                            hover_argb: (u8, u8, u8, u8)|
         -> Rect {
            let r = Rect::new(
                rect.left + pad * 0.5,
                row_top,
                rect.right - pad * 0.5,
                row_top + action_h,
            );
            let hovered = mouse_x >= r.left
                && mouse_x <= r.right
                && mouse_y >= r.top
                && mouse_y <= r.bottom;
            if hovered {
                let mut hp = Paint::default();
                hp.set_anti_alias(true);
                hp.set_color(Color::from_argb(
                    hover_argb.0, hover_argb.1, hover_argb.2, hover_argb.3,
                ));
                canvas.draw_round_rect(r, 4.0 * scale, 4.0 * scale, &hp);
            }
            let mut paint = Paint::default();
            paint.set_anti_alias(true);
            paint.set_color(color);
            let baseline =
                r.top + (action_h + (-am.ascent) - am.descent) * 0.5;
            canvas.draw_str(
                label,
                Point::new(r.left + pad * 2.0, baseline),
                &action_font,
                &paint,
            );
            row_top += action_h;
            r
        };

        // Delete row (red, hover red-tinted).
        let delete_rect = draw_row(
            "Delete cell",
            Color::from_rgb(0xc0, 0x30, 0x30),
            (0x20, 0xc0, 0x30, 0x30),
        );

        // Surface as reference — always present. Creates a new reference
        // cell at "now" pointing to this cell. Lands wherever a fresh
        // Ctrl+N cell would land. Hover uses the warm-tan tint.
        let surface_rect = draw_row(
            "Surface as reference",
            Color::from_rgb(0x40, 0x40, 0x40),
            (0x20, 0xb3, 0x92, 0x60),
        );

        // Surface bullet sub-tree as reference — only when right-click hit
        // a bullet inside an outline.
        let surface_subtree_rect = if has_subtree {
            let snip = menu.bullet_snippet.as_deref().unwrap_or("[empty]");
            let label = format!("Surface '{}' as reference", snip);
            Some(draw_row(
                &label,
                Color::from_rgb(0x40, 0x40, 0x40),
                (0x20, 0xb3, 0x92, 0x60),
            ))
        } else {
            None
        };

        self.last_cell_menu_delete_rect = Some(delete_rect);
        self.last_cell_menu_surface_rect = Some(surface_rect);
        self.last_cell_menu_surface_subtree_rect = surface_subtree_rect;
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
        self.last_entity_create_button_rect = None;

        let entity = match self.entities.iter().find(|e| e.id == entity_id).cloned() {
            Some(e) => e,
            None => {
                let font = Font::from_typeface(&self.typeface, ENTITY_META_FONT_SIZE * scale);
                let (_, fm) = font.metrics();
                let mut paint = Paint::default();
                paint.set_anti_alias(true);
                paint.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
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
        title_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
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
            .entity_alias_index
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
        meta_paint.set_color(Color::from_rgb(0x70, 0x68, 0x58));
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
        self.last_entity_active_toggle_rect = Some(toggle_rect);

        y += -mm.ascent + mm.descent;
        y += ENTITY_SECTION_GAP * scale;

        // BACKING CELL section header (sidebar-header styling).
        let header_font =
            Font::from_typeface(&self.typeface, SIDEBAR_HEADER_FONT_SIZE * scale);
        let (_, hm) = header_font.metrics();
        let mut header_paint = Paint::default();
        header_paint.set_anti_alias(true);
        header_paint.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
        canvas.draw_str(
            "BACKING CELL",
            Point::new(cells_left, y + (-hm.ascent)),
            &header_font,
            &header_paint,
        );
        y += -hm.ascent + hm.descent + ENTITY_SECTION_HEADER_GAP * scale;

        // Backing-cell body.
        if let Some(pid) = entity.primary_cell_id {
            if let Some(cell_idx) = self.cells.iter().position(|c| c.id == pid) {
                let focused_id = self.focused;
                let editing = self.editing;
                let cell = &mut self.cells[cell_idx];
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
                    outline.set_color(Color::from_argb(
                        CELL_OUTLINE_ALPHA,
                        0x1c,
                        0x1c,
                        0x1c,
                    ));
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
                paint.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
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
            bg.set_color(Color::from_argb(bg_alpha, 0x1c, 0x1c, 0x1c));
            canvas.draw_round_rect(btn_rect, 6.0 * scale, 6.0 * scale, &bg);
            let mut border = Paint::default();
            border.set_anti_alias(true);
            border.set_style(PaintStyle::Stroke);
            border.set_stroke_width(1.0);
            border.set_color(Color::from_argb(0x40, 0x1c, 0x1c, 0x1c));
            canvas.draw_round_rect(btn_rect, 6.0 * scale, 6.0 * scale, &border);

            let mut lp = Paint::default();
            lp.set_anti_alias(true);
            lp.set_color(Color::from_rgb(0x60, 0x58, 0x48));
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
            self.last_entity_create_button_rect = Some(btn_rect);
            y += btn_h;
        }

        // REFERENCED IN — list of cells that link to this entity. Rendered
        // as embed previews (warm-tan dashed wrapper + cached body), sorted
        // newest-first by `edited_at`. The previews aren't real cells —
        // they live only as long as this page render. Click an embed →
        // navigate to the source cell; rect-tracked in
        // `last_entity_page_ref_rects` for hit-test in `mouse_down`.
        self.last_entity_page_ref_rects.clear();
        let kept_url = format!("kept://{}", entity_id);
        let primary = entity.primary_cell_id;
        let mut mentions: Vec<(usize, i64)> = self
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
            ref_header_paint.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
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
                let target_cell_id = self.cells[target_idx].id;
                let target_ts = self.cells[target_idx].timestamp;
                // Fresh cache per frame — no selection persistence on the
                // entity page (acceptable for v1; click-to-navigate covers
                // the main interaction).
                let mut maybe_cache = self.build_reference_cache(
                    target_idx,
                    ReferenceTarget::WholeCell(target_cell_id),
                );
                let body_h = match &mut maybe_cache {
                    Some(cache) => {
                        cache.tick(canvas, body_x, y + pad, body_w, false, false)
                    }
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
                );
                self.last_entity_page_ref_rects.push((
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
        self.last_people_row_rects.clear();
        self.last_people_add_rect = None;
        self.last_people_show_inactive_toggle_rect = None;

        let mut y = MARGIN_TOP;

        // Title + "Show inactive" toggle, sharing a baseline.
        let title_font =
            Font::from_typeface(&self.typeface, ENTITY_TITLE_FONT_SIZE * scale);
        let (_, tm) = title_font.metrics();
        let mut title_paint = Paint::default();
        title_paint.set_anti_alias(true);
        title_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
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
        label_paint.set_color(Color::from_rgb(0x70, 0x68, 0x58));
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
        self.last_people_show_inactive_toggle_rect = Some(toggle_rect);

        y += -tm.ascent + tm.descent + 24.0 * scale;

        // Sorted snapshot — case-insensitive by display_name. When
        // `show_inactive` is off, hide inactive rows entirely; when on,
        // they stay in alphabetical order but render in muted color.
        let show_inactive = self.show_inactive;
        let mut people: Vec<(String, Uuid, bool)> = self
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
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
        let mut inactive_paint = Paint::default();
        inactive_paint.set_anti_alias(true);
        inactive_paint.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
        let mut divider_paint = Paint::default();
        divider_paint.set_anti_alias(true);
        divider_paint.set_color(Color::from_argb(0x18, 0x1c, 0x1c, 0x1c));
        divider_paint.set_stroke_width(1.0);
        let mut hover_paint = Paint::default();
        hover_paint.set_anti_alias(true);
        hover_paint.set_color(Color::from_argb(0x14, 0x1c, 0x1c, 0x1c));

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
            }

            // Hairline divider at the bottom of each row.
            canvas.draw_line(
                Point::new(row_rect.left, row_rect.bottom),
                Point::new(row_rect.right, row_rect.bottom),
                &divider_paint,
            );

            self.last_people_row_rects.push((*entity_id, row_rect));
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
            muted.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
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
        self.last_people_add_rect = Some(add_rect);
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
        let entity_pre = self.entities.iter().find(|e| e.id == entity_id).cloned();
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
        self.coalesce_break = true;
    }

    /// Count `kept://<entity_id>` mentions across every cell's links.
    /// Used to gate "Delete person" — a deleted entity with live
    /// mentions would leave dangling links. Walks all cells (title +
    /// body + nested elements) via `Cell::all_link_urls`.
    fn count_entity_references(&self, entity_id: Uuid) -> usize {
        let target = format!("kept://{}", entity_id);
        let mut n = 0usize;
        for cell in &self.cells {
            for url in cell.all_link_urls() {
                if url == target {
                    n += 1;
                }
            }
        }
        n
    }

    /// Open the People right-click context menu for `entity_id`,
    /// anchored at window-space `(x, y)`. Precomputes deletability
    /// (no backing cell + zero references). The menu closes on any
    /// subsequent click or Esc.
    fn open_people_context_menu(&mut self, entity_id: Uuid, x: f32, y: f32) {
        let primary = self
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
        self.coalesce_break = true;
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
        self.coalesce_break = true;
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
        self.coalesce_break = true;
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

        let header_font =
            Font::from_typeface(&self.typeface, SIDEBAR_HEADER_FONT_SIZE * scale);
        let mut header_paint = Paint::default();
        header_paint.set_anti_alias(true);
        header_paint.set_color(Color::from_rgb(0x90, 0x88, 0x7a));
        let (_, hm) = header_font.metrics();

        // Sidebar shows one row per local date that has any contexts.
        // Individual context rows are intentionally absent — clicking a date
        // opens the full day in the doc area; cross-context navigation
        // happens via Ctrl+Shift+Up/Down or arrow-edge crossing.
        self.last_sidebar_rects.clear();
        self.last_sidebar_date_rects.clear();
        self.last_sidebar_tag_rects.clear();
        self.last_sidebar_pages_rects.clear();

        let date_font_for_pages =
            Font::from_typeface(&self.typeface, SIDEBAR_DATE_FONT_SIZE * scale);
        let (_, dm_pages) = date_font_for_pages.metrics();
        let mouse_x_pages = self.mouse_pos.0;
        let mouse_y_pages = self.mouse_pos.1;

        // ---- PAGES section ----
        let pages_header_baseline = pad_top + (-hm.ascent);
        canvas.draw_str(
            "PAGES",
            Point::new(pad_x, pages_header_baseline),
            &header_font,
            &header_paint,
        );
        let mut sidebar_y = pad_top + header_h;
        // People row.
        let people_rect = Rect::new(
            pad_x * 0.5,
            sidebar_y,
            sb_w - pad_x * 0.5,
            sidebar_y + date_h,
        );
        let people_active = matches!(self.view.view_kind, ViewKind::People);
        let people_hovered = mouse_x_pages >= people_rect.left
            && mouse_x_pages <= people_rect.right
            && mouse_y_pages >= people_rect.top
            && mouse_y_pages <= people_rect.bottom;
        if people_active {
            let mut p = Paint::default();
            p.set_anti_alias(true);
            p.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));
            canvas.draw_round_rect(people_rect, radius, radius, &p);
        } else if people_hovered {
            let mut p = Paint::default();
            p.set_anti_alias(true);
            p.set_color(Color::from_argb(0x18, 0x1c, 0x1c, 0x1c));
            canvas.draw_round_rect(people_rect, radius, radius, &p);
        }
        let mut row_paint = Paint::default();
        row_paint.set_anti_alias(true);
        row_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
        let people_baseline =
            sidebar_y + (date_h + (-dm_pages.ascent) - dm_pages.descent) * 0.5;
        canvas.draw_str(
            "People",
            Point::new(pad_x, people_baseline),
            &date_font_for_pages,
            &row_paint,
        );
        self.last_sidebar_pages_rects
            .push((PageKind::People, people_rect));
        sidebar_y += date_h + item_gap + date_gap;

        // ---- CONTEXTS section ----
        let contexts_header_baseline = sidebar_y + (-hm.ascent);
        canvas.draw_str(
            "CONTEXTS",
            Point::new(pad_x, contexts_header_baseline),
            &header_font,
            &header_paint,
        );
        sidebar_y += header_h;

        // Date rows reflect "where notes live": every date that has at least
        // one cell, plus today (so a freshly-launched empty app still shows
        // a usable target), plus the active Date view's date if it's been
        // navigated away from any of those (so the active highlight has a
        // home).
        let mut dates_set: std::collections::BTreeSet<chrono::NaiveDate> =
            std::collections::BTreeSet::new();
        for c in &self.cells {
            dates_set.insert(local_date_for_ms(c.timestamp));
        }
        dates_set.insert(local_date_for_ms(now_epoch_ms()));
        let active_date = if matches!(self.view.view_kind, ViewKind::Ast) {
            match self.view.ast.include.time {
                Some(query::TimeFilter::Day(d)) => Some(d),
                _ => None,
            }
        } else {
            None
        };
        if let Some(d) = active_date {
            dates_set.insert(d);
        }
        // Show only the most-recent N dates so the TAGS section has room.
        // If the user has navigated to an older date, pin it in (in
        // descending position) so the active highlight has a home.
        let mut dates: Vec<chrono::NaiveDate> = dates_set
            .iter()
            .rev()
            .take(SIDEBAR_DATE_LIMIT)
            .copied()
            .collect();
        if let Some(active) = active_date {
            if !dates.contains(&active) {
                let pos = dates.iter().position(|d| *d < active).unwrap_or(dates.len());
                dates.insert(pos, active);
            }
        }

        let date_font = Font::from_typeface(&self.typeface, SIDEBAR_DATE_FONT_SIZE * scale);
        let (_, dm) = date_font.metrics();
        let mouse_x = self.mouse_pos.0;
        let mouse_y = self.mouse_pos.1;

        let mut y = sidebar_y;
        for d in dates {
            let date_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
            if date_rect.top > height {
                break;
            }
            let date_active = self.view == Query::date(d);
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
            y += date_h + item_gap + date_gap;
        }
        let _ = (item_h, indent); // sized constants reserved for future per-context rows

        // ----- TAGS section -----
        // Sourced from the DB tags table (alphabetical). Skipped when empty
        // so we don't render a stranded header.
        let tags: Vec<String> = self
            .db
            .as_ref()
            .and_then(|db| db.all_tags().ok())
            .unwrap_or_default();
        if !tags.is_empty() {
            // Section gap then "TAGS" header (same styling as CONTEXTS).
            y += date_gap;
            let tag_header_baseline = y + (-hm.ascent);
            canvas.draw_str(
                "TAGS",
                Point::new(pad_x, tag_header_baseline),
                &header_font,
                &header_paint,
            );
            y += header_h;

            for name in tags {
                let row_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
                if row_rect.top > height {
                    break;
                }
                let row_active = self.view.is_solo_tag(&name);
                let row_hovered = mouse_x >= row_rect.left
                    && mouse_x <= row_rect.right
                    && mouse_y >= row_rect.top
                    && mouse_y <= row_rect.bottom;
                if row_active {
                    let mut p = Paint::default();
                    p.set_anti_alias(true);
                    p.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));
                    canvas.draw_round_rect(row_rect, radius, radius, &p);
                } else if row_hovered {
                    let mut p = Paint::default();
                    p.set_anti_alias(true);
                    p.set_color(Color::from_argb(0x18, 0x1c, 0x1c, 0x1c));
                    canvas.draw_round_rect(row_rect, radius, radius, &p);
                }
                let mut tp = Paint::default();
                tp.set_anti_alias(true);
                tp.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
                let baseline = y + (date_h + (-dm.ascent) - dm.descent) * 0.5;
                canvas.draw_str(
                    format!("#{name}"),
                    Point::new(pad_x, baseline),
                    &date_font,
                    &tp,
                );
                self.last_sidebar_tag_rects.push((name, row_rect));
                y += date_h + item_gap;
            }
        }
    }

    fn render_search_popup(&mut self, canvas: &Canvas, width: f32) {
        if self.search.is_none() {
            self.last_search_input_rect = None;
            return;
        }
        let scale = self.font_scale;
        let pad = SEARCH_PAD * scale;
        let radius = SEARCH_RADIUS * scale;
        let popup_w = (SEARCH_WIDTH * scale).min(width - pad * 2.0).max(200.0);
        let popup_x = (width - popup_w) * 0.5;
        let popup_y = SEARCH_TOP * scale;

        let input_h = SEARCH_INPUT_H * scale;
        let result_h = SEARCH_RESULT_H * scale;
        let query = self
            .search
            .as_ref()
            .map(|s| s.input.text().to_string())
            .unwrap_or_default();
        // Only recompute results when the @-mention popup is closed —
        // otherwise the in-progress `@<query>` token would churn the list
        // on every keystroke. Cache survives until the mention popup
        // closes (commit or cancel), at which point the next render
        // refreshes against the now-final query text.
        let results: Vec<Uuid> = if self.mention_popup.is_none() {
            let fresh = self.search_results(&query);
            if let Some(state) = self.search.as_mut() {
                state.cached_results = fresh.clone();
            }
            fresh
        } else {
            self.search
                .as_ref()
                .map(|s| s.cached_results.clone())
                .unwrap_or_default()
        };
        let visible = results.len().min(SEARCH_MAX_VISIBLE);
        let popup_h = input_h + (visible as f32) * result_h + pad * 2.0;

        // Drop shadow.
        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        shadow.set_color(Color::from_argb(0x40, 0, 0, 0));
        shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 14.0, false));
        canvas.draw_round_rect(
            Rect::new(popup_x, popup_y + 4.0, popup_x + popup_w, popup_y + popup_h + 4.0),
            radius,
            radius,
            &shadow,
        );

        // Background card.
        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(Color::WHITE);
        let card = Rect::new(popup_x, popup_y, popup_x + popup_w, popup_y + popup_h);
        canvas.draw_round_rect(card, radius, radius, &bg);

        let mut border = Paint::default();
        border.set_anti_alias(true);
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(1.0);
        border.set_color(Color::from_rgb(0xc8, 0xc0, 0xb0));
        canvas.draw_round_rect(card, radius, radius, &border);

        // Input row: drive the TextBox directly so caret, selection, arrow
        // nav, word jumps, and line edges all work natively.
        let input_x = popup_x + pad;
        let input_y = popup_y + pad;
        let input_w = popup_w - pad * 2.0;
        if let Some(state) = self.search.as_mut() {
            state.input.tick(canvas, input_x, input_y, input_w, true, true);
        }
        self.last_search_input_rect = Some(Rect::new(
            input_x,
            input_y,
            input_x + input_w,
            input_y + input_h - SEARCH_PAD * scale,
        ));

        // Placeholder text rendered ON TOP only when the input is empty.
        if query.is_empty() {
            let input_font =
                Font::from_typeface(&self.typeface, SEARCH_INPUT_FONT_SIZE * scale);
            let (_, im) = input_font.metrics();
            let baseline = input_y + (-im.ascent);
            let mut hint = Paint::default();
            hint.set_anti_alias(true);
            hint.set_color(Color::from_rgb(0xa8, 0xa0, 0x90));
            canvas.draw_str(
                "Search…",
                Point::new(input_x, baseline),
                &input_font,
                &hint,
            );
        }

        // Divider between input and results.
        let div_y = popup_y + pad + input_h - 4.0 * scale;
        let mut div = Paint::default();
        div.set_anti_alias(false);
        div.set_color(Color::from_argb(0x30, 0x40, 0x40, 0x40));
        canvas.draw_line(
            (popup_x + pad, div_y),
            (popup_x + popup_w - pad, div_y),
            &div,
        );

        // Result rows.
        let result_font =
            Font::from_typeface(&self.typeface, SEARCH_RESULT_FONT_SIZE * scale);
        let date_font =
            Font::from_typeface(&self.typeface, SEARCH_DATE_FONT_SIZE * scale);
        let (_, rm) = result_font.metrics();
        let mut date_paint = Paint::default();
        date_paint.set_anti_alias(true);
        date_paint.set_color(Color::from_rgb(0x80, 0x78, 0x68));
        let mut row_paint = Paint::default();
        row_paint.set_anti_alias(true);
        row_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        let selected = self.search.as_ref().map(|s| s.selected).unwrap_or(0);
        let mut row_y = popup_y + pad + input_h;
        for (i, &id) in results.iter().take(SEARCH_MAX_VISIBLE).enumerate() {
            let is_selected = i == selected.min(visible.saturating_sub(1));
            if is_selected {
                let mut sel = Paint::default();
                sel.set_anti_alias(true);
                sel.set_color(Color::from_argb(0x40, 0x4a, 0x90, 0xe2));
                canvas.draw_rect(
                    Rect::new(
                        popup_x + pad * 0.5,
                        row_y,
                        popup_x + popup_w - pad * 0.5,
                        row_y + result_h,
                    ),
                    &sel,
                );
            }

            if let Some(cell) = self.cell(id) {
                let date_label = format_date_label(local_date_for_ms(cell.timestamp));
                let baseline = row_y + (result_h + (-rm.ascent) - rm.descent) * 0.5;
                let date_w = date_font
                    .measure_str(&date_label, Some(&date_paint))
                    .0;
                canvas.draw_str(
                    &date_label,
                    Point::new(popup_x + pad, baseline),
                    &date_font,
                    &date_paint,
                );
                let snippet = result_snippet(&cell.full_text(), &query);
                canvas.draw_str(
                    &snippet,
                    Point::new(popup_x + pad + date_w + 12.0 * scale, baseline),
                    &result_font,
                    &row_paint,
                );
            }
            row_y += result_h;
        }

        if visible == 0 && !query.trim().is_empty() {
            let baseline = popup_y + pad + input_h + (result_h + (-rm.ascent) - rm.descent) * 0.5;
            let mut empty_paint = Paint::default();
            empty_paint.set_anti_alias(true);
            empty_paint.set_color(Color::from_rgb(0x90, 0x88, 0x78));
            canvas.draw_str(
                "no matches",
                Point::new(popup_x + pad, baseline),
                &result_font,
                &empty_paint,
            );
        }
    }

    fn render_tag_context_menu(&mut self, canvas: &Canvas) {
        let Some(menu) = self.tag_context_menu.as_ref() else {
            self.last_tag_menu_delete_rect = None;
            return;
        };
        let scale = self.font_scale;
        let pad = 6.0 * scale;
        let row_h = 26.0 * scale;
        let menu_w = 160.0 * scale;
        let menu_h = row_h + pad * 2.0;
        let rect = Rect::new(
            menu.anchor_x,
            menu.anchor_y,
            menu.anchor_x + menu_w,
            menu.anchor_y + menu_h,
        );
        // Drop shadow.
        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        shadow.set_color(Color::from_argb(0x40, 0, 0, 0));
        shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 8.0, false));
        canvas.draw_round_rect(
            Rect::new(rect.left, rect.top + 2.0, rect.right, rect.bottom + 2.0),
            6.0 * scale,
            6.0 * scale,
            &shadow,
        );
        // Background card.
        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(Color::WHITE);
        canvas.draw_round_rect(rect, 6.0 * scale, 6.0 * scale, &bg);
        let mut border = Paint::default();
        border.set_anti_alias(true);
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(1.0);
        border.set_color(Color::from_rgb(0xc0, 0xc0, 0xc0));
        canvas.draw_round_rect(rect, 6.0 * scale, 6.0 * scale, &border);
        // "Delete tag" row.
        let row_rect = Rect::new(
            rect.left + pad * 0.5,
            rect.top + pad,
            rect.right - pad * 0.5,
            rect.top + pad + row_h,
        );
        let mouse_x = self.mouse_pos.0;
        let mouse_y = self.mouse_pos.1;
        let hovered = mouse_x >= row_rect.left
            && mouse_x <= row_rect.right
            && mouse_y >= row_rect.top
            && mouse_y <= row_rect.bottom;
        if hovered {
            let mut hp = Paint::default();
            hp.set_anti_alias(true);
            hp.set_color(Color::from_argb(0x20, 0xc0, 0x30, 0x30));
            canvas.draw_round_rect(row_rect, 4.0 * scale, 4.0 * scale, &hp);
        }
        let font = Font::from_typeface(&self.typeface, 13.0 * scale);
        let (_, m) = font.metrics();
        let baseline = row_rect.top + (row_h + (-m.ascent) - m.descent) * 0.5;
        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0xc0, 0x30, 0x30));
        canvas.draw_str(
            format!("Delete tag #{}", menu.name),
            Point::new(row_rect.left + pad, baseline),
            &font,
            &text_paint,
        );
        self.last_tag_menu_delete_rect = Some(row_rect);
    }

    /// Right-click menu rendered over a People-page row. Two actions:
    /// Rename (always enabled) and Delete person (disabled when the
    /// entity has a backing cell or any `kept://` references; the row
    /// shows the count so the user knows what's blocking).
    fn render_people_context_menu(&mut self, canvas: &Canvas) {
        let Some(menu) = self.people_context_menu.as_ref() else {
            self.last_people_menu_rename_rect = None;
            self.last_people_menu_delete_rect = None;
            return;
        };
        let scale = self.font_scale;
        let pad = 6.0 * scale;
        let row_h = 26.0 * scale;
        let menu_w = 200.0 * scale;
        let menu_h = row_h * 2.0 + pad * 2.0;
        let rect = Rect::new(
            menu.anchor_x,
            menu.anchor_y,
            menu.anchor_x + menu_w,
            menu.anchor_y + menu_h,
        );
        // Drop shadow.
        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        shadow.set_color(Color::from_argb(0x40, 0, 0, 0));
        shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 8.0, false));
        canvas.draw_round_rect(
            Rect::new(rect.left, rect.top + 2.0, rect.right, rect.bottom + 2.0),
            6.0 * scale,
            6.0 * scale,
            &shadow,
        );
        // Background.
        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(Color::WHITE);
        canvas.draw_round_rect(rect, 6.0 * scale, 6.0 * scale, &bg);
        let mut border = Paint::default();
        border.set_anti_alias(true);
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(1.0);
        border.set_color(Color::from_rgb(0xc0, 0xc0, 0xc0));
        canvas.draw_round_rect(rect, 6.0 * scale, 6.0 * scale, &border);

        let font = Font::from_typeface(&self.typeface, 13.0 * scale);
        let (_, m) = font.metrics();
        let mouse_x = self.mouse_pos.0;
        let mouse_y = self.mouse_pos.1;
        let mut text_paint = Paint::default();
        text_paint.set_anti_alias(true);
        text_paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));
        let mut dim_paint = Paint::default();
        dim_paint.set_anti_alias(true);
        dim_paint.set_color(Color::from_rgb(0xa0, 0x9a, 0x90));
        let mut delete_paint = Paint::default();
        delete_paint.set_anti_alias(true);
        delete_paint.set_color(Color::from_rgb(0xc0, 0x30, 0x30));

        // Rename row.
        let rename_rect = Rect::new(
            rect.left + pad * 0.5,
            rect.top + pad,
            rect.right - pad * 0.5,
            rect.top + pad + row_h,
        );
        let rename_hovered = mouse_x >= rename_rect.left
            && mouse_x <= rename_rect.right
            && mouse_y >= rename_rect.top
            && mouse_y <= rename_rect.bottom;
        if rename_hovered {
            let mut hp = Paint::default();
            hp.set_anti_alias(true);
            hp.set_color(Color::from_argb(0x18, 0x1c, 0x1c, 0x1c));
            canvas.draw_round_rect(rename_rect, 4.0 * scale, 4.0 * scale, &hp);
        }
        let rename_baseline =
            rename_rect.top + (row_h + (-m.ascent) - m.descent) * 0.5;
        canvas.draw_str(
            "Rename",
            Point::new(rename_rect.left + pad, rename_baseline),
            &font,
            &text_paint,
        );
        self.last_people_menu_rename_rect = Some(rename_rect);

        // Delete row.
        let delete_rect = Rect::new(
            rect.left + pad * 0.5,
            rect.top + pad + row_h,
            rect.right - pad * 0.5,
            rect.top + pad + row_h * 2.0,
        );
        let delete_hovered = menu.deletable
            && mouse_x >= delete_rect.left
            && mouse_x <= delete_rect.right
            && mouse_y >= delete_rect.top
            && mouse_y <= delete_rect.bottom;
        if delete_hovered {
            let mut hp = Paint::default();
            hp.set_anti_alias(true);
            hp.set_color(Color::from_argb(0x20, 0xc0, 0x30, 0x30));
            canvas.draw_round_rect(delete_rect, 4.0 * scale, 4.0 * scale, &hp);
        }
        let delete_baseline =
            delete_rect.top + (row_h + (-m.ascent) - m.descent) * 0.5;
        let label = if menu.deletable {
            "Delete person".to_string()
        } else {
            match menu.ref_count {
                Some(n) if n > 0 => format!("Delete person ({n} refs)"),
                _ => "Delete person (in use)".to_string(),
            }
        };
        let label_paint = if menu.deletable {
            &delete_paint
        } else {
            &dim_paint
        };
        canvas.draw_str(
            label,
            Point::new(delete_rect.left + pad, delete_baseline),
            &font,
            label_paint,
        );
        if menu.deletable {
            self.last_people_menu_delete_rect = Some(delete_rect);
        } else {
            self.last_people_menu_delete_rect = None;
        }
    }

    fn render_mention_popup(&self, canvas: &Canvas) {
        let Some(popup) = self.mention_popup.as_ref() else {
            return;
        };
        let (anchor_x, anchor_y_below) = match popup.source {
            MentionSource::Cell { cell_id, bullet_id } => {
                let Some(cell) = self.cell(cell_id) else {
                    return;
                };
                let Some((x, y)) = cell.anchor_doc_pos(bullet_id, popup.anchor_byte) else {
                    return;
                };
                // Doc-space → window-space: subtract scroll.
                (x, y - self.scroll_y)
            }
            MentionSource::SearchBar => {
                let Some(state) = self.search.as_ref() else { return };
                let Some((x, _)) = state.input.doc_position_of_byte(popup.anchor_byte) else {
                    return;
                };
                let Some((_, bot)) = state.input.line_y_band_of_byte(popup.anchor_byte) else {
                    return;
                };
                (x, bot)
            }
        };

        let scale = self.font_scale;
        let popup_w = MENTION_POPUP_WIDTH * scale;
        let row_h = MENTION_POPUP_ROW_H * scale;
        let pad = MENTION_POPUP_PAD * scale;
        let radius = MENTION_POPUP_RADIUS * scale;

        let candidates = self.person_mention_candidates();
        let items = filter_mentions(&candidates, &popup.query);
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
        let candidates = self.person_mention_candidates();
        let Some(p) = self.mention_popup.as_mut() else {
            return;
        };
        let count = filter_mentions(&candidates, &p.query)
            .len()
            .min(MENTION_POPUP_MAX_VISIBLE);
        if count == 0 {
            return;
        }
        let cur = p.selected.min(count - 1) as i32;
        let new = ((cur + delta).rem_euclid(count as i32)) as usize;
        p.selected = new;
    }

    /// Commit the highlighted person from the `@`-mention popup: replace
    /// the `@query` typeahead text with the person's title and attach a
    /// `kept://<source-cell-id>` link spanning it. Recorded as one undo.
    fn commit_mention(&mut self) -> bool {
        let Some(popup) = self.mention_popup.take() else {
            return false;
        };
        let entries = self.person_entries();
        let candidates = self.person_mention_candidates();
        let filtered = filter_mentions(&candidates, &popup.query);
        let Some(selected) = filtered.get(popup.selected) else {
            return true;
        };
        let chosen_name = selected.0.clone();
        let Some((_, source_id)) = entries.iter().find(|(n, _)| n == &chosen_name) else {
            return true;
        };
        let source_id = *source_id;

        let start = popup.anchor_byte;
        let end = start + 1 + popup.query.len();

        match popup.source {
            MentionSource::Cell { cell_id, bullet_id } => {
                let pre = match self.cell(cell_id) {
                    Some(c) => c.snapshot(),
                    None => return true,
                };
                let url = format!("kept://{}", source_id);
                if let Some(c) = self.cell_mut(cell_id) {
                    c.replace_focused_with_link(bullet_id, start..end, chosen_name, url);
                }
                if let Some(c) = self.cell(cell_id) {
                    let post = c.snapshot();
                    if !pre.doc_eq(&post) {
                        let saved_focused = self.focused;
                        self.focused = Some(cell_id);
                        self.record_edit(pre, post);
                        self.focused = saved_focused.or(Some(cell_id));
                    }
                }
            }
            MentionSource::SearchBar => {
                // Replace `@<query>` with `@<Title_Cased_With_Underscores>` so
                // the resulting query string is readable and parses cleanly
                // (entity tokens can't contain whitespace). The executor's
                // resolver normalizes both sides — strips whitespace and
                // underscores, lowercases — so `@Patrick_Foy` matches the
                // person cell titled "Patrick Foy".
                let slug = chosen_name
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("_");
                let replacement = format!("@{slug}");
                if let Some(state) = self.search.as_mut() {
                    let txt = state.input.text();
                    if start <= txt.len() && end <= txt.len() {
                        // Build new text: prefix + replacement + suffix.
                        let prefix = &txt[..start];
                        let suffix = &txt[end..];
                        let new_text = format!("{prefix}{replacement}{suffix}");
                        state.input.replace_text(new_text);
                        state
                            .input
                            .set_caret_at(start + replacement.len());
                    }
                    state.selected = 0;
                }
            }
        }
        self.coalesce_break = true;
        true
    }

    // ----- Search popup (Ctrl/Cmd+K) -----

    fn open_search(&mut self) {
        if self.search.is_some() {
            return;
        }
        let mut input = TextBox::new(self.typeface.clone(), String::new());
        input.set_font_scale(self.font_scale);
        self.search = Some(SearchState {
            input,
            selected: 0,
            cached_results: Vec::new(),
        });
        // Drop other transient overlays so they don't compete for input.
        self.mention_popup = None;
        self.cell_context_menu = None;
    }

    fn close_search_cancel(&mut self) {
        if self.search.take().is_some() {
            // Doc area was never replaced; nothing to restore.
            self.coalesce_break = true;
        }
    }

    /// Enter on the search popup: jump to the highlighted result. View
    /// becomes that cell's date and the cell is focused. Empty / no-match
    /// input just closes the popup.
    fn close_search_commit(&mut self, in_other_pane: bool) {
        let Some(state) = self.search.take() else { return };
        let query = state.input.text().to_string();
        let results = self.search_results(&query);
        let Some(&id) = results.get(state.selected) else {
            self.coalesce_break = true;
            return;
        };
        if let Some(cell) = self.cell(id) {
            let target_date = local_date_for_ms(cell.timestamp);
            if in_other_pane {
                // Alt+Enter: split-or-swap to the other pane, then nav there.
                self.open_in_other_pane(Query::date(target_date));
            } else {
                // Plain Enter: deliberate nav in active pane. Push the
                // pre-search view onto history so Cmd+[ returns to where
                // the user was before searching. No-op when target equals
                // current.
                self.push_view(Query::date(target_date));
            }
        }
        self.focused = Some(id);
        self.editing = false;
        self.coalesce_break = true;
        self.pending_caret_scroll = true;
    }

    /// Top-N matching cell IDs for the popup result list. Parses `query`
    /// through the language, runs the executor, and sorts most-recent first.
    fn search_results(&self, query: &str) -> Vec<Uuid> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let ast = query::parse(query);
        let ctx = query::MatchContext {
            today: local_date_for_ms(now_epoch_ms()),
            person_targets: query::resolve_persons(
                &ast.include.entities,
                &self.entity_alias_index,
                &self.entity_title_fallback,
            ),
            person_excludes: query::resolve_persons(
                &ast.exclude.entities,
                &self.entity_alias_index,
                &self.entity_title_fallback,
            ),
        };
        let mut hits: Vec<&Cell> = self
            .cells
            .iter()
            .filter(|c| query::matches(&ast, c, &ctx))
            .collect();
        hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        hits.into_iter().map(|c| c.id).collect()
    }

    fn search_move(&mut self, delta: i32) {
        let Some(state) = self.search.as_ref() else { return };
        let query = state.input.text().to_string();
        let results = self.search_results(&query);
        let count = results.len().min(SEARCH_MAX_VISIBLE);
        if count == 0 {
            return;
        }
        let cur = state.selected.min(count - 1) as i32;
        let new = ((cur + delta).rem_euclid(count as i32)) as usize;
        if let Some(s) = self.search.as_mut() {
            s.selected = new;
        }
    }

    /// Cmd/Ctrl+C while the search popup has focus: copy the input's
    /// selection. No fallback to "copy the whole query" — that's atypical
    /// for an input field and not worth the ambiguity.
    fn search_copy_to_clipboard(&mut self) -> bool {
        let Some(state) = self.search.as_ref() else { return false };
        let text = state.input.copy_primary_selection();
        if text.is_empty() {
            return false;
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text);
        }
        true
    }

    fn search_cut_to_clipboard(&mut self) -> bool {
        let Some(state) = self.search.as_mut() else { return false };
        let text = state.input.cut_primary_selection();
        if text.is_empty() {
            return false;
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text);
        }
        true
    }

    fn search_paste_from_clipboard(&mut self) -> bool {
        let Some(cb) = self.clipboard.as_mut() else { return false };
        let text = match cb.get_text() {
            Ok(t) => t,
            Err(_) => return false,
        };
        if text.is_empty() {
            return false;
        }
        // Search input is single-line; strip newlines on paste.
        let cleaned: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
        if let Some(state) = self.search.as_mut() {
            state.input.paste(&cleaned);
        }
        true
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
                    prev_view.clone(),
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
                self.view = prev_view.clone();
                bump_focused_edited = false;
            }
            UndoOp::RenamePersonEntity {
                entity_id,
                prev_name,
                cell_title_change,
                ..
            } => {
                if let Some(db) = self.db.as_mut() {
                    if let Err(e) = db.rename_person_entity(*entity_id, prev_name) {
                        eprintln!("kept: undo rename_person_entity failed: {e}");
                    }
                }
                if let Some((cell_id, prev_title, _)) = cell_title_change {
                    if let Some(cell) = self.cell_mut(*cell_id) {
                        if let Some(title) = cell.title_mut() {
                            title.replace_text(prev_title.clone());
                        }
                    }
                    self.mark_cell_dirty(*cell_id);
                }
                self.refresh_entities();
                bump_focused_edited = false;
            }
            UndoOp::CreateCelllessEntity { entity_id, .. } => {
                if let Some(db) = self.db.as_mut() {
                    if let Err(e) = db.delete_entity(*entity_id) {
                        eprintln!("kept: undo create-entity (delete) failed: {e}");
                    }
                }
                self.refresh_entities();
                bump_focused_edited = false;
            }
            UndoOp::DeleteCelllessEntity {
                entity_id,
                name,
                is_active,
                created_at,
            } => {
                if let Some(db) = self.db.as_mut() {
                    if let Err(e) = db.insert_person_entity_with_id(
                        *entity_id,
                        name,
                        *is_active,
                        *created_at,
                    ) {
                        eprintln!("kept: undo delete-entity (insert) failed: {e}");
                    }
                }
                self.refresh_entities();
                bump_focused_edited = false;
            }
            UndoOp::SetEntityActive { entity_id, prev, .. } => {
                if let Some(db) = self.db.as_mut() {
                    let _ = db.set_entity_active(*entity_id, *prev);
                }
                self.refresh_entities();
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
                self.apply_rotation(*closed_id, *new_end_time, new_context, new_view.clone());
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
                self.view = new_view.clone();
                bump_focused_edited = false;
            }
            UndoOp::RenamePersonEntity {
                entity_id,
                new_name,
                cell_title_change,
                ..
            } => {
                if let Some(db) = self.db.as_mut() {
                    if let Err(e) = db.rename_person_entity(*entity_id, new_name) {
                        eprintln!("kept: redo rename_person_entity failed: {e}");
                    }
                }
                if let Some((cell_id, _, new_title)) = cell_title_change {
                    if let Some(cell) = self.cell_mut(*cell_id) {
                        if let Some(title) = cell.title_mut() {
                            title.replace_text(new_title.clone());
                        }
                    }
                    self.mark_cell_dirty(*cell_id);
                }
                self.refresh_entities();
                bump_focused_edited = false;
            }
            UndoOp::CreateCelllessEntity {
                entity_id,
                name,
                created_at,
            } => {
                if let Some(db) = self.db.as_mut() {
                    // Add Person always creates an active entity, so a
                    // redo restores it active. (If the user toggled it
                    // inactive between create and undo, that's a separate
                    // SetEntityActive op on the stack and stays around
                    // for its own redo.)
                    if let Err(e) = db.insert_person_entity_with_id(
                        *entity_id,
                        name,
                        true,
                        *created_at,
                    ) {
                        eprintln!("kept: redo create-entity failed: {e}");
                    }
                }
                self.refresh_entities();
                bump_focused_edited = false;
            }
            UndoOp::DeleteCelllessEntity { entity_id, .. } => {
                if let Some(db) = self.db.as_mut() {
                    if let Err(e) = db.delete_entity(*entity_id) {
                        eprintln!("kept: redo delete-entity failed: {e}");
                    }
                }
                self.refresh_entities();
                bump_focused_edited = false;
            }
            UndoOp::SetEntityActive { entity_id, new, .. } => {
                if let Some(db) = self.db.as_mut() {
                    let _ = db.set_entity_active(*entity_id, *new);
                }
                self.refresh_entities();
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
                        // (e.g., AST views stay put).
                        let prev_view = self.view.clone();
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
                self.view = new_view.clone();
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
                self.view = prev_view.clone();
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

    fn insert_cell_after_focused(&mut self, kind: NewCellKind) -> bool {
        // If the user is viewing a closed context, jump to the current open
        // one before inserting. The note belongs in "today," not in history.
        let auto_switched = self.ensure_writable_context();
        // No-op if the focused cell is empty — the new-cell shortcut shouldn't
        // pile up empties. Skip when we just auto-switched: the destination's focused
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
        let mut new_cell = match kind {
            NewCellKind::Plain => Cell::new(self.typeface.clone(), String::new()),
            NewCellKind::Outline => Cell::new_outline(self.typeface.clone()),
            NewCellKind::PopPop => Cell::new_poppop(self.typeface.clone()),
            NewCellKind::Table => Cell::new_table(self.typeface.clone()),
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

    /// "Surface as reference" — create a new Reference cell pointing at
    /// `target` and insert it where any new cell would land: at "now," in
    /// the current writable context (auto-rotating to a fresh context when
    /// the user has been idle, just like `insert_cell_after_focused`).
    /// Focuses the new reference. Returns true on insert.
    fn surface_as_reference(&mut self, target: ReferenceTarget) -> bool {
        // If the user is viewing a closed context, jump to the current open
        // one before inserting — surfacing belongs in "today," not history.
        let _auto_switched = self.ensure_writable_context();
        // Idle rotation: same baseline logic as insert_cell_after_focused.
        let now = now_epoch_ms();
        let idle_ms = IDLE_CONTEXT_THRESHOLD.as_millis() as i64;
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
        let mut new_cell = Cell::new_reference(self.typeface.clone(), target);
        new_cell.set_font_scale(self.font_scale);
        new_cell.context_hint_id = self.writable_context_id();
        let new_id = new_cell.id;
        let snapshot = new_cell.snapshot();
        self.insert_cell_sorted(new_cell);
        self.focused = Some(new_id);
        // References never enter edit mode; just focus.
        self.editing = false;
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

    /// Right-click handler. Currently the only surface that does anything
    /// useful is sidebar tag rows: right-clicking on a tag with zero cells
    /// opens a one-item "Delete tag" context menu. Returns true if the
    /// click was consumed.
    pub fn right_click(&mut self, x: f32, y: f32) -> bool {
        // Right-clicking anywhere first closes any open menu.
        let was_open = self.tag_context_menu.take().is_some()
            | self.people_context_menu.take().is_some()
            | self.cell_context_menu.take().is_some();
        // Sidebar: tag rows offer a context menu (delete-tag for empty
        // tags). Everything else in the sidebar dismisses an open menu.
        if x < SIDEBAR_WIDTH * self.font_scale {
            for (name, rect) in self.last_sidebar_tag_rects.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    let count = self
                        .db
                        .as_ref()
                        .and_then(|db| db.cells_with_tag(&name).ok())
                        .map(|v| v.len())
                        .unwrap_or(usize::MAX);
                    if count == 0 {
                        self.tag_context_menu = Some(TagContextMenu {
                            name,
                            anchor_x: x,
                            anchor_y: y,
                        });
                        return true;
                    }
                    return was_open;
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
        if matches!(self.view.view_kind, ViewKind::People) {
            let doc_y = y + self.scroll_y;
            for (entity_id, rect) in self.last_people_row_rects.clone() {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    self.open_people_context_menu(entity_id, x, y);
                    return true;
                }
            }
            return was_open;
        }
        if matches!(
            self.view.view_kind,
            ViewKind::Ast | ViewKind::Context(_) | ViewKind::Entity(_)
        ) {
            let doc_y = y + self.scroll_y;
            if let Some(cell_id) = self.find_cell_at(x, doc_y) {
                // If the right-click landed on a specific bullet inside an
                // outline cell, capture (bullet_id, snippet) so the menu
                // can offer "Copy bullet sub-tree as embed."
                let (bullet_id, bullet_snippet) = self
                    .cell(cell_id)
                    .and_then(|c| match &c.kind {
                        CellKind::Outline(oc) => oc
                            .bullet_at_doc_y(doc_y)
                            .map(|(id, text)| (Some(id), Some(snippet(&text)))),
                        _ => None,
                    })
                    .unwrap_or((None, None));
                self.cell_context_menu = Some(CellContextMenu {
                    cell_id,
                    anchor_x: x,
                    anchor_y: y,
                    bullet_id,
                    bullet_snippet,
                });
                return true;
            }
        }
        was_open
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, modifiers: &Modifiers) -> bool {
        // Tag context menu intercepts left-clicks: clicking the "Delete
        // tag" row deletes; clicking anywhere else closes the menu and
        // falls through to normal click routing.
        if self.tag_context_menu.is_some() {
            if let Some(rect) = self.last_tag_menu_delete_rect {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.tag_context_menu.take() {
                        if let Some(db) = self.db.as_mut() {
                            if let Err(e) = db.delete_tag(&menu.name) {
                                eprintln!("kept: delete_tag failed for {}: {}", menu.name, e);
                            }
                        }
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
            if let Some(rect) = self.last_people_menu_rename_rect {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.people_context_menu.take() {
                        self.start_people_rename(menu.entity_id);
                    }
                    return true;
                }
            }
            if let Some(rect) = self.last_people_menu_delete_rect {
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

        // Search popup is modal-ish: while open, clicks inside the input
        // route to its TextBox; clicks elsewhere are swallowed so the cells
        // beneath don't get focus changes / selections.
        if self.search.is_some() {
            self.search_dragging = false;
            if let Some(rect) = self.last_search_input_rect {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.search_dragging = true;
                    if let Some(state) = self.search.as_mut() {
                        return state.input.mouse_down(x, y, modifiers, true);
                    }
                }
            }
            return true;
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
            self.focus_mode = false;
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
                    app.open_in_other_pane(q)
                } else {
                    app.push_view(q)
                }
            };
            // PAGES section first (top of the sidebar).
            for (kind, rect) in self.last_sidebar_pages_rects.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.cell_context_menu = None;
                    return match kind {
                        PageKind::People => open(self, Query::people()),
                    };
                }
            }
            // Context rows first (they're indented inside dates so their bbox
            // overlaps date row gaps in some edge cases — context wins).
            for (id, rect) in self.last_sidebar_rects.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.cell_context_menu = None;
                    return open(self, Query::context(id));
                }
            }
            for (date, rect) in self.last_sidebar_date_rects.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.cell_context_menu = None;
                    return open(self, Query::date(date));
                }
            }
            for (name, rect) in self.last_sidebar_tag_rects.clone() {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    self.cell_context_menu = None;
                    return open(self, Query::tag(name));
                }
            }
            self.cell_context_menu = None;
            return false;
        }

        // Click is in the pane area — make the clicked pane active before
        // computing doc-space coords. (doc_y depends on the pane's scroll.)
        if let Some(idx) = self.pane_at(x, y) {
            self.set_active_pane(idx);
        }

        let doc_y = y + self.scroll_y;

        // Cell context menu dispatch: click on Delete row deletes;
        // click anywhere else dismisses and falls through to normal
        // cell routing.
        if self.cell_context_menu.is_some() {
            if let Some(rect) = self.last_cell_menu_delete_rect {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        self.delete_cell_by_id(menu.cell_id);
                    }
                    return true;
                }
            }
            // "Surface as reference" — create a new reference cell at "now"
            // pointing to the right-clicked cell.
            if let Some(rect) = self.last_cell_menu_surface_rect {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        self.surface_as_reference(ReferenceTarget::WholeCell(menu.cell_id));
                    }
                    return true;
                }
            }
            // "Surface '<snippet>' as reference" — sub-tree target. Only
            // present when the menu was opened over a specific bullet.
            if let Some(rect) = self.last_cell_menu_surface_subtree_rect {
                if x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom {
                    if let Some(menu) = self.cell_context_menu.take() {
                        if let Some(bid) = menu.bullet_id {
                            self.surface_as_reference(ReferenceTarget::Subtree {
                                cell_id: menu.cell_id,
                                bullet_id: bid,
                            });
                        }
                    }
                    return true;
                }
            }
            self.cell_context_menu = None;
        }

        // Entity-page active/inactive toggle (always present in entity
        // view; rect is None outside it).
        if let ViewKind::Entity(eid) = self.view.view_kind {
            if let Some(rect) = self.last_entity_active_toggle_rect {
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
        if let Some(rect) = self.last_entity_create_button_rect {
            if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                // TODO(chunk-2): trigger create_backing_cell_for_entity.
                return true;
            }
        }

        // Entity-page "REFERENCED IN" embed cards: clicking any of them
        // navigates to the source cell at its real timeline location.
        // Snapshotted into a local first to avoid the &self borrow on
        // `last_entity_page_ref_rects` outliving the &mut self call to
        // `navigate_to_reference`.
        if matches!(self.view.view_kind, ViewKind::Entity(_)) {
            let hit = self
                .last_entity_page_ref_rects
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
        if matches!(self.view.view_kind, ViewKind::People) {
            // "Show inactive" header toggle wins over everything else
            // on the People page, including any in-progress rename
            // / add input — toggling the filter shouldn't lose typed
            // text but shouldn't get masked by the input rects either.
            if let Some(rect) = self.last_people_show_inactive_toggle_rect {
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
                    .last_people_row_rects
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
                if let Some(ar) = self.last_people_add_rect {
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
            if let Some(rect) = self.last_people_add_rect {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    self.start_people_add();
                    return true;
                }
            }
            for (entity_id, rect) in self.last_people_row_rects.clone() {
                if x >= rect.left && x <= rect.right && doc_y >= rect.top && doc_y <= rect.bottom {
                    return self.push_view(Query::entity(entity_id));
                }
            }
            return false;
        }

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
        let result = match self.cell_mut(target) {
            Some(cell) => cell.mouse_down(x, doc_y, modifiers, editing),
            None => false,
        };
        // The cell's mouse_down may have stashed a `kept://...` (or other)
        // URL because the click landed on a link. Drain it here and route
        // through the navigation policy that lives on `KeptApp`.
        let pending = self
            .cell_mut(target)
            .and_then(|c| c.take_pending_link_url());
        if let Some(url) = pending {
            self.handle_link_click(&url);
        }
        result
    }

    /// Click-on-embed → land on the original. Mirrors `close_search_commit`
    /// (app.rs:close_search_commit) for the cell-level case, plus an extra
    /// step for `Subtree` targets that focuses the specific bullet inside
    /// the target outline. No-op when the target cell is gone (the embed
    /// already showed a "[deleted]" placeholder).
    fn navigate_to_reference(&mut self, target: ReferenceTarget) {
        let cell_id = target.cell_id();
        let target_date = match self.cell(cell_id) {
            Some(c) => local_date_for_ms(c.timestamp),
            None => return,
        };
        self.push_view(Query::date(target_date));
        self.focused = Some(cell_id);
        self.editing = false;
        self.pending_caret_scroll = true;
        // Subtree target: drill into the outline cell and focus the
        // specific bullet. Cell-level focus alone is the fallback if the
        // bullet is missing or the cell isn't an outline anymore.
        if let ReferenceTarget::Subtree { bullet_id, .. } = target {
            if let Some(c) = self.cell_mut(cell_id) {
                if let CellKind::Outline(oc) = &mut c.kind {
                    let _ = oc.set_focused_bullet(bullet_id);
                }
            }
        }
    }

    /// Resolve a clicked link URL. `kept://<uuid>` routes by uuid kind:
    /// entity match → entity page; cell match → date view + focus the
    /// cell; neither → drop (don't shell out, that produces a useless
    /// OS error). Other URLs hand off to `cell::open_url` (xdg-open).
    fn handle_link_click(&mut self, url: &str) {
        if let Some(rest) = url.strip_prefix("kept://") {
            if let Ok(uuid) = Uuid::parse_str(rest) {
                if self.entities.iter().any(|e| e.id == uuid) {
                    self.push_view(Query::entity(uuid));
                    return;
                }
                if let Some(cell) = self.cell(uuid) {
                    let target_date = local_date_for_ms(cell.timestamp);
                    self.push_view(Query::date(target_date));
                    self.focused = Some(uuid);
                    self.editing = false;
                    self.pending_caret_scroll = true;
                    return;
                }
                eprintln!("kept: dangling kept:// link: {url}");
                return;
            }
        }
        cell::open_url(url);
    }

    pub fn mouse_drag_to(&mut self, x: f32, y: f32) -> bool {
        // Divider drag wins — recompute split_ratio relative to the pane
        // area (sidebar's right edge → window's right edge).
        if self.dragging_divider && self.panes.len() >= 2 {
            let pane_area_left = self.panes[0].last_rect.left;
            let pane_area_right = self.panes[self.panes.len() - 1].last_rect.right;
            let pane_area_w = (pane_area_right - pane_area_left).max(1.0);
            self.split_ratio = ((x - pane_area_left) / pane_area_w).clamp(SPLIT_MIN, SPLIT_MAX);
            return true;
        }
        if self.search_dragging {
            if let Some(state) = self.search.as_mut() {
                return state.input.mouse_drag_to(x, y);
            }
        }
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
        if self.dragging_divider {
            self.dragging_divider = false;
            return true;
        }
        if self.search_dragging {
            self.search_dragging = false;
            if let Some(state) = self.search.as_mut() {
                return state.input.mouse_up();
            }
        }
        if let Some(id) = self.dragging_cell.take() {
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

/// Draw `name` starting at `origin`, painting bytes in `match_indices` with
/// `match_paint` and the rest with `dim_paint`. Matches are byte indices; the
/// fuzzy matcher emits matches at lowercase-byte boundaries so this is safe
/// for ASCII names.
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
        track.set_color(Color::from_argb(0xff, 0x4a, 0x90, 0xe2));
    } else {
        track.set_color(Color::from_argb(0x60, 0x90, 0x88, 0x7a));
    }
    canvas.draw_round_rect(rect, radius, radius, &track);
    if hovered {
        let mut overlay = Paint::default();
        overlay.set_anti_alias(true);
        overlay.set_color(Color::from_argb(0x18, 0x1c, 0x1c, 0x1c));
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
    knob.set_color(Color::WHITE);
    canvas.draw_circle((cx, cy), knob_r, &knob);
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

/// Trim a cell's full text to a single-line snippet centered around the
/// match. If `query`'s residual text appears, show ~40 chars before + the
/// match + ~40 after. Falls back to the leading window for queries that are
/// entirely structured (`#tag`, `today`, etc. — no text to find).
/// Normalize an entity's `display_name` into the form the resolver's
/// title fallback substring-matches against. Same shape as
/// `query::normalize_entity_token` — lowercase, strip whitespace and
/// underscores — so a query token and a fallback entry compare cleanly.
fn normalize_title_for_fallback(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn result_snippet(text: &str, query: &str) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    // Pull the residual-text tail out of the parsed AST so structured
    // tokens (#tag / @person / today / dates) don't drive snippet centering.
    let residual = query::parse(query).text;
    let lower = flat.to_lowercase();
    let needle = residual.to_lowercase();
    let center = if needle.is_empty() {
        0
    } else {
        lower.find(&needle).unwrap_or(0)
    };
    let pre = SEARCH_SNIPPET_LEN / 2;
    let start_chars = center.saturating_sub(pre);
    let end_chars = (start_chars + SEARCH_SNIPPET_LEN).min(flat.chars().count());
    let mut iter = flat.chars();
    let snippet: String = iter
        .by_ref()
        .skip(start_chars)
        .take(end_chars - start_chars)
        .collect();
    let prefix = if start_chars > 0 { "…" } else { "" };
    let suffix = if end_chars < flat.chars().count() { "…" } else { "" };
    format!("{prefix}{snippet}{suffix}")
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

/// Build a `CellKind` clone for a reference cache. Returns None when the
/// source kind is itself a Reference (chained refs are short-circuited
/// upstream and shouldn't reach this path). Mirrors text + links + per-row
/// flags; uses the supplied typeface so the new widgets render in the
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_camelcase_boundary_outranks_inside_word_match() {
        // Both "PatrickFoy" and "PeterCarr" are spaceless person names —
        // the @-mention convention. With `pc` as the query, `PeterCarr`
        // should win because its `C` is a camelCase boundary, while
        // `PatrickFoy`'s `c` sits mid-word in "patrick".
        let pf = fuzzy_score("pc", "PatrickFoy").expect("matches");
        let pc = fuzzy_score("pc", "PeterCarr").expect("matches");
        assert!(
            pc.0 > pf.0,
            "PeterCarr ({}) must outrank PatrickFoy ({})",
            pc.0,
            pf.0,
        );
    }

    #[test]
    fn fuzzy_space_separator_still_wins() {
        // "pc" vs "Peter Carr" (with space) used to win on the
        // post-separator bonus. Make sure that still holds — the fix
        // adds a camelCase path; it doesn't remove the space path.
        let pf = fuzzy_score("pc", "PatrickFoy").expect("matches");
        let pc = fuzzy_score("pc", "Peter Carr").expect("matches");
        assert!(pc.0 > pf.0);
    }

    #[test]
    fn filter_mentions_orders_camelcase_correctly() {
        let cands = vec![
            ("PatrickFoy".to_string(), true),
            ("PeterCarr".to_string(), true),
        ];
        let ranked = filter_mentions(&cands, "pc");
        assert_eq!(ranked[0].0, "PeterCarr");
    }

    #[test]
    fn fuzzy_initials_beat_inside_word_contiguous() {
        // "th" against "TrevorHickey" (T + camelCase H) should outrank
        // "ThomasOttaway" (T + adjacent h inside "Thomas") even though
        // the latter has a contiguous-match bonus and the former does
        // not. Word-boundary bonus must dominate inside-word contiguity
        // for initials-style queries to feel right.
        let trevor = fuzzy_score("th", "TrevorHickey").expect("matches");
        let thomas = fuzzy_score("th", "ThomasOttaway").expect("matches");
        assert!(
            trevor.0 > thomas.0,
            "TrevorHickey ({}) must outrank ThomasOttaway ({})",
            trevor.0,
            thomas.0,
        );
    }

    #[test]
    fn fuzzy_inactive_is_downweighted() {
        // Active candidate beats inactive on a single-char query even
        // though the alphabetical tiebreak alone would put PatrickFoy
        // first. Inactive still appears in the result list — just last.
        let cands = vec![
            ("PatrickFoy".to_string(), false), // inactive
            ("PeterCarr".to_string(), true),   // active
        ];
        let ranked = filter_mentions(&cands, "p");
        assert_eq!(ranked[0].0, "PeterCarr");
        assert_eq!(ranked[1].0, "PatrickFoy");
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
