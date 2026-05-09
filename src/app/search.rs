use skia_safe::{
    BlurStyle, Canvas, Font, MaskFilter, Paint, PaintStyle, Point, Rect,
};
use uuid::Uuid;

use crate::cell::{Cell, TextBox, now_epoch_ms};
use crate::query;

use super::{KeptApp, Query, fit_text_ellipsized, format_date_label, local_date_for_ms};

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

pub(super) struct SearchState {
    pub(super) input: TextBox,
    /// Index of the highlighted result row. Reset to 0 on text change.
    pub(super) selected: usize,
    /// Result list from the last render where the @-mention popup was
    /// closed. While the user is mid-pick (mention popup open), we keep
    /// showing these so the search-popup result list doesn't churn on
    /// every keystroke of the in-progress `@<query>` token.
    pub(super) cached_results: Vec<Uuid>,
}

impl KeptApp {
    pub(super) fn render_search_popup(&mut self, canvas: &Canvas, width: f32) {
        if self.search.is_none() {
            self.hit_tests.search.input = None;
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
        shadow.set_color(crate::color::shadow_menu());
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
        bg.set_color(crate::color::bg_card());
        let card = Rect::new(popup_x, popup_y, popup_x + popup_w, popup_y + popup_h);
        canvas.draw_round_rect(card, radius, radius, &bg);

        let mut border = Paint::default();
        border.set_anti_alias(true);
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(2.0);
        border.set_color(crate::color::panel_border_warm());
        canvas.draw_round_rect(card, radius, radius, &border);

        // Input row: drive the TextBox directly so caret, selection, arrow
        // nav, word jumps, and line edges all work natively.
        let input_x = popup_x + pad;
        let input_y = popup_y + pad;
        let input_w = popup_w - pad * 2.0;
        if let Some(state) = self.search.as_mut() {
            state.input.tick(canvas, input_x, input_y, input_w, true, true);
        }
        self.hit_tests.search.input = Some(Rect::new(
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
            hint.set_color(crate::color::text_ghost_warm());
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
        div.set_color(crate::color::toggle_inactive_bg());
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
        date_paint.set_color(crate::color::text_muted_warm_soft());
        let mut row_paint = Paint::default();
        row_paint.set_anti_alias(true);
        row_paint.set_color(crate::color::text_primary());

        let selected = self.search.as_ref().map(|s| s.selected).unwrap_or(0);
        let mut row_y = popup_y + pad + input_h;
        for (i, &id) in results.iter().take(SEARCH_MAX_VISIBLE).enumerate() {
            let is_selected = i == selected.min(visible.saturating_sub(1));
            if is_selected {
                let mut sel = Paint::default();
                sel.set_anti_alias(true);
                sel.set_color(crate::color::accent_blue_selection());
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
                // Truncate to fit the row's remaining width (right
                // edge inset by `pad * 0.5` to match the highlight
                // bg). Otherwise long snippets overflow the popup
                // border into nothingness.
                let snippet_left = popup_x + pad + date_w + 12.0 * scale;
                let snippet_right = popup_x + popup_w - pad * 0.5;
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
            row_y += result_h;
        }

        if visible == 0 && !query.trim().is_empty() {
            let baseline = popup_y + pad + input_h + (result_h + (-rm.ascent) - rm.descent) * 0.5;
            let mut empty_paint = Paint::default();
            empty_paint.set_anti_alias(true);
            empty_paint.set_color(crate::color::text_section_header());
            canvas.draw_str(
                "no matches",
                Point::new(popup_x + pad, baseline),
                &result_font,
                &empty_paint,
            );
        }
    }

    pub(super) fn open_search(&mut self) {
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

    pub(super) fn close_search_cancel(&mut self) {
        if self.search.take().is_some() {
            // Doc area was never replaced; nothing to restore.
            self.coalesce_break = true;
        }
    }

    /// Enter on the search popup: jump to the highlighted result. View
    /// becomes that cell's date and the cell is focused. Empty / no-match
    /// input just closes the popup.
    pub(super) fn close_search_commit(&mut self, in_other_pane: bool) {
        let Some(state) = self.search.take() else { return };
        let query = state.input.text().to_string();
        let results = self.search_results(&query);
        let Some(&id) = results.get(state.selected) else {
            self.coalesce_break = true;
            return;
        };
        if let Some(cell) = self.cell(id) {
            let target_date = local_date_for_ms(cell.timestamp);
            // Track the destination pane so the cell-focus / scroll
            // step lands there, since `open_in_other_pane` preserves
            // the *active* pane (the user expects their typing focus
            // to stay where they were searching from).
            let dest_pane = if in_other_pane {
                self.open_in_other_pane(Query::date(target_date))
            } else if self.push_view(Query::date(target_date)) {
                Some(self.active_pane)
            } else {
                Some(self.active_pane)
            };
            if let Some(idx) = dest_pane {
                let pane = &mut self.panes[idx];
                pane.focused = Some(id);
                pane.editing = false;
                pane.coalesce_break = true;
                pane.pending_caret_scroll = true;
                return;
            }
        }
        // Fallback (cell vanished): no focus changes; just break
        // coalesce so the next edit starts a fresh undo entry.
        self.coalesce_break = true;
    }

    /// Top-N matching cell IDs for the popup result list. Parses `query`
    /// through the language, runs the executor, and sorts most-recent first.
    pub(super) fn search_results(&self, query: &str) -> Vec<Uuid> {
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
        // Inactive cells drop out of search results unless the
        // global "Show archived" toggle is on, mirroring the
        // visibility gate in `is_visible_for_view`. Otherwise an
        // archived cell could surface here, the user clicks it, and
        // navigates to a date view that has it filtered out — a
        // dead-end click. Bullet-level cascade isn't applied (search
        // returns whole cells; the cell-level gate is enough).
        let show_inactive_cells = self.show_inactive_cells;
        let mut hits: Vec<&Cell> = self
            .cells
            .iter()
            .filter(|c| (c.active || show_inactive_cells) && query::matches(&ast, c, &ctx))
            .collect();
        hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        hits.into_iter().map(|c| c.id).collect()
    }

    pub(super) fn search_move(&mut self, delta: i32) {
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
