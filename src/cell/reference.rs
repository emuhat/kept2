//! `ReferenceCell` — a read-only embed of another cell or bullet sub-tree.
//!
//! Holds only the pointer (`ReferenceTarget`) plus its own layout box and
//! a persistent preview cache. Renders via the app layer, which has the
//! full cell list available to resolve the target. Click navigates to the
//! source; mouse-drag inside the cached preview supports text selection.

use skia_safe::Typeface;
use uuid::Uuid;

use super::Cell;
use super::TextBox;

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

/// A reference target plus its lazily-rebuilt preview cache. Used both
/// by `ReferenceCell` (top-level read-only embed) and by the envelope
/// outline's `reference_header` slot. Carries no layout — geometry is
/// recorded on the host (cell or outline header band) by the app
/// layer's render pass.
///
/// Boxed `Cell` cache because `Cell` indirectly contains this struct;
/// without indirection the type would be infinitely sized.
pub struct EmbeddedReference {
    pub target: ReferenceTarget,
    /// Persistent cached preview of the target's content. Owns its own
    /// selection state across frames so drag-select inside the embed
    /// works exactly like in any other cell. Rebuilt whenever
    /// `cache_source_edited_at` doesn't match the source's `edited_at`,
    /// so edits at the original propagate. None when the target is
    /// missing (a placeholder is drawn instead).
    cache: Option<Box<Cell>>,
    /// Source's `edited_at` when `cache` was last rebuilt.
    cache_source_edited_at: Option<i64>,
}

impl EmbeddedReference {
    pub fn new(target: ReferenceTarget) -> Self {
        Self {
            target,
            cache: None,
            cache_source_edited_at: None,
        }
    }

    pub fn target(&self) -> ReferenceTarget {
        self.target
    }

    /// True if the cache needs rebuilding for `source_edited_at` (None
    /// means the target is gone — invalidate the cache).
    pub fn cache_is_stale_for(&self, source_edited_at: Option<i64>) -> bool {
        match (source_edited_at, self.cache_source_edited_at, &self.cache) {
            (None, _, Some(_)) => true,
            (None, _, None) => false,
            (Some(_), _, None) => true,
            (Some(src), Some(cached), Some(_)) => src != cached,
            (Some(_), None, Some(_)) => true,
        }
    }

    /// Replace the cache with `new_cache` (built by the caller from the
    /// resolved source data). Updates the staleness key. Pass `None` for
    /// `new_cache` when the target is missing.
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

    /// Take ownership of the cache, leaving the slot empty. Paired with
    /// `attach_cache` to let callers route the cache cell through code
    /// that needs `&mut self` on the host (the borrow on the host is
    /// dropped while the cache is detached). Does not touch the
    /// staleness key — the caller is shuffling, not invalidating.
    pub fn detach_cache(&mut self) -> Option<Box<Cell>> {
        self.cache.take()
    }

    /// Re-install a cache previously taken via `detach_cache`. Caller
    /// is responsible for passing back the same (or freshly rebuilt)
    /// cache; staleness key is left as-is.
    pub fn attach_cache(&mut self, cache: Option<Box<Cell>>) {
        self.cache = cache;
    }
}

/// A read-only window onto another cell (or sub-tree). Renders the
/// target's content in place; click navigates to the original. Stores
/// only the embed pointer + its own layout box — never caches target
/// content directly here. Lookup happens on the shared `Vec<Cell>` at
/// render time, with the cache living on the contained
/// `EmbeddedReference`.
pub struct ReferenceCell {
    head: EmbeddedReference,
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
}

impl ReferenceCell {
    pub fn new(typeface: Typeface, target: ReferenceTarget) -> Self {
        Self {
            head: EmbeddedReference::new(target),
            typeface,
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            font_scale: 1.0,
        }
    }

    /// True if the cache needs rebuilding for `source_edited_at` (None
    /// means the target is gone — invalidate the cache).
    pub fn cache_is_stale_for(&self, source_edited_at: Option<i64>) -> bool {
        self.head.cache_is_stale_for(source_edited_at)
    }

    /// Replace the cache with `new_cache` (built by the caller from the
    /// resolved source data). Updates the staleness key. Pass `None` for
    /// `new_cache` when the target is missing.
    pub fn install_cache(&mut self, new_cache: Option<Cell>, source_edited_at: Option<i64>) {
        self.head.install_cache(new_cache, source_edited_at);
    }

    pub fn cache_mut(&mut self) -> Option<&mut Cell> {
        self.head.cache_mut()
    }

    pub fn cache_ref(&self) -> Option<&Cell> {
        self.head.cache_ref()
    }

    pub fn detach_cache(&mut self) -> Option<Box<Cell>> {
        self.head.detach_cache()
    }

    pub fn attach_cache(&mut self, cache: Option<Box<Cell>>) {
        self.head.attach_cache(cache);
    }

    #[allow(dead_code)]
    pub fn typeface(&self) -> &Typeface {
        &self.typeface
    }

    pub fn target(&self) -> ReferenceTarget {
        self.head.target
    }

    /// Replace the target. Used by snapshot restore. Caller is
    /// responsible for invalidating the cache (the new target may
    /// resolve differently); typically restore() runs immediately
    /// after.
    pub fn set_target(&mut self, target: ReferenceTarget) {
        self.head.target = target;
        self.head.install_cache(None, None);
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

impl super::body::CellBody for ReferenceCell {
    /// References don't render through `Cell::tick` — the app's render
    /// layer resolves the target and draws the embed wrapper itself. Same
    /// behavior as the prior cell.rs `Reference(_) => 0.0` arm.
    fn tick(
        &mut self,
        _canvas: &skia_safe::Canvas,
        _x: f32,
        _y: f32,
        _width: f32,
        _focused: bool,
        _show_caret: bool,
    ) -> f32 {
        0.0
    }

    /// Keys bubble up so app-level arrow nav can step past a reference
    /// cell.
    fn handle_key(
        &mut self,
        _event: &winit::event::KeyEvent,
        _modifiers: &winit::event::Modifiers,
    ) -> bool {
        false
    }

    /// Forward into the cached preview so drag-select inside the embed
    /// works. `editing=false` regardless of the outer state — the cache
    /// is read-only by design.
    fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &winit::event::Modifiers,
        _editing: bool,
    ) -> bool {
        if let Some(cache) = self.head.cache_mut() {
            cache.mouse_down(abs_x, abs_y, modifiers, false);
        }
        true
    }

    /// Forward drag motion to the cache so kind-specific drag state
    /// (outline's BulletRange promotion, table's row drag, etc.) gets
    /// updated. The trait default only iterates per-textbox, which is
    /// the wrong shape for multi-bullet selection inside an embedded
    /// outline.
    fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        match self.head.cache_mut() {
            Some(cache) => cache.mouse_drag_to(abs_x, abs_y),
            None => false,
        }
    }

    fn mouse_up(&mut self) -> bool {
        match self.head.cache_mut() {
            Some(cache) => cache.mouse_up(),
            None => false,
        }
    }

    /// Descend into the cache's textboxes (title + body). When the cache
    /// is missing (dangling target), iterate nothing — the placeholder
    /// has no editable content.
    fn for_each_textbox(&self, f: &mut dyn FnMut(&TextBox)) {
        if let Some(cache) = self.head.cache_ref() {
            if let Some(title) = cache.title() {
                f(title);
            }
            cache.kind.body().for_each_textbox(f);
        }
    }

    fn for_each_textbox_mut(&mut self, f: &mut dyn FnMut(&mut TextBox)) {
        if let Some(cache) = self.head.cache_mut() {
            if let Some(title) = cache.title_mut() {
                f(title);
            }
            cache.kind.body_mut().for_each_textbox_mut(f);
        }
    }

    fn typeface(&self) -> &Typeface {
        &self.typeface
    }

    fn font_scale(&self) -> f32 {
        self.font_scale
    }

    fn set_font_scale(&mut self, scale: f32) {
        ReferenceCell::set_font_scale(self, scale);
    }

    /// References are never "empty" — the pointer itself is content even
    /// when the target is missing.
    fn is_empty(&self) -> bool {
        false
    }

    fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        None
    }

    /// References hold no caret; both edges always trivially satisfied so
    /// arrow keys cross out of a reference immediately.
    fn at_top_edge(&self) -> bool {
        true
    }

    fn at_bottom_edge(&self) -> bool {
        true
    }

    fn place_caret_at_start(&mut self) {}
    fn place_caret_at_end(&mut self) {}

    /// Forward to the cache so the user can select-all inside an embed
    /// preview (read-only selection, not edit).
    fn select_all_focused(&mut self) {
        if let Some(cache) = self.head.cache_mut() {
            cache.select_all_focused();
        }
    }

    fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        None
    }

    fn focused_textbox(&self) -> Option<&TextBox> {
        None
    }

    fn anchor_doc_pos_at_focused(&self, _byte: usize) -> Option<(f32, f32)> {
        None
    }

    /// Copy forwards into the cache's selection so Cmd+C inside an embed
    /// returns the dragged text.
    fn copy_text(&self) -> String {
        match self.head.cache_ref() {
            Some(cache) => cache.copy_text(),
            None => String::new(),
        }
    }

    fn cut_text(&mut self) -> String {
        String::new()
    }

    fn paste_text(&mut self, _s: &str) {}

    fn replace_at_focused_with_link(
        &mut self,
        _range: std::ops::Range<usize>,
        _text: String,
        _url: String,
    ) {
    }

    fn replace_at_focused_with_text(
        &mut self,
        _range: std::ops::Range<usize>,
        _text: String,
    ) {
    }

    fn replace_at_focused_with_tag(
        &mut self,
        _range: std::ops::Range<usize>,
        _text: String,
    ) {
    }

    fn add_link_to_first(&mut self, _range: std::ops::Range<usize>, _url: String) {}

    /// References contribute nothing to search-indexable text; search
    /// would otherwise return the same content twice (once for the
    /// source, once per embed).
    fn full_text(&self) -> String {
        String::new()
    }

    // ----- Overrides that prevent the defaults from leaking cache content
    // into cell-level metadata queries. Without these, an `all_link_urls`
    // pass would double-count source links via every Reference pointing
    // at the source.

    fn all_link_urls_into(&self, _out: &mut Vec<String>) {}
    fn all_tag_names_into(&self, _out: &mut Vec<String>) {}

    /// Removing tags from a Reference would mutate the cache without
    /// touching the source. The cache then drifts from the source until
    /// the next staleness rebuild. Safer to no-op.
    fn remove_tags_named(&mut self, _name: &str) -> bool {
        false
    }
}
