//! Quick-Add modal — "yeet a note without interrupting flow."
//!
//! Ctrl+H pops a centered modal with an outline editor; the user
//! types, presses Esc, and the note lands in today's timeline with
//! its timestamp = now. Ctrl+Shift+H opens the same modal with an
//! attached title slot pre-focused. Esc on an empty modal closes
//! without inserting a cell.
//!
//! The whole modal is a single `Cell` under the hood — same type
//! as anywhere else in the app — so title + outline behavior, key
//! handling, paste-with-links, etc. work without bespoke logic.
//! On commit we hand the `Cell` to the document with `now_epoch_ms`
//! as its timestamp and dirty it for persistence.

use std::time::Instant;

use skia_safe::{BlurStyle, Canvas, Font, MaskFilter, Paint, PaintStyle, Point, Rect, Typeface};
use winit::event::{KeyEvent, Modifiers};

use crate::cell::{Cell, CellSnapshot};

use super::{COALESCE_INTERVAL, KeptApp, UndoOp};

/// Modal width, logical px (scaled by `font_scale`).
const QUICK_ADD_WIDTH: f32 = 520.0;
/// Top inset of the card from the window's top edge.
const QUICK_ADD_TOP: f32 = 120.0;
/// Inner padding around the cell content inside the card.
const QUICK_ADD_PAD: f32 = 16.0;
/// Card corner radius.
const QUICK_ADD_RADIUS: f32 = 10.0;
/// Min card height so a single empty bullet doesn't render as a
/// sliver. Cap is implicit — the cell grows to fit and the card
/// follows.
const QUICK_ADD_MIN_H: f32 = 60.0;
/// Hint label below the card.
const QUICK_ADD_HINT_GAP: f32 = 8.0;
const QUICK_ADD_HINT_FONT_SIZE: f32 = 12.0;

pub(super) struct QuickAddState {
    /// The note being authored. Inserted into `document.cells` on
    /// commit; dropped on cancel.
    pub(super) cell: Cell,
    /// Card rect from the most recent render (window coords).
    /// Populated by `render`; consumed by mouse-down dispatch so
    /// a click outside the card dismisses+commits.
    pub(super) last_card_rect: Rect,
    /// Modal-local undo stack: each entry is the `CellSnapshot`
    /// captured BEFORE the *start* of a typing burst. Subsequent
    /// edits within `COALESCE_INTERVAL` coalesce into that one
    /// entry (the top-of-stack remains the right "pre" snapshot
    /// for the whole burst). Ctrl+Z pops one and restores the
    /// cell. Distinct from the app's `UndoOp::CellEdit` mechanism
    /// because the modal's cell isn't in `document.cells` until
    /// commit.
    pub(super) undo_stack: Vec<CellSnapshot>,
    pub(super) redo_stack: Vec<CellSnapshot>,
    /// Wall-clock time of the most recent edit (the timer for the
    /// coalesce window). Mirrors `TextBox::last_edit_at` /
    /// `KeptApp::last_edit_time`.
    pub(super) last_edit_at: Option<Instant>,
    /// "Decisive" gestures (Ctrl+Z, Ctrl+Y, undo/redo internally)
    /// flip this true; the next push refuses to coalesce. Matches
    /// the app's per-pane `coalesce_break`.
    pub(super) coalesce_break: bool,
}

impl QuickAddState {
    /// Build a fresh modal. `with_title=true` pre-attaches an empty
    /// title slot and focuses it (Ctrl+Shift+H entry).
    pub(super) fn new(typeface: Typeface, font_scale: f32, with_title: bool) -> Self {
        let mut cell = Cell::new_outline(typeface);
        cell.set_font_scale(font_scale);
        if with_title {
            cell.toggle_title_focus();
        }
        Self {
            cell,
            last_card_rect: Rect::new(0.0, 0.0, 0.0, 0.0),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit_at: None,
            coalesce_break: false,
        }
    }

    /// Record a pre-edit snapshot, coalescing with the previous
    /// one when the new edit lands within `COALESCE_INTERVAL` of
    /// the last and no decisive gesture intervened. Matches
    /// `TextBox::record_undo`'s shape exactly — typing a burst
    /// reverts as one undo, pause-then-type starts a new entry.
    pub(super) fn record_edit(&mut self, pre: CellSnapshot) {
        let now = Instant::now();
        let can_coalesce = !self.coalesce_break
            && !self.undo_stack.is_empty()
            && self
                .last_edit_at
                .map(|t| now.duration_since(t) < COALESCE_INTERVAL)
                .unwrap_or(false);
        if !can_coalesce {
            self.undo_stack.push(pre);
        }
        self.redo_stack.clear();
        self.last_edit_at = Some(now);
        self.coalesce_break = false;
    }

    /// True when the cell has nothing the user would expect to
    /// keep (no title text, body is_empty). Used by the commit
    /// path to suppress empty-cell inserts.
    pub(super) fn is_blank(&self) -> bool {
        self.cell.is_empty()
    }
}

impl KeptApp {
    /// Ctrl+H / Ctrl+Shift+H — toggle the Quick-Add modal.
    /// Pressing the same shortcut while open closes (and commits)
    /// the modal; same key with the title variant when open without
    /// a title attaches one without losing the current body content.
    pub(super) fn toggle_quick_add(&mut self, with_title: bool) {
        if let Some(state) = self.quick_add.as_mut() {
            // Already open: attach title if requested and not yet
            // present; otherwise toggle closed (committing what's
            // there).
            if with_title && state.cell.title().is_none() {
                state.cell.toggle_title_focus();
                return;
            }
            self.commit_quick_add();
            return;
        }
        // Drop competing overlays — Quick-Add steals input.
        self.mention_popup = None;
        self.cell_context_menu = None;
        self.bar_context_menu = None;
        self.tag_context_menu = None;
        self.people_context_menu = None;
        // Blur every pane's URL-bar pill so keystrokes don't race
        // for the same textbox.
        for p in &mut self.panes {
            p.header.blur();
        }
        let state = QuickAddState::new(self.typeface.clone(), self.font_scale, with_title);
        self.quick_add = Some(state);
    }

    /// Commit-and-close: if the modal's cell has any content,
    /// stamp it with the current time and insert into the document.
    /// Empty modals close silently. The focus / view of the
    /// underlying panes is left alone — Quick-Add is meant to not
    /// interrupt flow. Pushes `UndoOp::InsertCell` onto the app's
    /// main undo stack so Ctrl+Z after commit un-creates the
    /// note (matches `insert_cell_after_focused`'s contract).
    pub(super) fn commit_quick_add(&mut self) {
        let Some(mut state) = self.quick_add.take() else {
            return;
        };
        if state.is_blank() {
            return;
        }
        let now = crate::cell::now_epoch_ms();
        state.cell.timestamp = now;
        state.cell.edited_at = now;
        state.cell.context_hint_id = self.writable_context_id();
        state.cell.inbox = true;
        let new_id = state.cell.id;
        let snapshot = state.cell.snapshot();
        let pre_focused = self.pane().focused;
        self.insert_cell_sorted(state.cell);
        self.touch_cell(new_id);
        self.undo_stack.push(UndoOp::InsertCell {
            cell_id: new_id,
            snapshot,
            pre_focused,
        });
        self.redo_stack.clear();
        self.show_toast("Saved to today");
    }

    /// Drop the modal without inserting anything. Wired to a
    /// future "discard" shortcut; not used by Esc, which commits.
    #[allow(dead_code)]
    pub(super) fn discard_quick_add(&mut self) {
        self.quick_add = None;
    }

    /// Route a key event to the modal's `Cell` when open. Returns
    /// true iff the modal swallowed the event.
    pub(super) fn handle_quick_add_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        use super::MentionKind;
        use winit::event::ElementState;
        use winit::keyboard::{Key, NamedKey};
        if self.quick_add.is_none() {
            return false;
        }
        let mods = modifiers.state();
        let pressed = event.state == ElementState::Pressed;
        // While the @-mention / #-tag popup is open OVER the modal,
        // it owns Enter/Tab/Esc/Up/Down — those select / commit /
        // dismiss a candidate, NOT save the note.
        if pressed && self.mention_popup.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.mention_popup = None;
                    return true;
                }
                Key::Named(NamedKey::ArrowUp) => {
                    self.mention_popup_move(-1);
                    return true;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    self.mention_popup_move(1);
                    return true;
                }
                Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) => {
                    self.commit_mention();
                    return true;
                }
                _ => {}
            }
        }
        // Esc precedence inside the modal:
        //   1. Mention popup wants Esc → already handled above.
        //   2. Active selection on the modal cell → clear it
        //      (matches the in-timeline Esc-clears-selection rule).
        //   3. Otherwise → commit + close (the "yeet" exit).
        if pressed && matches!(event.logical_key, Key::Named(NamedKey::Escape)) {
            let has_sel = self
                .quick_add
                .as_ref()
                .map(|s| s.cell.has_any_selection())
                .unwrap_or(false);
            if has_sel {
                if let Some(state) = self.quick_add.as_mut() {
                    state.cell.clear_all_selections();
                    // Decisive gesture — next edit starts a fresh
                    // undo entry.
                    state.coalesce_break = true;
                }
                return true;
            }
            self.commit_quick_add();
            return true;
        }
        // Modal-local Cmd/Ctrl shortcuts: undo / redo on the
        // modal's own snapshot stack; Cmd/Ctrl+T toggles a title
        // slot on the modal cell (same shape as the in-timeline
        // editor's binding, but mutating `state.cell` instead of
        // the focused document cell).
        if pressed && super::primary_mod(mods) {
            if let Key::Character(s) = &event.logical_key {
                let s = s.as_str();
                if s.eq_ignore_ascii_case("a") {
                    // Select-all on the focused element of the
                    // modal's cell (focused bullet's textbox, or
                    // title when title is focused). Pure
                    // selection state — no doc change, no undo
                    // entry.
                    if let Some(state) = self.quick_add.as_mut() {
                        state.cell.select_all_focused();
                    }
                    return true;
                }
                if s.eq_ignore_ascii_case("c") {
                    self.quick_add_copy();
                    return true;
                }
                if s.eq_ignore_ascii_case("x") {
                    self.quick_add_cut();
                    return true;
                }
                if s.eq_ignore_ascii_case("v") {
                    // Shift+V = paste-as-plain-text.
                    self.quick_add_paste(mods.shift_key());
                    return true;
                }
                if s.eq_ignore_ascii_case("z") {
                    if mods.shift_key() {
                        self.quick_add_redo();
                    } else {
                        self.quick_add_undo();
                    }
                    return true;
                }
                // (Cmd/Ctrl+Y is the global Quick-Add toggle now,
                // intercepted in `handle_key` BEFORE this routes
                // here. Redo inside the modal goes through
                // Cmd/Ctrl+Shift+Z.)
                if s.eq_ignore_ascii_case("t") {
                    let pre = self.quick_add.as_ref().map(|s| s.cell.snapshot());
                    let changed = self
                        .quick_add
                        .as_mut()
                        .map(|s| s.cell.toggle_title_focus())
                        .unwrap_or(false);
                    if changed {
                        if let (Some(state), Some(pre)) = (self.quick_add.as_mut(), pre) {
                            // doc_eq catches the "title focus
                            // shifted but no text changed" path,
                            // so we don't pile up no-op undos.
                            let post = state.cell.snapshot();
                            if !pre.doc_eq(&post) {
                                state.record_edit(pre);
                            }
                        }
                    }
                    return true;
                }
            }
        }
        // Forward to the cell. Snapshot before; on real document
        // change, push the pre-snapshot onto the modal's undo
        // stack (coalescing inside `record_edit`).
        let pre = {
            let state = self.quick_add.as_ref().expect("checked above");
            state.cell.snapshot()
        };
        let popup_was_open = self.mention_popup.is_some();
        let handled = {
            let state = self.quick_add.as_mut().expect("checked above");
            state.cell.handle_key(event, modifiers)
        };
        if let Some(state) = self.quick_add.as_mut() {
            let post = state.cell.snapshot();
            if !pre.doc_eq(&post) {
                state.record_edit(pre);
            }
        }
        // Mention popup hooks: if the user just typed `@` / `#`,
        // open the popup anchored to that trigger. If the popup
        // was already open, re-sync against the new text / caret
        // so a shrinking query backs it out or filters it.
        if !popup_was_open {
            match event.text.as_deref() {
                Some("@") => self.try_open_mention_popup(MentionKind::Person),
                Some("#") => self.try_open_mention_popup(MentionKind::Tag),
                _ => {}
            }
        }
        self.sync_mention_popup();
        handled
    }

    fn quick_add_undo(&mut self) {
        let Some(state) = self.quick_add.as_mut() else {
            return;
        };
        let Some(prev) = state.undo_stack.pop() else {
            return;
        };
        let cur = state.cell.snapshot();
        state.cell.restore(prev);
        state.redo_stack.push(cur);
        // Undo is a decisive gesture — the next edit shouldn't
        // coalesce with whatever the undo left on top.
        state.coalesce_break = true;
    }

    fn quick_add_redo(&mut self) {
        let Some(state) = self.quick_add.as_mut() else {
            return;
        };
        let Some(next) = state.redo_stack.pop() else {
            return;
        };
        let cur = state.cell.snapshot();
        state.cell.restore(next);
        state.undo_stack.push(cur);
        state.coalesce_break = true;
    }

    /// Build a clipboard payload from the modal's cell and write
    /// it to the OS clipboard. Same shape as the main copy path,
    /// but reads from `self.quick_add.cell` via the free
    /// `build_copy_payload_for_cell` helper instead of looking up
    /// a document cell. Returns true when something was written.
    fn quick_add_copy(&mut self) -> bool {
        let payload = match self.quick_add.as_ref() {
            // Quick-Add cells aren't in `document.cells` yet, so
            // they have no thread memberships to carry — None.
            Some(s) => super::build_copy_payload_for_cell(&s.cell, true, None),
            None => None,
        };
        let Some(payload) = payload else { return false };
        self.write_payload_to_clipboard(&payload);
        true
    }

    /// Copy + delete the modal cell's current selection. Records
    /// the pre-snapshot on the modal's undo stack when the
    /// deletion actually changes the doc.
    fn quick_add_cut(&mut self) -> bool {
        if !self.quick_add_copy() {
            return false;
        }
        let pre = self.quick_add.as_ref().map(|s| s.cell.snapshot());
        let cut_text = self.quick_add.as_mut().map(|s| s.cell.cut_text());
        // `cut_text` returns an empty string when nothing was
        // selected — either way we already wrote the payload from
        // the copy path; only record on real change.
        if let Some(_text) = cut_text {
            if let (Some(pre), Some(state)) = (pre, self.quick_add.as_mut()) {
                let post = state.cell.snapshot();
                if !pre.doc_eq(&post) {
                    state.record_edit(pre);
                }
            }
        }
        true
    }

    /// Read the OS clipboard and apply to the modal cell.
    /// `alternate=true` mirrors Ctrl+Shift+V on the main path:
    /// strip formatting (text-only, no links, no outline
    /// structure). Reference payloads under default paste insert
    /// as inline `kept://` links — the modal can't materialize a
    /// fresh Reference cell mid-edit; that's the Ctrl+Shift+V
    /// alternate behavior on the main path which doesn't apply
    /// here either.
    fn quick_add_paste(&mut self, alternate: bool) -> bool {
        let html = self.clipboard.as_mut().and_then(|cb| cb.get().html().ok());
        let text = self
            .clipboard
            .as_mut()
            .and_then(|cb| cb.get_text().ok())
            .unwrap_or_default();
        if html.is_none() && text.is_empty() {
            return false;
        }
        let mut payload = crate::clipboard::from_clipboard(html.as_deref(), &text);
        if alternate {
            // Strip everything but text (paste-as-plain-text).
            payload = match payload {
                crate::clipboard::KeptPayload::Text { text, .. } => {
                    crate::clipboard::KeptPayload::Text {
                        text,
                        links: Vec::new(),
                    }
                }
                crate::clipboard::KeptPayload::Outline { bullets } => {
                    let (flat, _) = super::flatten_outline(&bullets);
                    crate::clipboard::KeptPayload::Text {
                        text: flat,
                        links: Vec::new(),
                    }
                }
                crate::clipboard::KeptPayload::Reference { snippet, .. } => {
                    crate::clipboard::KeptPayload::Text {
                        text: if snippet.trim().is_empty() {
                            "↗ reference".to_string()
                        } else {
                            format!("↗ {}", snippet)
                        },
                        links: Vec::new(),
                    }
                }
            };
        }
        let pre = self.quick_add.as_ref().map(|s| s.cell.snapshot());
        if let Some(state) = self.quick_add.as_mut() {
            // PasteResult.bullet_threads ignored: the modal cell
            // isn't in `document.cells` yet, so attaching
            // memberships now would dangle. The cell's promotion
            // path could one day attach pending threads on flush.
            let _ = super::apply_paste_into_cell(&mut state.cell, payload);
        }
        if let (Some(pre), Some(state)) = (pre, self.quick_add.as_mut()) {
            let post = state.cell.snapshot();
            if !pre.doc_eq(&post) {
                state.record_edit(pre);
            }
        }
        true
    }

    /// Forward a click while the modal is open. Clicks inside the
    /// card route into the cell; clicks outside commit + close.
    /// Returns true iff the modal handled the click.
    pub(super) fn handle_quick_add_mouse_down(
        &mut self,
        x: f32,
        y: f32,
        modifiers: &Modifiers,
    ) -> bool {
        let card = match self.quick_add.as_ref() {
            Some(s) => s.last_card_rect,
            None => return false,
        };
        let in_card = x >= card.left && x <= card.right && y >= card.top && y <= card.bottom;
        if !in_card {
            self.commit_quick_add();
            return true;
        }
        if let Some(state) = self.quick_add.as_mut() {
            state.cell.mouse_down(x, y, modifiers, true);
            // Mouse click is a decisive gesture — the next edit
            // starts a new undo entry instead of coalescing with
            // whatever burst was in progress before the click.
            state.coalesce_break = true;
        }
        true
    }

    /// Render the Quick-Add card (window-space overlay). No-op when
    /// closed. Called from `tick` after every other overlay so the
    /// modal sits on top.
    pub(super) fn render_quick_add(&mut self, canvas: &Canvas, width: f32, height: f32) {
        if self.quick_add.is_none() {
            return;
        }
        let scale = self.font_scale;
        let card_w = (QUICK_ADD_WIDTH * scale).min(width - 40.0).max(240.0);
        let card_x = ((width - card_w) * 0.5).max(0.0);
        let card_y = QUICK_ADD_TOP * scale;
        let pad = QUICK_ADD_PAD * scale;
        let inner_x = card_x + pad;
        let inner_w = (card_w - 2.0 * pad).max(80.0);

        // Dim the rest of the app so the modal pops. Drawn first
        // so the card sits on top.
        let mut scrim = Paint::default();
        scrim.set_anti_alias(false);
        scrim.set_color(crate::color::black_alpha(0x40));
        canvas.draw_rect(Rect::new(0.0, 0.0, width, height), &scrim);

        // Two-step: tick the cell into a throwaway picture to
        // measure its height, then draw the card sized to fit,
        // then tick the cell for real onto the canvas inside the
        // card. The first tick has only one side effect we care
        // about (cell.x_origin / y_origin), and the second tick
        // overwrites those with the real on-screen coords —
        // important for hit-testing into the modal's cell.
        let rec_bounds = Rect::new(-1.0e6, -1.0e6, 1.0e6, 1.0e6);
        let mut measurer = skia_safe::PictureRecorder::new();
        let measure_canvas = measurer.begin_recording(rec_bounds, None);
        let cell_h = {
            let state = self.quick_add.as_mut().expect("checked above");
            state
                .cell
                .tick(measure_canvas, 0.0, 0.0, inner_w, false, false)
        };
        let _ = measurer.finish_recording_as_picture(None); // discard
        let card_h = (cell_h + 2.0 * pad).max(QUICK_ADD_MIN_H * scale);
        let card_rect = Rect::new(card_x, card_y, card_x + card_w, card_y + card_h);

        // Drop shadow.
        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        shadow.set_color(crate::color::shadow_menu());
        shadow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 14.0, false));
        canvas.draw_round_rect(
            Rect::new(
                card_rect.left,
                card_rect.top + 6.0,
                card_rect.right,
                card_rect.bottom + 6.0,
            ),
            QUICK_ADD_RADIUS * scale,
            QUICK_ADD_RADIUS * scale,
            &shadow,
        );

        // Card fill + border.
        let mut bg = Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(crate::color::bg_card());
        canvas.draw_round_rect(
            card_rect,
            QUICK_ADD_RADIUS * scale,
            QUICK_ADD_RADIUS * scale,
            &bg,
        );
        let mut border = Paint::default();
        border.set_anti_alias(true);
        border.set_style(PaintStyle::Stroke);
        border.set_stroke_width(1.5);
        border.set_color(crate::color::panel_border_warm());
        canvas.draw_round_rect(
            card_rect,
            QUICK_ADD_RADIUS * scale,
            QUICK_ADD_RADIUS * scale,
            &border,
        );

        // Real tick: draw the cell on the canvas at the card's
        // interior, focused=true and show_caret=true so the user's
        // typing surface is fully active. This also updates the
        // cell's geometry caches to window-space coords so any
        // click that lands inside the card resolves correctly.
        if let Some(state) = self.quick_add.as_mut() {
            state
                .cell
                .tick(canvas, inner_x, card_y + pad, inner_w, true, true);
            state.last_card_rect = card_rect;
        }

        // Hint label below the card.
        let hint_font = Font::from_typeface(&self.typeface, QUICK_ADD_HINT_FONT_SIZE * scale);
        let (_, hm) = hint_font.metrics();
        let hint = "Quick add  ·  Esc to save";
        let hint_w = hint_font.measure_str(hint, None).0;
        let hint_x = card_rect.left + (card_w - hint_w) * 0.5;
        let hint_y = card_rect.bottom + QUICK_ADD_HINT_GAP * scale + (-hm.ascent);
        let mut hint_paint = Paint::default();
        hint_paint.set_anti_alias(true);
        hint_paint.set_color(crate::color::text_muted_warm_soft());
        canvas.draw_str(hint, Point::new(hint_x, hint_y), &hint_font, &hint_paint);
    }
}
