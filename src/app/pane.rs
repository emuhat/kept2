//! Per-pane render pipeline: layout, header bar, body dispatcher,
//! focus chrome, scroll finalization. Hot path for every frame.
//!
//! Lives as a child of `app`, so it can reach into `KeptApp`'s
//! private state directly. Only `tick_pane` is `pub(super)` — it's
//! the single entry point `app::mod` calls per pane per frame.
//!
//! Cell-stream / entity / people body rendering still lives in
//! `mod.rs` (`render_cell_stream`, `render_entity_page`,
//! `render_people_page`). This file's dispatcher (`render_pane_body`)
//! routes to them by view kind.

use skia_safe::{BlurStyle, Canvas, Font, MaskFilter, Paint, PaintStyle, Point, Rect, Typeface};
use uuid::Uuid;

use crate::cell::TextBox;
use crate::query;

use super::{
    CELL_GAP, DOC_BOTTOM_PAD, FOCUS_MODE_PAD, FOCUS_PAD, FOCUS_RADIUS, FOCUS_RING_ALPHA,
    FOCUS_RING_ALPHA_EDIT, FOCUS_SHADOW_ALPHA, FOCUS_SHADOW_BLUR, FOCUS_SHADOW_DY,
    FOCUS_STROKE, FOCUS_STROKE_EDIT, KeptApp, MARGIN_TOP, MARGIN_X, Query, ViewKind,
    fit_text_ellipsized, format_date_label, local_date_for_ms,
    search::{result_snippet, search_results},
};

/// Browser-style "URL bar" sitting at the top of each pane, showing
/// the current view's query text. Read-only in the v0 mockup — wiring
/// it to actually drive navigation comes later.
pub(super) const PANE_HEADER_H: f32 = 36.0;
/// Visual padding inside the URL pill (between the pill's edge and
/// the query text).
const PANE_HEADER_PAD_X: f32 = 12.0;
/// Vertical gap between the header band and the pane's top edge.
const PANE_HEADER_INSET_Y: f32 = 6.0;

/// Height of one result row in the URL-bar dropdown (logical px,
/// scaled by `font_scale`). Matches the search popup's row height
/// so the visual feel is identical.
const HEADER_RESULT_H: f32 = 32.0;
/// Font size for the snippet half of a result row.
const HEADER_RESULT_FONT_SIZE: f32 = 13.0;
/// Font size for the date prefix of a result row.
const HEADER_DATE_FONT_SIZE: f32 = 12.0;
/// Max number of result rows shown in the dropdown.
const HEADER_MAX_VISIBLE: usize = 8;

/// Per-pane URL-bar / search-suggestions state. Bundles everything
/// the header pill owns: the editable textbox, focus, the currently
/// highlighted result, cached results, and the view that the
/// textbox text was last synced from.
///
/// When `focused` is false the pill displays the current view's
/// `query_display_text(...)`. When `focused` is true the user is
/// typing freely: results pop under the pill, Arrow keys move
/// `selected`, Enter commits the selection like the old Ctrl+K
/// popup did. Esc and clicks outside blur.
/// One row in the URL-bar suggestion dropdown. Either a synthetic
/// entry that navigates to a non-cell destination (currently just
/// the entity page when the query is exactly `@person`) or a cell
/// match from the search executor.
#[derive(Clone, Copy)]
pub(super) enum HeaderResultEntry {
    /// "Page · <display_name>" — Enter / click jumps to the entity
    /// page (`Query::entity(uuid)`). Always rendered first when
    /// present.
    EntityPage(Uuid),
    /// A specific cell — Enter / click lands on `Query::cell(uuid)`.
    Cell(Uuid),
}

pub(super) struct PaneHeader {
    pub(super) textbox: TextBox,
    pub(super) focused: bool,
    /// Highlighted result row in `cached_results`, if any. `None`
    /// means "no row selected" — the default state; Enter in that
    /// state commits the typed text as a filter view (browser
    /// URL-bar feel). `Some(i)` means a row was explicitly picked
    /// with Down/Up arrow; Enter then commits that row's
    /// destination (entity page or cell).
    pub(super) selected: Option<usize>,
    /// Result list from the last render; reused for arrow nav,
    /// Enter commit, and result-row click dispatch. Stable while
    /// the @-mention popup is open over the pill so the dropdown
    /// doesn't churn on every keystroke of the `@<query>` token.
    pub(super) cached_results: Vec<HeaderResultEntry>,
    /// The `view` value the textbox was last synced for. Lets the
    /// frame loop skip re-running `query_display_text` when
    /// nothing's changed. `None` forces a sync on the next frame
    /// (initial state, after Esc / click-outside).
    pub(super) synced_view: Option<Query>,
}

impl PaneHeader {
    pub(super) fn new(typeface: Typeface) -> Self {
        Self {
            textbox: TextBox::new(typeface, String::new()),
            focused: false,
            selected: None,
            cached_results: Vec::new(),
            synced_view: None,
        }
    }

    /// Move the selection within the cached result count.
    /// `delta = +1` (Down) walks `None → Some(0) → Some(1) → … →
    /// Some(count-1) → None` and wraps. `delta = -1` (Up) walks the
    /// other way. The `None` rest stop lets the user dismiss a
    /// highlight without exiting focus.
    pub(super) fn move_selection(&mut self, delta: i32) {
        let count = self.cached_results.len().min(HEADER_MAX_VISIBLE);
        if count == 0 {
            self.selected = None;
            return;
        }
        // Encode `None` as `count` so the wrap math is uniform; map
        // back at the end.
        let cur = match self.selected {
            Some(i) => i.min(count - 1) as i32,
            None => count as i32,
        };
        let total = count as i32 + 1; // +1 for the `None` slot
        let next = (cur + delta).rem_euclid(total);
        self.selected = if next == count as i32 {
            None
        } else {
            Some(next as usize)
        };
    }

    /// Drop focus + reset the suggestion list. Caller invalidates
    /// `synced_view` so the next render re-syncs the pill text from
    /// the view summary.
    pub(super) fn blur(&mut self) {
        self.focused = false;
        self.cached_results.clear();
        self.selected = None;
        self.synced_view = None;
    }
}

/// Per-pane geometry, populated by `prepare_pane_layout` and consumed
/// by the body / header / scroll passes. Pure data — no mutation
/// after `prepare_pane_layout` returns.
pub(super) struct PaneLayout {
    /// Pane index in `KeptApp::panes`. Currently unused by the
    /// sub-render passes (they read state via Deref-to-active-pane),
    /// but kept for future passes that need to address the pane
    /// explicitly without relying on `active_pane` being swapped.
    #[allow(dead_code)]
    pub(super) pane_idx: usize,
    pub(super) pane_rect: Rect,
    pub(super) pane_h: f32,
    /// Left edge of the cell column (after MARGIN_X or FOCUS_MODE_PAD).
    pub(super) cells_left: f32,
    /// Outer width of a cell card (used as the right edge for the
    /// section-header rule).
    pub(super) outer_cell_width: f32,
    /// Usable content width inside a cell card.
    pub(super) content_width: f32,
}

/// Doc-space rectangle for the focused cell. Used by the card
/// backdrop (drawn before body content) and the focus ring (drawn
/// after); both read from the same struct so they stay in lockstep.
/// `(x, y, w, h)` covers the **body** content area (right of the
/// bar); `bar_left_dx` is how far to extend leftward to reach the
/// bar's left edge — the focus ring uses it to wrap the bar; the
/// backdrop ignores it (the bar is its own rounded shape).
#[derive(Clone, Copy)]
pub(super) struct FocusedCellGeom {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) w: f32,
    pub(super) h: f32,
    pub(super) bar_left_dx: f32,
}

impl KeptApp {
    /// Render a single pane. With `active_pane` swapped to `pane_idx`
    /// by the caller, all `self.X` field accesses (Deref) resolve to
    /// this pane. Pane geometry comes from
    /// `self.panes[pane_idx].last_rect`, populated by `layout_panes`.
    pub(super) fn tick_pane(&mut self, canvas: &Canvas, pane_idx: usize, _height: f32) {
        let layout = self.prepare_pane_layout(pane_idx);

        // Scratch view paints a distinct page background over this
        // pane's rect — the page hue is the at-a-glance signal that
        // you're in the throwaway space. Done before clip + scroll
        // translate so the whole pane (header band included) reads
        // as one tinted surface.
        if matches!(self.panes[pane_idx].view.view_kind, ViewKind::Scratch) {
            let mut bg = Paint::default();
            bg.set_anti_alias(false);
            bg.set_color(crate::color::bg_scratch_page());
            canvas.draw_rect(layout.pane_rect, &bg);
        }

        // Clip to this pane's rect so over-wide content / focus shadows
        // can't bleed across the divider into the other pane.
        // Translate into document space (doc y=0 → window y = -scroll_y)
        // so all sub-render passes paint in doc-coords.
        canvas.save();
        canvas.clip_rect(layout.pane_rect, None, true);
        canvas.translate((0.0, -self.pane_mut().scroll_y));

        // For cell-stream views, `render_pane_body` runs the two-phase
        // render internally: record cells to a Picture (which gives us
        // each cell's *this-pane* y via the running accumulator), then
        // draw backdrop → replay picture → draw ring with the
        // freshly-computed `FocusedCellGeom`. Entity / People pages
        // don't have focus chrome so they paint straight to `canvas`.
        let final_y = self.render_pane_body(canvas, &layout);

        canvas.restore();

        // Header sits in window-space at the top of the pane,
        // painted opaque AFTER the body so scrolled content tucks
        // under it. The body has already reserved `PANE_HEADER_H`
        // worth of top inset (baked into `MARGIN_TOP`).
        self.render_pane_header(canvas, &layout);

        self.finalize_pane_scroll(canvas, &layout, final_y);
    }

    pub(super) fn prepare_pane_layout(&mut self, pane_idx: usize) -> PaneLayout {
        // Kinetic decay step (no-op when wheel is still active or
        // velocity is below the floor).
        self.step_kinetic(pane_idx);

        // Clamp scroll using last frame's max_scroll before drawing this frame.
        self.pane_mut().scroll_y = self.pane_mut().scroll_y.clamp(0.0, self.pane_mut().max_scroll);

        let pane_rect = self.panes[pane_idx].last_rect;
        let pane_left = pane_rect.left;
        let pane_right = pane_rect.right;
        let pane_h = pane_rect.height();

        let scale = self.font_scale;
        // Single-cell view pulls the cell out near the pane's left edge
        // with smaller pad so it visually expands to fill the pane;
        // every other view uses MARGIN_X on both sides.
        let single_cell = matches!(self.pane().view.view_kind, ViewKind::Cell(_));
        let (cells_left, outer_cell_width) = if single_cell {
            let left = pane_left + FOCUS_MODE_PAD * scale;
            let outer = (pane_right - left - FOCUS_MODE_PAD * scale).max(80.0);
            (left, outer)
        } else {
            let left = pane_left + MARGIN_X;
            let outer = (pane_right - left - MARGIN_X).max(80.0);
            (left, outer)
        };
        let content_width = outer_cell_width.max(60.0);

        // Focused-cell geometry isn't captured here anymore — it used
        // to read `cell.y_origin()` which is a single field shared
        // across panes, so multi-pane setups would draw focus chrome
        // at whichever pane rendered most recently. The two-phase
        // render inside `render_cell_stream` now computes a
        // pane-local geometry post-record and threads it directly to
        // the backdrop / ring paints.

        PaneLayout {
            pane_idx,
            pane_rect,
            pane_h,
            cells_left,
            outer_cell_width,
            content_width,
        }
    }

    /// Browser-style URL bar mockup at the top of each pane.
    /// Currently read-only: shows the query text for whatever view
    /// the pane is on. The pill is rendered in window-space (sits
    /// above the scrolling content).
    fn render_pane_header(&mut self, canvas: &Canvas, layout: &PaneLayout) {
        let pane_rect = layout.pane_rect;
        let pane_idx = layout.pane_idx;
        let band = Rect::new(
            pane_rect.left,
            pane_rect.top,
            pane_rect.right,
            pane_rect.top + PANE_HEADER_H,
        );
        // Opaque background so doc content scrolled to the top
        // hides cleanly under the band. Sourced from
        // `pane_header_bg` so the header tone can be tuned without
        // pulling the URL-bar dropdown border along with it.
        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(crate::color::pane_header_bg());
        canvas.draw_rect(band, &bg);
        // Hairline separator along the band's bottom edge.
        let mut sep = Paint::default();
        sep.set_anti_alias(true);
        sep.set_color(crate::color::dark_alpha(0x18));
        canvas.draw_line(
            (band.left, band.bottom - 0.5),
            (band.right, band.bottom - 0.5),
            &sep,
        );

        // URL pill — rounded rect spanning the band horizontally.
        // Card-colored fill so it pops out of the panel-colored band.
        let pill_h = PANE_HEADER_H - 2.0 * PANE_HEADER_INSET_Y;
        // Smaller side inset than the body's `MARGIN_X` so the pill
        // spans most of the band — browser URL bars stretch nearly
        // edge-to-edge.
        let side_inset = PANE_HEADER_INSET_Y;
        let pill_rect = Rect::new(
            pane_rect.left + side_inset,
            pane_rect.top + PANE_HEADER_INSET_Y,
            pane_rect.right - side_inset,
            pane_rect.top + PANE_HEADER_INSET_Y + pill_h,
        );
        let mut pill_fill = Paint::default();
        pill_fill.set_anti_alias(true);
        pill_fill.set_color(crate::color::bg_card());
        let pill_radius = pill_h * 0.5;
        let pill_rr = skia_safe::RRect::new_rect_xy(pill_rect, pill_radius, pill_radius);
        canvas.draw_rrect(&pill_rr, &pill_fill);
        // Thin outline so the pill reads as input-like.
        let mut pill_stroke = Paint::default();
        pill_stroke.set_anti_alias(true);
        pill_stroke.set_style(PaintStyle::Stroke);
        pill_stroke.set_stroke_width(1.0);
        pill_stroke.set_color(crate::color::dark_alpha(0x22));
        canvas.draw_rrect(&pill_rr, &pill_stroke);

        // Record the pill rect so click dispatch can route a click
        // inside it to the textbox.
        self.hit_tests_builder.pane_headers.push((pane_idx, pill_rect));

        // Sync the textbox text from the view's query summary, but
        // only when the view itself has changed since the last sync
        // — `query_display_text` walks entities / cells, so we
        // don't want to re-run it on every frame just to discover
        // nothing changed. Also gated on focus so the user's
        // in-progress typing isn't clobbered.
        let header_focused = self.panes[pane_idx].header.focused;
        if !header_focused {
            let current_view = self.panes[pane_idx].view.clone();
            let needs_sync = self.panes[pane_idx]
                .header
                .synced_view
                .as_ref()
                .map_or(true, |v| v != &current_view);
            if needs_sync {
                let synced = self.query_display_text(pane_idx);
                self.panes[pane_idx].header.textbox.replace_text(synced);
                self.panes[pane_idx].header.synced_view = Some(current_view);
            }
        }
        let scale = self.font_scale;
        // Vertically center the textbox's body line inside the pill.
        // TextBox draws its body baseline starting from `y +
        // -ascent`, so reserve the slack above the line.
        let body_font = Font::new(self.typeface.clone(), 18.0 * scale);
        let (_, fm) = body_font.metrics();
        let line_h = -fm.ascent + fm.descent;
        let extra = (pill_h - line_h).max(0.0) * 0.5;
        let inner_x = pill_rect.left + PANE_HEADER_PAD_X;
        let inner_y = pill_rect.top + extra;
        let inner_w = (pill_rect.width() - 2.0 * PANE_HEADER_PAD_X).max(0.0);
        let tb = &mut self.panes[pane_idx].header.textbox;
        tb.set_font_scale(scale);
        tb.tick(canvas, inner_x, inner_y, inner_w, header_focused, header_focused);

        // While focused with a non-empty query, drop a result
        // dropdown under the pill — same shape as the old Ctrl+K
        // popup. Drawn last so it overlays anything else in the
        // pane band (it lives in window-space, after the body).
        if header_focused {
            self.render_pane_header_results(canvas, pill_rect, pane_idx);
        } else {
            // Make sure stale results don't drive click dispatch
            // on the next frame.
            self.panes[pane_idx].header.cached_results.clear();
        }
    }

    /// Drop the result-suggestions list under the focused pill.
    /// Caches the visible result IDs onto `PaneHeader.cached_results`
    /// so click / Enter dispatch can act on the same list the user
    /// is looking at. Records per-row hit rects in
    /// `hit_tests_builder.header_results`.
    fn render_pane_header_results(
        &mut self,
        canvas: &Canvas,
        pill_rect: Rect,
        pane_idx: usize,
    ) {
        let scale = self.font_scale;
        let query = self.panes[pane_idx].header.textbox.text().to_string();
        // While the @-mention popup is open over the pill, keep
        // showing the previously-computed results so the dropdown
        // doesn't churn on every keystroke of the in-progress
        // `@<query>` token.
        let mention_popup_active = self.mention_popup.is_some();
        let results: Vec<HeaderResultEntry> = if !mention_popup_active {
            let mut fresh: Vec<HeaderResultEntry> = Vec::new();
            // If the query is exactly `@person` with nothing else,
            // prepend a synthetic "Page · <display_name>" row that
            // commits to the entity page. Multiple matches all
            // appear, in resolver order.
            if let Some(ids) = self.entity_page_shortcuts_for(&query) {
                fresh.extend(ids.into_iter().map(HeaderResultEntry::EntityPage));
            }
            fresh.extend(
                search_results(
                    &query,
                    &self.document,
                    &self.entities,
                    self.show_inactive_cells,
                )
                .into_iter()
                .map(HeaderResultEntry::Cell),
            );
            self.panes[pane_idx].header.cached_results = fresh.clone();
            fresh
        } else {
            self.panes[pane_idx].header.cached_results.clone()
        };
        let visible = results.len().min(HEADER_MAX_VISIBLE);
        if visible == 0 && query.trim().is_empty() {
            return;
        }

        let row_h = HEADER_RESULT_H * scale;
        let pad = PANE_HEADER_PAD_X;
        let radius = pill_rect.height() * 0.5;

        // Anchor: just below the pill, same horizontal extent.
        let drop_top = pill_rect.bottom + 4.0 * scale;
        let drop_left = pill_rect.left;
        let drop_right = pill_rect.right;
        let drop_h = (visible.max(1) as f32) * row_h + pad;
        let drop_rect = Rect::new(drop_left, drop_top, drop_right, drop_top + drop_h);

        // Card background + outline matching the search popup.
        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        shadow.set_color(crate::color::shadow_menu());
        shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 14.0, false));
        canvas.draw_round_rect(
            Rect::new(drop_rect.left, drop_rect.top + 4.0, drop_rect.right, drop_rect.bottom + 4.0),
            radius,
            radius,
            &shadow,
        );
        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(crate::color::bg_card());
        canvas.draw_round_rect(drop_rect, radius, radius, &bg);
        let mut border = Paint::default();
        border.set_anti_alias(true);
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(2.0);
        border.set_color(crate::color::panel_border_warm());
        canvas.draw_round_rect(drop_rect, radius, radius, &border);

        // Empty-state row when the query produced no matches.
        let result_font =
            Font::from_typeface(&self.typeface, HEADER_RESULT_FONT_SIZE * scale);
        let date_font = Font::from_typeface(&self.typeface, HEADER_DATE_FONT_SIZE * scale);
        let (_, rm) = result_font.metrics();
        if visible == 0 {
            let baseline = drop_rect.top + pad * 0.5 + (row_h + (-rm.ascent) - rm.descent) * 0.5;
            let mut empty = Paint::default();
            empty.set_anti_alias(true);
            empty.set_color(crate::color::text_section_header());
            canvas.draw_str(
                "no matches",
                Point::new(drop_rect.left + pad, baseline),
                &result_font,
                &empty,
            );
            // Reset hit list — no clickable rows this frame.
            self.hit_tests_builder
                .header_results
                .push((pane_idx, Vec::new()));
            return;
        }

        let selected_idx = self.panes[pane_idx].header.selected;
        let mut date_paint = Paint::default();
        date_paint.set_anti_alias(true);
        date_paint.set_color(crate::color::text_muted_warm_soft());
        let mut row_paint = Paint::default();
        row_paint.set_anti_alias(true);
        row_paint.set_color(crate::color::text_primary());

        let mut row_rects: Vec<Rect> = Vec::with_capacity(visible);
        let mut row_y = drop_rect.top + pad * 0.5;
        for (i, entry) in results.iter().take(HEADER_MAX_VISIBLE).enumerate() {
            let row_rect = Rect::new(
                drop_rect.left + pad * 0.5,
                row_y,
                drop_rect.right - pad * 0.5,
                row_y + row_h,
            );
            row_rects.push(row_rect);
            if selected_idx == Some(i) {
                let mut sel = Paint::default();
                sel.set_anti_alias(true);
                sel.set_color(crate::color::accent_blue_selection());
                canvas.draw_rect(row_rect, &sel);
            }
            let baseline = row_y + (row_h + (-rm.ascent) - rm.descent) * 0.5;
            match *entry {
                HeaderResultEntry::EntityPage(entity_id) => {
                    // Entity-page shortcut: dot prefix in muted
                    // warm + "Page" tag in the date column, then
                    // the person's display name in the body column.
                    let tag = "Page";
                    let tag_w = date_font.measure_str(tag, Some(&date_paint)).0;
                    canvas.draw_str(
                        tag,
                        Point::new(drop_rect.left + pad, baseline),
                        &date_font,
                        &date_paint,
                    );
                    let name = self
                        .entities
                        .entities
                        .iter()
                        .find(|e| e.id == entity_id)
                        .map(|e| e.display_name.clone())
                        .unwrap_or_else(|| "(unknown)".to_string());
                    let name_left = drop_rect.left + pad + tag_w + 12.0 * scale;
                    let name_right = drop_rect.right - pad * 0.5;
                    let avail = (name_right - name_left).max(0.0);
                    let fitted =
                        fit_text_ellipsized(&name, avail, &result_font, &row_paint);
                    canvas.draw_str(
                        &fitted,
                        Point::new(name_left, baseline),
                        &result_font,
                        &row_paint,
                    );
                }
                HeaderResultEntry::Cell(cell_id) => {
                    if let Some(cell) = self.document.cell(cell_id) {
                        let date_label = format_date_label(local_date_for_ms(cell.timestamp));
                        let date_w = date_font.measure_str(&date_label, Some(&date_paint)).0;
                        canvas.draw_str(
                            &date_label,
                            Point::new(drop_rect.left + pad, baseline),
                            &date_font,
                            &date_paint,
                        );
                        let snippet = result_snippet(&cell.full_text(), &query);
                        let snippet_left = drop_rect.left + pad + date_w + 12.0 * scale;
                        let snippet_right = drop_rect.right - pad * 0.5;
                        let avail = (snippet_right - snippet_left).max(0.0);
                        let fitted =
                            fit_text_ellipsized(&snippet, avail, &result_font, &row_paint);
                        canvas.draw_str(
                            &fitted,
                            Point::new(snippet_left, baseline),
                            &result_font,
                            &row_paint,
                        );
                    }
                }
            }
            row_y += row_h;
        }
        self.hit_tests_builder
            .header_results
            .push((pane_idx, row_rects));
    }

    /// Resolve entity-page shortcuts for the URL-bar dropdown.
    /// Parses `query` and delegates to the free
    /// `entity_page_shortcuts_for_ast` predicate, supplying the
    /// app's entity caches as the resolution backing store. See
    /// that function for the qualification rule.
    fn entity_page_shortcuts_for(&self, query: &str) -> Option<Vec<Uuid>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return None;
        }
        let ast = query::parse(trimmed);
        entity_page_shortcuts_for_ast(
            &ast,
            &self.entities.alias_index,
            &self.entities.title_fallback,
        )
    }

    /// Single-line summary of a pane's current view, for the URL-bar
    /// mockup. Mirrors the navigation contract: `kept-tag://`/`@`-style
    /// sigils for filter views, free text for ASTs, the cell's title
    /// for single-cell views.
    pub(super) fn query_display_text(&self, pane_idx: usize) -> String {
        let view = &self.panes[pane_idx].view;
        match &view.view_kind {
            ViewKind::Ast => {
                let t = query::to_text(&view.ast);
                if t.is_empty() {
                    "(all)".to_string()
                } else {
                    t
                }
            }
            ViewKind::Context(id) => self
                .document
                .contexts
                .iter()
                .find(|c| c.id == *id)
                .and_then(|c| c.title.clone())
                .map(|t| format!("context:{}", t))
                .unwrap_or_else(|| "context".into()),
            ViewKind::Entity(eid) => self
                .entities
                .entities
                .iter()
                .find(|e| e.id == *eid)
                .map(|e| format!("@{}", e.display_name))
                .unwrap_or_else(|| "@entity".into()),
            ViewKind::People => "people".into(),
            ViewKind::Current => "current".into(),
            ViewKind::Scratch => "scratch".into(),
            ViewKind::Cell(cid) => {
                let label = self.cell(*cid).and_then(|c| {
                    c.heading_title().or_else(|| {
                        let s = c.full_text();
                        let first = s.lines().next().unwrap_or("").trim();
                        if first.is_empty() {
                            None
                        } else {
                            Some(first.to_string())
                        }
                    })
                });
                match label {
                    Some(l) => format!("cell:{}", l),
                    None => format!("cell:{}", cid),
                }
            }
            ViewKind::ThreadList => "threads".into(),
            ViewKind::Thread(tid) => self
                .threads
                .iter()
                .find(|t| t.id == *tid)
                .map(|t| format!("thread:{}", t.title))
                .unwrap_or_else(|| "thread".into()),
        }
    }

    /// View-kind dispatcher: routes to `render_cell_stream` (Ast /
    /// Context / Current / Cell), `render_entity_page` (Entity), or
    /// `render_people_page` (People). Returns the final `y` cursor
    /// used by `finalize_pane_scroll` to compute `doc_height`.
    ///
    /// The `+ CELL_GAP` on the entity / people branches matches the
    /// cell-loop convention (each cell adds CELL_GAP after itself; the
    /// post-body formula in `finalize_pane_scroll` subtracts one
    /// CELL_GAP and adds DOC_BOTTOM_PAD).
    fn render_pane_body(&mut self, canvas: &Canvas, layout: &PaneLayout) -> f32 {
        let scale = self.font_scale;
        let mouse_doc_x = self.mouse_pos.0;
        let mouse_doc_y = self.mouse_pos.1 + self.pane_mut().scroll_y;
        match self.pane_mut().view.view_kind.clone() {
            ViewKind::Ast
            | ViewKind::Context(_)
            | ViewKind::Current
            | ViewKind::Cell(_)
            | ViewKind::Scratch => self.render_cell_stream(canvas, layout),
            ViewKind::Entity(eid) => {
                let h = self.render_entity_page(
                    canvas,
                    eid,
                    layout.cells_left,
                    layout.content_width,
                    scale,
                    mouse_doc_x,
                    mouse_doc_y,
                );
                MARGIN_TOP + h + CELL_GAP
            }
            ViewKind::People => {
                let h = self.render_people_page(
                    canvas,
                    layout.cells_left,
                    layout.content_width,
                    scale,
                    mouse_doc_x,
                    mouse_doc_y,
                );
                MARGIN_TOP + h + CELL_GAP
            }
            ViewKind::ThreadList => {
                let h = self.render_thread_list_page(
                    canvas,
                    layout.cells_left,
                    layout.content_width,
                    scale,
                    mouse_doc_x,
                    mouse_doc_y,
                );
                MARGIN_TOP + h + CELL_GAP
            }
            ViewKind::Thread(tid) => {
                let h = self.render_thread_page(
                    canvas,
                    tid,
                    layout.cells_left,
                    layout.content_width,
                    scale,
                    mouse_doc_x,
                    mouse_doc_y,
                );
                MARGIN_TOP + h + CELL_GAP
            }
        }
    }

    /// Drop shadow + white rounded card painted behind the focused
    /// cell. No-op when `geom` is `None` (cell-stream views compute
    /// it inside `render_cell_stream` via the two-phase render so
    /// the y is always *this* pane's, never polluted by another
    /// pane's `cell.y_origin` overwrite).
    pub(super) fn render_focus_card_backdrop(
        &self,
        canvas: &Canvas,
        geom: Option<FocusedCellGeom>,
    ) {
        let Some(FocusedCellGeom { x: cx, y: cy, w: cw, h: ch, .. }) = geom else {
            return;
        };
        let card_rect = Rect::new(
            cx - FOCUS_PAD,
            cy - FOCUS_PAD,
            cx + cw + FOCUS_PAD,
            cy + ch + FOCUS_PAD,
        );
        // TL/BL flat (the bar supplies those outer corners),
        // TR/BR rounded. Same shape language as outline_rect.
        let r = FOCUS_RADIUS;
        let flat = skia_safe::Vector::new(0.0, 0.0);
        let round = skia_safe::Vector::new(r, r);
        // Drop shadow: blurred dark rect, offset down a few px.
        let mut shadow_paint = Paint::default();
        shadow_paint.set_anti_alias(true);
        shadow_paint.set_color(crate::color::black_alpha(FOCUS_SHADOW_ALPHA));
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
        let shadow_rr = skia_safe::RRect::new_rect_radii(
            shadow_rect,
            &[flat, round, round, flat],
        );
        canvas.draw_rrect(&shadow_rr, &shadow_paint);
        // White card fill.
        let mut fill_paint = Paint::default();
        fill_paint.set_anti_alias(true);
        fill_paint.set_color(crate::color::bg_card());
        let card_rr = skia_safe::RRect::new_rect_radii(
            card_rect,
            &[flat, round, round, flat],
        );
        canvas.draw_rrect(&card_rr, &fill_paint);
    }

    /// Section-header-green accent ring around the focused cell —
    /// subtle when viewing, brighter and thicker when editing. Color
    /// matches the WHAT / WHEN sidebar headers so the active cell
    /// visually rhymes with the sidebar's section accents. Drawn in
    /// every view (including single-cell), since the ring is also
    /// the edit-mode tell — without it the user can't see when
    /// they've entered edit mode. No-op when `geom` is None.
    pub(super) fn render_focus_ring(&self, canvas: &Canvas, geom: Option<FocusedCellGeom>) {
        let Some(FocusedCellGeom { x: cx, y: cy, w: cw, h: ch, bar_left_dx }) = geom
        else {
            return;
        };
        let (stroke, alpha) = if self.pane().editing {
            (FOCUS_STROKE_EDIT, FOCUS_RING_ALPHA_EDIT)
        } else {
            (FOCUS_STROKE, FOCUS_RING_ALPHA)
        };
        let mut focus_paint = Paint::default();
        focus_paint.set_anti_alias(true);
        focus_paint.set_style(PaintStyle::Stroke);
        focus_paint.set_stroke_width(stroke);
        focus_paint.set_color(crate::color::sidebar_section_header_alpha(alpha));
        // Extend leftward across the bar slice so the ring encloses
        // both the bar and the body. The bar isn't padded on its
        // outer edge (its left == `cells_left`), so the FOCUS_PAD
        // inset only applies on the body side. Drawn last in the
        // cell stream, so the ring's left edge paints over the bar.
        let rect = Rect::new(
            cx - bar_left_dx,
            cy - FOCUS_PAD,
            cx + cw + FOCUS_PAD,
            cy + ch + FOCUS_PAD,
        );
        let rr = skia_safe::RRect::new_rect_xy(rect, FOCUS_RADIUS, FOCUS_RADIUS);
        canvas.draw_rrect(&rr, &focus_paint);
    }

    /// Post-body bookkeeping: publish this frame's `doc_height` /
    /// `viewport_height` / `max_scroll`, re-clamp `scroll_y` in case
    /// content shrank, honor a pending caret-into-view request, then
    /// draw the scrollbar in window space. Called AFTER
    /// `canvas.restore` — the scrollbar is window-coord, not doc-coord.
    ///
    /// `final_y` is the y cursor accumulated by the view body (cell
    /// stream / entity page / people page); `doc_height` is computed
    /// as `final_y - CELL_GAP + DOC_BOTTOM_PAD` to match the cell-loop
    /// convention (each cell adds CELL_GAP after itself; the last gap
    /// is replaced by the bottom pad).
    fn finalize_pane_scroll(&mut self, canvas: &Canvas, layout: &PaneLayout, final_y: f32) {
        self.pane_mut().doc_height = final_y - CELL_GAP + DOC_BOTTOM_PAD;
        // Body viewport excludes the header band — the user can't see
        // content that scrolls behind it, so scroll math shouldn't
        // count those pixels as visible.
        let body_h = (layout.pane_h - PANE_HEADER_H).max(0.0);
        self.pane_mut().viewport_height = body_h;
        self.pane_mut().max_scroll =
            (self.pane_mut().doc_height - self.pane_mut().viewport_height).max(0.0);
        self.pane_mut().scroll_y = self.pane_mut().scroll_y.min(self.pane_mut().max_scroll);

        // After cells are laid out (y_origin/height fresh), honor any
        // caret-into-view request from this tick's events. Effect
        // lands on the next frame.
        if std::mem::take(&mut self.pane_mut().pending_caret_scroll) {
            self.scroll_caret_into_view();
        }

        // Per-pane scrollbar in window coords, anchored at the pane's
        // right edge. Track is inset by the header band so the
        // scrollbar lives entirely under the URL pill.
        let viewport_h = self.pane_mut().viewport_height;
        let doc_h = self.pane_mut().doc_height;
        self.pane_mut().scroller.draw_bar(
            canvas,
            layout.pane_rect.right,
            viewport_h,
            doc_h,
            PANE_HEADER_H,
        );
    }
}

/// Pure predicate over a parsed query AST: does this query qualify
/// for the entity-page shortcut in the URL-bar dropdown?
///
/// Qualifies iff the AST is *exactly* one or more `@person` includes
/// — no tags (include or exclude), no entity excludes, no time
/// filter, no residual text. Returns the resolved person UUIDs in
/// `query::resolve_persons` order. Unknown person names (no
/// matches) resolve to an empty Vec → `None`.
///
/// Free function so the qualification rule and the resolution path
/// are testable without standing up a `KeptApp`. The caller wires
/// the resolver's backing indices (`alias_index`, `title_fallback`)
/// in directly.
pub(super) fn entity_page_shortcuts_for_ast(
    ast: &query::Ast,
    alias_index: &[(String, Uuid, String)],
    title_fallback: &[(Uuid, String)],
) -> Option<Vec<Uuid>> {
    if !ast.include.tags.is_empty()
        || !ast.exclude.tags.is_empty()
        || !ast.exclude.entities.is_empty()
        || ast.include.time.is_some()
        || !ast.text.is_empty()
        || ast.include.entities.is_empty()
    {
        return None;
    }
    let ids = query::resolve_persons(&ast.include.entities, alias_index, title_fallback);
    if ids.is_empty() { None } else { Some(ids) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::FontMgr;

    fn typeface() -> Typeface {
        FontMgr::new()
            .new_from_data(include_bytes!("../../resources/fonts/Figtree.ttf"), None)
            .expect("font loads")
    }

    fn cell_entry() -> HeaderResultEntry {
        HeaderResultEntry::Cell(Uuid::new_v4())
    }

    // ----- PaneHeader::move_selection -----

    #[test]
    fn move_selection_empty_results_stays_none() {
        let mut h = PaneHeader::new(typeface());
        h.move_selection(1);
        assert_eq!(h.selected, None);
        h.move_selection(-1);
        assert_eq!(h.selected, None);
    }

    #[test]
    fn move_selection_down_from_none_lands_on_first_row() {
        let mut h = PaneHeader::new(typeface());
        h.cached_results = vec![cell_entry(), cell_entry(), cell_entry()];
        h.selected = None;
        h.move_selection(1);
        assert_eq!(h.selected, Some(0));
    }

    #[test]
    fn move_selection_up_from_none_lands_on_last_row() {
        let mut h = PaneHeader::new(typeface());
        h.cached_results = vec![cell_entry(), cell_entry(), cell_entry()];
        h.selected = None;
        h.move_selection(-1);
        assert_eq!(h.selected, Some(2));
    }

    #[test]
    fn move_selection_down_past_last_row_wraps_to_none() {
        // The "rest stop" between last and first is `None` — Enter
        // there commits the typed query as a view (filter-first
        // semantic).
        let mut h = PaneHeader::new(typeface());
        h.cached_results = vec![cell_entry(), cell_entry(), cell_entry()];
        h.selected = Some(2);
        h.move_selection(1);
        assert_eq!(h.selected, None);
    }

    #[test]
    fn move_selection_up_from_first_row_wraps_to_none() {
        let mut h = PaneHeader::new(typeface());
        h.cached_results = vec![cell_entry(), cell_entry(), cell_entry()];
        h.selected = Some(0);
        h.move_selection(-1);
        assert_eq!(h.selected, None);
    }

    #[test]
    fn move_selection_full_cycle_down_returns_to_start() {
        // count rows + the None rest stop = count + 1 positions.
        let mut h = PaneHeader::new(typeface());
        h.cached_results = vec![cell_entry(), cell_entry(), cell_entry()];
        h.selected = None;
        for _ in 0..4 {
            h.move_selection(1);
        }
        assert_eq!(h.selected, None);
    }

    #[test]
    fn move_selection_caps_results_at_max_visible() {
        // The dropdown shows at most HEADER_MAX_VISIBLE rows;
        // arrow nav should respect the same window so the user
        // can't highlight a row that isn't rendered.
        let mut h = PaneHeader::new(typeface());
        let huge = std::iter::repeat_with(cell_entry)
            .take(HEADER_MAX_VISIBLE + 5)
            .collect::<Vec<_>>();
        h.cached_results = huge;
        h.selected = Some(HEADER_MAX_VISIBLE - 1);
        h.move_selection(1);
        assert_eq!(h.selected, None);
    }

    // ----- entity_page_shortcuts_for_ast -----

    fn person_alias(name: &str, id: Uuid) -> (String, Uuid, String) {
        (name.to_string(), id, "person".to_string())
    }

    #[test]
    fn shortcut_single_person_resolves_via_alias_index() {
        let alice = Uuid::new_v4();
        let alias_index = vec![person_alias("alice", alice)];
        let ast = query::parse("@alice");
        let got = entity_page_shortcuts_for_ast(&ast, &alias_index, &[]);
        assert_eq!(got, Some(vec![alice]));
    }

    #[test]
    fn shortcut_multiple_persons_returns_all_in_resolver_order() {
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let alias_index = vec![person_alias("alice", alice), person_alias("bob", bob)];
        let ast = query::parse("@alice @bob");
        let got = entity_page_shortcuts_for_ast(&ast, &alias_index, &[]).expect("matched");
        assert_eq!(got.len(), 2);
        assert!(got.contains(&alice));
        assert!(got.contains(&bob));
    }

    #[test]
    fn shortcut_unknown_person_returns_none() {
        let alias_index: Vec<(String, Uuid, String)> = vec![];
        let ast = query::parse("@nobody");
        assert_eq!(
            entity_page_shortcuts_for_ast(&ast, &alias_index, &[]),
            None
        );
    }

    #[test]
    fn shortcut_disqualified_by_text() {
        // `@alice cluster` — residual text rules out the shortcut.
        let alice = Uuid::new_v4();
        let alias_index = vec![person_alias("alice", alice)];
        let ast = query::parse("@alice cluster");
        assert_eq!(
            entity_page_shortcuts_for_ast(&ast, &alias_index, &[]),
            None
        );
    }

    #[test]
    fn shortcut_disqualified_by_tag() {
        let alice = Uuid::new_v4();
        let alias_index = vec![person_alias("alice", alice)];
        let ast = query::parse("@alice #urgent");
        assert_eq!(
            entity_page_shortcuts_for_ast(&ast, &alias_index, &[]),
            None
        );
    }

    #[test]
    fn shortcut_disqualified_by_time() {
        let alice = Uuid::new_v4();
        let alias_index = vec![person_alias("alice", alice)];
        let ast = query::parse("@alice today");
        assert_eq!(
            entity_page_shortcuts_for_ast(&ast, &alias_index, &[]),
            None
        );
    }

    #[test]
    fn shortcut_disqualified_by_exclude_entity() {
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let alias_index = vec![person_alias("alice", alice), person_alias("bob", bob)];
        let ast = query::parse("@alice -@bob");
        assert_eq!(
            entity_page_shortcuts_for_ast(&ast, &alias_index, &[]),
            None
        );
    }

    #[test]
    fn shortcut_disqualified_by_empty_ast() {
        // No entities means nothing to shortcut to.
        let ast = query::parse("");
        assert_eq!(entity_page_shortcuts_for_ast(&ast, &[], &[]), None);
    }

    #[test]
    fn shortcut_resolves_via_title_fallback() {
        // Alias index misses; title fallback catches by display-name
        // substring. Same shape as the resolver's two-step lookup.
        let alice = Uuid::new_v4();
        let title_fallback = vec![(alice, "alicestone".to_string())];
        let ast = query::parse("@alice");
        let got = entity_page_shortcuts_for_ast(&ast, &[], &title_fallback);
        assert_eq!(got, Some(vec![alice]));
    }
}
