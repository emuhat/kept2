use skia_safe::{Canvas, Font, Paint, PaintStyle, Point, Rect, Typeface};
use uuid::Uuid;

use super::{HitTestState, KeptApp, clamp_rect_to_viewport};
use crate::thread::ThreadId;

/// Per-frame context the mention-popup render needs from `KeptApp`.
/// Used by `MentionPopup::render` (S8: explicit subsystem scope). The
/// anchor position and the candidate list are computed by the caller
/// and passed in as parameters — they involve borrows of cell /
/// search / entity state that the popup itself doesn't need to hold.
pub(super) struct MentionRenderCtx<'a> {
    pub(super) font_scale: f32,
    pub(super) typeface: &'a Typeface,
    pub(super) mouse_pos: (f32, f32),
    /// Per-frame hit-test write surface (see S2).
    pub(super) hit_tests: &'a mut HitTestState,
}

const MENTION_POPUP_WIDTH: f32 = 220.0;
const MENTION_POPUP_ROW_H: f32 = 28.0;
const MENTION_POPUP_PAD: f32 = 6.0;
const MENTION_POPUP_RADIUS: f32 = 6.0;
const MENTION_POPUP_MAX_VISIBLE: usize = 6;
const MENTION_BODY_FONT_SIZE: f32 = 16.0;
/// Smaller font for the right-justified mention count — metadata,
/// not primary content, so it shouldn't compete with the name.
const MENTION_COUNT_FONT_SIZE: f32 = 11.0;

/// Heavy penalty applied to inactive candidates in the @-mention popup.
/// Typical short-query fuzzy scores are in roughly `[0, 30]`, so an
/// inactive match always ranks below any active match — but the user can
/// still find an inactive person by typing enough of the name.
const INACTIVE_FUZZY_PENALTY: i32 = 50;

/// Per-mention score bonus added on top of the fuzzy score. Small
/// enough that a clearly-better fuzzy match still wins, but big
/// enough that two ambiguous initial-matches break in favor of the
/// person the user actually interacts with. The bonus saturates so
/// a single power-user contact doesn't completely drown out new
/// matches.
const MENTION_FREQUENCY_WEIGHT: i32 = 1;
const MENTION_FREQUENCY_CAP: i32 = 12;

pub(super) struct MentionPopup {
    /// What the popup is anchored to: a focused cell's text or the search
    /// bar's input. Drives sync, render-anchor, and commit behavior.
    pub(super) source: MentionSource,
    /// Whether this popup is for `@`-person mentions or `#`-tag tags.
    /// Determines candidate source (`person_mention_candidates` vs
    /// `tag_mention_candidates`), trigger character, prefix glyph in
    /// the rendered list, and commit semantics.
    kind: MentionKind,
    /// Byte position of the trigger character (`@` or `#`) in the
    /// source's text.
    anchor_byte: usize,
    /// Currently typed query. For text-anchored modes this mirrors
    /// the substring after the trigger character (kept in sync by
    /// `sync_mention_popup`). For ThreadAttach mode the
    /// `attach_input` `TextBox` is the authoritative store and
    /// `query` is just its `.text()` snapshot.
    query: String,
    /// Index of the highlighted item in the filtered list. `None`
    /// while the query is empty — no row is pre-selected on open,
    /// so an accidental Enter doesn't commit the first candidate.
    /// Set to `Some(0)` the moment the user types a character or
    /// presses an arrow key.
    selected: Option<usize>,
    /// Standalone text input — `Some` only for ThreadAttach mode,
    /// where the popup has no backing TextBox in the document. The
    /// input renders inside the query band and owns its own caret /
    /// selection / clipboard behavior just like a cell title.
    pub(super) attach_input: Option<crate::cell::TextBox>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum MentionKind {
    Person,
    Tag,
    Thread,
}

impl MentionKind {
    /// The trigger character that opens this popup kind.
    pub(super) fn trigger(self) -> &'static str {
        match self {
            MentionKind::Person => "@",
            MentionKind::Tag => "#",
            MentionKind::Thread => "%",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum MentionSource {
    Cell { cell_id: Uuid, bullet_id: Option<Uuid> },
    /// The pane's URL-bar pill (replaces the old standalone
    /// Ctrl+K popup). `pane_idx` is the pane the focused header
    /// belongs to.
    PaneHeader { pane_idx: usize },
    /// The Quick-Add modal's in-flight cell. There's at most one
    /// Quick-Add open at a time, so the source needs no
    /// disambiguator beyond the variant tag.
    QuickAdd,
    /// The "Attach to thread…" picker (right-click → menu row).
    /// No backing text — commit goes through `attach_to_thread`
    /// against `target`, anchored where the menu was. Only
    /// `MentionKind::Thread` opens with this source.
    ThreadAttach {
        target: crate::cell::ReferenceTarget,
        anchor: (f32, f32),
    },
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

/// Rank `names` by fuzzy match against `query`. Empty query returns
/// the names in their input order. Mention count contributes a small
/// Bring `selected` in line with the current query + visible count.
/// Empty query → no selection (so a stray Enter doesn't commit the
/// first row the user happens to be hovering); non-empty query →
/// `Some(0)` if there was no prior selection, else clamped into
/// `[0, count - 1]`. Skipped entirely when `count == 0` — the
/// caller already won't read the index in that case.
fn reconcile_selected(selected: &mut Option<usize>, query: &str, count: usize) {
    if query.is_empty() {
        *selected = None;
        return;
    }
    if count == 0 {
        return;
    }
    match selected {
        None => *selected = Some(0),
        Some(i) if *i >= count => *selected = Some(count - 1),
        _ => {}
    }
}

/// capped bonus (so frequently-mentioned people tie-break above
/// rare ones without overwhelming a clearly-better fuzzy match).
/// Returns `(name, match_indices, mention_count)` for each surviving
/// candidate so the renderer can display the count next to the name.
fn filter_mentions(
    candidates: &[(String, bool, usize)],
    query: &str,
) -> Vec<(String, Vec<usize>, usize)> {
    if query.is_empty() {
        return candidates
            .iter()
            .map(|(n, _, c)| (n.clone(), Vec::new(), *c))
            .collect();
    }
    let mut scored: Vec<(i32, String, Vec<usize>, usize)> = candidates
        .iter()
        .filter_map(|(name, is_active, mention_count)| {
            fuzzy_score(query, name).map(|(s, m)| {
                let mut s = if *is_active {
                    s
                } else {
                    s - INACTIVE_FUZZY_PENALTY
                };
                let bonus = (*mention_count as i32 * MENTION_FREQUENCY_WEIGHT)
                    .min(MENTION_FREQUENCY_CAP);
                s += bonus;
                (s, name.clone(), m, *mention_count)
            })
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, n, m, c)| (n, m, c)).collect()
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

impl MentionPopup {
    /// Render the popup card + its rows. Caller pre-computes
    /// `anchor_pos` (window-space `(x, y_below_trigger)`) and the
    /// `candidates` list — both involve borrows of cell / search /
    /// entity state that the popup itself doesn't need to hold.
    pub(super) fn render(
        &mut self,
        canvas: &Canvas,
        view_w: f32,
        view_h: f32,
        anchor_pos: (f32, f32),
        candidates: &[(String, bool, usize)],
        ctx: &mut MentionRenderCtx<'_>,
    ) {
        let (anchor_x, anchor_y_below) = anchor_pos;
        let kind = self.kind;
        let query = &self.query;
        let selected = self.selected;

        let scale = ctx.font_scale;
        let popup_w = MENTION_POPUP_WIDTH * scale;
        let row_h = MENTION_POPUP_ROW_H * scale;
        let pad = MENTION_POPUP_PAD * scale;
        let radius = MENTION_POPUP_RADIUS * scale;

        let items = filter_mentions(candidates, query);
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
        // ThreadAttach has no backing text source, so we render a
        // visible "query band" at the top of the popup. Other modes
        // surface the query via the user's typed `@`/`#`/`%` chars
        // in the cell text underneath.
        let show_query_band = matches!(self.source, MentionSource::ThreadAttach { .. });
        let query_band_h = if show_query_band { row_h } else { 0.0 };
        let popup_h = (row_count as f32) * row_h + query_band_h + pad * 2.0;

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

        let body_font = Font::from_typeface(ctx.typeface, MENTION_BODY_FONT_SIZE * scale);
        let (_, m) = body_font.metrics();
        let row_text_height = -m.ascent + m.descent;
        let text_offset_in_row = (row_h - row_text_height) * 0.5 + (-m.ascent);

        // ThreadAttach query band: hosts a real `TextBox` so caret,
        // selection, arrow nav, clipboard, etc. behave like any
        // other editable text in the app. Other popup modes show
        // their query in the underlying cell text instead.
        if show_query_band {
            let band_top = popup_y + pad;
            let band_left = popup_x + 12.0 * scale;
            let band_right = popup_x + popup_w - 12.0 * scale;
            let band_w = (band_right - band_left).max(40.0);
            // Placeholder paints when the input is empty so the band
            // doesn't read as a blank strip. Drawn behind the input
            // so the caret still lands on top.
            let input_is_empty = self
                .attach_input
                .as_ref()
                .map(|tb| tb.text().is_empty())
                .unwrap_or(true);
            if input_is_empty {
                let mut placeholder_paint = Paint::default();
                placeholder_paint.set_anti_alias(true);
                placeholder_paint.set_color(crate::color::text_muted_grey());
                let baseline = band_top + text_offset_in_row;
                canvas.draw_str(
                    "Find or create thread…",
                    Point::new(band_left, baseline),
                    &body_font,
                    &placeholder_paint,
                );
            }
            if let Some(input) = self.attach_input.as_mut() {
                // TextBox treats `y` as the TOP of the text band
                // (glyphs span [y, y + ascent..descent]). Center the
                // band inside the row by insetting half the slack.
                let tb_y = band_top + (row_h - row_text_height) * 0.5;
                input.tick(canvas, band_left, tb_y, band_w, true, true);
            }
            // Hairline under the band.
            let mut div = Paint::default();
            div.set_anti_alias(false);
            div.set_color(crate::color::hairline_divider());
            let line_y = band_top + row_h - 0.5;
            canvas.draw_line(
                Point::new(popup_x + pad, line_y),
                Point::new(popup_x + popup_w - pad, line_y),
                &div,
            );
            ctx.hit_tests.mention_popup.input_band = Some(Rect::new(
                popup_x + pad,
                band_top,
                popup_x + popup_w - pad,
                band_top + row_h,
            ));
        }
        let body_top = popup_y + pad + query_band_h;
        // Record the full popup rect so `mouse_down` can distinguish
        // clicks inside the popup (which should be consumed) from
        // clicks outside (which dismiss). Important for the picker:
        // clicks in the input band must reach the TextBox, not
        // fall through to the cell.
        ctx.hit_tests.mention_popup.menu = Some(popup_rect);

        if items.is_empty() {
            // Hint row.
            let mut hint_paint = Paint::default();
            hint_paint.set_anti_alias(true);
            hint_paint.set_color(crate::color::text_muted_grey());
            let hint_y = body_top;
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
                let mouse = ctx.mouse_pos;
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
                let label = match kind {
                    MentionKind::Thread => format!("Create thread \"{}\"", query),
                    _ => format!("Add {}{}", kind.trigger(), query),
                };
                canvas.draw_str(
                    label,
                    Point::new(popup_x + 12.0 * scale, baseline),
                    &body_font,
                    &text_paint,
                );
                ctx.hit_tests.mention_popup.add_row = Some(add_rect);
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

        // `sel_idx = None` means the user hasn't typed or arrow-
        // keyed yet — render no highlight band so an accidental
        // Enter doesn't commit anything. Mouse hover still works
        // independently.
        let sel_idx = selected.map(|s| s.min(visible - 1));
        let mut row_y = body_top;
        for (i, (item, matches, mention_count)) in items.iter().take(visible).enumerate() {
            let row_rect = Rect::new(
                popup_x + 4.0 * scale,
                row_y,
                popup_x + popup_w - 4.0 * scale,
                row_y + row_h,
            );
            let mouse = ctx.mouse_pos;
            let mouse_hover = mouse.0 >= row_rect.left
                && mouse.0 <= row_rect.right
                && mouse.1 >= row_rect.top
                && mouse.1 <= row_rect.bottom;
            if sel_idx == Some(i) || mouse_hover {
                canvas.draw_round_rect(row_rect, 4.0 * scale, 4.0 * scale, &hl_paint);
            }
            let baseline = row_y + text_offset_in_row;
            let text_x = popup_x + 12.0 * scale;
            // Thread suggestions render the bare title — the `%`
            // prefix is a keyboard convention, not a piece of the
            // name. Person / Tag suggestions keep their prefix so
            // the popup reads as a real `@name` / `#tag` token.
            let trigger_w = if matches!(kind, MentionKind::Thread) {
                0.0
            } else {
                let trigger = kind.trigger();
                let w = body_font.measure_str(trigger, Some(&dim_paint)).0;
                canvas.draw_str(trigger, Point::new(text_x, baseline), &body_font, &dim_paint);
                w
            };
            draw_runs_with_matches(
                canvas,
                item,
                matches,
                Point::new(text_x + trigger_w, baseline),
                &body_font,
                &match_paint,
                &dim_paint,
            );
            // Right-justified mention count when > 0 — visual cue
            // for "who you interact with often". Suppressed at 0
            // (tags + cold-start people) to avoid noise.
            if *mention_count > 0 {
                let count_font =
                    Font::from_typeface(ctx.typeface, MENTION_COUNT_FONT_SIZE * scale);
                let mut count_paint = Paint::default();
                count_paint.set_anti_alias(true);
                // Even softer than `dim_paint` — pulls the count
                // further back so the eye lands on the name first.
                count_paint.set_color(crate::color::text_ghost());
                let count_text = format!("{}", mention_count);
                let count_w = count_font.measure_str(&count_text, Some(&count_paint)).0;
                let count_x = popup_x + popup_w - 12.0 * scale - count_w;
                canvas.draw_str(
                    &count_text,
                    Point::new(count_x, baseline),
                    &count_font,
                    &count_paint,
                );
            }
            ctx.hit_tests.mention_popup.rows.push(row_rect);
            row_y += row_h;
        }
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

    /// `(display_name, is_active, mention_count)` for every person
    /// entity, in alphabetical order. Fed to `filter_mentions` so the
    /// popup can downweight inactive matches AND nudge frequently-
    /// mentioned people above rare ones when fuzzy scores tie.
    fn person_mention_candidates(&self) -> Vec<(String, bool, usize)> {
        let mut out: Vec<(String, bool, usize)> = self
            .entities
            .entities
            .iter()
            .filter(|e| e.kind == "person")
            .map(|e| (
                e.display_name.clone(),
                e.is_active,
                self.entities.mention_count(e.id),
            ))
            .collect();
        out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        out
    }

    /// Tag candidates for the `#`-autocomplete popup. Aggregated from
    /// the in-memory cells so a tag committed in this session shows up
    /// in the candidate list before the debounced save flushes. All
    /// tags are treated as "active" — there's no inactive-tag concept.
    /// Mention count is 0 since we don't track per-tag use frequency
    /// (would require a separate index; not yet motivated).
    fn tag_mention_candidates(&self) -> Vec<(String, bool, usize)> {
        self.all_tag_names_in_memory()
            .into_iter()
            .map(|n| (n, true, 0))
            .collect()
    }

    /// Thread candidates for the `%`-autocomplete popup.
    /// `(title, is_open, member_count)` — closed threads stay in the
    /// list but flow through the same `is_active` downweight the
    /// person popup uses, so an active match always beats a closed
    /// one. Member count substitutes for "mention frequency" — a
    /// thread the user has been steadily adding to ranks above a
    /// barely-used one when fuzzy scores tie.
    fn thread_mention_candidates(&self) -> Vec<(String, bool, usize)> {
        let mut out: Vec<(String, bool, usize)> = self
            .threads
            .iter()
            .map(|t| {
                let n = self
                    .thread_memberships
                    .iter()
                    .filter(|m| m.thread_id == t.id)
                    .count();
                (t.title.clone(), t.closed_at.is_none(), n)
            })
            .collect();
        out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        out
    }

    /// `(title, thread_id)` for every thread, sorted alphabetically.
    /// Used by `commit_thread_mention` to resolve the chosen title
    /// back to an id at commit time.
    fn thread_entries(&self) -> Vec<(String, ThreadId)> {
        let mut out: Vec<(String, ThreadId)> = self
            .threads
            .iter()
            .map(|t| (t.title.clone(), t.id))
            .collect();
        out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        out
    }

    /// Pick the candidate set for the popup's current kind.
    fn mention_candidates_for(&self, kind: MentionKind) -> Vec<(String, bool, usize)> {
        match kind {
            MentionKind::Person => self.person_mention_candidates(),
            MentionKind::Tag => self.tag_mention_candidates(),
            MentionKind::Thread => self.thread_mention_candidates(),
        }
    }

    /// Open the mention popup in "thread attach" mode for the given
    /// target. Anchored at `(x, y)` in window coords (typically the
    /// right-click position). The picker owns its own query string
    /// (no backing text), so key events go through
    /// `handle_thread_attach_key` instead of the cell text path.
    pub(super) fn open_thread_attach_picker(
        &mut self,
        target: crate::cell::ReferenceTarget,
        anchor: (f32, f32),
    ) {
        self.dismiss_open_context_menu();
        let input = crate::cell::TextBox::new(self.typeface.clone(), String::new());
        self.mention_popup = Some(MentionPopup {
            source: MentionSource::ThreadAttach { target, anchor },
            kind: MentionKind::Thread,
            anchor_byte: 0,
            query: String::new(),
            selected: None,
            attach_input: Some(input),
        });
    }

    /// True iff the currently-open popup is the thread-attach
    /// picker. Used by the key router to forward keystrokes to the
    /// embedded TextBox instead of an underlying cell.
    pub(super) fn is_thread_attach_picker_open(&self) -> bool {
        matches!(
            self.mention_popup.as_ref().map(|p| p.source),
            Some(MentionSource::ThreadAttach { .. })
        )
    }

    pub(super) fn try_open_mention_popup(&mut self, kind: MentionKind) {
        let trigger = kind.trigger();
        // Highest priority: the Quick-Add modal. Its cell isn't in
        // `document.cells`, so the Cell-source path would miss it
        // entirely. Modal-source wires through `self.quick_add`.
        if let Some(state) = self.quick_add.as_ref() {
            if let Some((text, caret)) = state.cell.focused_text_and_caret() {
                if caret > 0 && text.get(caret - 1..caret) == Some(trigger) {
                    self.mention_popup = Some(MentionPopup {
                        source: MentionSource::QuickAdd,
                        kind,
                        anchor_byte: caret - 1,
                        query: String::new(),
                        selected: None,
                        attach_input: None,
                    });
                }
            }
            return;
        }
        // Next: the focused pane header — typing in the URL-bar
        // pill never goes through cell.handle_key, so the
        // cell-source path would be a no-op here.
        if let Some(idx) = self.panes.iter().position(|p| p.header.focused) {
            let tb = &self.panes[idx].header.textbox;
            let text = tb.text();
            let caret = tb.primary_caret().map(|(_, h)| h).unwrap_or(0);
            if caret == 0 {
                return;
            }
            if text.get(caret - 1..caret) != Some(trigger) {
                return;
            }
            self.mention_popup = Some(MentionPopup {
                source: MentionSource::PaneHeader { pane_idx: idx },
                kind,
                anchor_byte: caret - 1,
                query: String::new(),
                selected: None,
                attach_input: None,
            });
            return;
        }
        let Some(focused_id) = self.pane_mut().focused else {
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
            selected: None,
            attach_input: None,
        });
    }

    pub(super) fn sync_mention_popup(&mut self) {
        // Snapshot the popup state up front so subsequent `self.cell` /
        // `self.pane()` accesses don't fight the `self.mention_popup`
        // borrow.
        let (anchor_byte, source, kind) = {
            let Some(popup) = self.mention_popup.as_ref() else {
                return;
            };
            (popup.anchor_byte, popup.source, popup.kind)
        };
        // ThreadAttach owns its own TextBox; mirror its current
        // text into `p.query` so the filter + commit paths read a
        // single source of truth, then clamp the highlight.
        if matches!(source, MentionSource::ThreadAttach { .. }) {
            let next_query = self
                .mention_popup
                .as_ref()
                .and_then(|p| p.attach_input.as_ref())
                .map(|tb| tb.text().to_string())
                .unwrap_or_default();
            let candidates = self.mention_candidates_for(kind);
            if let Some(p) = self.mention_popup.as_mut() {
                p.query = next_query;
                let count = filter_mentions(&candidates, &p.query)
                    .len()
                    .min(MENTION_POPUP_MAX_VISIBLE);
                reconcile_selected(&mut p.selected, &p.query, count);
            }
            return;
        }
        // Pull the current `(text, caret)` from whichever source is anchored.
        let cur: Option<(String, usize)> = match source {
            MentionSource::Cell { cell_id, bullet_id } => {
                if self.pane().focused != Some(cell_id) {
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
            MentionSource::PaneHeader { pane_idx } => self
                .panes
                .get(pane_idx)
                .filter(|p| p.header.focused)
                .and_then(|p| {
                    let caret = p.header.textbox.primary_caret().map(|(_, h)| h)?;
                    Some((p.header.textbox.text().to_string(), caret))
                }),
            MentionSource::QuickAdd => self.quick_add.as_ref().and_then(|s| {
                s.cell
                    .focused_text_and_caret()
                    .map(|(t, c)| (t.to_string(), c))
            }),
            MentionSource::ThreadAttach { .. } => unreachable!("handled above"),
        };
        let Some((text, caret)) = cur else {
            self.mention_popup = None;
            return;
        };
        // The trigger character must still be at anchor_byte.
        let trigger = kind.trigger();
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
        let candidates = self.mention_candidates_for(kind);
        if let Some(p) = self.mention_popup.as_mut() {
            let count = filter_mentions(&candidates, &query)
                .len()
                .min(MENTION_POPUP_MAX_VISIBLE);
            p.query = query;
            reconcile_selected(&mut p.selected, &p.query, count);
        }
    }

    pub(super) fn render_mention_popup(&mut self, canvas: &Canvas, view_w: f32, view_h: f32) {
        // Phase 1: pull the bits we need to compute anchor + candidates,
        // dropping the popup borrow before reaching for `self.cell` /
        // `self.pane_mut().scroll_y` (which borrow self in conflicting shapes).
        let (source, kind, anchor_byte) = {
            let Some(popup) = self.mention_popup.as_ref() else {
                return;
            };
            (popup.source, popup.kind, popup.anchor_byte)
        };

        let anchor_pos = match source {
            MentionSource::Cell { cell_id, bullet_id: _ } => {
                let Some(cell) = self.cell(cell_id) else {
                    return;
                };
                let Some((x, y)) = cell.anchor_doc_pos(anchor_byte) else {
                    return;
                };
                // Doc-space → window-space: subtract scroll. Use direct
                // field path to disjoin from the later `self.mention_popup`
                // re-borrow.
                let scroll_y = self.panes[self.active_pane].scroller.scroll_y;
                (x, y - scroll_y)
            }
            MentionSource::PaneHeader { pane_idx } => {
                let Some(pane) = self.panes.get(pane_idx) else { return };
                let tb = &pane.header.textbox;
                let Some((x, _)) = tb.doc_position_of_byte(anchor_byte) else {
                    return;
                };
                let Some((_, bot)) = tb.line_y_band_of_byte(anchor_byte) else {
                    return;
                };
                (x, bot)
            }
            MentionSource::QuickAdd => {
                let Some(state) = self.quick_add.as_ref() else { return };
                // The modal's cell renders in window-space (the
                // overlay isn't doc-translated), so its
                // `anchor_doc_pos` is already a window-space point
                // — no scroll subtraction needed.
                let Some((x, y)) = state.cell.anchor_doc_pos(anchor_byte) else {
                    return;
                };
                (x, y)
            }
            MentionSource::ThreadAttach { anchor, .. } => anchor,
        };

        let candidates = self.mention_candidates_for(kind);

        // Phase 2: re-borrow the popup and delegate to its render
        // method. `&mut` because the ThreadAttach picker needs to
        // tick its embedded TextBox during render (caret + selection
        // are part of the per-frame paint).
        let Some(popup) = self.mention_popup.as_mut() else {
            return;
        };
        let mut ctx = MentionRenderCtx {
            font_scale: self.font_scale,
            typeface: &self.typeface,
            mouse_pos: self.mouse_pos,
            hit_tests: &mut self.hit_tests_builder,
        };
        popup.render(canvas, view_w, view_h, anchor_pos, &candidates, &mut ctx);
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
        // First arrow press out of "no selection" lands on row 0
        // regardless of direction — there's no prior position to
        // step relative to.
        let cur = match p.selected {
            Some(i) => i.min(count - 1) as i32,
            None => {
                p.selected = Some(0);
                return;
            }
        };
        let new = ((cur + delta).rem_euclid(count as i32)) as usize;
        p.selected = Some(new);
    }

    /// Commit the highlighted item from the mention popup. For person
    /// (`@`) mentions, replaces `@query` with the person's title and
    /// attaches a `kept://<source-cell-id>` link span. For tag (`#`)
    /// mentions, replaces `#query` with the literal `#tagname` as plain
    /// text — the title's tag-extraction pass picks it up. For thread
    /// (`%`) mentions, replaces `%query` with the thread's title +
    /// `kept://<thread-id>` link AND attaches the cell to the thread.
    /// All record one undo entry.
    pub(super) fn commit_mention(&mut self) -> bool {
        // Bail without committing when no row is selected — empty
        // query + Enter should be a no-op (matches the user's
        // "don't pre-match the first row" expectation). Take()
        // dismisses the popup either way; the empty-query path
        // returns the popup to None without inserting anything.
        let Some(popup) = self.mention_popup.take() else {
            return false;
        };
        let Some(selected_idx) = popup.selected else {
            // Put the popup back — empty Enter shouldn't dismiss
            // the picker; the user can keep typing.
            self.mention_popup = Some(popup);
            return true;
        };
        let candidates = self.mention_candidates_for(popup.kind);
        let filtered = filter_mentions(&candidates, &popup.query);
        let Some(selected) = filtered.get(selected_idx) else {
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
            MentionKind::Thread => {
                let entries = self.thread_entries();
                let Some((_, thread_id)) =
                    entries.iter().find(|(n, _)| n == &chosen_name)
                else {
                    return true;
                };
                let thread_id = *thread_id;
                self.commit_thread_mention(
                    popup.source,
                    start,
                    end,
                    chosen_name,
                    thread_id,
                );
            }
        }
        self.pane_mut().coalesce_break = true;
        true
    }

    /// Commit a specific row by index (mouse click). Sets the popup's
    /// selected index and runs the same path as keyboard Enter.
    pub(super) fn commit_mention_row(&mut self, idx: usize) -> bool {
        if let Some(p) = self.mention_popup.as_mut() {
            p.selected = Some(idx);
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
            MentionKind::Thread => {
                // Brand-new thread → create it, then run the standard
                // commit path so the same insert + attach happens.
                let Some(new_id) = self.create_thread(query.clone()) else {
                    return false;
                };
                self.commit_thread_mention(popup.source, start, end, query, new_id);
            }
        }
        self.pane_mut().coalesce_break = true;
        true
    }

    /// Insert a thread mention: a clickable `kept://<thread_id>` link
    /// in the source text plus an attachment of the containing cell
    /// (or bullet, for ThreadAttach picker source) to the thread.
    /// Pane-header source is text-only — search doesn't speak thread
    /// syntax yet — so it falls back to a slug-style insert.
    fn commit_thread_mention(
        &mut self,
        source: MentionSource,
        start: usize,
        end: usize,
        chosen_name: String,
        thread_id: ThreadId,
    ) {
        let url = format!("kept://{}", thread_id);
        match source {
            MentionSource::Cell { cell_id, bullet_id: _ } => {
                let pre = match self.cell(cell_id) {
                    Some(c) => c.snapshot(),
                    None => return,
                };
                if let Some(c) = self.cell_mut(cell_id) {
                    c.replace_focused_with_link(start..end, chosen_name, url);
                }
                if let Some(c) = self.cell(cell_id) {
                    let post = c.snapshot();
                    if !pre.doc_eq(&post) {
                        let saved_focused = self.pane_mut().focused;
                        self.pane_mut().focused = Some(cell_id);
                        self.record_edit(pre, post);
                        self.pane_mut().focused = saved_focused.or(Some(cell_id));
                    }
                }
                // The mention also creates a real attachment so the
                // thread page actually contains the cell. Idempotent
                // — a second `%` mention of the same thread is a
                // no-op on the memberships table.
                self.attach_to_thread(
                    thread_id,
                    crate::cell::ReferenceTarget::WholeCell(cell_id),
                );
            }
            MentionSource::PaneHeader { .. } => {
                let slug = chosen_name
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("_");
                self.replace_search_or_cell_text(
                    source,
                    start,
                    end,
                    format!("%{slug}"),
                );
            }
            MentionSource::QuickAdd => {
                let pre = match self.quick_add.as_ref() {
                    Some(s) => s.cell.snapshot(),
                    None => return,
                };
                if let Some(s) = self.quick_add.as_mut() {
                    s.cell.replace_focused_with_link(start..end, chosen_name, url);
                }
                if let Some(s) = self.quick_add.as_mut() {
                    let post = s.cell.snapshot();
                    if !pre.doc_eq(&post) {
                        s.record_edit(pre);
                    }
                }
                // Quick-Add's cell isn't in `document.cells` yet, so
                // we don't attach here — the cell gains an
                // attachment only after it flushes into the timeline
                // (a future improvement; for now Quick-Add `%`
                // mentions are link-only).
            }
            MentionSource::ThreadAttach { target, .. } => {
                // Right-click → Attach picker. No text to replace;
                // commit means "attach this target to the chosen
                // thread."
                self.attach_to_thread(thread_id, target);
            }
        }
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
                        let saved_focused = self.pane_mut().focused;
                        self.pane_mut().focused = Some(cell_id);
                        self.record_edit(pre, post);
                        self.pane_mut().focused = saved_focused.or(Some(cell_id));
                    }
                }
            }
            MentionSource::PaneHeader { .. } => {
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
            MentionSource::QuickAdd => {
                // Same link-insertion shape as the Cell arm, but
                // mutating the modal's in-flight cell and pushing
                // its pre-snapshot onto the modal's own undo
                // stack so Ctrl+Z inside the modal walks back over
                // the mention insert.
                let pre = match self.quick_add.as_ref() {
                    Some(s) => s.cell.snapshot(),
                    None => return,
                };
                let url = format!("kept://{}", source_id);
                if let Some(s) = self.quick_add.as_mut() {
                    s.cell.replace_focused_with_link(start..end, chosen_name, url);
                }
                if let Some(s) = self.quick_add.as_mut() {
                    let post = s.cell.snapshot();
                    if !pre.doc_eq(&post) {
                        s.record_edit(pre);
                    }
                }
            }
            MentionSource::ThreadAttach { .. } => {
                // Person mentions never open with ThreadAttach source —
                // that source is exclusive to `MentionKind::Thread`.
                debug_assert!(false, "person mention from ThreadAttach source");
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
                        let saved_focused = self.pane_mut().focused;
                        self.pane_mut().focused = Some(cell_id);
                        self.record_edit(pre, post);
                        self.pane_mut().focused = saved_focused.or(Some(cell_id));
                    }
                }
            }
            MentionSource::PaneHeader { .. } => {
                self.replace_search_or_cell_text(source, start, end, replacement);
            }
            MentionSource::QuickAdd => {
                let pre = match self.quick_add.as_ref() {
                    Some(s) => s.cell.snapshot(),
                    None => return,
                };
                if let Some(s) = self.quick_add.as_mut() {
                    s.cell.replace_focused_with_tag(start..end, replacement);
                }
                if let Some(s) = self.quick_add.as_mut() {
                    let post = s.cell.snapshot();
                    if !pre.doc_eq(&post) {
                        s.record_edit(pre);
                    }
                }
            }
            MentionSource::ThreadAttach { .. } => {
                debug_assert!(false, "tag mention from ThreadAttach source");
            }
        }
    }

    /// Plain-text replacement of `[start..end]` with `replacement` in
    /// whichever source the popup was anchored on. Records an undo edit
    /// for cell sources; mutates the URL-bar pill's textbox otherwise.
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
                        let saved_focused = self.pane_mut().focused;
                        self.pane_mut().focused = Some(cell_id);
                        self.record_edit(pre, post);
                        self.pane_mut().focused = saved_focused.or(Some(cell_id));
                    }
                }
            }
            MentionSource::PaneHeader { pane_idx } => {
                if let Some(pane) = self.panes.get_mut(pane_idx) {
                    let tb = &mut pane.header.textbox;
                    let txt = tb.text();
                    if start <= txt.len() && end <= txt.len() {
                        let prefix = &txt[..start];
                        let suffix = &txt[end..];
                        let new_text = format!("{prefix}{replacement}{suffix}");
                        tb.replace_text(new_text);
                        tb.set_caret_at(start + replacement.len());
                    }
                    pane.header.selected = None;
                }
            }
            MentionSource::QuickAdd => {
                let pre = match self.quick_add.as_ref() {
                    Some(s) => s.cell.snapshot(),
                    None => return,
                };
                if let Some(s) = self.quick_add.as_mut() {
                    s.cell.replace_focused_with_text(start..end, replacement);
                }
                if let Some(s) = self.quick_add.as_mut() {
                    let post = s.cell.snapshot();
                    if !pre.doc_eq(&post) {
                        s.record_edit(pre);
                    }
                }
            }
            MentionSource::ThreadAttach { .. } => {
                debug_assert!(false, "text replace from ThreadAttach source");
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
            ("PatrickFoy".to_string(), true, 0),
            ("PeterCarr".to_string(), true, 0),
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
            ("PatrickFoy".to_string(), false, 0), // inactive
            ("PeterCarr".to_string(), true, 0),   // active
        ];
        let ranked = filter_mentions(&cands, "p");
        assert_eq!(ranked[0].0, "PeterCarr");
        assert_eq!(ranked[1].0, "PatrickFoy");
    }

    #[test]
    fn fuzzy_mention_count_breaks_ties() {
        // Same fuzzy score (same query, same shape) — the
        // higher-mention-count candidate should rank first. This
        // captures the user complaint: the "short" initials match
        // shouldn't always pick the rarely-interacted person.
        let cands = vec![
            ("PatrickFoy".to_string(), true, 0),   // rarely mentioned
            ("PeterCarr".to_string(), true, 50),   // often mentioned
        ];
        let ranked = filter_mentions(&cands, "p");
        assert_eq!(ranked[0].0, "PeterCarr");
        assert_eq!(ranked[1].0, "PatrickFoy");
    }
}
