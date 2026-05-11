use skia_safe::{Canvas, Font, Paint, PaintStyle, Point, Rect};
use uuid::Uuid;

use super::{KeptApp, clamp_rect_to_viewport};

const MENTION_POPUP_WIDTH: f32 = 220.0;
const MENTION_POPUP_ROW_H: f32 = 28.0;
const MENTION_POPUP_PAD: f32 = 6.0;
const MENTION_POPUP_RADIUS: f32 = 6.0;
const MENTION_POPUP_MAX_VISIBLE: usize = 6;
const MENTION_BODY_FONT_SIZE: f32 = 16.0;

/// Heavy penalty applied to inactive candidates in the @-mention popup.
/// Typical short-query fuzzy scores are in roughly `[0, 30]`, so an
/// inactive match always ranks below any active match — but the user can
/// still find an inactive person by typing enough of the name.
const INACTIVE_FUZZY_PENALTY: i32 = 50;

pub(super) struct MentionPopup {
    /// What the popup is anchored to: a focused cell's text or the search
    /// bar's input. Drives sync, render-anchor, and commit behavior.
    source: MentionSource,
    /// Whether this popup is for `@`-person mentions or `#`-tag tags.
    /// Determines candidate source (`person_mention_candidates` vs
    /// `tag_mention_candidates`), trigger character, prefix glyph in
    /// the rendered list, and commit semantics.
    kind: MentionKind,
    /// Byte position of the trigger character (`@` or `#`) in the
    /// source's text.
    anchor_byte: usize,
    /// Currently typed query (text after the trigger, no whitespace).
    query: String,
    /// Index of the highlighted item in the filtered list.
    selected: usize,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum MentionKind {
    Person,
    Tag,
}

impl MentionKind {
    /// The trigger character that opens this popup kind.
    fn trigger(self) -> &'static str {
        match self {
            MentionKind::Person => "@",
            MentionKind::Tag => "#",
        }
    }
}

#[derive(Clone, Copy)]
enum MentionSource {
    Cell { cell_id: Uuid, bullet_id: Option<Uuid> },
    SearchBar,
}

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

impl KeptApp {
    /// `(display_name, entity_id)` for every person entity, sorted
    /// alphabetically. Thin view over `self.entities`. Drives the
    /// `@`-mention popup; commit inserts `kept://<entity_id>` (invariant
    /// #1 — the @-popup speaks entity-id space).
    fn person_entries(&self) -> Vec<(String, Uuid)> {
        let mut out: Vec<(String, Uuid)> = self
            .entities
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
            .entities
            .iter()
            .filter(|e| e.kind == "person")
            .map(|e| (e.display_name.clone(), e.is_active))
            .collect();
        out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        out
    }

    /// Tag candidates for the `#`-autocomplete popup. Aggregated from
    /// the in-memory cells so a tag committed in this session shows up
    /// in the candidate list before the debounced save flushes. All
    /// tags are treated as "active" — there's no inactive-tag concept.
    fn tag_mention_candidates(&self) -> Vec<(String, bool)> {
        self.all_tag_names_in_memory()
            .into_iter()
            .map(|n| (n, true))
            .collect()
    }

    /// Pick the candidate set for the popup's current kind.
    fn mention_candidates_for(&self, kind: MentionKind) -> Vec<(String, bool)> {
        match kind {
            MentionKind::Person => self.person_mention_candidates(),
            MentionKind::Tag => self.tag_mention_candidates(),
        }
    }

    pub(super) fn try_open_mention_popup(&mut self, kind: MentionKind) {
        let trigger = kind.trigger();
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
            if text.get(caret - 1..caret) != Some(trigger) {
                return;
            }
            self.mention_popup = Some(MentionPopup {
                source: MentionSource::SearchBar,
                kind,
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
        // Tag autocomplete: open in any editable text slot EXCEPT the
        // PopPop body, where `#` is the comment-line marker (not a tag).
        if kind == MentionKind::Tag
            && !cell.title_focused
            && matches!(cell.kind, crate::cell::CellKind::PopPop(_))
        {
            return;
        }
        let Some((text, caret)) = cell.focused_text_and_caret() else {
            return;
        };
        if caret == 0 {
            return;
        }
        // Caret should be just past the trigger character.
        if text.get(caret - 1..caret) != Some(trigger) {
            return;
        }
        self.mention_popup = Some(MentionPopup {
            source: MentionSource::Cell {
                cell_id: focused_id,
                bullet_id: cell.focused_bullet_id(),
            },
            kind,
            anchor_byte: caret - 1,
            query: String::new(),
            selected: 0,
        });
    }

    pub(super) fn sync_mention_popup(&mut self) {
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
        // The trigger character must still be at anchor_byte.
        let trigger = popup.kind.trigger();
        if text
            .get(anchor_byte..)
            .map_or(true, |s| !s.starts_with(trigger))
        {
            self.mention_popup = None;
            return;
        }
        // Caret must be at or past the trigger itself.
        if caret < anchor_byte + 1 {
            self.mention_popup = None;
            return;
        }
        // Query is everything between the trigger and the caret. Whitespace
        // breaks it.
        let Some(q) = text.get(anchor_byte + 1..caret) else {
            self.mention_popup = None;
            return;
        };
        if q.chars().any(|c| c.is_whitespace()) {
            self.mention_popup = None;
            return;
        }
        let query = q.to_string();
        let kind = popup.kind;
        let candidates = self.mention_candidates_for(kind);
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

    pub(super) fn render_mention_popup(&mut self, canvas: &Canvas, view_w: f32, view_h: f32) {
        // Snapshot the popup state into locals so subsequent &mut self
        // writes (hit_tests_builder) don't fight the popup borrow.
        let (source, kind, anchor_byte, query, selected) = {
            let Some(popup) = self.mention_popup.as_ref() else {
                return;
            };
            (
                popup.source,
                popup.kind,
                popup.anchor_byte,
                popup.query.clone(),
                popup.selected,
            )
        };

        let (anchor_x, anchor_y_below) = match source {
            MentionSource::Cell { cell_id, bullet_id: _ } => {
                let Some(cell) = self.cell(cell_id) else {
                    return;
                };
                let Some((x, y)) = cell.anchor_doc_pos(anchor_byte) else {
                    return;
                };
                // Doc-space → window-space: subtract scroll.
                (x, y - self.scroll_y)
            }
            MentionSource::SearchBar => {
                let Some(state) = self.search.as_ref() else { return };
                let Some((x, _)) = state.input.doc_position_of_byte(anchor_byte) else {
                    return;
                };
                let Some((_, bot)) = state.input.line_y_band_of_byte(anchor_byte) else {
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

        let candidates = self.mention_candidates_for(kind);
        let items = filter_mentions(&candidates, &query);
        let visible = items.len().min(MENTION_POPUP_MAX_VISIBLE);
        // The "Add @X" / "Add #X" row appears at the bottom only when
        // the user typed something AND nothing matched. Mouse-only —
        // see commit_mention_via_keyboard for why.
        let show_add_row = items.is_empty() && !query.is_empty();
        let row_count = if items.is_empty() {
            // Hint row ("No matches…" / "Type to search…") plus the
            // optional Add row.
            if show_add_row { 2 } else { 1 }
        } else {
            visible
        };
        let popup_h = (row_count as f32) * row_h + pad * 2.0;

        // Anchor below the trigger char, then clamp into the viewport
        // so a popup near the right or bottom edge doesn't paint past
        // the window.
        let initial_top = anchor_y_below + 4.0 * scale;
        let clamped = clamp_rect_to_viewport(
            Rect::new(anchor_x, initial_top, anchor_x + popup_w, initial_top + popup_h),
            view_w,
            view_h,
            4.0,
        );
        let popup_x = clamped.left;
        let popup_y = clamped.top;

        // Drop shadow (drawn first, slightly offset).
        let mut shadow_paint = Paint::default();
        shadow_paint.set_anti_alias(true);
        shadow_paint.set_color(crate::color::shadow_soft());
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
        bg_paint.set_color(crate::color::bg_card());
        let popup_rect = Rect::new(popup_x, popup_y, popup_x + popup_w, popup_y + popup_h);
        canvas.draw_round_rect(popup_rect, radius, radius, &bg_paint);

        // Border.
        let mut border_paint = Paint::default();
        border_paint.set_anti_alias(true);
        border_paint.set_style(PaintStyle::Stroke);
        border_paint.set_stroke_width(1.0);
        border_paint.set_color(crate::color::menu_border());
        canvas.draw_round_rect(popup_rect, radius, radius, &border_paint);

        let body_font = Font::from_typeface(&self.typeface, MENTION_BODY_FONT_SIZE * scale);
        let (_, m) = body_font.metrics();
        let row_text_height = -m.ascent + m.descent;
        let text_offset_in_row = (row_h - row_text_height) * 0.5 + (-m.ascent);

        if items.is_empty() {
            // Hint row.
            let mut hint_paint = Paint::default();
            hint_paint.set_anti_alias(true);
            hint_paint.set_color(crate::color::text_muted_grey());
            let hint_y = popup_y + pad;
            let baseline = hint_y + text_offset_in_row;
            let label = if query.is_empty() {
                "Type to search…".to_string()
            } else {
                format!("No matches for \"{}\"", query)
            };
            canvas.draw_str(
                label,
                Point::new(popup_x + 12.0 * scale, baseline),
                &body_font,
                &hint_paint,
            );

            if show_add_row {
                let add_y = hint_y + row_h;
                let add_rect = Rect::new(
                    popup_x + 4.0 * scale,
                    add_y,
                    popup_x + popup_w - 4.0 * scale,
                    add_y + row_h,
                );
                let mouse = self.mouse_pos;
                let hovered = mouse.0 >= add_rect.left
                    && mouse.0 <= add_rect.right
                    && mouse.1 >= add_rect.top
                    && mouse.1 <= add_rect.bottom;
                if hovered {
                    let mut hp = Paint::default();
                    hp.set_anti_alias(true);
                    hp.set_color(crate::color::accent_blue_selection());
                    canvas.draw_round_rect(add_rect, 4.0 * scale, 4.0 * scale, &hp);
                }
                let mut text_paint = Paint::default();
                text_paint.set_anti_alias(true);
                text_paint.set_color(crate::color::text_primary());
                let baseline = add_y + text_offset_in_row;
                canvas.draw_str(
                    format!("Add {}{}", kind.trigger(), query),
                    Point::new(popup_x + 12.0 * scale, baseline),
                    &body_font,
                    &text_paint,
                );
                self.hit_tests_builder.mention_popup.add_row = Some(add_rect);
            }
            return;
        }

        let mut dim_paint = Paint::default();
        dim_paint.set_anti_alias(true);
        dim_paint.set_color(crate::color::text_muted_grey());

        let mut match_paint = Paint::default();
        match_paint.set_anti_alias(true);
        match_paint.set_color(crate::color::text_primary());

        let mut hl_paint = Paint::default();
        hl_paint.set_anti_alias(true);
        hl_paint.set_color(crate::color::accent_blue_selection());

        let sel_idx = selected.min(visible - 1);
        let mut row_y = popup_y + pad;
        for (i, (item, matches)) in items.iter().take(visible).enumerate() {
            let row_rect = Rect::new(
                popup_x + 4.0 * scale,
                row_y,
                popup_x + popup_w - 4.0 * scale,
                row_y + row_h,
            );
            let mouse = self.mouse_pos;
            let mouse_hover = mouse.0 >= row_rect.left
                && mouse.0 <= row_rect.right
                && mouse.1 >= row_rect.top
                && mouse.1 <= row_rect.bottom;
            if i == sel_idx || mouse_hover {
                canvas.draw_round_rect(row_rect, 4.0 * scale, 4.0 * scale, &hl_paint);
            }
            let baseline = row_y + text_offset_in_row;
            let text_x = popup_x + 12.0 * scale;
            // Render the trigger in dim, then alternate dim / match-paint
            // runs across the suggestion's letters.
            let trigger = kind.trigger();
            let trigger_w = body_font.measure_str(trigger, Some(&dim_paint)).0;
            canvas.draw_str(trigger, Point::new(text_x, baseline), &body_font, &dim_paint);
            draw_runs_with_matches(
                canvas,
                item,
                matches,
                Point::new(text_x + trigger_w, baseline),
                &body_font,
                &match_paint,
                &dim_paint,
            );
            self.hit_tests_builder.mention_popup.rows.push(row_rect);
            row_y += row_h;
        }
    }

    pub(super) fn mention_popup_move(&mut self, delta: i32) {
        let Some(kind) = self.mention_popup.as_ref().map(|p| p.kind) else {
            return;
        };
        let candidates = self.mention_candidates_for(kind);
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

    /// Commit the highlighted item from the mention popup. For person
    /// (`@`) mentions, replaces `@query` with the person's title and
    /// attaches a `kept://<source-cell-id>` link span. For tag (`#`)
    /// mentions, replaces `#query` with the literal `#tagname` as plain
    /// text — the title's tag-extraction pass picks it up. Both record
    /// one undo entry.
    pub(super) fn commit_mention(&mut self) -> bool {
        let Some(popup) = self.mention_popup.take() else {
            return false;
        };
        let candidates = self.mention_candidates_for(popup.kind);
        let filtered = filter_mentions(&candidates, &popup.query);
        let Some(selected) = filtered.get(popup.selected) else {
            return true;
        };
        let chosen_name = selected.0.clone();

        let start = popup.anchor_byte;
        let end = start + 1 + popup.query.len();

        match popup.kind {
            MentionKind::Person => {
                let entries = self.person_entries();
                let Some((_, source_id)) =
                    entries.iter().find(|(n, _)| n == &chosen_name)
                else {
                    return true;
                };
                let source_id = *source_id;
                self.commit_person_mention(popup.source, start, end, chosen_name, source_id);
            }
            MentionKind::Tag => {
                self.commit_tag_mention(popup.source, start, end, chosen_name);
            }
        }
        self.coalesce_break = true;
        true
    }

    /// Commit a specific row by index (mouse click). Sets the popup's
    /// selected index and runs the same path as keyboard Enter.
    pub(super) fn commit_mention_row(&mut self, idx: usize) -> bool {
        if let Some(p) = self.mention_popup.as_mut() {
            p.selected = idx;
        }
        self.commit_mention()
    }

    /// Commit the typed query as a brand-new entity (for `@`) or tag
    /// (for `#`). Reachable only via mouse-click on the "Add @X" /
    /// "Add #X" row — keyboard Enter dismisses without commit so an
    /// accidental Return doesn't create something the user didn't
    /// pick deliberately.
    pub(super) fn commit_add_mention(&mut self) -> bool {
        let Some(popup) = self.mention_popup.take() else {
            return false;
        };
        let query = popup.query.trim().to_string();
        if query.is_empty() {
            return false;
        }
        let start = popup.anchor_byte;
        let end = start + 1 + popup.query.len();

        match popup.kind {
            MentionKind::Person => {
                let new_id = match self.db.as_mut() {
                    Some(db) => match db.create_cell_less_person_entity(&query) {
                        Ok(id) => id,
                        Err(e) => {
                            eprintln!(
                                "kept: create_cell_less_person_entity failed: {e}",
                            );
                            return false;
                        }
                    },
                    None => return false,
                };
                self.refresh_entities();
                let created_at = self
                    .entities
                    .entities
                    .iter()
                    .find(|e| e.id == new_id)
                    .map(|e| e.created_at)
                    .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
                self.undo_stack.push(super::UndoOp::CreateCelllessEntity {
                    entity_id: new_id,
                    name: query.clone(),
                    created_at,
                });
                self.redo_stack.clear();
                self.commit_person_mention(popup.source, start, end, query, new_id);
            }
            MentionKind::Tag => {
                self.commit_tag_mention(popup.source, start, end, query);
            }
        }
        self.coalesce_break = true;
        true
    }

    /// Insert a person mention: a clickable `kept://<entity_id>` link
    /// (cell context) or a `@Slug_Underscored` token (search context).
    fn commit_person_mention(
        &mut self,
        source: MentionSource,
        start: usize,
        end: usize,
        chosen_name: String,
        source_id: Uuid,
    ) {
        match source {
            MentionSource::Cell { cell_id, bullet_id: _ } => {
                let pre = match self.cell(cell_id) {
                    Some(c) => c.snapshot(),
                    None => return,
                };
                let url = format!("kept://{}", source_id);
                if let Some(c) = self.cell_mut(cell_id) {
                    c.replace_focused_with_link(start..end, chosen_name, url);
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
                // Replace `@<query>` with `@<Title_Cased_With_Underscores>`
                // so the resulting query string is readable and parses
                // cleanly (entity tokens can't contain whitespace). The
                // executor's resolver normalizes both sides — strips
                // whitespace and underscores, lowercases — so
                // `@Patrick_Foy` matches the person cell titled
                // "Patrick Foy".
                let slug = chosen_name
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("_");
                self.replace_search_or_cell_text(
                    source,
                    start,
                    end,
                    format!("@{slug}"),
                );
            }
        }
    }

    /// Insert a tag mention: replace `#<query>` with `#<chosen>`. For
    /// cell sources, the inserted run is also marked as a `TagSpan`
    /// so persistence recognizes it as a tag (typed `#X` without a
    /// span is just plain text). For the search-input source there's
    /// no styled rendering, so a plain text replace is enough — the
    /// query parser reads `#tag` syntactically.
    fn commit_tag_mention(
        &mut self,
        source: MentionSource,
        start: usize,
        end: usize,
        chosen_name: String,
    ) {
        let replacement = format!("#{chosen_name}");
        match source {
            MentionSource::Cell { cell_id, bullet_id: _ } => {
                let pre = match self.cell(cell_id) {
                    Some(c) => c.snapshot(),
                    None => return,
                };
                if let Some(c) = self.cell_mut(cell_id) {
                    c.replace_focused_with_tag(start..end, replacement);
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
                self.replace_search_or_cell_text(source, start, end, replacement);
            }
        }
    }

    /// Plain-text replacement of `[start..end]` with `replacement` in
    /// whichever source the popup was anchored on. Records an undo edit
    /// for cell sources; mutates the search input directly otherwise.
    fn replace_search_or_cell_text(
        &mut self,
        source: MentionSource,
        start: usize,
        end: usize,
        replacement: String,
    ) {
        match source {
            MentionSource::Cell { cell_id, bullet_id: _ } => {
                let pre = match self.cell(cell_id) {
                    Some(c) => c.snapshot(),
                    None => return,
                };
                if let Some(c) = self.cell_mut(cell_id) {
                    c.replace_focused_with_text(start..end, replacement);
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
                if let Some(state) = self.search.as_mut() {
                    let txt = state.input.text();
                    if start <= txt.len() && end <= txt.len() {
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
    }
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
}
