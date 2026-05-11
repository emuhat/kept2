//! `Document` — the cell / context source-of-truth subsystem.
//!
//! Owns the global append-only cell stream plus its in-memory dirty
//! state. Every cell or context mutation that needs to round-trip to
//! the DB goes through this struct's API; the mutation methods are the
//! single place where dirty / pending sets get touched (S4: centralize
//! dirty discipline). Forgetting to mark a cell dirty after editing
//! it used to be a silent persistence bug; now it's a missed method
//! call against a documented mutation API.
//!
//! Identity is by `Cell.id` / `Context.id`. Cells are kept sorted by
//! `(timestamp, id)` ascending (the natural cell-stream order); the
//! sidebar / cell-stream render walks the slice in reverse for
//! "newest first" display.

use std::collections::HashSet;

use uuid::Uuid;

use crate::cell::{Cell, now_epoch_ms};

/// Time-window overlay on top of the cell stream. Carried alongside
/// cells because most persistence paths touch both.
#[derive(Clone)]
pub struct Context {
    pub id: Uuid,
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub title: Option<String>,
}

pub struct Document {
    /// Global, append-only stream of cells. Source of truth.
    /// Always sorted ascending by `(timestamp, id)`.
    pub cells: Vec<Cell>,
    /// Time-window overlays. Membership is derived (timestamp-based),
    /// not stored on cells.
    pub contexts: Vec<Context>,
    /// Cells with in-memory edits not yet flushed to the DB.
    pub dirty_cells: HashSet<Uuid>,
    /// Cells deleted in memory; the DB row will be dropped on the
    /// next flush.
    pub pending_deletes: HashSet<Uuid>,
    /// Contexts with in-memory edits not yet flushed.
    pub dirty_contexts: HashSet<Uuid>,
    /// Contexts deleted in memory; the DB row will be dropped on the
    /// next flush.
    pub pending_context_deletes: HashSet<Uuid>,
}

impl Document {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            contexts: Vec::new(),
            dirty_cells: HashSet::new(),
            pending_deletes: HashSet::new(),
            dirty_contexts: HashSet::new(),
            pending_context_deletes: HashSet::new(),
        }
    }

    // ----- Read access -----

    /// Index of `id` in `cells`. `None` when no such cell exists.
    pub fn cell_idx(&self, id: Uuid) -> Option<usize> {
        self.cells.iter().position(|c| c.id == id)
    }

    pub fn cell(&self, id: Uuid) -> Option<&Cell> {
        self.cells.iter().find(|c| c.id == id)
    }

    pub fn cell_mut(&mut self, id: Uuid) -> Option<&mut Cell> {
        self.cells.iter_mut().find(|c| c.id == id)
    }

    // ----- Cell mutation (dirty discipline) -----

    /// Insert `cell` at its natural sorted position. The cell is
    /// automatically marked dirty — a freshly inserted cell needs to
    /// reach disk on the next flush. Returns the inserted index.
    pub fn insert_cell_sorted(&mut self, cell: Cell) -> usize {
        let id = cell.id;
        let pos = self
            .cells
            .binary_search_by(|c| c.timestamp.cmp(&cell.timestamp).then(c.id.cmp(&cell.id)));
        let at = match pos {
            Ok(i) => i,
            Err(i) => i,
        };
        self.cells.insert(at, cell);
        self.dirty_cells.insert(id);
        at
    }

    /// Mark `id`'s content as edited: bumps `Cell.edited_at` AND adds
    /// to `dirty_cells`. The standard entry point for any content
    /// mutation — call after the underlying edit (text/spans/active
    /// flag) lands.
    ///
    /// Equivalent to the prior `KeptApp::touch_cell`.
    pub fn touch_cell(&mut self, id: Uuid) {
        let now = now_epoch_ms();
        if let Some(cell) = self.cell_mut(id) {
            cell.edited_at = now;
        }
        self.dirty_cells.insert(id);
    }

    /// Add to `dirty_cells` without bumping `edited_at`. Reserved for
    /// the rare path that wants a save without making the cell look
    /// freshly edited (e.g., metadata-only writes). Almost everywhere
    /// you should call `touch_cell` instead.
    pub fn mark_cell_dirty(&mut self, id: Uuid) {
        self.dirty_cells.insert(id);
    }

    /// Mark a context's in-memory state as edited. Contexts have no
    /// `edited_at` field; this is purely a flush-queue entry.
    pub fn mark_context_dirty(&mut self, id: Uuid) {
        self.dirty_contexts.insert(id);
    }

    /// Remove the cell with `id` from the in-memory stream and queue
    /// it for deletion on the next flush. Idempotent — returns the
    /// removed cell (or `None` if it wasn't present). Also clears
    /// any pending dirty flag on it (a deleted cell can't be dirty).
    pub fn queue_cell_delete(&mut self, id: Uuid) -> Option<Cell> {
        let idx = self.cell_idx(id)?;
        let cell = self.cells.remove(idx);
        self.dirty_cells.remove(&id);
        self.pending_deletes.insert(id);
        Some(cell)
    }

    /// Remove a context from in-memory state and queue it for
    /// deletion. Mirrors `queue_cell_delete`.
    pub fn queue_context_delete(&mut self, id: Uuid) -> bool {
        let Some(idx) = self.contexts.iter().position(|c| c.id == id) else {
            return false;
        };
        self.contexts.remove(idx);
        self.dirty_contexts.remove(&id);
        self.pending_context_deletes.insert(id);
        true
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}
