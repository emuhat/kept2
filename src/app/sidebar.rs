use skia_safe::{Canvas, Font, Paint, Point, Rect, Typeface};

use crate::cell::now_epoch_ms;
use crate::document::Document;
use crate::query;

use super::{
    HitTestState, KeptApp, PageKind, Query, SIDEBAR_HEADER_FONT_SIZE, SIDEBAR_WIDTH, Scroller,
    ViewKind, draw_toggle, format_date_label, local_date_for_ms,
};

/// Per-frame context the sidebar renderer needs from `KeptApp`. The
/// sidebar has no state struct — it renders a synthesized view over
/// the document + active query — so the entry point is a free
/// function `render` (S8: explicit subsystem scope without inventing a
/// stateless owner type).
pub(super) struct SidebarRenderCtx<'a> {
    pub(super) font_scale: f32,
    pub(super) typeface: &'a Typeface,
    pub(super) mouse_pos: (f32, f32),
    pub(super) view: &'a Query,
    pub(super) document: &'a Document,
    pub(super) show_inactive_cells: bool,
    /// Precomputed tag names (alphabetical, lowercase-sorted), passed
    /// in instead of re-derived here so the caller can hold a
    /// `&Document` borrow shape that's compatible with the rest of the
    /// render pass.
    pub(super) tag_names: Vec<String>,
    /// Sidebar's own kinetic scroller — read scroll_y, set max, draw
    /// the scrollbar.
    pub(super) scroll: &'a mut Scroller,
    /// Per-frame hit-test write surface (see S2).
    pub(super) hit_tests: &'a mut HitTestState,
}

const SIDEBAR_PAD_X: f32 = 12.0;
const SIDEBAR_PAD_TOP: f32 = 18.0;
const SIDEBAR_HEADER_H: f32 = 28.0;
const SIDEBAR_DATE_H: f32 = 28.0;
#[allow(dead_code)]
const SIDEBAR_ITEM_H: f32 = 26.0;
const SIDEBAR_ITEM_GAP: f32 = 2.0;
const SIDEBAR_DATE_GAP: f32 = 6.0;
#[allow(dead_code)]
const SIDEBAR_INDENT: f32 = 14.0;
const SIDEBAR_ITEM_RADIUS: f32 = 6.0;
const SIDEBAR_DATE_FONT_SIZE: f32 = 13.0;
/// Cap on date rows in the sidebar so the TAGS section has room. Older
/// dates are reachable via Ctrl+Shift+Up/Down and search; the active date
/// is always pinned in even if it falls outside the most-recent N.
const SIDEBAR_DATE_LIMIT: usize = 10;
#[allow(dead_code)]
const SIDEBAR_ITEM_FONT_SIZE: f32 = 12.0;

/// Active/hover background + label for a sidebar row. The three sidebar
/// row kinds (People, Date, Tag) share this paint pass — only the
/// hit-test list they push into differs, so the caller handles that.
fn draw_sidebar_row(
    canvas: &Canvas,
    rect: Rect,
    label: &str,
    is_active: bool,
    is_hovered: bool,
    radius: f32,
    text_x: f32,
    font: &Font,
) {
    if is_active {
        let mut p = Paint::default();
        p.set_anti_alias(true);
        p.set_color(crate::color::accent_blue_selection());
        canvas.draw_round_rect(rect, radius, radius, &p);
    } else if is_hovered {
        let mut p = Paint::default();
        p.set_anti_alias(true);
        p.set_color(crate::color::hover_faint());
        canvas.draw_round_rect(rect, radius, radius, &p);
    }
    let mut text = Paint::default();
    text.set_anti_alias(true);
    text.set_color(crate::color::sidebar_text_primary());
    let (_, m) = font.metrics();
    let baseline = rect.top + (rect.height() + (-m.ascent) - m.descent) * 0.5;
    canvas.draw_str(label, Point::new(text_x, baseline), font, &text);
}

/// Render the entire sidebar — backdrop, CONTEXTS section (weeks +
/// dates), TAGS, PAGES, "Show inactive" toggle, scrollbar. Stateless
/// free function (S8): the sidebar has no per-frame state owner of
/// its own, so the render entry point is a free function taking a
/// `SidebarRenderCtx` instead of being a method on a synthesized
/// owner type.
pub(super) fn render(canvas: &Canvas, height: f32, ctx: &mut SidebarRenderCtx<'_>) {
    let scale = ctx.font_scale;
    let sb_w = SIDEBAR_WIDTH * scale;
    let pad_x = SIDEBAR_PAD_X * scale;
    let pad_top = SIDEBAR_PAD_TOP * scale;
    let header_h = SIDEBAR_HEADER_H * scale;
    let date_h = SIDEBAR_DATE_H * scale;
    let item_gap = SIDEBAR_ITEM_GAP * scale;
    let date_gap = SIDEBAR_DATE_GAP * scale;
    let radius = SIDEBAR_ITEM_RADIUS * scale;

    // Background panel.
    let mut bg_paint = Paint::default();
    bg_paint.set_anti_alias(true);
    bg_paint.set_color(crate::color::bg_panel());
    canvas.draw_rect(Rect::new(0.0, 0.0, sb_w, height.max(0.0)), &bg_paint);
    // (No right-edge separator — the sidebar's `bg_panel` and the
    // pane's white card already provide enough contrast at the
    // seam, and a colored 1px stripe was visually noisy.)

    // Everything below the bg + edge separator scrolls together
    // under `sidebar_scroll.scroll_y`. Clip so off-screen rows
    // don't leak into the doc area, then translate so y values
    // inside this block stay in natural content coordinates (rects
    // are recorded in content-space; hover tests below add
    // scroll_y to mouse_y before comparing).
    let scroll_y = ctx.scroll.scroll_y;
    canvas.save();
    canvas.clip_rect(Rect::new(0.0, 0.0, sb_w, height.max(0.0)), None, true);
    canvas.translate((0.0, -scroll_y));

    let header_font =
        Font::from_typeface(ctx.typeface, SIDEBAR_HEADER_FONT_SIZE * scale);
    let mut header_paint = Paint::default();
    header_paint.set_anti_alias(true);
    header_paint.set_color(crate::color::sidebar_section_header());
    let (_, hm) = header_font.metrics();

    let row_font = Font::from_typeface(ctx.typeface, SIDEBAR_DATE_FONT_SIZE * scale);
    let mouse_x = ctx.mouse_pos.0;
    // Content-coords mouse_y so hover tests match content-space rects.
    let mouse_y = ctx.mouse_pos.1 + scroll_y;
    let in_row = |r: Rect| {
        mouse_x >= r.left && mouse_x <= r.right && mouse_y >= r.top && mouse_y <= r.bottom
    };

    // ---- WHAT section (working-attention surface) ----
    // Sidebar order: WHAT first (the "what am I doing now" pile),
    // then WHEN (time-window navigation: weeks + per-day),
    // then TAGS (filter by topic), then PAGES (People). The
    // WHAT/WHEN/TAGS/PAGES grouping reads as the four axes a
    // user navigates by — Current is conceptually "what".
    let mut y = pad_top;
    let what_header_baseline = y + (-hm.ascent);
    canvas.draw_str(
        "WHAT",
        Point::new(pad_x, what_header_baseline),
        &header_font,
        &header_paint,
    );
    y += header_h;
    let current_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
    draw_sidebar_row(
        canvas,
        current_rect,
        "Current",
        matches!(ctx.view.view_kind, ViewKind::Current),
        in_row(current_rect),
        radius,
        pad_x,
        &row_font,
    );
    ctx.hit_tests.sidebar.pages.push((PageKind::Current, current_rect));
    y += date_h + item_gap;

    // Scratch sits at the end of WHAT so the principled
    // "what am I doing now" surfaces (Current) read first; Scratch
    // is the auxiliary throwaway pad below them.
    let scratch_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
    draw_sidebar_row(
        canvas,
        scratch_rect,
        "Scratch",
        matches!(ctx.view.view_kind, ViewKind::Scratch),
        in_row(scratch_rect),
        radius,
        pad_x,
        &row_font,
    );
    ctx.hit_tests.sidebar.pages.push((PageKind::Scratch, scratch_rect));
    y += date_h + date_gap;

    // ---- WHEN section ----
    let contexts_header_baseline = y + (-hm.ascent);
    canvas.draw_str(
        "WHEN",
        Point::new(pad_x, contexts_header_baseline),
        &header_font,
        &header_paint,
    );
    y += header_h;

    // Relative-week rows above the per-day list. Each is just a
    // pre-built TimeFilter we hand to push_view; the active
    // highlight uses Query::is_solo_time so navigating away to a
    // single date or a tag drops the highlight as expected. Use
    // the same row pitch as the per-day rows so the spacing
    // through CONTEXTS reads as one continuous list.
    for (label, filter) in [
        ("This Week", query::TimeFilter::ThisWeek),
        ("Last Week", query::TimeFilter::LastWeek),
    ] {
        let week_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
        draw_sidebar_row(
            canvas,
            week_rect,
            label,
            ctx.view.is_solo_time(filter.clone()),
            in_row(week_rect),
            radius,
            pad_x,
            &row_font,
        );
        ctx.hit_tests.sidebar.weeks.push((filter, week_rect));
        y += date_h + item_gap + date_gap;
    }

    // Date rows reflect "where notes live": every date that has at least
    // one cell, plus today (so a freshly-launched empty app still shows
    // a usable target), plus the active Date view's date if it's been
    // navigated away from any of those (so the active highlight has a
    // home).
    let mut dates_set: std::collections::BTreeSet<chrono::NaiveDate> =
        std::collections::BTreeSet::new();
    // Skip cells filtered out by the global "Show archived" toggle
    // so a date with only inactive cells doesn't leave a clickable
    // row that lands on an empty timeline.
    for c in &ctx.document.cells {
        if !c.is_open() && !ctx.show_inactive_cells {
            continue;
        }
        dates_set.insert(local_date_for_ms(c.timestamp));
    }
    dates_set.insert(local_date_for_ms(now_epoch_ms()));
    let active_date = if matches!(ctx.view.view_kind, ViewKind::Ast) {
        match ctx.view.ast.include.time {
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

    for d in dates {
        let date_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
        draw_sidebar_row(
            canvas,
            date_rect,
            &format_date_label(d),
            *ctx.view == Query::date(d),
            in_row(date_rect),
            radius,
            pad_x,
            &row_font,
        );
        ctx.hit_tests.sidebar.dates.push((d, date_rect));
        y += date_h + item_gap + date_gap;
    }

    // ----- TAGS section -----
    // Tag names were precomputed by the caller (avoids re-walking the
    // cell list with a borrow shape that fights the rest of the ctx).
    if !ctx.tag_names.is_empty() {
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

        for name in ctx.tag_names.iter() {
            let row_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
            draw_sidebar_row(
                canvas,
                row_rect,
                &format!("#{name}"),
                ctx.view.is_solo_tag(name),
                in_row(row_rect),
                radius,
                pad_x,
                &row_font,
            );
            ctx.hit_tests.sidebar.tags.push((name.clone(), row_rect));
            y += date_h + item_gap;
        }
    }

    // ---- PAGES section (bottom) ----
    // Single-item section in v1 (People). Lives at the bottom so
    // CONTEXTS and TAGS — the daily working surfaces — own the
    // top of the sidebar.
    y += date_gap;
    let pages_header_baseline = y + (-hm.ascent);
    canvas.draw_str(
        "PAGES",
        Point::new(pad_x, pages_header_baseline),
        &header_font,
        &header_paint,
    );
    y += header_h;
    let people_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
    draw_sidebar_row(
        canvas,
        people_rect,
        "People",
        matches!(ctx.view.view_kind, ViewKind::People),
        in_row(people_rect),
        radius,
        pad_x,
        &row_font,
    );
    ctx.hit_tests.sidebar.pages.push((PageKind::People, people_rect));
    y += date_h + item_gap;

    // "Show archived" toggle pill at the very bottom of the
    // sidebar. Single global flag (`KeptApp::show_inactive_cells`)
    // that surfaces inactive cells/bullets across every view —
    // see the cascade-aware visibility filter in
    // `is_visible_for_view` / `OutlineCell::tick`. Layout mirrors
    // the People-page toggle: muted label on the left, pill on
    // the right, both centered on the row's text band.
    y += date_gap;
    let toggle_row_top = y;
    let toggle_row_bot = y + date_h;
    let toggle_h = (date_h * 0.6).max(14.0);
    let toggle_w = toggle_h * 1.8;
    let row_mid = (toggle_row_top + toggle_row_bot) * 0.5;
    let toggle_right = sb_w - pad_x;
    let toggle_left = toggle_right - toggle_w;
    let toggle_rect = Rect::new(
        toggle_left,
        row_mid - toggle_h * 0.5,
        toggle_right,
        row_mid + toggle_h * 0.5,
    );
    let toggle_hovered = mouse_x >= toggle_rect.left
        && mouse_x <= toggle_rect.right
        && mouse_y >= toggle_rect.top
        && mouse_y <= toggle_rect.bottom;
    // Label (same muted-grey style as section headers — this row
    // is metadata, not navigation).
    let label_font =
        Font::from_typeface(ctx.typeface, SIDEBAR_HEADER_FONT_SIZE * scale);
    let mut label_paint = Paint::default();
    label_paint.set_anti_alias(true);
    label_paint.set_color(crate::color::sidebar_section_header());
    let (_, lm) = label_font.metrics();
    let label_baseline = row_mid + (-lm.ascent + lm.descent) * 0.5 - lm.descent;
    canvas.draw_str(
        "Show inactive",
        Point::new(pad_x, label_baseline),
        &label_font,
        &label_paint,
    );
    draw_toggle(canvas, toggle_rect, ctx.show_inactive_cells, toggle_hovered);
    ctx.hit_tests.sidebar.show_inactive_toggle = Some(toggle_rect);
    y = toggle_row_bot + item_gap;

    canvas.restore();

    // Update scroll bounds. `y` is the bottom of the rendered
    // content in content-space; the gap between content and the
    // visible viewport (`height`) is the legal scroll range.
    let total_h = y;
    ctx.scroll.set_max_scroll((total_h - height).max(0.0));

    // Sidebar scrollbar — anchored just inside the right-edge
    // separator (`sb_w - 1.0` is the separator itself).
    ctx.scroll.draw_bar(canvas, sb_w - 1.0, height, total_h, 0.0);
}

impl KeptApp {
    pub(super) fn render_sidebar(&mut self, canvas: &Canvas, height: f32) {
        let tag_names = self.all_tag_names_in_memory();
        // Access view via direct field path (self.panes[i].view) rather
        // than the Deref-to-active-pane shortcut so the borrow disjoins
        // from `&mut self.sidebar_scroll` / `&mut self.hit_tests_builder`.
        let active = self.active_pane;
        let mut ctx = SidebarRenderCtx {
            font_scale: self.font_scale,
            typeface: &self.typeface,
            mouse_pos: self.mouse_pos,
            view: &self.panes[active].view,
            document: &self.document,
            show_inactive_cells: self.show_inactive_cells,
            tag_names,
            scroll: &mut self.sidebar_scroll,
            hit_tests: &mut self.hit_tests_builder,
        };
        render(canvas, height, &mut ctx);
    }
}
