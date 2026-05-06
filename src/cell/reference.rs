//! `ReferenceCell` — a read-only embed of another cell or bullet sub-tree.
//!
//! Holds only the pointer (`ReferenceTarget`) plus its own layout box and
//! a persistent preview cache. Renders via the app layer, which has the
//! full cell list available to resolve the target. Click navigates to the
//! source; mouse-drag inside the cached preview supports text selection.

use skia_safe::Typeface;
use uuid::Uuid;

use super::Cell;

/// What a reference cell points at — either a whole cell, or a specific
/// bullet sub-tree within an outline (root bullet + its contiguous deeper-
/// depth followers). Cheap to copy; passed by value through navigation /
/// snapshot paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceTarget {
    WholeCell(Uuid),
    Subtree { cell_id: Uuid, bullet_id: Uuid },
}

impl ReferenceTarget {
    /// The cell id (whether or not we further narrow to a bullet). Used by
    /// navigation and dangling-ref lookups.
    pub fn cell_id(self) -> Uuid {
        match self {
            ReferenceTarget::WholeCell(id) => id,
            ReferenceTarget::Subtree { cell_id, .. } => cell_id,
        }
    }
}

/// A read-only window onto another cell (or sub-tree). Renders the
/// target's content in place; click navigates to the original. Stores
/// only the pointer + its own layout box — never caches target content
/// directly. Lookup happens on the shared `Vec<Cell>` at render time.
pub struct ReferenceCell {
    pub target: ReferenceTarget,
    /// Carried so the cache can reconstitute body widgets. The reference
    /// itself doesn't render text directly.
    typeface: Typeface,
    /// Layout box for the embed itself, written by the app layer's render
    /// dispatch. Distinct from the target's own `(x_origin, y_origin)` —
    /// those belong to the target's render at its real timeline location.
    x_origin: f32,
    y_origin: f32,
    width: f32,
    height: f32,
    font_scale: f32,
    /// Persistent cached preview of the target's content. Owns its own
    /// selection state across frames so drag-select inside the embed
    /// works exactly like in any other cell. Rebuilt whenever
    /// `cache_source_edited_at` doesn't match the source's `edited_at`,
    /// so edits at the original propagate. None when the target is
    /// missing or chained (a placeholder is drawn instead).
    /// Boxed because `Cell` contains `CellKind` which contains
    /// `ReferenceCell` — without indirection the type would be infinitely
    /// sized.
    cache: Option<Box<Cell>>,
    /// Source's `edited_at` when `cache` was last rebuilt.
    cache_source_edited_at: Option<i64>,
}

impl ReferenceCell {
    pub fn new(typeface: Typeface, target: ReferenceTarget) -> Self {
        Self {
            target,
            typeface,
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            font_scale: 1.0,
            cache: None,
            cache_source_edited_at: None,
        }
    }

    /// True if the cache needs rebuilding for `source_edited_at` (None
    /// means the target is gone — invalidate the cache).
    pub fn cache_is_stale_for(&self, source_edited_at: Option<i64>) -> bool {
        match (source_edited_at, self.cache_source_edited_at, &self.cache) {
            // Target gone. Cache should be cleared if it isn't already.
            (None, _, Some(_)) => true,
            (None, _, None) => false,
            // Target present, cache missing.
            (Some(_), _, None) => true,
            // Target present, cache stale.
            (Some(src), Some(cached), Some(_)) => src != cached,
            // Target present, cache built but no edited_at recorded.
            (Some(_), None, Some(_)) => true,
        }
    }

    /// Replace the cache with `new_cache` (built by the caller from the
    /// resolved source data). Updates the staleness key. Pass `None` for
    /// `new_cache` when the target is missing / chained.
    pub fn install_cache(&mut self, new_cache: Option<Cell>, source_edited_at: Option<i64>) {
        self.cache = new_cache.map(Box::new);
        self.cache_source_edited_at = source_edited_at;
    }

    pub fn cache_mut(&mut self) -> Option<&mut Cell> {
        self.cache.as_deref_mut()
    }

    pub fn cache_ref(&self) -> Option<&Cell> {
        self.cache.as_deref()
    }

    #[allow(dead_code)]
    pub fn typeface(&self) -> &Typeface {
        &self.typeface
    }

    pub fn target(&self) -> ReferenceTarget {
        self.target
    }

    #[allow(dead_code)]
    pub fn font_scale(&self) -> f32 {
        self.font_scale
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        self.font_scale = scale;
    }

    #[allow(dead_code)]
    pub fn x_origin(&self) -> f32 {
        self.x_origin
    }

    #[allow(dead_code)]
    pub fn y_origin(&self) -> f32 {
        self.y_origin
    }

    #[allow(dead_code)]
    pub fn width(&self) -> f32 {
        self.width
    }

    #[allow(dead_code)]
    pub fn height(&self) -> f32 {
        self.height
    }

    /// Called by the app's render layer (which has access to the full
    /// cell list) after computing the embed's height. The reference cell
    /// itself can't render — it doesn't have access to its target.
    pub fn set_view_geometry(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.x_origin = x;
        self.y_origin = y;
        self.width = width;
        self.height = height;
    }
}
