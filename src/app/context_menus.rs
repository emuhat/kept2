use skia_safe::{
    BlurStyle, Canvas, Color, Font, MaskFilter, Paint, PaintStyle, Point, Rect,
};
use uuid::Uuid;

use crate::cell::ReferenceTarget;

use super::{KeptApp, clamp_rect_to_viewport, fit_text_ellipsized};

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
    /// Cell id used as the *source* for any "Surface as reference"
    /// action invoked from this menu (whole-cell or subtree). Equals
    /// `cell_id` when the menu is anchored on a normal cell. When
    /// anchored on a Reference cell, this is `Reference.target.cell_id()`
    /// — references always resolve to the original source, never to
    /// another reference (no chained-reference creation).
    pub(super) reference_origin_cell_id: Uuid,
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

impl KeptApp {
    pub(super) fn render_cell_context_menu(
        &mut self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
    ) {
        let Some(menu) = self.cell_context_menu.as_ref() else {
            self.hit_tests.cell_menu.delete = None;
            self.hit_tests.cell_menu.surface = None;
            self.hit_tests.cell_menu.surface_subtree = None;
            return;
        };
        let Some(cell) = self.cell(menu.cell_id) else {
            self.hit_tests.cell_menu.delete = None;
            self.hit_tests.cell_menu.surface = None;
            self.hit_tests.cell_menu.surface_subtree = None;
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
        let rect = clamp_rect_to_viewport(
            Rect::new(
                menu.anchor_x,
                menu.anchor_y,
                menu.anchor_x + menu_w,
                menu.anchor_y + menu_h,
            ),
            view_w,
            view_h,
            4.0,
        );

        draw_menu_card(canvas, rect, scale);

        // Two muted info lines.
        let info_font = Font::from_typeface(&self.typeface, 12.0 * scale);
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

        // Hairline divider above the action rows.
        let divider_y = rect.top + pad + info_h * 2.0 + 0.5;
        let mut divider = Paint::default();
        divider.set_anti_alias(false);
        divider.set_color(crate::color::hairline_divider());
        canvas.draw_line(
            Point::new(rect.left + pad, divider_y),
            Point::new(rect.right - pad, divider_y),
            &divider,
        );

        let action_font = Font::from_typeface(&self.typeface, 13.0 * scale);
        let mouse = self.mouse_pos;
        let mut row_top = rect.top + pad + info_h * 2.0 + 1.0;
        let mut emit_row = |label: &str, color: Color, hover_bg: Color| -> Rect {
            let r = Rect::new(
                rect.left + pad * 0.5,
                row_top,
                rect.right - pad * 0.5,
                row_top + action_h,
            );
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
            row_top += action_h;
            r
        };

        // Delete row (red, hover red-tinted).
        let delete_rect = emit_row(
            "Delete cell",
            crate::color::delete_text(),
            crate::color::delete_hover_bg(),
        );

        // Surface as reference — always present. Creates a new reference
        // cell at "now" pointing to this cell. Lands wherever a fresh
        // Ctrl+N cell would land. Hover uses the warm-tan tint.
        let surface_rect = emit_row(
            "Surface as reference",
            crate::color::text_menu_row(),
            crate::color::embed_hover(),
        );

        // Surface bullet sub-tree as reference — only when right-click hit
        // a bullet inside an outline.
        let surface_subtree_rect = if has_subtree {
            let snip = menu.bullet_snippet.as_deref().unwrap_or("[empty]");
            let label = format!("Surface '{}' as reference", snip);
            Some(emit_row(
                &label,
                crate::color::text_menu_row(),
                crate::color::embed_hover(),
            ))
        } else {
            None
        };

        self.hit_tests.cell_menu.delete = Some(delete_rect);
        self.hit_tests.cell_menu.surface = Some(surface_rect);
        self.hit_tests.cell_menu.surface_subtree = surface_subtree_rect;
    }

    pub(super) fn render_tag_context_menu(
        &mut self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
    ) {
        let Some(menu) = self.tag_context_menu.as_ref() else {
            self.hit_tests.tag_menu.delete = None;
            return;
        };
        let scale = self.font_scale;
        let pad = 6.0 * scale;
        let row_h = 26.0 * scale;
        let menu_w = 160.0 * scale;
        let menu_h = row_h + pad * 2.0;
        let rect = clamp_rect_to_viewport(
            Rect::new(
                menu.anchor_x,
                menu.anchor_y,
                menu.anchor_x + menu_w,
                menu.anchor_y + menu_h,
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
        let font = Font::from_typeface(&self.typeface, 13.0 * scale);
        draw_menu_row(
            canvas,
            row_rect,
            &format!("Delete tag #{}", menu.name),
            crate::color::delete_text(),
            crate::color::delete_hover_bg(),
            true,
            pad,
            false,
            scale,
            &font,
            self.mouse_pos,
        );
        self.hit_tests.tag_menu.delete = Some(row_rect);
    }

    /// Right-click menu rendered over a People-page row. Two actions:
    /// Rename (always enabled) and Delete person (disabled when the
    /// entity has a backing cell or any `kept://` references; the row
    /// shows the count so the user knows what's blocking).
    pub(super) fn render_people_context_menu(
        &mut self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
    ) {
        let Some(menu) = self.people_context_menu.as_ref() else {
            self.hit_tests.people_menu.rename = None;
            self.hit_tests.people_menu.delete = None;
            return;
        };
        let scale = self.font_scale;
        let pad = 6.0 * scale;
        let row_h = 26.0 * scale;
        let menu_w = 200.0 * scale;
        let menu_h = row_h * 2.0 + pad * 2.0;
        let rect = clamp_rect_to_viewport(
            Rect::new(
                menu.anchor_x,
                menu.anchor_y,
                menu.anchor_x + menu_w,
                menu.anchor_y + menu_h,
            ),
            view_w,
            view_h,
            4.0,
        );
        draw_menu_card(canvas, rect, scale);

        let font = Font::from_typeface(&self.typeface, 13.0 * scale);
        let mouse = self.mouse_pos;

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
        self.hit_tests.people_menu.rename = Some(rename_rect);

        // Delete row. Disabled when not deletable — same label paint
        // path either way (text color differs), but the hover background
        // only paints when deletable.
        let delete_rect = Rect::new(
            rect.left + pad * 0.5,
            rect.top + pad + row_h,
            rect.right - pad * 0.5,
            rect.top + pad + row_h * 2.0,
        );
        let label = if menu.deletable {
            "Delete person".to_string()
        } else {
            match menu.ref_count {
                Some(n) if n > 0 => format!("Delete person ({n} refs)"),
                _ => "Delete person (in use)".to_string(),
            }
        };
        let text_color = if menu.deletable {
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
            menu.deletable,
            pad,
            false,
            scale,
            &font,
            mouse,
        );
        if menu.deletable {
            self.hit_tests.people_menu.delete = Some(delete_rect);
        } else {
            self.hit_tests.people_menu.delete = None;
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
