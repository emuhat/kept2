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
}

impl EntityCache {
    pub fn empty() -> Self {
        Self {
            entities: Vec::new(),
            alias_index: Vec::new(),
            cell_to_entity: HashMap::new(),
            title_fallback: Vec::new(),
        }
    }

    /// Initial load from the DB. Equivalent to `empty()` followed by
    /// `refresh(db)`, but eliminates one temporary allocation for the
    /// caller. Returns an empty cache if `db` is None (test / headless
    /// builds).
    pub fn load(db: Option<&Db>) -> Self {
        let mut cache = Self::empty();
        cache.refresh(db);
        cache
    }

    /// Re-fetch all four indices from the DB. The single invalidation
    /// entry point — call after any entity-table write to bring the
    /// in-memory caches back in line with disk.
    ///
    /// Each sub-query is independent; a failure on one logs and leaves
    /// the corresponding cache slot empty rather than poisoning the
    /// other three (matches the prior `refresh_entities` behavior).
    pub fn refresh(&mut self, db: Option<&Db>) {
        let Some(db) = db else {
            return;
        };
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
