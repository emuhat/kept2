//! `EntityCache` — the in-memory mirror of the DB's entity tables.
//!
//! Entities (people, ...) are the persistent identity layer that lives
//! one level above cells. The DB owns canonical state; this struct is
//! the read-side cache the UI walks (@-mention popup, entity page,
//! People page). After any entity-table write — `save_cell` that
//! observes `#person`, rename, delete, active-toggle — call
//! `refresh(db)` to repopulate. That's the **single invalidation
//! entry point** (the S5b S4-style win): forgetting to refresh used
//! to be a 15-call-site burden where any missed call left stale
//! @-mention candidates in the UI; now it's a missed method call
//! against a documented API.
//!
//! The four cache slots are kept consistent in lockstep — `refresh`
//! repopulates all four together so an observer never sees a
//! partial update.

use std::collections::HashMap;

use uuid::Uuid;

use crate::cell::Cell;
use crate::persist::{Db, Entity};

pub struct EntityCache {
    /// All entity rows from the DB. Source of identity (kind, display_name).
    pub entities: Vec<Entity>,
    /// `(alias, entity_id, kind)` index. Built from the DB; rebuilt on
    /// every `refresh`.
    pub alias_index: Vec<(String, Uuid, String)>,
    /// `cell_id → entity_id` for entities with a backing cell. Gates
    /// the title fallback (invariant #2) and lets the @-popup speak in
    /// entity-id space without scanning entities each time.
    pub cell_to_entity: HashMap<Uuid, Uuid>,
    /// `(entity_id, normalize(display_name))` for entities with a
    /// backing cell. The title-fallback corpus — entirely
    /// entity-derived. Cells without a corresponding entity are *not*
    /// here, even if their title matches (invariant #2).
    pub title_fallback: Vec<(Uuid, String)>,
    /// `entity_id → number of `kept://<id>` link spans in cell
    /// content`. Rebuilt by `refresh` from the in-memory cells (the
    /// counts can't be derived from the DB alone — mentions live as
    /// link spans inside cell JSON bodies). Used to gate "Delete
    /// person", weight @-mention fuzzy ranking, and surface a count
    /// next to people in the picker / People page.
    pub mentions: HashMap<Uuid, usize>,
}

impl EntityCache {
    pub fn empty() -> Self {
        Self {
            entities: Vec::new(),
            alias_index: Vec::new(),
            cell_to_entity: HashMap::new(),
            title_fallback: Vec::new(),
            mentions: HashMap::new(),
        }
    }

    /// Initial load from the DB + the in-memory cells. Equivalent to
    /// `empty()` followed by `refresh(db, cells)`. Returns an empty
    /// cache if `db` is None (test / headless builds).
    pub fn load(db: Option<&Db>, cells: &[Cell]) -> Self {
        let mut cache = Self::empty();
        cache.refresh(db, cells);
        cache
    }

    /// Re-fetch every index from the DB + recompute mention counts
    /// from `cells`. The single invalidation entry point — call after
    /// any entity-table write OR any cell mutation that may have
    /// changed link spans, to bring the in-memory caches back in
    /// line with disk + memory.
    ///
    /// Each sub-query is independent; a failure on one logs and leaves
    /// the corresponding cache slot empty rather than poisoning the
    /// others.
    pub fn refresh(&mut self, db: Option<&Db>, cells: &[Cell]) {
        if let Some(db) = db {
            match db.all_entities() {
                Ok(rows) => self.entities = rows,
                Err(e) => eprintln!("kept: refresh_entities failed: {e}"),
            }
            match db.entity_alias_index() {
                Ok(rows) => self.alias_index = rows,
                Err(e) => eprintln!("kept: entity_alias_index reload failed: {e}"),
            }
            match db.cell_to_entity_index() {
                Ok(rows) => self.cell_to_entity = rows.into_iter().collect(),
                Err(e) => eprintln!("kept: cell_to_entity_index reload failed: {e}"),
            }
            self.title_fallback = self
                .entities
                .iter()
                .filter(|e| e.primary_cell_id.is_some())
                .map(|e| (e.id, normalize_title_for_fallback(&e.display_name)))
                .collect();
        }
        self.refresh_mentions(cells);
    }

    /// Rebuild `mentions` from the in-memory cells. Pulled out so
    /// cell-only mutations (cell save / delete) can update mention
    /// counts without re-fetching every entity row from the DB.
    pub fn refresh_mentions(&mut self, cells: &[Cell]) {
        let mut counts: HashMap<Uuid, usize> = HashMap::new();
        for cell in cells {
            for url in cell.all_link_urls() {
                if let Some(id) = parse_kept_entity_url(&url) {
                    *counts.entry(id).or_insert(0) += 1;
                }
            }
        }
        self.mentions = counts;
    }

    /// O(1) lookup. Returns 0 for an entity with no recorded mentions
    /// (either unknown or genuinely zero — callers don't need to
    /// distinguish for the use cases we have today).
    pub fn mention_count(&self, entity_id: Uuid) -> usize {
        self.mentions.get(&entity_id).copied().unwrap_or(0)
    }
}

/// Parse `kept://<uuid>` → entity_id. Returns None for any URL that
/// isn't of that shape (external links, malformed kept URLs, etc).
fn parse_kept_entity_url(url: &str) -> Option<Uuid> {
    let suffix = url.strip_prefix("kept://")?;
    Uuid::parse_str(suffix).ok()
}

impl Default for EntityCache {
    fn default() -> Self {
        Self::empty()
    }
}

/// Normalize an entity's `display_name` into the form the resolver's
/// title fallback substring-matches against. Same shape as
/// `query::normalize_entity_token` — lowercase, strip whitespace and
/// underscores — so a query token and a fallback entry compare cleanly.
pub fn normalize_title_for_fallback(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}
