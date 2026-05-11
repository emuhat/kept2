use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use skia_safe::Typeface;
use uuid::Uuid;

use crate::cell::{
    parse_inline_tags, Bullet, Cell, CellKind, OutlineCell, PlainCell, PopPopCell,
    ReferenceCell, ReferenceTarget, TableCell, TextBox,
};

/// Resolved database path: env override → OS data dir → CWD fallback.
pub fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("KEPT_DB_PATH") {
        return PathBuf::from(p);
    }
    if let Some(dir) = dirs::data_dir() {
        return dir.join("kept").join("notes.db");
    }
    PathBuf::from("notes.db")
}

pub struct Db {
    conn: Connection,
}

/// View of a context loaded from the DB. Mirrors the in-memory `Context` shape
/// but lives here so `persist` doesn't depend on app types.
pub struct ContextRow {
    pub id: Uuid,
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub title: Option<String>,
}

pub struct ContextRef<'a> {
    pub id: Uuid,
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub title: Option<&'a str>,
}

/// First-class entity. `id` is canonical identity — runtime must never
/// assume it equals any cell id (the migration bootstraps them equal for
/// `#person` cells, but that's a one-shot convention, not an invariant).
#[derive(Clone)]
pub struct Entity {
    pub id: Uuid,
    pub kind: String,
    pub display_name: String,
    pub primary_cell_id: Option<Uuid>,
    /// People-page UI shows / hides inactive rows; the @-mention popup
    /// downweights them heavily but still surfaces them on a literal
    /// query. Default true; flipped via the entity-page toggle.
    pub is_active: bool,
    #[allow(dead_code)]
    pub created_at: i64,
    #[allow(dead_code)]
    pub updated_at: i64,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> rusqlite::Result<()> {
        let version: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 2 {
            // v1 → v2: drop the entry-owned schema entirely; v2 is fresh.
            self.conn.execute_batch(
                "BEGIN;
                 DROP TABLE IF EXISTS cells;
                 DROP TABLE IF EXISTS entries;
                 CREATE TABLE cells (
                     id              BLOB PRIMARY KEY,
                     timestamp       INTEGER NOT NULL,
                     body            TEXT NOT NULL,
                     edited_at       INTEGER NOT NULL,
                     context_hint_id BLOB
                 );
                 CREATE INDEX cells_by_time ON cells (timestamp);
                 CREATE TABLE contexts (
                     id          BLOB PRIMARY KEY,
                     start_time  INTEGER NOT NULL,
                     end_time    INTEGER,
                     title       TEXT
                 );
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }
        if version < 3 {
            // v2 → v3: tags + cell_tags join table. Foreign-key cascade so
            // deleting a cell drops its associations automatically.
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE tags (
                     id   BLOB PRIMARY KEY,
                     name TEXT NOT NULL UNIQUE
                 );
                 CREATE TABLE cell_tags (
                     cell_id BLOB NOT NULL REFERENCES cells(id) ON DELETE CASCADE,
                     tag_id  BLOB NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                     PRIMARY KEY (cell_id, tag_id)
                 );
                 CREATE INDEX cell_tags_by_tag ON cell_tags (tag_id);
                 PRAGMA user_version = 3;
                 COMMIT;",
            )?;
            self.backfill_cell_tags()?;
        }
        if version < 4 {
            // v3 → v4: explicit title slot per cell. Walk every row, extract
            // any leading `# ...` heading line into the new `title` field on
            // the wrapped JSON, drop the `# ` from body text, and rebuild the
            // tag index from the now-canonical title source.
            self.migrate_extract_titles()?;
            self.conn.execute_batch(
                "BEGIN; PRAGMA user_version = 4; COMMIT;",
            )?;
            self.backfill_cell_tags()?;
        }
        if version < 5 {
            // v4 → v5: first-class entity table. `#person` cells bootstrap
            // entity rows (entity.id = cell.id, primary_cell_id = cell.id)
            // with normalized aliases. Bootstrap-only — runtime code never
            // assumes the equality.
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE entities (
                     id              BLOB PRIMARY KEY,
                     kind            TEXT NOT NULL,
                     display_name    TEXT NOT NULL,
                     primary_cell_id BLOB,
                     created_at      INTEGER NOT NULL,
                     updated_at      INTEGER NOT NULL
                 );
                 CREATE INDEX entities_by_kind ON entities (kind);
                 CREATE INDEX entities_by_primary_cell ON entities (primary_cell_id);
                 CREATE TABLE entity_aliases (
                     entity_id BLOB NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
                     alias     TEXT NOT NULL,
                     PRIMARY KEY (entity_id, alias)
                 );
                 CREATE INDEX entity_aliases_by_alias ON entity_aliases (alias);
                 PRAGMA user_version = 5;
                 COMMIT;",
            )?;
            self.backfill_entities_from_persons()?;
        }
        if version < 6 {
            // v5 → v6: per-entity active flag. Existing rows default to
            // active (1). New writes pick up explicit values.
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE entities ADD COLUMN is_active INTEGER NOT NULL DEFAULT 1;
                 PRAGMA user_version = 6;
                 COMMIT;",
            )?;
        }
        if version < 7 {
            // v6 → v7: tags became span-based. Walk every cell and
            // backfill the new `tags` field from the existing
            // text-parse rules so existing tags keep working. After
            // this migration, typing `#X` without committing through
            // the popup leaves no span and no tag.
            self.migrate_tags_to_spans()?;
            self.conn
                .execute_batch("BEGIN; PRAGMA user_version = 7; COMMIT;")?;
        }
        Ok(())
    }

    fn migrate_tags_to_spans(&mut self) -> rusqlite::Result<()> {
        let rows: Vec<(Vec<u8>, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, body FROM cells")?;
            let it = stmt.query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let to_records = |ranges: Vec<std::ops::Range<usize>>| -> Vec<TagRecord> {
            ranges
                .into_iter()
                .filter(|r| r.end > r.start + 1)
                .map(|r| TagRecord {
                    start: r.start,
                    end: r.end,
                })
                .collect()
        };
        let tx = self.conn.transaction()?;
        for (id_bytes, body_json) in rows {
            let mut pc: PersistedCell = match serde_json::from_str(&body_json) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if let Some(t) = pc.title.as_mut() {
                if t.tags.is_empty() {
                    let heading_end = t.text.find('\n').unwrap_or(t.text.len());
                    t.tags = to_records(parse_trailing_tags(&t.text, heading_end));
                }
            }
            match &mut pc.body {
                CellBody::Plain { text, tags, .. } => {
                    if tags.is_empty() {
                        *tags = to_records(parse_inline_tags(text));
                    }
                }
                CellBody::Outline { blocks, .. } => {
                    for b in blocks {
                        if b.tags.is_empty() {
                            b.tags = to_records(parse_inline_tags(&b.text));
                        }
                    }
                }
                CellBody::Table { cells, .. } => {
                    for row in cells {
                        for c in row {
                            if c.tags.is_empty() {
                                c.tags = to_records(parse_inline_tags(&c.text));
                            }
                        }
                    }
                }
                CellBody::PopPop { .. } | CellBody::Reference { .. } => {}
            }
            let new_json = match serde_json::to_string(&pc) {
                Ok(s) => s,
                Err(_) => continue,
            };
            tx.execute(
                "UPDATE cells SET body = ?1 WHERE id = ?2",
                params![new_json, id_bytes],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// v4 → v5 step: walk every cell row; for each cell carrying the
    /// `#person` tag with a non-empty extractable title, insert an entity
    /// row + canonical alias. Bootstrap rule: `entity.id = cell.id`,
    /// `primary_cell_id = cell.id`. Idempotent.
    fn backfill_entities_from_persons(&mut self) -> rusqlite::Result<()> {
        let rows: Vec<(Vec<u8>, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, body FROM cells")?;
            let it = stmt.query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = chrono::Utc::now().timestamp_millis();
        let tx = self.conn.transaction()?;
        for (id_bytes, body_json) in rows {
            let pc: PersistedCell = match serde_json::from_str(&body_json) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !tag_names_from_persisted_legacy(&pc)
                .iter()
                .any(|n| n == "person")
            {
                continue;
            }
            let title_text = match pc.title.as_ref() {
                Some(t) => t.text.as_str(),
                None => continue,
            };
            let Some(display) = extract_display_name(title_text) else {
                continue;
            };
            let alias = normalize_alias(&display);
            tx.execute(
                "INSERT OR REPLACE INTO entities \
                    (id, kind, display_name, primary_cell_id, created_at, updated_at) \
                 VALUES (?1, 'person', ?2, ?1, \
                         COALESCE((SELECT created_at FROM entities WHERE id = ?1), ?3), \
                         ?3)",
                params![id_bytes, display, now],
            )?;
            tx.execute(
                "DELETE FROM entity_aliases WHERE entity_id = ?1",
                params![id_bytes],
            )?;
            tx.execute(
                "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
                params![id_bytes, alias],
            )?;
        }
        tx.commit()
    }

    /// v3 → v4 step: walk every cell row and, where the body opens with a
    /// markdown `# ` heading line, extract it into the new top-level `title`
    /// field. Idempotent — cells already carrying a title are left alone, as
    /// are cells whose body does not open with `# `.
    fn migrate_extract_titles(&mut self) -> rusqlite::Result<()> {
        let rows: Vec<(Vec<u8>, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, body FROM cells")?;
            let it = stmt.query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let tx = self.conn.transaction()?;
        for (id_bytes, body_json) in rows {
            let Some(updated) = extract_title_from_body_json(&body_json) else {
                continue;
            };
            tx.execute(
                "UPDATE cells SET body = ?1 WHERE id = ?2",
                params![updated, id_bytes],
            )?;
        }
        tx.commit()
    }

    /// Walk every existing cell row, parse its stored body JSON for tags,
    /// and populate `tags` + `cell_tags`. Idempotent — safe to run after
    /// schema upgrade with existing content.
    fn backfill_cell_tags(&mut self) -> rusqlite::Result<()> {
        let rows: Vec<(Vec<u8>, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, body FROM cells")?;
            let it = stmt.query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id_bytes, body_json) in rows {
            let pc: PersistedCell = match serde_json::from_str(&body_json) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let names = tag_names_from_persisted_legacy(&pc);
            self.write_cell_tags(&id_bytes, &names)?;
        }
        Ok(())
    }

    /// Replace the cell_tags rows for `cell_id_bytes` with the given tag
    /// names. Inserts any new tag names into the `tags` table on demand.
    fn write_cell_tags(
        &mut self,
        cell_id_bytes: &[u8],
        names: &[String],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM cell_tags WHERE cell_id = ?1",
            params![cell_id_bytes],
        )?;
        for name in names {
            let new_id = Uuid::now_v7().as_bytes().to_vec();
            tx.execute(
                "INSERT OR IGNORE INTO tags (id, name) VALUES (?1, ?2)",
                params![new_id, name],
            )?;
            let tag_id_bytes: Vec<u8> = tx.query_row(
                "SELECT id FROM tags WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO cell_tags (cell_id, tag_id) VALUES (?1, ?2)",
                params![cell_id_bytes, tag_id_bytes],
            )?;
        }
        tx.commit()
    }

    pub fn load_cells(&self, typeface: &Typeface) -> rusqlite::Result<Vec<Cell>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, body, edited_at, context_hint_id \
             FROM cells ORDER BY timestamp ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let timestamp: i64 = row.get(1)?;
                let body: String = row.get(2)?;
                let edited_at: i64 = row.get(3)?;
                let hint_bytes: Option<Vec<u8>> = row.get(4)?;
                let id = Uuid::from_slice(&id_bytes).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?;
                let hint = match hint_bytes {
                    Some(b) => Some(Uuid::from_slice(&b).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Blob,
                            Box::new(e),
                        )
                    })?),
                    None => None,
                };
                Ok((id, timestamp, body, edited_at, hint))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut cells = Vec::with_capacity(rows.len());
        for (id, timestamp, body_json, edited_at, hint) in rows {
            let pc: PersistedCell = serde_json::from_str(&body_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let title = pc.title.map(|t| {
                let mut tb = TextBox::new(typeface.clone(), t.text);
                tb.set_force_heading(true);
                for l in t.links {
                    tb.add_link(l.start..l.end, l.url);
                }
                if t.tags.is_empty() {
                    // Pre-span data: backfill from trailing #-tokens.
                    tb.migrate_tags_from_text();
                } else {
                    for tag in t.tags {
                        tb.add_tag(tag.start..tag.end);
                    }
                }
                tb
            });
            let active = pc.active;
            let kind = body_to_kind(pc.body, typeface);
            cells.push(Cell::from_parts(
                id, kind, title, timestamp, edited_at, hint, active,
            ));
        }
        Ok(cells)
    }

    pub fn load_contexts(&self) -> rusqlite::Result<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, start_time, end_time, title FROM contexts ORDER BY start_time ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let id = Uuid::from_slice(&id_bytes).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?;
                Ok(ContextRow {
                    id,
                    start_time: row.get(1)?,
                    end_time: row.get(2)?,
                    title: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn save_cell(&mut self, cell: &Cell) -> rusqlite::Result<()> {
        let pc = persisted_cell_from(cell);
        let body_json = serde_json::to_string(&pc)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let hint_bytes = cell.context_hint_id.map(|u| u.as_bytes().to_vec());
        let cell_id_bytes = cell.id.as_bytes().to_vec();
        self.conn.execute(
            "INSERT INTO cells (id, timestamp, body, edited_at, context_hint_id) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(id) DO UPDATE SET \
                 timestamp = excluded.timestamp, \
                 body = excluded.body, \
                 edited_at = excluded.edited_at, \
                 context_hint_id = excluded.context_hint_id",
            params![
                cell_id_bytes,
                cell.timestamp,
                body_json,
                cell.edited_at,
                hint_bytes,
            ],
        )?;
        let names = tag_names_from_persisted(&pc);
        self.write_cell_tags(&cell_id_bytes, &names)?;

        // Entity sync (invariants #5, #6): only when `#person` is observed
        // AND a non-empty display_name extracts from the title. Otherwise
        // the entity table is left untouched — identity stays frozen.
        if names.iter().any(|n| n == "person") {
            if let Some(title) = pc.title.as_ref() {
                if let Some(display) = extract_display_name(&title.text) {
                    self.upsert_person_entity(cell.id, &display)?;
                }
            }
        }
        Ok(())
    }

    /// Distinct tag names currently attached to `cell_id`, alphabetically.
    #[allow(dead_code)]
    pub fn tags_for_cell(&self, cell_id: Uuid) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.name FROM cell_tags ct \
             JOIN tags t ON t.id = ct.tag_id \
             WHERE ct.cell_id = ?1 ORDER BY t.name",
        )?;
        let rows = stmt
            .query_map(params![cell_id.as_bytes().to_vec()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All known tag names, sorted alphabetically.
    /// Cell ids that currently carry the given tag, by timestamp ascending.
    #[allow(dead_code)]
    pub fn cells_with_tag(&self, name: &str) -> rusqlite::Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id FROM cell_tags ct \
             JOIN tags t  ON t.id = ct.tag_id \
             JOIN cells c ON c.id = ct.cell_id \
             WHERE t.name = ?1 ORDER BY c.timestamp ASC",
        )?;
        let rows = stmt
            .query_map(params![name], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for b in rows {
            if let Ok(id) = Uuid::from_slice(&b) {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Drop a tag row from the `tags` table. Caller is responsible for
    /// ensuring no `cell_tags` rows reference it (the FK cascade would
    /// silently strip them otherwise — fine, but the right-click affordance
    /// only exposes this for empty tags).
    pub fn delete_tag(&mut self, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM tags WHERE name = ?1",
            params![name],
        )?;
        Ok(())
    }

    pub fn delete_cell(&mut self, id: Uuid) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM cells WHERE id = ?1",
            params![id.as_bytes().to_vec()],
        )?;
        // Detach (don't delete) any entities backed by this cell.
        // Entity lifecycle is independent of cell lifecycle.
        self.detach_entities_from_cell(id)?;
        Ok(())
    }

    /// All entities, in insertion order. Used by KeptApp to refresh its
    /// in-memory entity caches after save/delete.
    pub fn all_entities(&self) -> rusqlite::Result<Vec<Entity>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, display_name, primary_cell_id, is_active, \
                    created_at, updated_at \
             FROM entities ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let kind: String = row.get(1)?;
                let display_name: String = row.get(2)?;
                let primary_bytes: Option<Vec<u8>> = row.get(3)?;
                let is_active_int: i64 = row.get(4)?;
                let created_at: i64 = row.get(5)?;
                let updated_at: i64 = row.get(6)?;
                Ok(Entity {
                    id: Uuid::from_slice(&id_bytes).unwrap_or_else(|_| Uuid::nil()),
                    kind,
                    display_name,
                    primary_cell_id: primary_bytes
                        .and_then(|b| Uuid::from_slice(&b).ok()),
                    is_active: is_active_int != 0,
                    created_at,
                    updated_at,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Look up a single entity by id. Returns `Ok(None)` when the row
    /// doesn't exist (caller decides what "missing" means — e.g., the
    /// entity page renders an error stub). Used by the entity-page render
    /// path and the `kept://<uuid>` click resolver.
    #[allow(dead_code)]
    pub fn find_entity(&self, id: Uuid) -> rusqlite::Result<Option<Entity>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, display_name, primary_cell_id, is_active, \
                    created_at, updated_at \
             FROM entities WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id.as_bytes().to_vec()], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let kind: String = row.get(1)?;
                let display_name: String = row.get(2)?;
                let primary_bytes: Option<Vec<u8>> = row.get(3)?;
                let is_active_int: i64 = row.get(4)?;
                let created_at: i64 = row.get(5)?;
                let updated_at: i64 = row.get(6)?;
                Ok(Entity {
                    id: Uuid::from_slice(&id_bytes).unwrap_or_else(|_| Uuid::nil()),
                    kind,
                    display_name,
                    primary_cell_id: primary_bytes
                        .and_then(|b| Uuid::from_slice(&b).ok()),
                    is_active: is_active_int != 0,
                    created_at,
                    updated_at,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// All `(alias, entity_id, kind)` rows. Used to build the in-memory
    /// resolver index without per-query joins.
    pub fn entity_alias_index(&self) -> rusqlite::Result<Vec<(String, Uuid, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.alias, a.entity_id, e.kind \
             FROM entity_aliases a \
             JOIN entities e ON e.id = a.entity_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let alias: String = row.get(0)?;
                let id_bytes: Vec<u8> = row.get(1)?;
                let kind: String = row.get(2)?;
                Ok((
                    alias,
                    Uuid::from_slice(&id_bytes).unwrap_or_else(|_| Uuid::nil()),
                    kind,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(primary_cell_id, entity_id)` pairs for entities with a backing
    /// cell. Used by the title-fallback gate (invariant #2).
    pub fn cell_to_entity_index(&self) -> rusqlite::Result<Vec<(Uuid, Uuid)>> {
        let mut stmt = self.conn.prepare(
            "SELECT primary_cell_id, id FROM entities \
             WHERE primary_cell_id IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let cell_bytes: Vec<u8> = row.get(0)?;
                let entity_bytes: Vec<u8> = row.get(1)?;
                Ok((
                    Uuid::from_slice(&cell_bytes).unwrap_or_else(|_| Uuid::nil()),
                    Uuid::from_slice(&entity_bytes).unwrap_or_else(|_| Uuid::nil()),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Insert-or-update for a cell-backed person entity. Re-applied every
    /// time `save_cell` observes `#person` on a cell — including across
    /// tag-removal / tag-readd cycles. The first call for a given cell
    /// allocates the entity with `entity.id = cell.id` (bootstrap rule).
    /// Subsequent calls re-derive `display_name` and replace alias rows.
    pub fn upsert_person_entity(
        &mut self,
        cell_id: Uuid,
        display_name: &str,
    ) -> rusqlite::Result<()> {
        let id_bytes = cell_id.as_bytes().to_vec();
        let now = chrono::Utc::now().timestamp_millis();
        let alias = normalize_alias(display_name);
        let tx = self.conn.transaction()?;
        // Preserve created_at + is_active on update; bump updated_at.
        // The COALESCE on is_active is critical — every save_cell call
        // re-runs this upsert, and clobbering is_active would mean a
        // single edit on the cell flips a manually-deactivated person
        // back to active.
        tx.execute(
            "INSERT OR REPLACE INTO entities \
                (id, kind, display_name, primary_cell_id, is_active, \
                 created_at, updated_at) \
             VALUES (?1, 'person', ?2, ?1, \
                     COALESCE((SELECT is_active FROM entities WHERE id = ?1), 1), \
                     COALESCE((SELECT created_at FROM entities WHERE id = ?1), ?3), \
                     ?3)",
            params![id_bytes, display_name, now],
        )?;
        tx.execute(
            "DELETE FROM entity_aliases WHERE entity_id = ?1",
            params![id_bytes],
        )?;
        tx.execute(
            "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
            params![id_bytes, alias],
        )?;
        tx.commit()
    }

    /// Create a cell-less person entity with a freshly allocated id and
    /// `primary_cell_id = NULL`. Used by the People page's "Add person…"
    /// affordance. Caller must subsequently `refresh_entities` so the
    /// in-memory caches see the new row.
    pub fn create_cell_less_person_entity(
        &mut self,
        display_name: &str,
    ) -> rusqlite::Result<Uuid> {
        let id = Uuid::now_v7();
        let id_bytes = id.as_bytes().to_vec();
        let now = chrono::Utc::now().timestamp_millis();
        let alias = normalize_alias(display_name);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO entities \
                (id, kind, display_name, primary_cell_id, is_active, \
                 created_at, updated_at) \
             VALUES (?1, 'person', ?2, NULL, 1, ?3, ?3)",
            params![id_bytes, display_name, now],
        )?;
        tx.execute(
            "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
            params![id_bytes, alias],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Rename an existing entity. Updates `display_name`, replaces alias
    /// rows with a fresh normalization. The caller is responsible for
    /// rewriting the backing cell's title (in-memory + dirty flag) when
    /// `primary_cell_id` is set — this method touches only the entity
    /// tables.
    pub fn rename_person_entity(
        &mut self,
        entity_id: Uuid,
        new_display_name: &str,
    ) -> rusqlite::Result<()> {
        let id_bytes = entity_id.as_bytes().to_vec();
        let now = chrono::Utc::now().timestamp_millis();
        let alias = normalize_alias(new_display_name);
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE entities SET display_name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id_bytes, new_display_name, now],
        )?;
        tx.execute(
            "DELETE FROM entity_aliases WHERE entity_id = ?1",
            params![id_bytes],
        )?;
        tx.execute(
            "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
            params![id_bytes, alias],
        )?;
        tx.commit()
    }

    /// Insert a person entity row at a specific id + created_at +
    /// is_active state, with alias rebuilt from `display_name`. Used to
    /// reverse Add (redo) and Delete (undo) on cell-less entities —
    /// preserving the original id keeps any pre-existing `kept://`
    /// mentions valid through the undo round-trip, and preserving
    /// is_active means an inactive entity that gets deleted comes back
    /// inactive on undo.
    pub fn insert_person_entity_with_id(
        &mut self,
        entity_id: Uuid,
        display_name: &str,
        is_active: bool,
        created_at: i64,
    ) -> rusqlite::Result<()> {
        let id_bytes = entity_id.as_bytes().to_vec();
        let now = chrono::Utc::now().timestamp_millis();
        let alias = normalize_alias(display_name);
        let active_int: i64 = if is_active { 1 } else { 0 };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO entities \
                (id, kind, display_name, primary_cell_id, is_active, \
                 created_at, updated_at) \
             VALUES (?1, 'person', ?2, NULL, ?3, ?4, ?5)",
            params![id_bytes, display_name, active_int, created_at, now],
        )?;
        tx.execute(
            "DELETE FROM entity_aliases WHERE entity_id = ?1",
            params![id_bytes],
        )?;
        tx.execute(
            "INSERT INTO entity_aliases (entity_id, alias) VALUES (?1, ?2)",
            params![id_bytes, alias],
        )?;
        tx.commit()
    }

    /// Toggle the `is_active` flag on a single entity. Bumps `updated_at`
    /// so observers (sync, conflict resolution if any) see the change.
    pub fn set_entity_active(
        &mut self,
        entity_id: Uuid,
        is_active: bool,
    ) -> rusqlite::Result<()> {
        let id_bytes = entity_id.as_bytes().to_vec();
        let now = chrono::Utc::now().timestamp_millis();
        let active_int: i64 = if is_active { 1 } else { 0 };
        self.conn.execute(
            "UPDATE entities SET is_active = ?2, updated_at = ?3 WHERE id = ?1",
            params![id_bytes, active_int, now],
        )?;
        Ok(())
    }

    /// Drop an entity row + its alias rows. Caller must ensure the
    /// entity has no incoming `kept://<id>` mentions and no backing cell
    /// (otherwise existing links go stale and cell saves will reinsert
    /// it). Used by the People page's right-click → Delete person flow,
    /// gated on those preconditions in the UI layer.
    pub fn delete_entity(&mut self, entity_id: Uuid) -> rusqlite::Result<()> {
        let id_bytes = entity_id.as_bytes().to_vec();
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM entity_aliases WHERE entity_id = ?1",
            params![id_bytes],
        )?;
        tx.execute(
            "DELETE FROM entities WHERE id = ?1",
            params![id_bytes],
        )?;
        tx.commit()
    }

    /// Set `primary_cell_id = NULL` on every entity that points to this
    /// cell. Called from `delete_cell`. Does NOT delete the entity —
    /// identity persists as orphan (invariant #7).
    pub fn detach_entities_from_cell(&mut self, cell_id: Uuid) -> rusqlite::Result<()> {
        let id_bytes = cell_id.as_bytes().to_vec();
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            "UPDATE entities SET primary_cell_id = NULL, updated_at = ?2 \
             WHERE primary_cell_id = ?1",
            params![id_bytes, now],
        )?;
        Ok(())
    }

    pub fn save_context(&mut self, ctx: &ContextRef<'_>) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO contexts (id, start_time, end_time, title) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET \
                 start_time = excluded.start_time, \
                 end_time = excluded.end_time, \
                 title = excluded.title",
            params![ctx.id.as_bytes().to_vec(), ctx.start_time, ctx.end_time, ctx.title],
        )?;
        Ok(())
    }

    pub fn delete_context(&mut self, id: Uuid) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM contexts WHERE id = ?1",
            params![id.as_bytes().to_vec()],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Serde shape for cells.body — kept private to this module.
// ---------------------------------------------------------------------------

/// Top-level shape persisted as the JSON `body` column. Wraps a kind-tagged
/// `CellBody` with an optional structured title slot. Old rows stored as
/// bare CellBody (`{"kind":"plain","text":"..."}`) deserialize as
/// `PersistedCell { title: None, body: ... }` thanks to `#[serde(flatten)]`.
#[derive(Serialize, Deserialize)]
struct PersistedCell {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<TitleRecord>,
    /// "Archived" status. Default true (active) so legacy JSON
    /// without this field — every cell saved before this feature
    /// landed — loads as active. Skip-if-default keeps the JSON
    /// shape identical for the common case.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    active: bool,
    #[serde(flatten)]
    body: CellBody,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Serialize, Deserialize)]
struct TitleRecord {
    text: String,
    #[serde(default)]
    links: Vec<LinkRecord>,
    /// `#tag` spans within the title. Absent in pre-span data — see
    /// `migrate_tags_in_textbox` for the load-time backfill.
    #[serde(default)]
    tags: Vec<TagRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum CellBody {
    Plain {
        text: String,
        #[serde(default)]
        links: Vec<LinkRecord>,
        #[serde(default)]
        tags: Vec<TagRecord>,
    },
    Outline {
        blocks: Vec<BlockRecord>,
        /// Pinned reference at the top of the outline ("envelope"
        /// cells). Default-None for plain outlines and for legacy
        /// pre-envelope data; never serialized when absent so the
        /// existing JSON shape doesn't change for any non-envelope
        /// outline.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference_header: Option<ReferenceTargetRecord>,
    },
    #[serde(alias = "pop")]
    PopPop {
        text: String,
        #[serde(default)]
        links: Vec<LinkRecord>,
    },
    Table {
        rows: usize,
        cols: usize,
        cells: Vec<Vec<TableEntryRecord>>,
    },
    Reference {
        target: ReferenceTargetRecord,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ReferenceTargetRecord {
    /// Whole-cell embed.
    Cell { cell_id: Uuid },
    /// Sub-tree of an outline (root bullet + descendants).
    Subtree { cell_id: Uuid, bullet_id: Uuid },
}

impl From<ReferenceTarget> for ReferenceTargetRecord {
    fn from(t: ReferenceTarget) -> Self {
        match t {
            ReferenceTarget::WholeCell(cell_id) => ReferenceTargetRecord::Cell { cell_id },
            ReferenceTarget::Subtree { cell_id, bullet_id } => {
                ReferenceTargetRecord::Subtree { cell_id, bullet_id }
            }
        }
    }
}

impl From<ReferenceTargetRecord> for ReferenceTarget {
    fn from(r: ReferenceTargetRecord) -> Self {
        match r {
            ReferenceTargetRecord::Cell { cell_id } => ReferenceTarget::WholeCell(cell_id),
            ReferenceTargetRecord::Subtree { cell_id, bullet_id } => {
                ReferenceTarget::Subtree { cell_id, bullet_id }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct LinkRecord {
    start: usize,
    end: usize,
    url: String,
}

#[derive(Serialize, Deserialize)]
struct BlockRecord {
    id: Uuid,
    depth: u32,
    text: String,
    #[serde(default)]
    links: Vec<LinkRecord>,
    #[serde(default)]
    tags: Vec<TagRecord>,
    /// Per-bullet "archived" flag. Default true so legacy JSON
    /// loads bullets as active; serializer skips the field for the
    /// common active case so the JSON shape stays identical for
    /// existing data.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    active: bool,
}

#[derive(Serialize, Deserialize)]
struct TableEntryRecord {
    text: String,
    #[serde(default)]
    links: Vec<LinkRecord>,
    #[serde(default)]
    tags: Vec<TagRecord>,
    #[serde(default)]
    readonly: bool,
}

#[derive(Serialize, Deserialize)]
struct TagRecord {
    start: usize,
    end: usize,
}

/// Build the wrapped persisted form from a live Cell. Captures the title
/// slot (if any) and the kind-tagged body.
fn persisted_cell_from(cell: &Cell) -> PersistedCell {
    let title = cell.title().map(|tb| TitleRecord {
        text: tb.text().to_string(),
        links: tb
            .links()
            .iter()
            .map(|l| LinkRecord {
                start: l.range.start,
                end: l.range.end,
                url: l.url.clone(),
            })
            .collect(),
        tags: tb
            .tags()
            .iter()
            .map(|t| TagRecord {
                start: t.range.start,
                end: t.range.end,
            })
            .collect(),
    });
    PersistedCell {
        title,
        active: cell.active,
        body: cell_to_body(cell),
    }
}

fn cell_to_body(cell: &Cell) -> CellBody {
    let tag_records = |tb: &TextBox| -> Vec<TagRecord> {
        tb.tags()
            .iter()
            .map(|t| TagRecord {
                start: t.range.start,
                end: t.range.end,
            })
            .collect()
    };
    match &cell.kind {
        CellKind::Plain(pc) => CellBody::Plain {
            text: pc.body().text().to_string(),
            links: pc
                .body()
                .links()
                .iter()
                .map(|l| LinkRecord {
                    start: l.range.start,
                    end: l.range.end,
                    url: l.url.clone(),
                })
                .collect(),
            tags: tag_records(pc.body()),
        },
        CellKind::Outline(oc) => CellBody::Outline {
            blocks: oc
                .bullets()
                .iter()
                .map(|b| BlockRecord {
                    id: b.id(),
                    depth: b.depth(),
                    text: b.textbox().text().to_string(),
                    links: b
                        .textbox()
                        .links()
                        .iter()
                        .map(|l| LinkRecord {
                            start: l.range.start,
                            end: l.range.end,
                            url: l.url.clone(),
                        })
                        .collect(),
                    tags: tag_records(b.textbox()),
                    active: b.active(),
                })
                .collect(),
            reference_header: oc
                .reference_header()
                .map(|h| ReferenceTargetRecord::from(h.target())),
        },
        CellKind::PopPop(pc) => CellBody::PopPop {
            text: pc.textbox().text().to_string(),
            links: pc
                .textbox()
                .links()
                .iter()
                .map(|l| LinkRecord {
                    start: l.range.start,
                    end: l.range.end,
                    url: l.url.clone(),
                })
                .collect(),
        },
        CellKind::Table(tc) => {
            let cells: Vec<Vec<TableEntryRecord>> = tc
                .rows_view()
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|e| TableEntryRecord {
                            text: e.textbox.text().to_string(),
                            links: e
                                .textbox
                                .links()
                                .iter()
                                .map(|l| LinkRecord {
                                    start: l.range.start,
                                    end: l.range.end,
                                    url: l.url.clone(),
                                })
                                .collect(),
                            tags: tag_records(&e.textbox),
                            readonly: e.readonly,
                        })
                        .collect()
                })
                .collect();
            CellBody::Table {
                rows: tc.rows(),
                cols: tc.cols(),
                cells,
            }
        }
        CellKind::Reference(rc) => CellBody::Reference {
            target: rc.target().into(),
        },
    }
}

/// Migration helper: deserialize legacy/new body JSON, and if the body
/// starts with a `# ` heading (and no explicit `title` is already set),
/// extract the heading line into a `title` field. Returns Some(json) when
/// the row needs to be rewritten, or None to skip.
fn extract_title_from_body_json(body_json: &str) -> Option<String> {
    let mut pc: PersistedCell = serde_json::from_str(body_json).ok()?;
    if pc.title.is_some() {
        return None;
    }
    let extracted = match &mut pc.body {
        CellBody::Plain { text, links, .. } => take_heading_from_inline(text, links),
        CellBody::PopPop { text, links } => take_heading_from_inline(text, links),
        CellBody::Outline { blocks, .. } => take_heading_from_outline(blocks),
        CellBody::Table { cells, .. } => take_heading_from_table(cells),
        // Reference cells never had inline headings to migrate from.
        CellBody::Reference { .. } => None,
    };
    let title = extracted?;
    pc.title = Some(title);
    serde_json::to_string(&pc).ok()
}

/// Pull a leading `# ...` heading paragraph out of a Plain/PopPop body's
/// `(text, links)` pair. Returns the extracted TitleRecord (with `# ` stripped
/// and link offsets shifted to the title's local frame) and mutates `text` /
/// `links` in place to reflect the post-heading remainder. None when the
/// text does not start with `# `.
fn take_heading_from_inline(
    text: &mut String,
    links: &mut Vec<LinkRecord>,
) -> Option<TitleRecord> {
    if !text.starts_with("# ") {
        return None;
    }
    let heading_end = text.find('\n').unwrap_or(text.len());
    // Title text: drop the leading "# " (2 bytes).
    let title_text = text[2..heading_end].to_string();

    // Partition links: those wholly inside [2, heading_end) move to the
    // title (offsets shifted by -2); those wholly past heading_end+1
    // remain on the body (offsets shifted by -(heading_end + nl_skip)).
    let nl_skip = if heading_end < text.len() { 1 } else { 0 };
    let body_start = heading_end + nl_skip;
    let mut title_links: Vec<LinkRecord> = Vec::new();
    let mut body_links: Vec<LinkRecord> = Vec::new();
    for l in links.drain(..) {
        if l.start >= 2 && l.end <= heading_end {
            title_links.push(LinkRecord {
                start: l.start - 2,
                end: l.end - 2,
                url: l.url,
            });
        } else if l.start >= body_start {
            body_links.push(LinkRecord {
                start: l.start - body_start,
                end: l.end - body_start,
                url: l.url,
            });
        }
        // Links that straddle the heading/body boundary are dropped — there's
        // no sensible place to put them, and they're vanishingly rare in
        // existing data (the heading line ends at a `\n`).
    }
    *links = body_links;
    *text = text[body_start..].to_string();
    Some(TitleRecord {
        text: title_text,
        links: title_links,
        // Empty — the v7 migration runs after this and backfills tag
        // spans from the trailing-tags rule.
        tags: Vec::new(),
    })
}

fn take_heading_from_outline(blocks: &mut Vec<BlockRecord>) -> Option<TitleRecord> {
    if !blocks.first().map(|b| b.text.starts_with("# ")).unwrap_or(false) {
        return None;
    }
    let first = blocks.remove(0);
    let title_text = first.text[2..].to_string();
    let title_links: Vec<LinkRecord> = first
        .links
        .into_iter()
        .filter(|l| l.start >= 2 && l.end >= 2)
        .map(|l| LinkRecord {
            start: l.start - 2,
            end: l.end - 2,
            url: l.url,
        })
        .collect();
    if blocks.is_empty() {
        // Maintain the "outline always has at least one block" invariant.
        blocks.push(BlockRecord {
            id: Uuid::now_v7(),
            depth: 0,
            text: String::new(),
            links: Vec::new(),
            tags: Vec::new(),
            active: true,
        });
    }
    Some(TitleRecord {
        text: title_text,
        links: title_links,
        tags: Vec::new(),
    })
}

fn take_heading_from_table(
    cells: &mut Vec<Vec<TableEntryRecord>>,
) -> Option<TitleRecord> {
    let first = cells.first_mut()?.first_mut()?;
    if !first.text.starts_with("# ") {
        return None;
    }
    let heading_end = first.text.find('\n').unwrap_or(first.text.len());
    let title_text = first.text[2..heading_end].to_string();
    let nl_skip = if heading_end < first.text.len() { 1 } else { 0 };
    let body_start = heading_end + nl_skip;
    let mut title_links: Vec<LinkRecord> = Vec::new();
    let mut body_links: Vec<LinkRecord> = Vec::new();
    for l in first.links.drain(..) {
        if l.start >= 2 && l.end <= heading_end {
            title_links.push(LinkRecord {
                start: l.start - 2,
                end: l.end - 2,
                url: l.url,
            });
        } else if l.start >= body_start {
            body_links.push(LinkRecord {
                start: l.start - body_start,
                end: l.end - body_start,
                url: l.url,
            });
        }
    }
    first.links = body_links;
    first.text = first.text[body_start..].to_string();
    Some(TitleRecord {
        text: title_text,
        links: title_links,
        tags: Vec::new(),
    })
}

/// Distinct, source-ordered tag names sourced from the cell's title slot.
/// Body content never contributes to the tag index after v4 — the title is
/// the single source of truth.
/// Extract a person entity's display_name from a cell's title text.
/// Mirrors `Cell::heading_title` — strips trailing `#tag` tokens and
/// trims surrounding whitespace. Returns None when nothing is left
/// (title is empty / only tags / only whitespace).
fn extract_display_name(title_text: &str) -> Option<String> {
    let title_end = title_text.find('\n').unwrap_or(title_text.len());
    let tags = parse_trailing_tags(title_text, title_end);
    let bytes = title_text.as_bytes();
    let mut end = tags.first().map(|r| r.start).unwrap_or(title_end);
    while end > 0 && (bytes[end - 1] as char).is_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    Some(title_text[..end].to_string())
}

/// Canonical alias derived from a display_name: lowercase + spaces → `_`.
/// `"Patrick Foy" → "patrick_foy"`. Resolution-time normalization (strip
/// underscores too) happens in `query::normalize_entity_token`.
fn normalize_alias(display_name: &str) -> String {
    display_name
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join("_")
}

/// Names from saved span ranges; if no spans are present (legacy data
/// or a freshly-typed `#X` that the user never committed), the result
/// is empty — span absence means "not a tag." Falling back to a
/// text-parse here would re-introduce the accidental-tag bug.
fn names_from_tag_ranges(text: &str, tags: &[TagRecord], sink: &mut dyn FnMut(String)) {
    for t in tags {
        if t.end <= t.start + 1 || t.end > text.len() {
            continue;
        }
        let s = &text[t.start..t.end];
        if let Some(name) = s.strip_prefix('#') {
            if !name.is_empty() {
                sink(name.to_string());
            }
        }
    }
}

/// Strict (post-v7) reading: tags come from spans only. Used at save
/// time. New cells where the user typed `#X` without committing
/// produce no tag — exactly what the no-accidental-tags rule requires.
fn tag_names_from_persisted(pc: &PersistedCell) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    };
    if let Some(t) = pc.title.as_ref() {
        names_from_tag_ranges(&t.text, &t.tags, &mut |n| push(n));
    }
    match &pc.body {
        CellBody::Plain { text, tags, .. } => {
            names_from_tag_ranges(text, tags, &mut |n| push(n));
        }
        CellBody::Outline { blocks, .. } => {
            for b in blocks {
                names_from_tag_ranges(&b.text, &b.tags, &mut |n| push(n));
            }
        }
        CellBody::Table { cells, .. } => {
            for row in cells {
                for c in row {
                    names_from_tag_ranges(&c.text, &c.tags, &mut |n| push(n));
                }
            }
        }
        CellBody::PopPop { .. } | CellBody::Reference { .. } => {}
    }
    out
}

/// Legacy reading: span-derived names plus a text-parse fallback for
/// any record whose `tags` Vec is empty. Used by the v3/v4/v5
/// migrations that run before the v7 backfill — without the fallback,
/// pre-v7 data would lose every tag-derived link as the migration
/// chain rolls forward. Once v7 has run, every record has explicit
/// spans and the fallback never fires.
fn tag_names_from_persisted_legacy(pc: &PersistedCell) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    };
    let title_names = |t: &TitleRecord, sink: &mut dyn FnMut(String)| {
        if !t.tags.is_empty() {
            names_from_tag_ranges(&t.text, &t.tags, sink);
        } else {
            let heading_end = t.text.find('\n').unwrap_or(t.text.len());
            for r in parse_trailing_tags(&t.text, heading_end) {
                if r.end > r.start + 1 {
                    sink(t.text[r.start + 1..r.end].to_string());
                }
            }
        }
    };
    let body_names = |text: &str, tags: &[TagRecord], sink: &mut dyn FnMut(String)| {
        if !tags.is_empty() {
            names_from_tag_ranges(text, tags, sink);
        } else {
            for r in parse_inline_tags(text) {
                if r.end > r.start + 1 {
                    sink(text[r.start + 1..r.end].to_string());
                }
            }
        }
    };
    if let Some(t) = pc.title.as_ref() {
        title_names(t, &mut |n| push(n));
    }
    match &pc.body {
        CellBody::Plain { text, tags, .. } => {
            body_names(text, tags, &mut |n| push(n));
        }
        CellBody::Outline { blocks, .. } => {
            for b in blocks {
                body_names(&b.text, &b.tags, &mut |n| push(n));
            }
        }
        CellBody::Table { cells, .. } => {
            for row in cells {
                for c in row {
                    body_names(&c.text, &c.tags, &mut |n| push(n));
                }
            }
        }
        CellBody::PopPop { .. } | CellBody::Reference { .. } => {}
    }
    out
}

#[allow(dead_code)]
fn tag_names_from_body(body: &CellBody) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_from = |text: &str| {
        if !text.starts_with("# ") {
            return;
        }
        let heading_end = text.find('\n').unwrap_or(text.len());
        for r in parse_trailing_tags(text, heading_end) {
            // Strip leading `#`; require a non-empty body (skips bare `#`).
            if r.end > r.start + 1 {
                let name = text[r.start + 1..r.end].to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    };
    match body {
        CellBody::Plain { text, .. } => push_from(text),
        CellBody::Outline { blocks, .. } => {
            for b in blocks {
                push_from(&b.text);
            }
        }
        CellBody::PopPop { text, .. } => push_from(text),
        CellBody::Table { cells, .. } => {
            // Heading lives in [0][0] only — matches the Cell::heading_*
            // delegation. Other table cells don't contribute tags.
            if let Some(first) = cells.first().and_then(|row| row.first()) {
                push_from(&first.text);
            }
        }
        // Reference cells contribute no tags directly; the source cell's
        // tags are already indexed at its real location.
        CellBody::Reference { .. } => {}
    }
    out
}

/// Mirror of the heading-tag parser in `cell.rs`. Walks back through
/// whitespace-delimited `#` tokens at the end of the heading paragraph.
fn parse_trailing_tags(text: &str, heading_end: usize) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut tags = Vec::new();
    let mut end = heading_end;
    loop {
        while end > 0 && (bytes[end - 1] as char).is_whitespace() {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        let mut start = end;
        while start > 0 && !(bytes[start - 1] as char).is_whitespace() {
            start -= 1;
        }
        if start < end && bytes[start] == b'#' {
            tags.push(start..end);
            end = start;
        } else {
            break;
        }
    }
    tags.reverse();
    tags
}

fn body_to_kind(body: CellBody, typeface: &Typeface) -> CellKind {
    // Apply persisted tag spans to a body TextBox, falling back to the
    // legacy text-parse migration when no spans were stored — which is
    // the case for every cell saved before tags became span-based.
    // Migration is one-shot: once the cell saves again, the spans land
    // in the JSON and `migrate_tags_from_text` becomes a no-op.
    let load_body_tags = |tb: &mut TextBox, tags: Vec<TagRecord>| {
        if tags.is_empty() {
            tb.migrate_tags_from_text();
        } else {
            for t in tags {
                tb.add_tag(t.start..t.end);
            }
        }
    };
    match body {
        CellBody::Plain { text, links, tags } => {
            let mut tb = TextBox::new(typeface.clone(), text);
            for l in links {
                tb.add_link(l.start..l.end, l.url);
            }
            load_body_tags(&mut tb, tags);
            CellKind::Plain(PlainCell::from_textbox(tb))
        }
        CellBody::Outline { blocks, reference_header } => {
            let bullets: Vec<Bullet> = blocks
                .into_iter()
                .map(|b| {
                    let mut tb = TextBox::new(typeface.clone(), b.text);
                    for l in b.links {
                        tb.add_link(l.start..l.end, l.url);
                    }
                    load_body_tags(&mut tb, b.tags);
                    let mut bullet = Bullet::new(b.id, tb, b.depth);
                    bullet.set_active(b.active);
                    bullet
                })
                .collect();
            let header = reference_header
                .map(|r| crate::cell::EmbeddedReference::new(r.into()));
            CellKind::Outline(OutlineCell::from_bullets_with_header(
                typeface.clone(),
                bullets,
                header,
            ))
        }
        CellBody::PopPop { text, links } => {
            let mut pc = PopPopCell::new(typeface.clone());
            pc.textbox_mut().replace_text(text);
            for l in links {
                pc.textbox_mut().add_link(l.start..l.end, l.url);
            }
            CellKind::PopPop(pc)
        }
        CellBody::Table { rows: _, cols: _, cells } => {
            // `rows`/`cols` are advisory; trust the actual `cells` shape so
            // a hand-edited or migrated row that disagrees still loads.
            // Build (text, links, tags, readonly) tuples for TableCell.
            let entries: Vec<Vec<(String, Vec<(std::ops::Range<usize>, String)>, Vec<std::ops::Range<usize>>, bool)>> =
                cells
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|e| {
                                let links: Vec<(std::ops::Range<usize>, String)> = e
                                    .links
                                    .into_iter()
                                    .map(|l| (l.start..l.end, l.url))
                                    .collect();
                                let tags: Vec<std::ops::Range<usize>> = e
                                    .tags
                                    .into_iter()
                                    .map(|t| t.start..t.end)
                                    .collect();
                                (e.text, links, tags, e.readonly)
                            })
                            .collect()
                    })
                    .collect();
            CellKind::Table(TableCell::from_records_with_tags(typeface.clone(), entries))
        }
        CellBody::Reference { target } => {
            CellKind::Reference(ReferenceCell::new(typeface.clone(), target.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Bullet, Cell, OutlineCell, ReferenceTarget, TableCell, now_epoch_ms};
    use skia_safe::FontMgr;

    fn typeface() -> Typeface {
        FontMgr::new()
            .new_from_data(
                include_bytes!("../resources/fonts/Figtree.ttf"),
                None,
            )
            .expect("font loads")
    }

    /// Send a `Cell` through the persistence pipeline (build PersistedCell,
    /// serialize to JSON, deserialize, reconstruct Cell) and return the
    /// reborn cell. Mirrors what `Db::load_cells` does in production.
    fn round_trip(cell: &Cell, typeface: &Typeface) -> Cell {
        let pc = persisted_cell_from(cell);
        let json = serde_json::to_string(&pc).expect("serialize");
        let pc: PersistedCell = serde_json::from_str(&json).expect("deserialize");
        let title = pc.title.map(|t| {
            let mut tb = TextBox::new(typeface.clone(), t.text);
            tb.set_force_heading(true);
            for l in t.links {
                tb.add_link(l.start..l.end, l.url);
            }
            if t.tags.is_empty() {
                tb.migrate_tags_from_text();
            } else {
                for tag in t.tags {
                    tb.add_tag(tag.start..tag.end);
                }
            }
            tb
        });
        let active = pc.active;
        let kind = body_to_kind(pc.body, typeface);
        Cell::from_parts(
            cell.id,
            kind,
            title,
            cell.timestamp,
            cell.edited_at,
            cell.context_hint_id,
            active,
        )
    }

    #[test]
    fn round_trip_plain_cell_preserves_text_and_links() {
        let tf = typeface();
        let mut cell = Cell::new(tf.clone(), "hello LINK world".to_string());
        cell.add_link_to_first(6..10, "https://example.com/a".to_string());
        let back = round_trip(&cell, &tf);
        match (&cell.kind, &back.kind) {
            (CellKind::Plain(orig), CellKind::Plain(reborn)) => {
                let (o, r) = (orig.body(), reborn.body());
                assert_eq!(o.text(), r.text());
                assert_eq!(o.links().len(), r.links().len(), "link count");
                let (ol, rl) = (&o.links()[0], &r.links()[0]);
                assert_eq!(ol.range, rl.range);
                assert_eq!(ol.url, rl.url);
            }
            _ => panic!("variant must round-trip as Plain"),
        }
        assert_eq!(cell.id, back.id);
        assert_eq!(cell.timestamp, back.timestamp);
    }

    #[test]
    fn round_trip_outline_preserves_bullets_depths_and_links() {
        let tf = typeface();
        let mut cell = Cell::new_outline(tf.clone());
        // Mutate the lone seed bullet via OutlineCell directly to set up a
        // known shape: text + a link.
        if let CellKind::Outline(oc) = &mut cell.kind {
            // Grab the seed bullet's id, then build a richer set replacing it.
            let seed_id = oc.bullets()[0].id();
            let mut tb1 = TextBox::new(tf.clone(), "root LINK here".to_string());
            tb1.add_link(5..9, "https://example.com/r".to_string());
            let b1 = Bullet::new(seed_id, tb1, 0);
            let b2 = Bullet::new(
                Uuid::now_v7(),
                TextBox::new(tf.clone(), "child".to_string()),
                1,
            );
            let b3 = Bullet::new(
                Uuid::now_v7(),
                TextBox::new(tf.clone(), "grandchild".to_string()),
                2,
            );
            *oc = OutlineCell::from_bullets(tf.clone(), vec![b1, b2, b3]);
        }
        let back = round_trip(&cell, &tf);
        match (&cell.kind, &back.kind) {
            (CellKind::Outline(orig), CellKind::Outline(reborn)) => {
                assert_eq!(orig.bullets().len(), reborn.bullets().len());
                for (o, r) in orig.bullets().iter().zip(reborn.bullets().iter()) {
                    assert_eq!(o.id(), r.id());
                    assert_eq!(o.depth(), r.depth());
                    assert_eq!(o.textbox().text(), r.textbox().text());
                    assert_eq!(o.textbox().links().len(), r.textbox().links().len());
                }
                // Specifically: the link on bullet 0 survived.
                let r_links = reborn.bullets()[0].textbox().links();
                assert_eq!(r_links.len(), 1);
                assert_eq!(r_links[0].range, 5..9);
                assert_eq!(r_links[0].url, "https://example.com/r");
            }
            _ => panic!("variant must round-trip as Outline"),
        }
    }

    #[test]
    fn round_trip_poppop_preserves_input_and_links() {
        let tf = typeface();
        let mut cell = Cell::new_poppop(tf.clone());
        if let CellKind::PopPop(pc) = &mut cell.kind {
            pc.textbox_mut().replace_text("2 + 3\nx LINK".to_string());
            pc.textbox_mut().add_link(8..12, "https://example.com/p".to_string());
        }
        let back = round_trip(&cell, &tf);
        match (&cell.kind, &back.kind) {
            (CellKind::PopPop(orig), CellKind::PopPop(reborn)) => {
                assert_eq!(orig.textbox().text(), reborn.textbox().text());
                assert_eq!(orig.textbox().links().len(), reborn.textbox().links().len());
                let (o, r) = (&orig.textbox().links()[0], &reborn.textbox().links()[0]);
                assert_eq!(o.range, r.range);
                assert_eq!(o.url, r.url);
            }
            _ => panic!("variant must round-trip as PopPop"),
        }
    }

    #[test]
    fn round_trip_table_preserves_grid_and_readonly_flags() {
        let tf = typeface();
        // Build a 2×3 table with mixed text + a readonly cell + a link.
        let triples: Vec<Vec<(String, Vec<(std::ops::Range<usize>, String)>, bool)>> = vec![
            vec![
                ("h1".to_string(), Vec::new(), true),
                ("h2".to_string(), Vec::new(), true),
                ("h3".to_string(), Vec::new(), true),
            ],
            vec![
                (
                    "a LINK".to_string(),
                    vec![(2..6, "https://example.com/t".to_string())],
                    false,
                ),
                ("b".to_string(), Vec::new(), false),
                ("c".to_string(), Vec::new(), false),
            ],
        ];
        let table = TableCell::from_records(tf.clone(), triples);
        let cell = Cell::from_parts(
            Uuid::now_v7(),
            CellKind::Table(table),
            None,
            now_epoch_ms(),
            now_epoch_ms(),
            None,
            true,
        );
        let back = round_trip(&cell, &tf);
        match (&cell.kind, &back.kind) {
            (CellKind::Table(orig), CellKind::Table(reborn)) => {
                assert_eq!(orig.rows(), reborn.rows());
                assert_eq!(orig.cols(), reborn.cols());
                let orig_rows = orig.rows_view();
                let reborn_rows = reborn.rows_view();
                for (o_row, r_row) in orig_rows.iter().zip(reborn_rows.iter()) {
                    for (o, r) in o_row.iter().zip(r_row.iter()) {
                        assert_eq!(o.textbox.text(), r.textbox.text());
                        assert_eq!(o.readonly, r.readonly);
                        assert_eq!(o.textbox.links().len(), r.textbox.links().len());
                    }
                }
                // Link on (1,0) survived.
                let r_links = reborn_rows[1][0].textbox.links();
                assert_eq!(r_links.len(), 1);
                assert_eq!(r_links[0].range, 2..6);
                assert_eq!(r_links[0].url, "https://example.com/t");
            }
            _ => panic!("variant must round-trip as Table"),
        }
    }

    #[test]
    fn round_trip_reference_cell_preserves_target() {
        let tf = typeface();
        let target_id = Uuid::now_v7();
        let cell = Cell::new_reference(tf.clone(), ReferenceTarget::WholeCell(target_id));
        let back = round_trip(&cell, &tf);
        match &back.kind {
            CellKind::Reference(rc) => match rc.target() {
                ReferenceTarget::WholeCell(id) => assert_eq!(id, target_id),
                _ => panic!("WholeCell target lost"),
            },
            _ => panic!("variant must round-trip as Reference"),
        }
    }

    #[test]
    fn round_trip_envelope_outline_preserves_header_and_bullets() {
        let tf = typeface();
        let target_id = Uuid::now_v7();
        let mut oc = OutlineCell::with_envelope(
            tf.clone(),
            ReferenceTarget::WholeCell(target_id),
        );
        // Type something into the seed bullet so we can verify the
        // bullet body round-trips alongside the header.
        let bullet_id = oc.bullets()[0].id();
        oc.replace_in_bullet_with_text(bullet_id, 0..0, "my note".to_string());

        let cell = Cell::from_parts(
            Uuid::now_v7(),
            CellKind::Outline(oc),
            None,
            now_epoch_ms(),
            now_epoch_ms(),
            None,
            true,
        );
        let back = round_trip(&cell, &tf);
        match &back.kind {
            CellKind::Outline(reborn) => {
                let header = reborn
                    .reference_header()
                    .expect("header survives round-trip");
                match header.target() {
                    ReferenceTarget::WholeCell(id) => assert_eq!(id, target_id),
                    _ => panic!("WholeCell target lost"),
                }
                assert_eq!(reborn.bullets().len(), 1);
                assert_eq!(reborn.bullets()[0].textbox().text(), "my note");
            }
            _ => panic!("variant must round-trip as Outline"),
        }
    }

    #[test]
    fn cell_active_round_trips_through_persistence() {
        // Mark a cell inactive, run it through serialize / deserialize,
        // and assert the flag survives. Mirrors the snapshot test but
        // exercises the JSON path (PersistedCell.active +
        // body-to-kind) the on-disk DB uses.
        let tf = typeface();
        let mut cell = Cell::new(tf.clone(), "archived note".to_string());
        cell.active = false;
        let back = round_trip(&cell, &tf);
        assert!(!back.active, "active=false survives round-trip");
    }

    #[test]
    fn legacy_persisted_cell_without_active_loads_as_active() {
        // JSON shape from before this feature — no `active` field on
        // PersistedCell. serde-default = "default_true" so legacy data
        // deserializes as active. Without that the upgrade path would
        // mark every existing cell archived on first load.
        let json = r##"{"kind":"plain","text":"legacy","links":[],"tags":[]}"##;
        let pc: PersistedCell = serde_json::from_str(json).expect("legacy plain parses");
        assert!(pc.active, "missing field defaults to true (active)");
    }

    #[test]
    fn legacy_block_record_without_active_loads_as_active() {
        // BlockRecord (outline bullet) has the same default-true
        // shape. Legacy outlines saved before this feature should
        // load with every bullet active.
        let json = r##"{"kind":"outline","blocks":[{"id":"00000000-0000-7000-8000-000000000001","depth":0,"text":"legacy bullet","links":[],"tags":[]}]}"##;
        let body: CellBody = serde_json::from_str(json).expect("legacy outline parses");
        let kind = body_to_kind(body, &typeface());
        match kind {
            CellKind::Outline(oc) => {
                assert_eq!(oc.bullets().len(), 1);
                assert!(
                    oc.bullets()[0].active(),
                    "legacy bullet defaults to active"
                );
            }
            _ => panic!("expected Outline kind"),
        }
    }

    #[test]
    fn bullet_active_round_trips_through_persistence() {
        let tf = typeface();
        let mut oc = OutlineCell::new(tf.clone());
        let bid = oc.bullets()[0].id();
        oc.set_bullet_active(bid, false);
        let cell = Cell::from_parts(
            Uuid::now_v7(),
            CellKind::Outline(oc),
            None,
            now_epoch_ms(),
            now_epoch_ms(),
            None,
            true,
        );
        let back = round_trip(&cell, &tf);
        match &back.kind {
            CellKind::Outline(oc) => {
                assert!(
                    !oc.bullets()[0].active(),
                    "bullet active flag survives round-trip"
                );
            }
            _ => panic!("variant lost"),
        }
    }

    #[test]
    fn legacy_outline_without_header_field_loads_with_no_header() {
        // JSON shape from before reference_header existed — only
        // `kind` and `blocks`. serde-default makes the new field
        // None on load, so old DBs upgrade cleanly.
        let json = r##"{"kind":"outline","blocks":[{"id":"00000000-0000-7000-8000-000000000001","depth":0,"text":"plain bullet","links":[],"tags":[]}]}"##;
        let body: CellBody = serde_json::from_str(json).expect("legacy outline parses");
        let kind = body_to_kind(body, &typeface());
        match kind {
            CellKind::Outline(oc) => {
                assert!(oc.reference_header().is_none());
                assert_eq!(oc.bullets().len(), 1);
                assert_eq!(oc.bullets()[0].textbox().text(), "plain bullet");
            }
            _ => panic!("expected Outline kind"),
        }
    }

    #[test]
    fn round_trip_title_preserves_text_and_tags() {
        // Title round-trip with trailing tags. The title TextBox is rebuilt
        // with force_heading=true on load; both the text and any inline
        // links survive the cycle. Tag spans round-trip via the new
        // `tags` field; this fixture seeds the spans through the
        // legacy text-parse migration.
        let tf = typeface();
        let mut cell = Cell::new(tf.clone(), "body".to_string());
        cell.set_title(Some({
            let mut tb = TextBox::new(tf.clone(), "Project Notes #urgent #planning".to_string());
            tb.set_force_heading(true);
            tb.add_link(0..7, "https://example.com/h".to_string());
            tb.migrate_tags_from_text();
            tb
        }));
        let back = round_trip(&cell, &tf);
        let orig_title = cell.title().expect("orig has title");
        let reborn_title = back.title().expect("title survives round-trip");
        assert_eq!(orig_title.text(), reborn_title.text());
        assert_eq!(orig_title.links().len(), reborn_title.links().len());
        let (o, r) = (&orig_title.links()[0], &reborn_title.links()[0]);
        assert_eq!(o.range, r.range);
        assert_eq!(o.url, r.url);
        // Tags are parsed off the title text by heading_tag_names.
        assert_eq!(
            back.heading_tag_names(),
            vec!["urgent".to_string(), "planning".to_string()]
        );
    }

    #[test]
    fn reference_cell_round_trips_through_json() {
        // Whole-cell embed.
        let cid = Uuid::now_v7();
        let body = CellBody::Reference {
            target: ReferenceTargetRecord::Cell { cell_id: cid },
        };
        let json = serde_json::to_string(&body).unwrap();
        let parsed: CellBody = serde_json::from_str(&json).unwrap();
        match parsed {
            CellBody::Reference {
                target: ReferenceTargetRecord::Cell { cell_id },
            } => assert_eq!(cell_id, cid),
            _ => panic!("whole-cell reference must round-trip"),
        }

        // Sub-tree embed.
        let bid = Uuid::now_v7();
        let body = CellBody::Reference {
            target: ReferenceTargetRecord::Subtree {
                cell_id: cid,
                bullet_id: bid,
            },
        };
        let json = serde_json::to_string(&body).unwrap();
        let parsed: CellBody = serde_json::from_str(&json).unwrap();
        match parsed {
            CellBody::Reference {
                target: ReferenceTargetRecord::Subtree { cell_id, bullet_id },
            } => {
                assert_eq!(cell_id, cid);
                assert_eq!(bullet_id, bid);
            }
            _ => panic!("subtree reference must round-trip"),
        }
    }

    #[test]
    fn migration_extracts_plain_heading_into_title() {
        // Legacy v3 row: bare CellBody, body opens with `# Title #urgent`.
        let json = r##"{"kind":"plain","text":"# My Notes #urgent\nbody line","links":[]}"##;
        let updated = extract_title_from_body_json(json).expect("should rewrite");
        let pc: PersistedCell = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            pc.title.as_ref().map(|t| t.text.as_str()),
            Some("My Notes #urgent")
        );
        match &pc.body {
            CellBody::Plain { text, .. } => assert_eq!(text, "body line"),
            _ => panic!("kind survives migration"),
        }
        // The migration helper produces a TitleRecord with empty tag
        // spans; the v7 backfill (or `tag_names_from_persisted_legacy`)
        // is responsible for surfacing the trailing-text tags.
        assert_eq!(
            tag_names_from_persisted_legacy(&pc),
            vec!["urgent".to_string()],
        );
    }

    #[test]
    fn migration_skips_cells_without_heading_prefix() {
        let json = r##"{"kind":"plain","text":"no heading here","links":[]}"##;
        assert!(extract_title_from_body_json(json).is_none());
    }

    #[test]
    fn migration_is_idempotent_on_already_migrated_rows() {
        let json =
            r##"{"title":{"text":"Foo","links":[]},"kind":"plain","text":"body","links":[]}"##;
        assert!(extract_title_from_body_json(json).is_none());
    }

    #[test]
    fn migration_extracts_outline_first_bullet() {
        let json = r##"{"kind":"outline","blocks":[{"id":"00000000-0000-7000-8000-000000000001","depth":0,"text":"# Topic #person","links":[]},{"id":"00000000-0000-7000-8000-000000000002","depth":0,"text":"first child","links":[]}]}"##;
        let updated = extract_title_from_body_json(json).expect("should rewrite");
        let pc: PersistedCell = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            pc.title.as_ref().map(|t| t.text.as_str()),
            Some("Topic #person")
        );
        match &pc.body {
            CellBody::Outline { blocks, .. } => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, "first child");
            }
            _ => panic!("kind survives migration"),
        }
        assert_eq!(
            tag_names_from_persisted_legacy(&pc),
            vec!["person".to_string()],
        );
    }
}
