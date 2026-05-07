use skia_safe::{Canvas, Font, Paint, Point, Rect};

use crate::cell::now_epoch_ms;
use crate::query;

use super::{
    KeptApp, PageKind, Query, SIDEBAR_HEADER_FONT_SIZE, SIDEBAR_WIDTH, ViewKind,
    format_date_label, local_date_for_ms,
};

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
        p.set_color(crate::color::ACCENT_BLUE_SELECTION);
        canvas.draw_round_rect(rect, radius, radius, &p);
    } else if is_hovered {
        let mut p = Paint::default();
        p.set_anti_alias(true);
        p.set_color(crate::color::HOVER_FAINT);
        canvas.draw_round_rect(rect, radius, radius, &p);
    }
    let mut text = Paint::default();
    text.set_anti_alias(true);
    text.set_color(crate::color::TEXT_PRIMARY);
    let (_, m) = font.metrics();
    let baseline = rect.top + (rect.height() + (-m.ascent) - m.descent) * 0.5;
    canvas.draw_str(label, Point::new(text_x, baseline), font, &text);
}

impl KeptApp {
    pub(super) fn render_sidebar(&mut self, canvas: &Canvas, height: f32) {
        let scale = self.font_scale;
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
        bg_paint.set_color(crate::color::BG_PANEL);
        canvas.draw_rect(Rect::new(0.0, 0.0, sb_w, height.max(0.0)), &bg_paint);
        // Right-edge separator.
        let mut sep = Paint::default();
        sep.set_anti_alias(false);
        sep.set_color(crate::color::DIVIDER_PANE);
        canvas.draw_rect(
            Rect::new(sb_w - 1.0, 0.0, sb_w, height.max(0.0)),
            &sep,
        );

        // Everything below the bg + edge separator scrolls together
        // under `sidebar_scroll.scroll_y`. Clip so off-screen rows
        // don't leak into the doc area, then translate so y values
        // inside this block stay in natural content coordinates (rects
        // are recorded in content-space; hover tests below add
        // scroll_y to mouse_y before comparing).
        let scroll_y = self.sidebar_scroll.scroll_y;
        canvas.save();
        canvas.clip_rect(Rect::new(0.0, 0.0, sb_w, height.max(0.0)), None, true);
        canvas.translate((0.0, -scroll_y));

        let header_font =
            Font::from_typeface(&self.typeface, SIDEBAR_HEADER_FONT_SIZE * scale);
        let mut header_paint = Paint::default();
        header_paint.set_anti_alias(true);
        header_paint.set_color(crate::color::TEXT_SECTION_HEADER);
        let (_, hm) = header_font.metrics();

        // Row hit-tests are rebuilt every frame; clear stale ones.
        self.hit_tests.sidebar.contexts.clear();
        self.hit_tests.sidebar.dates.clear();
        self.hit_tests.sidebar.tags.clear();
        self.hit_tests.sidebar.pages.clear();

        let row_font =
            Font::from_typeface(&self.typeface, SIDEBAR_DATE_FONT_SIZE * scale);
        let mouse_x = self.mouse_pos.0;
        // Content-coords mouse_y so hover tests match content-space rects.
        let mouse_y = self.mouse_pos.1 + scroll_y;
        let in_row = |r: Rect| {
            mouse_x >= r.left && mouse_x <= r.right && mouse_y >= r.top && mouse_y <= r.bottom
        };

        // ---- PAGES section ----
        let pages_header_baseline = pad_top + (-hm.ascent);
        canvas.draw_str(
            "PAGES",
            Point::new(pad_x, pages_header_baseline),
            &header_font,
            &header_paint,
        );
        let mut y = pad_top + header_h;
        let people_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
        draw_sidebar_row(
            canvas,
            people_rect,
            "People",
            matches!(self.view.view_kind, ViewKind::People),
            in_row(people_rect),
            radius,
            pad_x,
            &row_font,
        );
        self.hit_tests.sidebar.pages.push((PageKind::People, people_rect));
        y += date_h + item_gap + date_gap;

        // ---- CONTEXTS section ----
        let contexts_header_baseline = y + (-hm.ascent);
        canvas.draw_str(
            "CONTEXTS",
            Point::new(pad_x, contexts_header_baseline),
            &header_font,
            &header_paint,
        );
        y += header_h;

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

        for d in dates {
            let date_rect = Rect::new(pad_x * 0.5, y, sb_w - pad_x * 0.5, y + date_h);
            draw_sidebar_row(
                canvas,
                date_rect,
                &format_date_label(d),
                self.view == Query::date(d),
                in_row(date_rect),
                radius,
                pad_x,
                &row_font,
            );
            self.hit_tests.sidebar.dates.push((d, date_rect));
            y += date_h + item_gap + date_gap;
        }

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
                draw_sidebar_row(
                    canvas,
                    row_rect,
                    &format!("#{name}"),
                    self.view.is_solo_tag(&name),
                    in_row(row_rect),
                    radius,
                    pad_x,
                    &row_font,
                );
                self.hit_tests.sidebar.tags.push((name, row_rect));
                y += date_h + item_gap;
            }
        }

        canvas.restore();

        // Update scroll bounds. `y` is the bottom of the rendered
        // content in content-space; the gap between content and the
        // visible viewport (`height`) is the legal scroll range.
        let total_h = y;
        self.sidebar_scroll
            .set_max_scroll((total_h - height).max(0.0));

        // Sidebar scrollbar — anchored just inside the right-edge
        // separator (`sb_w - 1.0` is the separator itself).
        self.sidebar_scroll
            .draw_bar(canvas, sb_w - 1.0, height, total_h);
    }
}
