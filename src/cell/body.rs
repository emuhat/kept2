//! The `CellBody` trait — single dispatch surface for the five cell kinds.
//!
//! Replaces 200+ hand-written 5-arm `match` blocks across `cell.rs` and
//! `app/mod.rs` with one trait. Default methods key off `for_each_textbox`
//! so multi-textbox containers (outline / table / poppop) inherit
//! `mouse_drag_to`, `mouse_up`, `clear_all_selections`, `link_at_doc_pos`,
//! `take_pending_link_url`, `remove_tags_named`, `all_link_urls_into`,
//! `all_tag_names_into` for free.
//!
//! ### Header / cache descent (asymmetric)
//!
//! `for_each_textbox` iterates the textboxes a cell **owns**. It does NOT
//! descend into:
//! - An outline cell's `reference_header` cache (envelope outlines): owned
//!   by an `EmbeddedReference`, not by the outline. Outline overrides the
//!   methods that need to see header content (e.g. `take_pending_link_url`,
//!   `clear_all_selections`, `link_at_doc_pos`).
//! - A reference cell's cache: similar story. Reference's `for_each_textbox`
//!   *does* descend (the cache is the reference's only content), but
//!   queries that ask about "cell-level metadata" (`all_link_urls_into`,
//!   `all_tag_names_into`, `remove_tags_named`) are overridden as no-ops so
//!   we don't double-count source links/tags or mutate stale cache state.
//!
//! ### Snapshot / restore
//!
//! Deliberately not on the trait. Each variant's snapshot type differs and
//! `CellSnapshot` already routes through the `CellKind` enum. Polymorphic
//! snapshot via `dyn CellBody` would need type erasure that buys nothing.

use std::ops::Range;

use skia_safe::{Canvas, Typeface};
use uuid::Uuid;
use winit::event::{KeyEvent, Modifiers};

use super::TextBox;

pub trait CellBody {
    // ---------------------------------------------------------------------
    // Render & input — required.
    // ---------------------------------------------------------------------

    fn tick(
        &mut self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32;

    fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool;

    fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool;

    // ---------------------------------------------------------------------
    // Multi-textbox iteration — the keystone for the defaults below.
    // ---------------------------------------------------------------------

    /// Visit each `TextBox` this cell body owns, in stable order.
    fn for_each_textbox(&self, f: &mut dyn FnMut(&TextBox));

    /// Mutable visit. The closure gets one `&mut TextBox` at a time; callers
    /// can't observe multiple textboxes simultaneously through this API.
    fn for_each_textbox_mut(&mut self, f: &mut dyn FnMut(&mut TextBox));

    // ---------------------------------------------------------------------
    // Geometry & styling — required (each kind has its own field layout).
    // ---------------------------------------------------------------------

    fn typeface(&self) -> &Typeface;
    fn font_scale(&self) -> f32;
    fn set_font_scale(&mut self, scale: f32);
    fn is_empty(&self) -> bool;
    fn caret_doc_y_band(&self) -> Option<(f32, f32)>;

    // ---------------------------------------------------------------------
    // Focus & navigation — required.
    // ---------------------------------------------------------------------

    fn at_top_edge(&self) -> bool;
    fn at_bottom_edge(&self) -> bool;
    fn place_caret_at_start(&mut self);
    fn place_caret_at_end(&mut self);
    fn select_all_focused(&mut self);
    fn focused_text_and_caret(&self) -> Option<(&str, usize)>;
    fn focused_textbox(&self) -> Option<&TextBox>;

    /// Position of byte `byte` in the cell's focused textbox, in doc-space
    /// `(x, y_below_line)`. Used by the @-mention popup anchor.
    fn anchor_doc_pos_at_focused(&self, byte: usize) -> Option<(f32, f32)>;

    /// Outline-only: id of the currently focused bullet. Returns `None` for
    /// every other kind so callers can ask without matching.
    fn focused_bullet_id(&self) -> Option<Uuid> {
        None
    }

    // ---------------------------------------------------------------------
    // Editing surface — required (focused-element-internal; no bullet ids
    // leak through the trait API).
    // ---------------------------------------------------------------------

    fn copy_text(&self) -> String;
    fn cut_text(&mut self) -> String;
    fn paste_text(&mut self, s: &str);

    /// Paste with authoritative `LinkSpan`s (from the clipboard
    /// payload's structured form). Default routes through
    /// `paste_text`, losing the links — kinds that have a focused
    /// `TextBox` override this to apply the links atomically.
    fn paste_with_links(&mut self, text: &str, _links: &[super::common::LinkSpan]) {
        self.paste_text(text);
    }

    fn replace_at_focused_with_link(
        &mut self,
        range: Range<usize>,
        text: String,
        url: String,
    );
    fn replace_at_focused_with_text(&mut self, range: Range<usize>, text: String);
    fn replace_at_focused_with_tag(&mut self, range: Range<usize>, text: String);

    /// Attach a link span to the cell's "first" textbox (whatever that means
    /// per kind). Used at seed time and by legacy paths; new code should
    /// prefer the focused variant.
    fn add_link_to_first(&mut self, range: Range<usize>, url: String);

    /// Concatenated text for search-indexing / `full_text` round-tripping.
    /// Kind-specific formatting (outline joins with newlines + indent;
    /// table joins with tabs; reference contributes nothing).
    fn full_text(&self) -> String;

    // ---------------------------------------------------------------------
    // Multi-textbox defaults — override only where the semantics genuinely
    // diverge (see header notes at top of file).
    // ---------------------------------------------------------------------

    fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        let mut any = false;
        self.for_each_textbox_mut(&mut |tb| {
            if tb.mouse_drag_to(abs_x, abs_y) {
                any = true;
            }
        });
        any
    }

    fn mouse_up(&mut self) -> bool {
        let mut any = false;
        self.for_each_textbox_mut(&mut |tb| {
            if tb.mouse_up() {
                any = true;
            }
        });
        any
    }

    fn clear_all_selections(&mut self) {
        self.for_each_textbox_mut(&mut |tb| tb.clear_selection());
    }

    fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        let mut hit = false;
        self.for_each_textbox(&mut |tb| {
            if !hit && tb.link_at_doc_pos(abs_x, abs_y) {
                hit = true;
            }
        });
        hit
    }

    fn take_pending_link_url(&mut self) -> Option<String> {
        let mut out: Option<String> = None;
        self.for_each_textbox_mut(&mut |tb| {
            if out.is_none() {
                out = tb.take_pending_link_url();
            }
        });
        out
    }

    fn remove_tags_named(&mut self, name: &str) -> bool {
        let mut changed = false;
        self.for_each_textbox_mut(&mut |tb| {
            if tb.remove_tags_named(name) {
                changed = true;
            }
        });
        changed
    }

    fn all_link_urls_into(&self, out: &mut Vec<String>) {
        self.for_each_textbox(&mut |tb| {
            for l in tb.links() {
                out.push(l.url.clone());
            }
        });
    }

    fn all_tag_names_into(&self, out: &mut Vec<String>) {
        self.for_each_textbox(&mut |tb| {
            for n in tb.all_tag_names() {
                if !n.is_empty() && !out.contains(&n) {
                    out.push(n);
                }
            }
        });
    }
}
