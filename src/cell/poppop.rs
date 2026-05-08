//! `PopPopCell` — a calculator-style "REPL" cell. Two columns: input on
//! the left (an editable `TextBox`), formatted results on the right
//! (read-only). Each `\n`-separated input source line gets one output
//! line. Errors render in red beneath their failing input row.

use skia_safe::{Canvas, Font, Paint, Point, Typeface};
use winit::event::{KeyEvent, Modifiers};

use super::TextBox;
use super::common::TextBoxSnapshot;
use super::grid::{GRID_DIVIDER_PAD, draw_alternating_row_stripes, draw_vertical_divider};

/// Width split between input (left) and output (right) in a PopPop cell.
const POPPOP_INPUT_RATIO: f32 = 0.7;
/// Font size used to render PopPop error messages beneath the input row
/// that produced them. Chosen smaller than the body so a long error fits
/// in the gap and reads as subordinate to the expression above it.
const POPPOP_ERROR_FONT_SIZE: f32 = 13.0;
/// Horizontal indent (logical px) of the error text from the cell's left
/// edge. Mild indent makes the error visually attach to its row.
const POPPOP_ERROR_INDENT: f32 = 14.0;
/// Padding (logical px) added below the error baseline before the next
/// row begins. Keeps the error from kissing the divider above the row
/// underneath.
const POPPOP_ERROR_BOTTOM_PAD: f32 = 4.0;

/// Calculator-style "REPL" cell. Single TextBox for input on the left;
/// each `\n`-separated source line gets a sentinel `42` rendered on the
/// right in dark blue. If the first line starts with `# ` it's treated
/// as a title (heading + tags + mentions, same as Outline / Plain) and
/// gets no output — calc lines start from the second source line.
pub struct PopPopCell {
    #[allow(dead_code)]
    typeface: Typeface,
    pub(super) textbox: TextBox,
    /// Read-only TextBox holding the formatted result of each committed
    /// (non-last) input source line. Recomputed only when input text
    /// changes — see `cached_input`. Selectable + copyable; never
    /// receives keyboard input.
    output: TextBox,
    /// Stateful evaluator. Held across renders so we can `reset()` and
    /// replay instead of paying for unit-registry construction every
    /// recompute.
    engine: poppop::Engine,
    /// Last input-text snapshot the output was computed against. Output
    /// is recomputed only when this differs from the live `textbox.text()`.
    cached_input: String,
    /// `(committed_paragraph_idx, message)` for input lines that failed
    /// to evaluate. Refreshed on each recompute alongside the output.
    /// Rendered in dark red beneath the input row at draw time.
    errors: Vec<(usize, String)>,
    x_origin: f32,
    y_origin: f32,
    width: f32,
    height: f32,
}

/// For each non-last `\n`-separated paragraph in `input`, evaluate it
/// through the poppop engine and produce the formatted result. The last
/// paragraph is skipped — it's the line being typed, where mid-stream
/// parse errors would be noisy. Output index `i` aligns with input
/// paragraph `i`. Empty / whitespace-only lines and parse / eval errors
/// render as empty strings in the output column; errors are surfaced
/// separately as `(paragraph_idx, message)` so the cell can render them
/// in red beneath the failing row.
///
/// `engine.reset()` runs at the top so stale bindings from prior
/// invocations don't leak. The caller controls when this helper runs
/// (currently: only when input text changes), so the cost is bounded.
pub(crate) fn compute_poppop_output(
    input: &str,
    engine: &mut poppop::Engine,
) -> (Vec<String>, Vec<(usize, String)>) {
    engine.reset();
    let paragraphs: Vec<&str> = input.split('\n').collect();
    if paragraphs.len() <= 1 {
        return (Vec::new(), Vec::new());
    }
    let committed = &paragraphs[..paragraphs.len() - 1];
    let mut out: Vec<String> = Vec::with_capacity(committed.len());
    let mut errs: Vec<(usize, String)> = Vec::new();
    for (idx, line) in committed.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            out.push(String::new());
            continue;
        }
        // Comments: lines starting with `#` are notes, not expressions.
        // They render in dark green on the input side and emit no output.
        if trimmed.starts_with('#') {
            out.push(String::new());
            continue;
        }
        match engine.eval(line) {
            Ok(answer) => {
                let v = match &answer {
                    poppop::Answer::Bare(v) => v,
                    poppop::Answer::Assigned { value, .. } => value,
                };
                out.push(poppop::format_value(v));
            }
            Err(e) => {
                out.push(String::new());
                // Some errors (notably pest parse errors) carry multi-line
                // messages with visual pointers. Trim to the first line so
                // the error fits on a single line under the input row.
                let full = e.to_string();
                let first = full.lines().next().unwrap_or("").to_string();
                errs.push((idx, first));
            }
        }
    }
    (out, errs)
}

impl PopPopCell {
    pub fn new(typeface: Typeface) -> Self {
        let mut output = TextBox::new(typeface.clone(), String::new());
        output.set_text_color(crate::color::poppop_output());
        let mut input = TextBox::new(typeface.clone(), String::new());
        // PopPop is the only cell type that wants `#`-prefixed lines
        // colored as comments (and skipped by the evaluator).
        input.set_enable_comment_coloring(true);
        Self {
            typeface: typeface.clone(),
            textbox: input,
            output,
            engine: poppop::Engine::new(),
            cached_input: String::new(),
            errors: Vec::new(),
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
        }
    }

    pub fn textbox(&self) -> &TextBox {
        &self.textbox
    }

    /// Drain a pending link URL from either the input or output column.
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        self.textbox
            .take_pending_link_url()
            .or_else(|| self.output.take_pending_link_url())
    }

    pub fn textbox_mut(&mut self) -> &mut TextBox {
        &mut self.textbox
    }

    #[allow(dead_code)]
    pub fn x_origin(&self) -> f32 {
        self.x_origin
    }
    #[allow(dead_code)]
    pub fn y_origin(&self) -> f32 {
        self.y_origin
    }
    #[allow(dead_code)]
    pub fn width(&self) -> f32 {
        self.width
    }
    #[allow(dead_code)]
    pub fn height(&self) -> f32 {
        self.height
    }

    fn input_width(&self, total: f32) -> f32 {
        ((total - GRID_DIVIDER_PAD * 2.0) * POPPOP_INPUT_RATIO).max(40.0)
    }

    pub fn tick(
        &mut self,
        canvas: &Canvas,
        x: f32,
        y: f32,
        width: f32,
        focused: bool,
        show_caret: bool,
    ) -> f32 {
        self.x_origin = x;
        self.y_origin = y;
        self.width = width;

        let scale = self.textbox.font_scale();
        let pad = GRID_DIVIDER_PAD * scale;
        let input_w = ((width - pad * 2.0) * POPPOP_INPUT_RATIO).max(40.0);
        let divider_x = x + input_w + pad;
        let output_x = divider_x + pad;
        let output_w = (x + width - output_x).max(20.0);

        // 1) Layout the input column (no draw). Populates body_lines so
        //    we can size error padding per row.
        self.textbox.layout(x, y, input_w);

        // 2) Sync output text + errors. Each non-last input source line
        //    gets the formatted poppop result; output row N aligns with
        //    input row N. Recompute only when the input text has changed
        //    (gated on `cached_input`) so we don't re-eval every render.
        let current_input = self.textbox.text().to_string();
        if current_input != self.cached_input {
            let (lines, errs) = compute_poppop_output(&current_input, &mut self.engine);
            let new_output_text = lines.join("\n");
            if self.output.text() != new_output_text {
                self.output.replace_text(new_output_text);
            }
            self.errors = errs;
            self.cached_input = current_input;
        }

        // 3) Reserve vertical space beneath any erroring row for the
        //    error message. Apply identical extras to both columns so
        //    rows stay aligned. v1: assumes one body_line per source
        //    paragraph (no wrap), so paragraph_idx == body_line_idx.
        let err_font = Font::from_typeface(
            &self.typeface,
            POPPOP_ERROR_FONT_SIZE * scale,
        );
        let (_, err_m) = err_font.metrics();
        let err_step = -err_m.ascent + err_m.descent + err_m.leading;
        let err_total = err_step + POPPOP_ERROR_BOTTOM_PAD * scale;

        let input_line_count = self.textbox.visual_line_count();
        let mut input_extras = vec![0.0_f32; input_line_count];
        for &(paragraph_idx, _) in &self.errors {
            if let Some(slot) = input_extras.get_mut(paragraph_idx) {
                *slot += err_total;
            }
        }
        self.textbox.set_line_extra_below(input_extras);
        // Re-layout the input so source_line_y_bands reflects the new
        // extras (the next textbox.tick call will be a no-op layout).
        self.textbox.layout(x, y, input_w);

        let output_line_count = self.output.visual_line_count();
        let mut output_extras = vec![0.0_f32; output_line_count];
        for &(paragraph_idx, _) in &self.errors {
            if let Some(slot) = output_extras.get_mut(paragraph_idx) {
                *slot += err_total;
            }
        }
        self.output.set_line_extra_below(output_extras);
        self.output.layout(output_x, y, output_w);

        let bands = self.textbox.source_line_y_bands();

        // 4) Alternating stripes BEHIND text.
        let calc_bands: Vec<(f32, f32)> = bands
            .iter()
            .map(|&(top, bot, _)| (top, bot))
            .collect();
        draw_alternating_row_stripes(canvas, &calc_bands, x, x + width);

        // 5) Draw input text on top of stripes.
        let input_h = self
            .textbox
            .tick(canvas, x, y, input_w, focused, show_caret);

        // 6) Vertical divider, muted.
        draw_vertical_divider(canvas, divider_x, y + 2.0, y + input_h - 2.0);

        // 7) Output column. Render with focused=has_selection so its
        //    selection highlight shows even though the cell's keyboard focus
        //    is on the input. Caret is suppressed (read-only).
        let output_focused = self.output.has_selection();
        self.output
            .tick(canvas, output_x, y, output_w, output_focused, false);

        // 8) Draw error messages in dark red within the reserved extra
        //    space at the bottom of each erroring row's input band.
        if !self.errors.is_empty() {
            let mut err_paint = Paint::default();
            err_paint.set_anti_alias(true);
            err_paint.set_color(crate::color::poppop_error());
            for (paragraph_idx, msg) in &self.errors {
                let Some(&(_, bot_abs, _)) = bands.get(*paragraph_idx) else {
                    continue;
                };
                let baseline = bot_abs - POPPOP_ERROR_BOTTOM_PAD * scale - err_m.descent;
                canvas.draw_str(
                    msg,
                    Point::new(x + POPPOP_ERROR_INDENT * scale, baseline),
                    &err_font,
                    &err_paint,
                );
            }
        }

        self.height = input_h;
        input_h
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        self.textbox.handle_key(event, modifiers)
    }

    /// Click in the input column drags input selection; click in the
    /// output column drags output selection. Whichever side wasn't clicked
    /// has its selection collapsed so only one column shows a selection
    /// at a time.
    pub fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
        let input_right = self.x_origin + self.input_width(self.width);
        if abs_x <= input_right {
            // Input click: clear output's selection so copy uses input's.
            self.output.set_caret_at(0);
            self.textbox.mouse_down(abs_x, abs_y, modifiers, editing)
        } else {
            // Output click: clear input's selection.
            self.textbox.set_caret_at(self.textbox.text().len());
            // Output is read-only; pass `editing=false` so a Cmd-click on
            // a link in the (very unlikely) output text does the right
            // thing.
            self.output.mouse_down(abs_x, abs_y, modifiers, false)
        }
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        // Both textboxes ignore drag-to when they don't have an active
        // drag, so we can forward to both — only the one that started the
        // drag does any work.
        let input_drag = self.textbox.mouse_drag_to(abs_x, abs_y);
        let output_drag = self.output.mouse_drag_to(abs_x, abs_y);
        input_drag || output_drag
    }

    pub fn mouse_up(&mut self) -> bool {
        let a = self.textbox.mouse_up();
        let b = self.output.mouse_up();
        a || b
    }

    /// True if `(abs_x, abs_y)` lands on a link in the input column. The
    /// output column has no links by construction, so we only check input.
    pub fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        self.textbox.link_at_doc_pos(abs_x, abs_y)
    }

    /// Copy whichever column has a non-empty selection. Output wins ties
    /// on the assumption that an output drag is the most recent gesture
    /// (a fresh input click clears the output selection in `mouse_down`).
    pub fn copy_selection(&self) -> String {
        let out = self.output.copy_primary_selection();
        if !out.is_empty() {
            return out;
        }
        self.textbox.copy_primary_selection()
    }

    pub fn snapshot(&self) -> TextBoxSnapshot {
        self.textbox.snapshot()
    }

    pub fn restore(&mut self, snap: TextBoxSnapshot) {
        self.textbox.restore(snap);
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        self.textbox.set_font_scale(scale);
        self.output.set_font_scale(scale);
    }
}
