use skia_safe::{
    BlurStyle, Canvas, Color, Font, MaskFilter, Paint, PaintStyle, Point, Rect, Typeface,
};
use uuid::Uuid;

use crate::cell::{Cell, CellKind, ReferenceTarget};
use crate::thread::ThreadId;

use super::{HitTestState, clamp_rect_to_viewport, fit_text_ellipsized};

/// Hit rects emitted by the shared `threads_group` helper. Same shape
/// for both `CellContextMenu` and `BarContextMenu` — the bar menu
/// just never asks for the subtree row.
#[derive(Default)]
pub(super) struct ThreadsGroupHits {
    pub(super) attach_whole: Option<Rect>,
    pub(super) attach_subtree: Option<Rect>,
    pub(super) detach: Vec<(ThreadId, ReferenceTarget, Rect)>,
}

/// How many rows `threads_group` will emit, used to size the menu
/// rect before rendering. Paired with `threads_group` so the height
/// calc and the render loop can't drift.
pub(super) fn threads_group_row_count(
    is_scratch: bool,
    show_whole_attach: bool,
    has_subtree: bool,
    attached_count: usize,
) -> usize {
    if is_scratch {
        return 0;
    }
    (if show_whole_attach { 1 } else { 0 })
        + (if has_subtree { 1 } else { 0 })
        + attached_count
}

/// Render the Threads group of a context menu: whole-cell "Attach to
/// thread…" + optional subtree "Attach '<snippet>' to thread…" + one
/// "Detach from <title>" row per existing membership, terminated by
/// a separator. Suppressed on scratch cells.
///
/// `emit_row` and `emit_separator` mirror the local closures both
/// menus already define for their other groups; passed by reference
/// so this helper inherits the caller's row geometry / colors / etc.
/// without re-deriving any of it. The detach rows carry their own
/// `ReferenceTarget` so a single click can route the right membership
/// even when a thread has both whole-cell and subtree attachments.
pub(super) fn threads_group(
    is_scratch: bool,
    show_whole_attach: bool,
    subtree_snippet: Option<&str>,
    attached: &[(ThreadId, ReferenceTarget, String)],
    row_top: &mut f32,
    emit_row: &impl Fn(&mut f32, &str, Color, Color) -> Rect,
    emit_separator: &impl Fn(&mut f32),
) -> ThreadsGroupHits {
    let mut hits = ThreadsGroupHits::default();
    if is_scratch {
        return hits;
    }
    let row_color = crate::color::text_menu_row();
    let hover = crate::color::embed_hover();
    if show_whole_attach {
        hits.attach_whole = Some(emit_row(row_top, "Attach to thread…", row_color, hover));
    }
    if let Some(snip) = subtree_snippet {
        let label = format!("Attach '{}' to thread…", snip);
        hits.attach_subtree = Some(emit_row(row_top, &label, row_color, hover));
    }
    for (tid, target, title) in attached {
        // Disambiguate when a thread has both whole-cell and subtree
        // attachments to the same cell — without the suffix the two
        // detach rows would be indistinguishable.
        let suffix = match target {
            ReferenceTarget::Subtree { .. } => " (subtree)",
            ReferenceTarget::WholeCell(_) => "",
        };
        let label = format!("Detach from {}{}", title, suffix);
        let rect = emit_row(row_top, &label, row_color, hover);
        hits.detach.push((*tid, *target, rect));
    }
    emit_separator(row_top);
    hits
}

/// Per-frame context the menu render methods need from `KeptApp`. Pure
/// data slice — the menus take this by mutable reference instead of
/// reaching into the whole app, which makes the scope of each render
/// pass explicit at the type level (S8).
pub(super) struct MenuRenderCtx<'a> {
    pub(super) font_scale: f32,
    pub(super) typeface: &'a Typeface,
    pub(super) mouse_pos: (f32, f32),
    /// Per-frame hit-test write surface (see S2).
    pub(super) hit_tests: &'a mut HitTestState,
}

/// Cell context menu (right-click). Two muted timestamp lines + a
/// "Delete cell" action separated by a hairline.
const CELL_MENU_WIDTH: f32 = 220.0;
const CELL_MENU_INFO_H: f32 = 22.0;
const CELL_MENU_ACTION_H: f32 = 26.0;
const CELL_MENU_PAD: f32 = 6.0;

pub(super) struct TagContextMenu {
    pub(super) name: String,
    pub(super) anchor_x: f32,
    pub(super) anchor_y: f32,
}

/// Right-click on a cell's left-edge bar opens this menu. Always
/// operates on the whole cell — never on a bullet — so it carries
/// just the cell id + anchor + a couple of state flags that drive
/// label/conditional-row logic. Created/edited timestamp info
/// comes from the cell itself at render time.
pub(super) struct BarContextMenu {
    pub(super) cell_id: Uuid,
    pub(super) anchor_x: f32,
    pub(super) anchor_y: f32,
}

/// Right-click menu anchored on a cell in the doc area. Replaces the
/// old kebab affordance: timestamps render as muted info rows, and a
/// "Delete cell" row is the only action.
pub(super) struct CellContextMenu {
    pub(super) cell_id: Uuid,
    pub(super) anchor_x: f32,
    pub(super) anchor_y: f32,
    /// When the right-click hit-tested onto a specific bullet inside an
    /// outline cell, the bullet's id + a short snippet of its text. Drives
    /// the "Copy '<snippet>' bullet sub-tree as embed" menu row. None for
    /// non-outline cells or right-clicks landing in outline whitespace.
    pub(super) bullet_id: Option<Uuid>,
    pub(super) bullet_snippet: Option<String>,
    /// Cell id used as the source for the *whole-cell* "Surface as
    /// reference" action invoked from this menu. Equals `cell_id`
    /// when the menu is anchored on a normal cell. When anchored on
    /// a Reference cell, this is `Reference.target.cell_id()` —
    /// references always resolve to the original source, never to
    /// another reference (no chained-reference creation).
    ///
    /// The *subtree* surface uses `bullet_origin_cell_id` instead;
    /// the two diverge only for envelope outlines (whole-cell
    /// surfaces target the envelope itself; a header-bullet subtree
    /// surfaces from the header's embedded source).
    pub(super) reference_origin_cell_id: Uuid,
    /// Cell id that owns `bullet_id` — set whenever `bullet_id` is.
    /// May differ from `reference_origin_cell_id` when the click
    /// landed inside a nested embed (envelope header, recursive
    /// embed): the bullet's source is the deepest embed's target,
    /// not the outermost cell's surface origin. Drives the
    /// "Surface '<snippet>' as reference" subtree row.
    pub(super) bullet_origin_cell_id: Option<Uuid>,
    /// `Some` when the menu was opened on a Reference cell — captures
    /// that reference's actual target (WholeCell or Subtree). Drives
    /// the whole-cell surface row so re-surfacing a Subtree reference
    /// produces another Subtree pointing at the same bullet, instead
    /// of degrading to a WholeCell of the source.
    pub(super) source_reference_target: Option<ReferenceTarget>,
}

/// Right-click menu over a People-page row. `deletable` and `ref_count`
/// are precomputed at open time so the menu render doesn't have to walk
/// every cell's links each frame; if the user creates a new mention
/// while the menu is open, they'll see stale state — that's fine, the
/// menu is dismissed by any click anyway.
pub(super) struct PeopleContextMenu {
    pub(super) entity_id: Uuid,
    pub(super) anchor_x: f32,
    pub(super) anchor_y: f32,
    /// True when the entity has no `primary_cell_id` AND zero `kept://`
    /// references in any cell. Drives the Delete row's enabled state.
    pub(super) deletable: bool,
    /// Reference count surfaced as muted text under "Delete" when the
    /// entity isn't deletable. `None` when deletable (zero, suppressed).
    pub(super) ref_count: Option<usize>,
}

/// Floating-card chrome shared by every context menu: drop shadow,
/// rounded background, hairline border. Caller draws content on top.
fn draw_menu_card(canvas: &Canvas, rect: Rect, scale: f32) {
    let radius = 6.0 * scale;
    let mut shadow = Paint::default();
    shadow.set_anti_alias(true);
    shadow.set_color(crate::color::shadow_menu());
    shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 8.0, false));
    canvas.draw_round_rect(
        Rect::new(rect.left, rect.top + 2.0, rect.right, rect.bottom + 2.0),
        radius,
        radius,
        &shadow,
    );
    let mut bg = Paint::default();
    bg.set_anti_alias(true);
    bg.set_color(crate::color::bg_card());
    canvas.draw_round_rect(rect, radius, radius, &bg);
    let mut border = Paint::default();
    border.set_anti_alias(true);
    border.set_style(PaintStyle::Stroke);
    border.set_stroke_width(1.0);
    border.set_color(crate::color::menu_border());
    canvas.draw_round_rect(rect, radius, radius, &border);
}

/// Draw a labelled menu row inside `rect`. Paints a hover background
/// (rounded, in `hover_bg`) when the mouse is inside `rect` AND
/// `hoverable` is true. Renders the label centered vertically, in
/// `text_color`, inset by `text_inset_x` from the row's left edge. When
/// `ellipsize` is true, long labels collapse with an ellipsis to fit
/// the available width.
fn draw_menu_row(
    canvas: &Canvas,
    rect: Rect,
    label: &str,
    text_color: Color,
    hover_bg: Color,
    hoverable: bool,
    text_inset_x: f32,
    ellipsize: bool,
    scale: f32,
    font: &Font,
    mouse: (f32, f32),
) {
    let hovered = hoverable
        && mouse.0 >= rect.left
        && mouse.0 <= rect.right
        && mouse.1 >= rect.top
        && mouse.1 <= rect.bottom;
    if hovered {
        let mut hp = Paint::default();
        hp.set_anti_alias(true);
        hp.set_color(hover_bg);
        canvas.draw_round_rect(rect, 4.0 * scale, 4.0 * scale, &hp);
    }
    let (_, m) = font.metrics();
    let baseline = rect.top + (rect.height() + (-m.ascent) - m.descent) * 0.5;
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(text_color);
    let text_left = rect.left + text_inset_x;
    let text = if ellipsize {
        let avail = (rect.right - text_inset_x - text_left).max(0.0);
        fit_text_ellipsized(label, avail, font, &paint)
    } else {
        label.to_string()
    };
    canvas.draw_str(&text, Point::new(text_left, baseline), font, &paint);
}

impl CellContextMenu {
    /// What the primary "Attach to thread…" row should operate on.
    /// Always whole-cell semantics, mirroring the Surface row above
    /// it: for a Reference source, preserve the reference's target
    /// (so re-attaching a Subtree reference keeps the subtree
    /// pointer); otherwise a WholeCell of the resolved source.
    pub(super) fn whole_attach_target(&self) -> ReferenceTarget {
        self.source_reference_target
            .unwrap_or(ReferenceTarget::WholeCell(self.reference_origin_cell_id))
    }

    /// Target for the secondary "Attach '<snippet>' to thread…" row.
    /// Returns `Some` only when the right-click landed on a bullet
    /// that lives in *this* cell's outline (not inside a nested
    /// embed — mirrors the surface-subtree gate).
    pub(super) fn subtree_attach_target(&self) -> Option<ReferenceTarget> {
        let origin = self.bullet_origin_cell_id?;
        let bid = self.bullet_id?;
        if origin != self.cell_id {
            return None;
        }
        Some(ReferenceTarget::Subtree {
            cell_id: origin,
            bullet_id: bid,
        })
    }

    pub(super) fn render(
        &self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
        cell: &Cell,
        attached_threads: &[(ThreadId, ReferenceTarget, String)],
        ctx: &mut MenuRenderCtx<'_>,
    ) {
        let scale = ctx.font_scale;
        let pad = CELL_MENU_PAD * scale;
        let action_h = CELL_MENU_ACTION_H * scale;
        let menu_w = CELL_MENU_WIDTH * scale;

        // Compute action rows. Order matches the visual stack.
        let has_subtree = self.bullet_id.is_some();
        // Envelope is offered only when the menu was opened on a
        // Bullet-active toggle: only shown when the clicked bullet
        // lives in *this* cell's outline. For clicks inside a nested
        // embed (Reference cache, envelope header), the bullet's
        // origin is a different cell — toggling its active flag from
        // here would cross-mutate, which we don't want from a menu
        // anchored on the wrapping cell.
        //
        // Note: Envelope / Unwrap are whole-cell operations and now
        // live on the `BarContextMenu` instead of here. Right-click
        // anywhere on the cell while holding Ctrl (or right-click
        // the bar) opens that menu.
        let has_bullet_toggle =
            self.bullet_id.is_some() && self.bullet_origin_cell_id == Some(self.cell_id);
        // Scratch cells aren't linkable — no Surface row, no
        // Surface-subtree row. The "Surface group" collapses to
        // zero rows + zero separator on scratch.
        let is_scratch = cell.scratch;
        let mut action_count: usize = 0;
        if !is_scratch {
            action_count += 1; // Surface as reference
            if has_subtree {
                action_count += 1; // Surface subtree
            }
        }
        action_count += 1; // Close / Reopen (cell)
        if has_bullet_toggle {
            action_count += 1; // Close / Reopen (bullet)
        }
        action_count += super::SNOOZE_PRESETS.len(); // 6 Snooze rows
        // "Unsnooze" — present iff the target (bullet if clicking
        // landed on one, else cell) is currently snoozed. Compute the
        // target's `resurface_after` to size the menu correctly; the
        // emit loop further down recomputes it for the actual row.
        let target_resurface_present = if has_bullet_toggle {
            self.bullet_id.and_then(|bid| match &cell.kind {
                CellKind::Outline(oc) => oc
                    .bullets()
                    .iter()
                    .find(|b| b.id() == bid)
                    .map(|b| b.resurface_after()),
                _ => None,
            }).flatten().is_some()
        } else {
            cell.resurface_after.is_some()
        };
        if target_resurface_present {
            action_count += 1;
        }
        // Threads group is shared with the bar menu — see
        // `threads_group` for layout. Cell-body menu: when the
        // right-click landed on a bullet inside this cell, the menu
        // is bullet-scoped — show only the subtree Attach row, hide
        // the whole-cell variant. Whole-cell attach lives on the
        // bar (left-edge) menu. When the click missed a bullet (or
        // hit a Reference cell, etc.), the whole-cell row is the
        // only relevant action.
        let has_subtree_attach = self.subtree_attach_target().is_some();
        let show_whole_attach = !has_subtree_attach;
        action_count += threads_group_row_count(
            is_scratch,
            show_whole_attach,
            has_subtree_attach,
            attached_threads.len(),
        );
        // Group separators: Surface | Snooze | Threads | Close. Scratch
        // skips the Surface AND Threads groups → one fewer separator each.
        let separator_h = pad;
        let separator_count: usize = if is_scratch { 1 } else { 3 };
        let menu_h = pad
            + action_h * action_count as f32
            + separator_h * separator_count as f32
            + pad;
        let rect = clamp_rect_to_viewport(
            Rect::new(
                self.anchor_x,
                self.anchor_y,
                self.anchor_x + menu_w,
                self.anchor_y + menu_h,
            ),
            view_w,
            view_h,
            4.0,
        );

        draw_menu_card(canvas, rect, scale);

        let action_font = Font::from_typeface(ctx.typeface, 13.0 * scale);
        let mouse = ctx.mouse_pos;
        let mut row_top = rect.top + pad;
        let row_left = rect.left + pad * 0.5;
        let row_right = rect.right - pad * 0.5;
        let emit_row = |row_top_ref: &mut f32,
                            label: &str,
                            color: Color,
                            hover_bg: Color|
         -> Rect {
            let r = Rect::new(row_left, *row_top_ref, row_right, *row_top_ref + action_h);
            draw_menu_row(
                canvas,
                r,
                label,
                color,
                hover_bg,
                true,
                pad * 2.0,
                true,
                scale,
                &action_font,
                mouse,
            );
            *row_top_ref += action_h;
            r
        };
        let emit_separator = |row_top_ref: &mut f32| {
            // Hairline divider with `pad/2` breathing band above
            // and below so groups read as distinct without the
            // menu feeling sparse.
            let band = separator_h;
            let line_y = *row_top_ref + band * 0.5;
            let mut div = Paint::default();
            div.set_anti_alias(false);
            div.set_color(crate::color::hairline_divider());
            canvas.draw_line(
                Point::new(rect.left + pad, line_y),
                Point::new(rect.right - pad, line_y),
                &div,
            );
            *row_top_ref += band;
        };

        // ----- Group 1: Surface actions -----
        // Suppressed entirely on scratch cells — they're not
        // linkable. The group's trailing separator is suppressed
        // along with it so the menu doesn't open with a stray
        // hairline above the snoozes.
        let surface_rect: Option<Rect> = if is_scratch {
            None
        } else {
            Some(emit_row(
                &mut row_top,
                "Surface as reference",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        };
        let surface_subtree_rect = if !is_scratch && has_subtree {
            let snip = self.bullet_snippet.as_deref().unwrap_or("[empty]");
            let label = format!("Surface '{}' as reference", snip);
            Some(emit_row(
                &mut row_top,
                &label,
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        } else {
            None
        };

        if !is_scratch {
            emit_separator(&mut row_top);
        }

        // ----- Group 2: Snoozes -----
        // Six fuzzy presets + optional "Unsnooze". Targets the
        // bullet when the right-click landed on one; else the cell.
        let snooze_targets_bullet = has_bullet_toggle;
        let target_resurface = if snooze_targets_bullet {
            self.bullet_id.and_then(|bid| match &cell.kind {
                CellKind::Outline(oc) => oc
                    .bullets()
                    .iter()
                    .find(|b| b.id() == bid)
                    .map(|b| b.resurface_after()),
                _ => None,
            }).flatten()
        } else {
            cell.resurface_after
        };

        let mut snooze_rects: [Option<Rect>; 6] = [None; 6];
        for (i, (_, label)) in super::SNOOZE_PRESETS.iter().enumerate() {
            snooze_rects[i] = Some(emit_row(
                &mut row_top,
                label,
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ));
        }
        let unsnooze_rect = if target_resurface.is_some() {
            Some(emit_row(
                &mut row_top,
                "Unsnooze",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        } else {
            None
        };

        emit_separator(&mut row_top);

        // ----- Group 3: Threads (shared with the bar menu) -----
        let subtree_snippet = if has_subtree_attach {
            Some(self.bullet_snippet.as_deref().unwrap_or("[empty]"))
        } else {
            None
        };
        let threads_hits = threads_group(
            is_scratch,
            show_whole_attach,
            subtree_snippet,
            attached_threads,
            &mut row_top,
            &emit_row,
            &emit_separator,
        );

        // ----- Group 4: Close (state change, scoped to body context) -----
        // Whole-cell shape transforms (Envelope / Unwrap) live on
        // the BarContextMenu now — they're whole-cell operations
        // and don't belong in a bullet-specific menu.
        let cell_active_label = if cell.is_open() { "Close" } else { "Reopen" };
        let toggle_cell_active_rect = emit_row(
            &mut row_top,
            cell_active_label,
            crate::color::text_menu_row(),
            crate::color::embed_hover(),
        );
        let toggle_bullet_active_rect = if has_bullet_toggle {
            let bullet_open = self.bullet_id.and_then(|bid| match &cell.kind {
                CellKind::Outline(oc) => oc
                    .bullets()
                    .iter()
                    .find(|b| b.id() == bid)
                    .map(|b| b.is_open()),
                _ => None,
            }).unwrap_or(true);
            let label = if bullet_open {
                "Close sub-outline"
            } else {
                "Reopen sub-outline"
            };
            Some(emit_row(
                &mut row_top,
                label,
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        } else {
            None
        };

        ctx.hit_tests.cell_menu.surface = surface_rect;
        ctx.hit_tests.cell_menu.surface_subtree = surface_subtree_rect;
        ctx.hit_tests.cell_menu.toggle_cell_active = Some(toggle_cell_active_rect);
        ctx.hit_tests.cell_menu.toggle_bullet_active = toggle_bullet_active_rect;
        ctx.hit_tests.cell_menu.snooze = snooze_rects;
        ctx.hit_tests.cell_menu.unsnooze = unsnooze_rect;
        ctx.hit_tests.cell_menu.snooze_targets_bullet = snooze_targets_bullet;
        ctx.hit_tests.cell_menu.attach_thread = threads_hits.attach_whole;
        ctx.hit_tests.cell_menu.attach_thread_subtree = threads_hits.attach_subtree;
        ctx.hit_tests.cell_menu.detach_thread = threads_hits.detach;
    }
}

impl BarContextMenu {
    /// Render the bar (whole-cell) context menu: two muted info
    /// rows (Created / Last edited), the 6-preset Snooze group, an
    /// optional Unsnooze, and a Delete row at the bottom. Mirrors
    /// the visual structure of `CellContextMenu` so the two read as
    /// siblings.
    pub(super) fn render(
        &self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
        cell: &Cell,
        attached_threads: &[(ThreadId, ReferenceTarget, String)],
        ctx: &mut MenuRenderCtx<'_>,
    ) {
        let scale = ctx.font_scale;
        let pad = CELL_MENU_PAD * scale;
        let info_h = CELL_MENU_INFO_H * scale;
        let action_h = CELL_MENU_ACTION_H * scale;
        let menu_w = CELL_MENU_WIDTH * scale;
        let separator_h = pad;

        let has_unsnooze = cell.resurface_after.is_some();
        // Envelope is offered for Reference cells; Unwrap for
        // envelope outlines. Mutually exclusive — a cell is either
        // a Reference, an envelope-Outline, or neither, never both.
        let has_envelope = matches!(cell.kind, CellKind::Reference(_));
        let has_unwrap = matches!(
            &cell.kind,
            CellKind::Outline(oc) if oc.has_reference_header()
        );
        let has_shape_group = has_envelope || has_unwrap;
        // Scratch cells aren't linkable, so the Surface / Copy
        // reference rows are suppressed. In their place sits a
        // single "Move to Timeline" row that promotes the cell.
        let is_scratch = cell.scratch;
        let mut action_count: usize = super::SNOOZE_PRESETS.len();
        if has_unsnooze {
            action_count += 1;
        }
        if has_envelope {
            action_count += 1;
        }
        if has_unwrap {
            action_count += 1;
        }
        if is_scratch {
            action_count += 1; // Move to Timeline
        } else {
            action_count += 1; // Surface as reference
            action_count += 1; // Copy reference
        }
        // Threads group is shared with the cell menu — see
        // `threads_group`. Bar menu is the whole-cell home: always
        // show the whole-cell Attach row, never the subtree row.
        action_count +=
            threads_group_row_count(is_scratch, true, false, attached_threads.len());
        action_count += 1; // Delete cell
        // Separators: info | surface+copyref (or move-to-timeline) |
        // snoozes | [shape (envelope/unwrap)] | threads | delete.
        let mut separator_count: usize = if is_scratch { 3 } else { 4 };
        if has_shape_group {
            separator_count += 1;
        }
        let menu_h = pad
            + info_h * 2.0
            + action_h * action_count as f32
            + separator_h * separator_count as f32
            + pad;
        let rect = clamp_rect_to_viewport(
            Rect::new(
                self.anchor_x,
                self.anchor_y,
                self.anchor_x + menu_w,
                self.anchor_y + menu_h,
            ),
            view_w,
            view_h,
            4.0,
        );

        draw_menu_card(canvas, rect, scale);

        // ----- Group 1: muted info lines -----
        let info_font = Font::from_typeface(ctx.typeface, 12.0 * scale);
        let mut info_paint = Paint::default();
        info_paint.set_anti_alias(true);
        info_paint.set_color(crate::color::text_muted_grey());
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

        let action_font = Font::from_typeface(ctx.typeface, 13.0 * scale);
        let mouse = ctx.mouse_pos;
        let row_left = rect.left + pad * 0.5;
        let row_right = rect.right - pad * 0.5;
        let mut row_top = rect.top + pad + info_h * 2.0;
        let emit_row = |row_top_ref: &mut f32,
                        label: &str,
                        color: Color,
                        hover_bg: Color|
         -> Rect {
            let r = Rect::new(row_left, *row_top_ref, row_right, *row_top_ref + action_h);
            draw_menu_row(
                canvas,
                r,
                label,
                color,
                hover_bg,
                true,
                pad * 2.0,
                true,
                scale,
                &action_font,
                mouse,
            );
            *row_top_ref += action_h;
            r
        };
        let emit_separator = |row_top_ref: &mut f32| {
            let band = separator_h;
            let line_y = *row_top_ref + band * 0.5;
            let mut div = Paint::default();
            div.set_anti_alias(false);
            div.set_color(crate::color::hairline_divider());
            canvas.draw_line(
                Point::new(rect.left + pad, line_y),
                Point::new(rect.right - pad, line_y),
                &div,
            );
            *row_top_ref += band;
        };

        emit_separator(&mut row_top);

        // ----- Group 2: reference actions (regular cells) OR
        // promotion (scratch cells). The two paths share a slot. -----
        let mut surface_rect: Option<Rect> = None;
        let mut copy_reference_rect: Option<Rect> = None;
        let mut move_to_timeline_rect: Option<Rect> = None;
        if is_scratch {
            move_to_timeline_rect = Some(emit_row(
                &mut row_top,
                "Move to Timeline",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ));
        } else {
            surface_rect = Some(emit_row(
                &mut row_top,
                "Surface as reference",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ));
            copy_reference_rect = Some(emit_row(
                &mut row_top,
                "Copy reference",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ));
        }

        emit_separator(&mut row_top);

        // ----- Group 3: Snoozes (always whole-cell) -----
        let mut snooze_rects: [Option<Rect>; 6] = [None; 6];
        for (i, (_, label)) in super::SNOOZE_PRESETS.iter().enumerate() {
            snooze_rects[i] = Some(emit_row(
                &mut row_top,
                label,
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ));
        }
        let unsnooze_rect = if has_unsnooze {
            Some(emit_row(
                &mut row_top,
                "Unsnooze",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        } else {
            None
        };

        emit_separator(&mut row_top);

        // ----- Group 4: Shape transforms (Envelope / Unwrap) -----
        // Mutually exclusive — only one row possible per cell, and
        // only when the cell is a Reference (Envelope) or an
        // envelope-Outline (Unwrap envelope). Skip the whole group
        // (no separator either) when neither applies.
        let envelope_rect = if has_envelope {
            Some(emit_row(
                &mut row_top,
                "Envelope",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        } else {
            None
        };
        let unwrap_rect = if has_unwrap {
            Some(emit_row(
                &mut row_top,
                "Unwrap envelope",
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        } else {
            None
        };
        if has_shape_group {
            emit_separator(&mut row_top);
        }

        // ----- Group 5: Threads (shared with the cell menu) -----
        let threads_hits = threads_group(
            is_scratch,
            true,  // whole-cell Attach is the bar menu's reason for offering threads
            None,  // bar menu has no subtree variant
            attached_threads,
            &mut row_top,
            &emit_row,
            &emit_separator,
        );

        // ----- Group 6: Delete (destructive, last) -----
        let delete_rect = emit_row(
            &mut row_top,
            "Delete cell",
            crate::color::delete_text(),
            crate::color::delete_hover_bg(),
        );

        ctx.hit_tests.bar_menu.surface = surface_rect;
        ctx.hit_tests.bar_menu.copy_reference = copy_reference_rect;
        ctx.hit_tests.bar_menu.move_to_timeline = move_to_timeline_rect;
        ctx.hit_tests.bar_menu.snooze = snooze_rects;
        ctx.hit_tests.bar_menu.unsnooze = unsnooze_rect;
        ctx.hit_tests.bar_menu.envelope = envelope_rect;
        ctx.hit_tests.bar_menu.unwrap = unwrap_rect;
        ctx.hit_tests.bar_menu.attach_thread = threads_hits.attach_whole;
        // Bar menu never asks for a subtree row; the field is unused here.
        debug_assert!(threads_hits.attach_subtree.is_none());
        ctx.hit_tests.bar_menu.detach_thread = threads_hits
            .detach
            .into_iter()
            .map(|(tid, _, rect)| (tid, rect))
            .collect();
        ctx.hit_tests.bar_menu.delete = Some(delete_rect);
    }
}

impl TagContextMenu {
    pub(super) fn render(
        &self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
        ctx: &mut MenuRenderCtx<'_>,
    ) {
        let scale = ctx.font_scale;
        let pad = 6.0 * scale;
        let row_h = 26.0 * scale;
        let menu_w = 160.0 * scale;
        let menu_h = row_h + pad * 2.0;
        let rect = clamp_rect_to_viewport(
            Rect::new(
                self.anchor_x,
                self.anchor_y,
                self.anchor_x + menu_w,
                self.anchor_y + menu_h,
            ),
            view_w,
            view_h,
            4.0,
        );
        draw_menu_card(canvas, rect, scale);

        let row_rect = Rect::new(
            rect.left + pad * 0.5,
            rect.top + pad,
            rect.right - pad * 0.5,
            rect.top + pad + row_h,
        );
        let font = Font::from_typeface(ctx.typeface, 13.0 * scale);
        draw_menu_row(
            canvas,
            row_rect,
            &format!("Delete tag #{}", self.name),
            crate::color::delete_text(),
            crate::color::delete_hover_bg(),
            true,
            pad,
            false,
            scale,
            &font,
            ctx.mouse_pos,
        );
        ctx.hit_tests.tag_menu.delete = Some(row_rect);
    }
}

impl PeopleContextMenu {
    /// Right-click menu rendered over a People-page row. Two actions:
    /// Rename (always enabled) and Delete person (disabled when the
    /// entity has a backing cell or any `kept://` references; the row
    /// shows the count so the user knows what's blocking).
    pub(super) fn render(
        &self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
        ctx: &mut MenuRenderCtx<'_>,
    ) {
        let scale = ctx.font_scale;
        let pad = 6.0 * scale;
        let row_h = 26.0 * scale;
        let menu_w = 200.0 * scale;
        let menu_h = row_h * 2.0 + pad * 2.0;
        let rect = clamp_rect_to_viewport(
            Rect::new(
                self.anchor_x,
                self.anchor_y,
                self.anchor_x + menu_w,
                self.anchor_y + menu_h,
            ),
            view_w,
            view_h,
            4.0,
        );
        draw_menu_card(canvas, rect, scale);

        let font = Font::from_typeface(ctx.typeface, 13.0 * scale);
        let mouse = ctx.mouse_pos;

        // Rename row.
        let rename_rect = Rect::new(
            rect.left + pad * 0.5,
            rect.top + pad,
            rect.right - pad * 0.5,
            rect.top + pad + row_h,
        );
        draw_menu_row(
            canvas,
            rename_rect,
            "Rename",
            crate::color::text_primary(),
            crate::color::hover_faint(),
            true,
            pad,
            false,
            scale,
            &font,
            mouse,
        );
        ctx.hit_tests.people_menu.rename = Some(rename_rect);

        // Delete row. Disabled when not deletable — same label paint
        // path either way (text color differs), but the hover background
        // only paints when deletable.
        let delete_rect = Rect::new(
            rect.left + pad * 0.5,
            rect.top + pad + row_h,
            rect.right - pad * 0.5,
            rect.top + pad + row_h * 2.0,
        );
        let label = if self.deletable {
            "Delete person".to_string()
        } else {
            match self.ref_count {
                Some(n) if n > 0 => format!("Delete person ({n} refs)"),
                _ => "Delete person (in use)".to_string(),
            }
        };
        let text_color = if self.deletable {
            crate::color::delete_text()
        } else {
            crate::color::text_disabled()
        };
        draw_menu_row(
            canvas,
            delete_rect,
            &label,
            text_color,
            crate::color::delete_hover_bg(),
            self.deletable,
            pad,
            false,
            scale,
            &font,
            mouse,
        );
        if self.deletable {
            ctx.hit_tests.people_menu.delete = Some(delete_rect);
        }
    }
}

fn format_timestamp(epoch_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_millis_opt(epoch_ms)
        .single()
        .unwrap_or_else(|| Local.timestamp_millis_opt(0).single().unwrap());
    dt.format("%-d %B %Y, %-I:%M %p").to_string()
}
