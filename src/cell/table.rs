//! `TableCell` — a grid of `M` rows by `N` cols, each cell an
//! independently-editable `TextBox`. Visual style mirrors PopPop
//! (alternating row stripes, muted column dividers) via the shared `grid`
//! helpers. Heading / tag behavior delegates to `cells[0][0]` so typing
//! `# Title #tag` in the top-left makes the table participate in the
//! title/tag system like other cells.

use std::ops::Range;

use skia_safe::{Canvas, Typeface};
use winit::{
    event::{ElementState, KeyEvent, Modifiers},
    keyboard::{Key, NamedKey},
};

use super::TextBox;
use super::common::TextBoxSnapshot;
use super::grid::{draw_alternating_row_stripes, draw_vertical_divider};

/// Default dimensions for a freshly-created Table cell.
const TABLE_DEFAULT_ROWS: usize = 3;
const TABLE_DEFAULT_COLS: usize = 3;
/// Inset between the column boundary and a cell's TextBox content.
const TABLE_CELL_PAD_X: f32 = 6.0;
const TABLE_CELL_PAD_Y: f32 = 4.0;

/// One slot in a Table. `readonly` blocks printable input
/// (data-model-only; no UI to toggle yet — populated via the persistence
/// layer or future API).
pub struct TableEntry {
    pub textbox: TextBox,
    pub readonly: bool,
}

#[derive(Clone)]
pub struct TableEntrySnapshot {
    pub textbox: TextBoxSnapshot,
    pub readonly: bool,
}

#[derive(Clone)]
pub struct TableSnapshot {
    pub cells: Vec<Vec<TableEntrySnapshot>>,
}

pub struct TableCell {
    typeface: Typeface,
    /// Row-major: `cells[r][c]`. Invariant: every row has the same length.
    cells: Vec<Vec<TableEntry>>,
    /// Active inner cell; receives keystrokes and owns the caret. The
    /// non-focused cells render with `focused=false` so caret/selection
    /// don't show in them.
    focused: (usize, usize),
    x_origin: f32,
    y_origin: f32,
    width: f32,
    height: f32,
    font_scale: f32,
}

impl TableCell {
    pub fn new(typeface: Typeface) -> Self {
        Self::with_dimensions(typeface, TABLE_DEFAULT_ROWS, TABLE_DEFAULT_COLS)
    }

    pub fn with_dimensions(typeface: Typeface, rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let mut grid: Vec<Vec<TableEntry>> = Vec::with_capacity(rows);
        for _ in 0..rows {
            let mut row: Vec<TableEntry> = Vec::with_capacity(cols);
            for _ in 0..cols {
                row.push(TableEntry {
                    textbox: TextBox::new(typeface.clone(), String::new()),
                    readonly: false,
                });
            }
            grid.push(row);
        }
        Self {
            typeface,
            cells: grid,
            focused: (0, 0),
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            font_scale: 1.0,
        }
    }

    pub fn rows(&self) -> usize {
        self.cells.len()
    }
    pub fn cols(&self) -> usize {
        self.cells.first().map(|r| r.len()).unwrap_or(0)
    }
    pub fn typeface(&self) -> &Typeface {
        &self.typeface
    }
    pub fn font_scale(&self) -> f32 {
        self.font_scale
    }
    pub fn cell_at(&self, r: usize, c: usize) -> Option<&TableEntry> {
        self.cells.get(r).and_then(|row| row.get(c))
    }
    pub fn cell_at_mut(&mut self, r: usize, c: usize) -> Option<&mut TableEntry> {
        self.cells.get_mut(r).and_then(|row| row.get_mut(c))
    }
    pub fn focused_index(&self) -> (usize, usize) {
        self.focused
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

    /// Empty when every inner TextBox is empty. Lets `Ctrl+Enter` skip
    /// the "no-op on empty cell" check the same way Plain/Outline/PopPop
    /// do.
    pub fn is_empty(&self) -> bool {
        self.cells
            .iter()
            .all(|row| row.iter().all(|e| e.textbox.is_empty()))
    }

    pub fn set_font_scale(&mut self, scale: f32) {
        self.font_scale = scale;
        for row in &mut self.cells {
            for entry in row.iter_mut() {
                entry.textbox.set_font_scale(scale);
            }
        }
    }

    /// Equal-split column widths. Inner-cell padding is subtracted before
    /// handing widths to the TextBoxes.
    fn col_layout(&self, total_width: f32) -> Vec<(f32, f32)> {
        let cols = self.cols().max(1);
        let col_w = (total_width / cols as f32).max(40.0);
        let mut out: Vec<(f32, f32)> = Vec::with_capacity(cols);
        for c in 0..cols {
            let left = self.x_origin + c as f32 * col_w;
            out.push((left, col_w));
        }
        out
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

        let scale = self.font_scale;
        let pad_x = TABLE_CELL_PAD_X * scale;
        let pad_y = TABLE_CELL_PAD_Y * scale;

        let cols_geom = self.col_layout(width);

        // ---- Layout pass: lay out every TextBox, compute row heights. ----
        let mut row_bands: Vec<(f32, f32)> = Vec::with_capacity(self.rows());
        let mut cur_y = y;
        for row in self.cells.iter_mut() {
            let row_top = cur_y;
            let mut row_h = 0.0_f32;
            for (c, entry) in row.iter_mut().enumerate() {
                let (col_left, col_w) = cols_geom[c];
                let inner_w = (col_w - pad_x * 2.0).max(20.0);
                entry
                    .textbox
                    .layout(col_left + pad_x, row_top + pad_y, inner_w);
                row_h = row_h.max(entry.textbox.height() + pad_y * 2.0);
            }
            row_bands.push((row_top, row_top + row_h));
            cur_y = row_top + row_h;
        }
        let y_bot = cur_y;
        self.height = y_bot - y;

        // ---- Draw pass ----

        // 1) Alternating row stripes spanning the full table width.
        draw_alternating_row_stripes(canvas, &row_bands, x, x + width);

        // 2) Vertical dividers between columns. Skip the leading edge (c=0)
        //    so the table doesn't get an outer left border.
        for c in 1..self.cols() {
            let dx = cols_geom[c].0;
            draw_vertical_divider(canvas, dx, y + 2.0, y_bot - 2.0);
        }

        // 3) Each TextBox. Only the focused (r, c) shows caret/selection;
        //    others render with focused=false so they're "asleep."
        let (fr, fc) = self.focused;
        for (r, row) in self.cells.iter_mut().enumerate() {
            for (c, entry) in row.iter_mut().enumerate() {
                let is_focused_inner = focused && r == fr && c == fc;
                let inner_caret = show_caret && is_focused_inner;
                let (col_left, col_w) = cols_geom[c];
                let inner_w = (col_w - pad_x * 2.0).max(20.0);
                entry.textbox.tick(
                    canvas,
                    col_left + pad_x,
                    row_bands[r].0 + pad_y,
                    inner_w,
                    is_focused_inner,
                    inner_caret,
                );
            }
        }

        self.height
    }

    /// Move keyboard focus to `(r, c)`. Collapses the previously-focused
    /// cell's selection so it doesn't keep showing highlight after focus
    /// leaves it (matches PopPop's two-textbox convention).
    fn focus_cell(&mut self, r: usize, c: usize) {
        if (r, c) == self.focused {
            return;
        }
        let (pr, pc) = self.focused;
        if let Some(prev) = self.cells.get_mut(pr).and_then(|row| row.get_mut(pc)) {
            // Collapse to caret at current head (whichever side the head is on).
            if let Some((_anchor, head)) = prev.textbox.primary_caret() {
                prev.textbox.set_caret_at(head);
            }
        }
        self.focused = (r, c);
        if let Some(next) = self.cells.get_mut(r).and_then(|row| row.get_mut(c)) {
            // Park the caret at the end of the destination cell so the
            // user can immediately type at the tail (mirrors the rest of
            // the app's "enter cell → caret at end" convention).
            let end = next.textbox.text().len();
            next.textbox.set_caret_at(end);
        }
    }

    /// `(rows, cols)` clamp helper used by Tab and arrow nav.
    fn dims(&self) -> (usize, usize) {
        (self.rows(), self.cols())
    }

    /// Move focus by one cell in the row-major direction (forward = right
    /// then wrap to next row's col 0). Returns true if focus moved. At
    /// the last cell going forward (or the first going backward), it
    /// stays.
    pub fn step_focus(&mut self, forward: bool) -> bool {
        let (rows, cols) = self.dims();
        let (r, c) = self.focused;
        let (nr, nc) = if forward {
            if c + 1 < cols {
                (r, c + 1)
            } else if r + 1 < rows {
                (r + 1, 0)
            } else {
                return false;
            }
        } else if c > 0 {
            (r, c - 1)
        } else if r > 0 {
            (r - 1, cols - 1)
        } else {
            return false;
        };
        self.focus_cell(nr, nc);
        true
    }

    pub fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        // TextBox filters Pressed-only at its own entry; we have to do the
        // same here because Tab/arrows are intercepted *before* forwarding,
        // so a release event would step focus again.
        if event.state != ElementState::Pressed {
            return false;
        }
        let mods = modifiers.state();
        let (rows, cols) = self.dims();
        let (r, c) = self.focused;

        // ---- Cross-cell navigation that the inner TextBox doesn't own. ----
        match &event.logical_key {
            Key::Named(NamedKey::Tab) => {
                self.step_focus(!mods.shift_key());
                return true;
            }
            Key::Named(NamedKey::ArrowUp) if !mods.shift_key() => {
                let at_top = self
                    .cell_at(r, c)
                    .map(|e| e.textbox.at_top_visual_line())
                    .unwrap_or(true);
                if at_top && r > 0 {
                    self.focus_cell(r - 1, c);
                    return true;
                }
            }
            Key::Named(NamedKey::ArrowDown) if !mods.shift_key() => {
                let at_bot = self
                    .cell_at(r, c)
                    .map(|e| e.textbox.at_bottom_visual_line())
                    .unwrap_or(true);
                if at_bot && r + 1 < rows {
                    self.focus_cell(r + 1, c);
                    return true;
                }
            }
            Key::Named(NamedKey::ArrowLeft) if !mods.shift_key() => {
                let at_start = self
                    .cell_at(r, c)
                    .and_then(|e| e.textbox.primary_caret())
                    .map(|(_, h)| h == 0)
                    .unwrap_or(true);
                if at_start && c > 0 {
                    self.focus_cell(r, c - 1);
                    return true;
                }
            }
            Key::Named(NamedKey::ArrowRight) if !mods.shift_key() => {
                let (caret, len) = match self.cell_at(r, c) {
                    Some(e) => (
                        e.textbox.primary_caret().map(|(_, h)| h).unwrap_or(0),
                        e.textbox.text().len(),
                    ),
                    None => (0, 0),
                };
                if caret == len && c + 1 < cols {
                    self.focus_cell(r, c + 1);
                    return true;
                }
            }
            _ => {}
        }

        // ---- Forward to the focused TextBox, gated on readonly. ----
        let entry = match self.cell_at_mut(r, c) {
            Some(e) => e,
            None => return false,
        };
        if entry.readonly {
            // Readonly cells eat printable input but allow caret/selection
            // movement (Home/End/non-edge arrows/etc — TextBox's nav keys
            // don't mutate text).
            match &event.logical_key {
                Key::Named(NamedKey::Backspace)
                | Key::Named(NamedKey::Delete)
                | Key::Named(NamedKey::Enter) => return false,
                Key::Character(_) => return false,
                _ => {}
            }
            // Fall through: TextBox handles arrows/Home/End harmlessly.
        }
        entry.textbox.handle_key(event, modifiers)
    }

    pub fn mouse_down(
        &mut self,
        abs_x: f32,
        abs_y: f32,
        modifiers: &Modifiers,
        editing: bool,
    ) -> bool {
        let target = self.hit_test(abs_x, abs_y);
        if let Some((r, c)) = target {
            if (r, c) != self.focused {
                self.focus_cell(r, c);
            }
            if let Some(entry) = self.cell_at_mut(r, c) {
                let allow_editing = editing && !entry.readonly;
                return entry
                    .textbox
                    .mouse_down(abs_x, abs_y, modifiers, allow_editing);
            }
        }
        false
    }

    fn hit_test(&self, abs_x: f32, abs_y: f32) -> Option<(usize, usize)> {
        if self.cells.is_empty() {
            return None;
        }
        // Find the row whose y-band contains abs_y. Bands abut, so any
        // out-of-range y clamps to first/last.
        let mut row_idx = 0usize;
        for r in 0..self.rows() {
            let entry = self.cells[r].first()?;
            let top = entry.textbox.y_origin() - TABLE_CELL_PAD_Y * self.font_scale;
            let bot_entry = self.cells[r]
                .iter()
                .map(|e| e.textbox.y_origin() + e.textbox.height())
                .fold(top, f32::max);
            let bot = bot_entry + TABLE_CELL_PAD_Y * self.font_scale;
            if abs_y < top {
                row_idx = r;
                break;
            }
            row_idx = r;
            if abs_y < bot {
                break;
            }
        }
        // Find the col by x position using the stored col layout.
        let cols_geom = self.col_layout(self.width);
        let mut col_idx = 0usize;
        for (c, &(left, w)) in cols_geom.iter().enumerate() {
            col_idx = c;
            if abs_x < left + w {
                break;
            }
        }
        Some((row_idx, col_idx))
    }

    pub fn mouse_drag_to(&mut self, abs_x: f32, abs_y: f32) -> bool {
        let mut any = false;
        for row in &mut self.cells {
            for entry in row.iter_mut() {
                if entry.textbox.mouse_drag_to(abs_x, abs_y) {
                    any = true;
                }
            }
        }
        any
    }

    pub fn mouse_up(&mut self) -> bool {
        let mut any = false;
        for row in &mut self.cells {
            for entry in row.iter_mut() {
                if entry.textbox.mouse_up() {
                    any = true;
                }
            }
        }
        any
    }

    pub fn link_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        for row in &self.cells {
            for entry in row {
                if entry.textbox.link_at_doc_pos(abs_x, abs_y) {
                    return true;
                }
            }
        }
        false
    }

    pub fn tag_at_doc_pos(&self, abs_x: f32, abs_y: f32) -> bool {
        for row in &self.cells {
            for entry in row {
                if entry.textbox.tag_at_doc_pos(abs_x, abs_y) {
                    return true;
                }
            }
        }
        false
    }

    pub fn copy_selection(&self) -> String {
        for row in &self.cells {
            for entry in row {
                let s = entry.textbox.copy_primary_selection();
                if !s.is_empty() {
                    return s;
                }
            }
        }
        String::new()
    }

    pub fn cut_focused(&mut self) -> String {
        let (r, c) = self.focused;
        let entry = match self.cell_at_mut(r, c) {
            Some(e) => e,
            None => return String::new(),
        };
        if entry.readonly {
            return entry.textbox.copy_primary_selection();
        }
        entry.textbox.cut_primary_selection()
    }

    pub fn paste_focused(&mut self, s: &str) {
        let (r, c) = self.focused;
        if let Some(entry) = self.cell_at_mut(r, c) {
            if !entry.readonly {
                entry.textbox.paste(s);
            }
        }
    }

    /// Concatenated cell text in row-major order — tabs between cells in
    /// a row, newlines between rows. Used by the search popup for
    /// substring matching across the table.
    pub fn full_text(&self) -> String {
        let mut out = String::new();
        for (r, row) in self.cells.iter().enumerate() {
            if r > 0 {
                out.push('\n');
            }
            for (c, entry) in row.iter().enumerate() {
                if c > 0 {
                    out.push('\t');
                }
                out.push_str(entry.textbox.text());
            }
        }
        out
    }

    pub fn caret_doc_y_band(&self) -> Option<(f32, f32)> {
        let (r, c) = self.focused;
        self.cell_at(r, c)?.textbox.caret_doc_y_band()
    }

    pub fn snapshot(&self) -> TableSnapshot {
        let cells = self
            .cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|e| TableEntrySnapshot {
                        textbox: e.textbox.snapshot(),
                        readonly: e.readonly,
                    })
                    .collect()
            })
            .collect();
        TableSnapshot { cells }
    }

    pub fn restore(&mut self, snap: TableSnapshot) {
        let rows = snap.cells.len().max(1);
        let cols = snap.cells.first().map(|r| r.len()).unwrap_or(1).max(1);
        let mut grid: Vec<Vec<TableEntry>> = Vec::with_capacity(rows);
        for snap_row in snap.cells {
            let mut row: Vec<TableEntry> = Vec::with_capacity(cols);
            for entry_snap in snap_row {
                let mut tb = TextBox::new(self.typeface.clone(), String::new());
                tb.set_font_scale(self.font_scale);
                tb.restore(entry_snap.textbox);
                row.push(TableEntry {
                    textbox: tb,
                    readonly: entry_snap.readonly,
                });
            }
            grid.push(row);
        }
        // Clamp focused into the restored shape.
        let (fr, fc) = self.focused;
        self.focused = (fr.min(rows.saturating_sub(1)), fc.min(cols.saturating_sub(1)));
        self.cells = grid;
    }

    /// Build a TableCell from raw rows of `(text, links, readonly)`
    /// triples. Used by the persistence layer.
    pub fn from_records(
        typeface: Typeface,
        rows: Vec<Vec<(String, Vec<(Range<usize>, String)>, bool)>>,
    ) -> Self {
        let row_count = rows.len().max(1);
        let col_count = rows.first().map(|r| r.len()).unwrap_or(1).max(1);
        let mut grid: Vec<Vec<TableEntry>> = Vec::with_capacity(row_count);
        for row_recs in rows {
            let mut row: Vec<TableEntry> = Vec::with_capacity(col_count);
            for (text, links, readonly) in row_recs {
                let mut tb = TextBox::new(typeface.clone(), text);
                for (range, url) in links {
                    tb.add_link(range, url);
                }
                row.push(TableEntry { textbox: tb, readonly });
            }
            // Pad short rows with empty cells (defensive against
            // malformed JSON).
            while row.len() < col_count {
                row.push(TableEntry {
                    textbox: TextBox::new(typeface.clone(), String::new()),
                    readonly: false,
                });
            }
            grid.push(row);
        }
        Self {
            typeface,
            cells: grid,
            focused: (0, 0),
            x_origin: 0.0,
            y_origin: 0.0,
            width: 0.0,
            height: 0.0,
            font_scale: 1.0,
        }
    }

    /// Read access to the grid for persistence.
    pub fn rows_view(&self) -> &[Vec<TableEntry>] {
        &self.cells
    }

    /// Drain the first pending link URL from any of the inner cells.
    pub fn take_pending_link_url(&mut self) -> Option<String> {
        for row in &mut self.cells {
            for entry in row {
                if let Some(url) = entry.textbox.take_pending_link_url() {
                    return Some(url);
                }
            }
        }
        None
    }

    /// Drain the first pending inline-tag name from any of the inner
    /// cells (mirrors `take_pending_link_url`).
    pub fn take_pending_tag_name(&mut self) -> Option<String> {
        for row in &mut self.cells {
            for entry in row {
                if let Some(name) = entry.textbox.take_pending_tag_name() {
                    return Some(name);
                }
            }
        }
        None
    }
}
