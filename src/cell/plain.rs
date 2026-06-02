//! `PlainCell` — wraps a `TextBox` as a cell body.
//!
//! TextBox is the leaf editor primitive used in non-cell contexts too
//! (search input, people-rename, mention popup query, cell titles). Its
//! API shouldn't carry cell-body operations like `full_text` or
//! `replace_at_focused_with_link`. `PlainCell` exists so those methods
//! live on the cell-shaped wrapper and TextBox stays pure infra.
//!
//! Persistence shape is unchanged: a Plain cell still serializes as
//! `{kind: "plain", text, links, tags}` — the wrapper has no extra
//! fields. `CellSnapshotKind::Plain(TextBoxSnapshot)` stays for the
//! same reason.

use std::ops::Range;

use skia_safe::{Canvas, Typeface};
use winit::event::{KeyEvent, Modifiers};

use super::TextBox;
use super::body::CellBody;

pub struct PlainCell {
    body: TextBox,
}

impl PlainCell {
    pub fn new(typeface: Typeface, text: String) -> Self {
        Self {
            body: TextBox::new(typeface, text),
        }
    }

    pub fn from_textbox(body: TextBox) -> Self {
        Self { body }
    }

    pub fn body(&self) -> &TextBox {
        &self.body
    }

    pub fn body_mut(&mut self) -> &mut TextBox {
        &mut self.body
    }

    /// Deep-copy for the reference embed cache. Forwards the underlying
    /// TextBox's `clone_for_cache` so links / tags / scale carry through.
    pub fn clone_for_cache(&self, typeface: Typeface, scale: f32) -> Self {
        Self {
            body: self.body.clone_for_cache(typeface, scale),
        }
    }
}

impl CellBody for PlainCell {
    fn tick(
        &mut self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32 {
        self.body.tick(canvas, x, y, width, focused, show_caret)
    }

    fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        self.body.handle_key(event, modifiers)
    }

    fn mouse_down(&mut self, abs_x: f32, abs_y: f32, modifiers: &Modifiers, editing: bool) -> bool {
        self.body.mouse_down(abs_x, abs_y, modifiers, editing)
    }

    fn for_each_textbox(&self, f: &mut dyn FnMut(&TextBox)) {
        f(&self.body);
    }

    fn for_each_textbox_mut(&mut self, f: &mut dyn FnMut(&mut TextBox)) {
        f(&mut self.body);
    }

    fn typeface(&self) -> &Typeface {
        self.body.typeface()
    }

    fn font_scale(&self) -> f32 {
        self.body.font_scale()
    }

    fn set_font_scale(&mut self, scale: f32) {
        self.body.set_font_scale(scale);
    }

    fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        self.body.caret_doc_y_band()
    }

    fn at_top_edge(&self) -> bool {
        self.body.at_top_visual_line()
    }

    fn at_bottom_edge(&self) -> bool {
        self.body.at_bottom_visual_line()
    }

    fn place_caret_at_start(&mut self) {
        self.body.set_caret_at(0);
    }

    fn place_caret_at_end(&mut self) {
        let end = self.body.text().len();
        self.body.set_caret_at(end);
    }

    fn select_all_focused(&mut self) {
        self.body.select_all();
    }

    fn focused_text_and_caret(&self) -> Option<(&str, usize)> {
        self.body
            .primary_caret()
            .map(|(_, h)| (self.body.text(), h))
    }

    fn focused_textbox(&self) -> Option<&TextBox> {
        Some(&self.body)
    }

    fn anchor_doc_pos_at_focused(&self, byte: usize) -> Option<(f32, f32)> {
        let (x, _) = self.body.doc_position_of_byte(byte)?;
        let (_, bot) = self.body.line_y_band_of_byte(byte)?;
        Some((x, bot))
    }

    fn copy_text(&self) -> String {
        self.body.copy_primary_selection()
    }

    fn cut_text(&mut self) -> String {
        self.body.cut_primary_selection()
    }

    fn paste_text(&mut self, s: &str) {
        self.body.paste(s);
    }

    fn paste_with_links(&mut self, text: &str, links: &[super::common::LinkSpan]) {
        self.body.paste_with_links(text, links);
    }

    fn replace_at_focused_with_link(&mut self, range: Range<usize>, text: String, url: String) {
        self.body.replace_with_link(range, text, url);
    }

    fn replace_at_focused_with_text(&mut self, range: Range<usize>, text: String) {
        self.body.replace_with_text(range, text);
    }

    fn replace_at_focused_with_tag(&mut self, range: Range<usize>, text: String) {
        self.body.replace_with_tag(range, text);
    }

    fn add_link_to_first(&mut self, range: Range<usize>, url: String) {
        self.body.add_link(range, url);
    }

    fn full_text(&self) -> String {
        self.body.text().to_string()
    }
}
