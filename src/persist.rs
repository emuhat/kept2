use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use skia_safe::Typeface;
use uuid::Uuid;

use crate::cell::{
    parse_inline_tags, Bullet, Cell, CellKind, OutlineCell, PlainCell, PopPopCell,
    ReferenceCell, ReferenceTarget, TableCell, TextBox,
};
use crate::thread::{Thread, ThreadId, ThreadMembership};

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
        if version < 8 {
            // v7 → v8: scratch flag for transient notes. Existing
            // rows default to 0 (regular cells); the Scratch view
            // is populated only by new writes from Cmd+S → Ctrl+O.
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE cells ADD COLUMN scratch INTEGER NOT NULL DEFAULT 0;
                 PRAGMA user_version = 8;
                 COMMIT;",
            )?;
        }
        if version < 9 {
            // v8 → v9: manual threads. `thread_memberships` uses
            // `bullet_id IS NULL` to mean "whole cell" (mirroring the
            // `ReferenceTarget::{WholeCell, Subtree}` shape — see
            // `src/cell/reference.rs`). Cell-delete cascades through
            // FK; bullet deletes don't reach SQL so dangling rows are
            // expected and rendered as missing-reference placeholders.
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE threads (
                     id          BLOB PRIMARY KEY,
                     title       TEXT NOT NULL,
                     created_at  INTEGER NOT NULL,
                     edited_at   INTEGER NOT NULL,
                     closed_at   INTEGER
                 );
                 CREATE TABLE thread_memberships (
                     thread_id   BLOB NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                     cell_id     BLOB NOT NULL REFERENCES cells(id)   ON DELETE CASCADE,
                     bullet_id   BLOB,
                     attached_at INTEGER NOT NULL
                 );
                 CREATE UNIQUE INDEX thread_memberships_unique
                     ON thread_memberships (thread_id, cell_id, IFNULL(bullet_id, x''));
                 CREATE INDEX thread_memberships_by_cell
                     ON thread_memberships (cell_id);
                 PRAGMA user_version = 9;
                 COMMIT;",
            )?;
        }
        if version < 10 {
            // v9 → v10: Inbox flag. Existing cells default to 0 (not in inbox).
            // Yeet and snooze-expiry set it to 1 at runtime going forward.
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE cells ADD COLUMN inbox INTEGER NOT NULL DEFAULT 0;
                 PRAGMA user_version = 10;
                 COMMIT;",
            )?;
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
            "SELECT id, timestamp, body, edited_at, context_hint_id, scratch, inbox \
             FROM cells ORDER BY timestamp ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let timestamp: i64 = row.get(1)?;
                let body: String = row.get(2)?;
                let edited_at: i64 = row.get(3)?;
                let hint_bytes: Option<Vec<u8>> = row.get(4)?;
                let scratch: i64 = row.get(5)?;
                let inbox: i64 = row.get(6)?;
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
                Ok((id, timestamp, body, edited_at, hint, scratch != 0, inbox != 0))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut cells = Vec::with_capacity(rows.len());
        for (id, timestamp, body_json, edited_at, hint, scratch, inbox) in rows {
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
            // Back-compat: legacy rows store an `active` bool. New
            // rows store `closed_at`. Prefer the explicit timestamp;
            // fall back to "close happened at edited_at" for legacy
            // archived rows so the imprecise migration still produces
            // a sensible timestamp.
            let closed_at = pc.closed_at.or_else(|| {
                if !pc.active { Some(edited_at) } else { None }
            });
            let resurface_after = pc.resurface_after;
            let kind = body_to_kind(pc.body, typeface);
            cells.push(Cell::from_parts(
                id, kind, title, timestamp, edited_at, hint, closed_at, resurface_after, scratch,
                inbox,
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
            "INSERT INTO cells (id, timestamp, body, edited_at, context_hint_id, scratch, inbox) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(id) DO UPDATE SET \
                 timestamp = excluded.timestamp, \
                 body = excluded.body, \
                 edited_at = excluded.edited_at, \
                 context_hint_id = excluded.context_hint_id, \
                 scratch = excluded.scratch, \
                 inbox = excluded.inbox",
            params![
                cell_id_bytes,
                cell.timestamp,
                body_json,
                cell.edited_at,
                hint_bytes,
                if cell.scratch { 1_i64 } else { 0_i64 },
                if cell.inbox { 1_i64 } else { 0_i64 },
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

    /// Delete every scratch cell whose `timestamp` is older than
    /// `expire_before_ms`, returning the ids that were removed so
    /// the in-memory `Document` can drop them too. Used by the
    /// 10-day Scratch TTL pruner (run at startup and once per
    /// session-day). `cell_tags` rows cascade via the FK.
    pub fn prune_expired_scratch(
        &mut self,
        expire_before_ms: i64,
    ) -> rusqlite::Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM cells WHERE scratch = 1 AND timestamp < ?1",
        )?;
        let ids: Vec<Uuid> = stmt
            .query_map(params![expire_before_ms], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                Uuid::from_slice(&id_bytes).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        if ids.is_empty() {
            return Ok(ids);
        }
        self.conn.execute(
            "DELETE FROM cells WHERE scratch = 1 AND timestamp < ?1",
            params![expire_before_ms],
        )?;
        Ok(ids)
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

    // ----- threads -----------------------------------------------------

    /// Every thread known to the DB. Caller filters by `closed_at` when
    /// it wants the open-only view (the "Show inactive" toggle pattern).
    /// Ordered by `created_at ASC` so list views are stable across loads.
    pub fn all_threads(&self) -> rusqlite::Result<Vec<Thread>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, created_at, edited_at, closed_at \
             FROM threads ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                Ok(Thread {
                    id: Uuid::from_slice(&id_bytes).unwrap_or_else(|_| Uuid::nil()),
                    title: row.get(1)?,
                    created_at: row.get(2)?,
                    edited_at: row.get(3)?,
                    closed_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    #[allow(dead_code)]
    pub fn thread_by_id(&self, id: ThreadId) -> rusqlite::Result<Option<Thread>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, created_at, edited_at, closed_at \
             FROM threads WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id.as_bytes().to_vec()], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                Ok(Thread {
                    id: Uuid::from_slice(&id_bytes).unwrap_or_else(|_| Uuid::nil()),
                    title: row.get(1)?,
                    created_at: row.get(2)?,
                    edited_at: row.get(3)?,
                    closed_at: row.get(4)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Insert-or-replace a thread row. Caller is responsible for bumping
    /// `edited_at` when the thread's user-visible state changes (title,
    /// close/reopen). We don't auto-touch it on every save — `closed_at`
    /// transitions, for example, want the user's commit timestamp.
    pub fn upsert_thread(&mut self, thread: &Thread) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO threads (id, title, created_at, edited_at, closed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(id) DO UPDATE SET \
                 title = excluded.title, \
                 edited_at = excluded.edited_at, \
                 closed_at = excluded.closed_at",
            params![
                thread.id.as_bytes().to_vec(),
                thread.title,
                thread.created_at,
                thread.edited_at,
                thread.closed_at,
            ],
        )?;
        Ok(())
    }

    pub fn delete_thread(&mut self, id: ThreadId) -> rusqlite::Result<()> {
        // FK cascade drops thread_memberships rows for us.
        self.conn.execute(
            "DELETE FROM threads WHERE id = ?1",
            params![id.as_bytes().to_vec()],
        )?;
        Ok(())
    }

    pub fn set_thread_closed_at(
        &mut self,
        id: ThreadId,
        closed_at: Option<i64>,
        edited_at: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE threads SET closed_at = ?2, edited_at = ?3 WHERE id = ?1",
            params![id.as_bytes().to_vec(), closed_at, edited_at],
        )?;
        Ok(())
    }

    /// Every membership in the DB. Small enough to load into memory once
    /// at startup (and after each mutation) the same way entities are.
    pub fn all_thread_memberships(&self) -> rusqlite::Result<Vec<ThreadMembership>> {
        let mut stmt = self.conn.prepare(
            "SELECT thread_id, cell_id, bullet_id, attached_at \
             FROM thread_memberships ORDER BY attached_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let thread_bytes: Vec<u8> = row.get(0)?;
                let cell_bytes: Vec<u8> = row.get(1)?;
                let bullet_bytes: Option<Vec<u8>> = row.get(2)?;
                let attached_at: i64 = row.get(3)?;
                let cell_id = Uuid::from_slice(&cell_bytes).unwrap_or_else(|_| Uuid::nil());
                let target = match bullet_bytes
                    .and_then(|b| Uuid::from_slice(&b).ok())
                {
                    Some(bullet_id) => ReferenceTarget::Subtree { cell_id, bullet_id },
                    None => ReferenceTarget::WholeCell(cell_id),
                };
                Ok(ThreadMembership {
                    thread_id: Uuid::from_slice(&thread_bytes)
                        .unwrap_or_else(|_| Uuid::nil()),
                    target,
                    attached_at,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Attach a target to a thread. Idempotent — duplicate attach is a
    /// no-op (the unique index covers `(thread_id, cell_id, bullet_id)`
    /// with `NULL` collapsed to an empty-blob sentinel).
    pub fn attach_thread(
        &mut self,
        thread_id: ThreadId,
        target: ReferenceTarget,
        attached_at: i64,
    ) -> rusqlite::Result<()> {
        let (cell_id, bullet_id) = split_target(target);
        self.conn.execute(
            "INSERT OR IGNORE INTO thread_memberships \
                (thread_id, cell_id, bullet_id, attached_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                thread_id.as_bytes().to_vec(),
                cell_id.as_bytes().to_vec(),
                bullet_id.map(|id| id.as_bytes().to_vec()),
                attached_at,
            ],
        )?;
        Ok(())
    }

    /// Drop any Subtree memberships at this cell whose bullet_id is
    /// NOT in `valid_bullet_ids`. Called from the persistence flush
    /// after a cell save to sweep ghosts left by hard-deletes
    /// (bullet selection → Delete), cut-without-paste, or any path
    /// where a bullet disappears without an explicit transfer.
    /// Whole-cell memberships are unaffected — the cells FK cascade
    /// already keeps those consistent. No-op when the cell has no
    /// subtree memberships.
    pub fn reconcile_subtree_memberships(
        &mut self,
        cell_id: Uuid,
        valid_bullet_ids: &[Uuid],
    ) -> rusqlite::Result<usize> {
        // Pull every subtree row at this cell. Filter in memory so
        // we don't have to build a SQL IN-list for the bullet ids
        // (the list is small in practice and this runs once per
        // cell save).
        let cell_bytes = cell_id.as_bytes().to_vec();
        let valid: std::collections::HashSet<Uuid> =
            valid_bullet_ids.iter().copied().collect();
        let stale: Vec<Vec<u8>> = {
            let mut stmt = self.conn.prepare(
                "SELECT bullet_id FROM thread_memberships \
                 WHERE cell_id = ?1 AND bullet_id IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![cell_bytes.clone()], |row| {
                let b: Vec<u8> = row.get(0)?;
                Ok(b)
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|bytes| {
                    Uuid::from_slice(bytes)
                        .map(|u| !valid.contains(&u))
                        .unwrap_or(true)
                })
                .collect()
        };
        if stale.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut removed = 0usize;
        for bullet_bytes in &stale {
            removed += tx.execute(
                "DELETE FROM thread_memberships \
                 WHERE cell_id = ?1 AND bullet_id = ?2",
                params![cell_bytes, bullet_bytes],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Replace every membership at `(cell_id, bullet_id)` with the
    /// given `(thread_id, attached_at)` list. Used by the undo of a
    /// bullet merge to restore a snapshotted pre-merge state. Empty
    /// `memberships` simply detaches the bullet from every thread.
    pub fn set_bullet_memberships(
        &mut self,
        cell_id: Uuid,
        bullet_id: Uuid,
        memberships: &[(ThreadId, i64)],
    ) -> rusqlite::Result<()> {
        let cell_bytes = cell_id.as_bytes().to_vec();
        let bullet_bytes = bullet_id.as_bytes().to_vec();
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM thread_memberships \
             WHERE cell_id = ?1 AND bullet_id = ?2",
            params![cell_bytes, bullet_bytes],
        )?;
        for (thread_id, attached_at) in memberships {
            tx.execute(
                "INSERT OR IGNORE INTO thread_memberships \
                    (thread_id, cell_id, bullet_id, attached_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    thread_id.as_bytes().to_vec(),
                    cell_bytes,
                    bullet_bytes,
                    attached_at,
                ],
            )?;
        }
        tx.commit()
    }

    /// Move every membership pointing at `(cell_id, from_bullet)`
    /// to `(cell_id, into_bullet)`. Used when two bullets are
    /// merged in the editor — without this, threads attached to
    /// the disappearing bullet silently orphan because the unique
    /// index keys on `bullet_id`. Idempotent w.r.t. dedup: if a
    /// thread already attaches the `into` bullet, the `from`
    /// duplicate is dropped instead of double-inserting.
    pub fn move_bullet_memberships(
        &mut self,
        cell_id: Uuid,
        from_bullet: Uuid,
        into_bullet: Uuid,
    ) -> rusqlite::Result<()> {
        if from_bullet == into_bullet {
            return Ok(());
        }
        let cell_bytes = cell_id.as_bytes().to_vec();
        let from_bytes = from_bullet.as_bytes().to_vec();
        let into_bytes = into_bullet.as_bytes().to_vec();
        let tx = self.conn.transaction()?;
        // Idempotent transfer: ensure each thread that touches the
        // from-bullet also touches the into-bullet, then drop the
        // from-side rows in bulk.
        tx.execute(
            "INSERT OR IGNORE INTO thread_memberships \
                (thread_id, cell_id, bullet_id, attached_at) \
             SELECT thread_id, cell_id, ?3, attached_at \
             FROM thread_memberships \
             WHERE cell_id = ?1 AND bullet_id = ?2",
            params![cell_bytes, from_bytes, into_bytes],
        )?;
        tx.execute(
            "DELETE FROM thread_memberships \
             WHERE cell_id = ?1 AND bullet_id = ?2",
            params![cell_bytes, from_bytes],
        )?;
        tx.commit()
    }

    pub fn detach_thread(
        &mut self,
        thread_id: ThreadId,
        target: ReferenceTarget,
    ) -> rusqlite::Result<()> {
        let (cell_id, bullet_id) = split_target(target);
        match bullet_id {
            Some(bid) => self.conn.execute(
                "DELETE FROM thread_memberships \
                 WHERE thread_id = ?1 AND cell_id = ?2 AND bullet_id = ?3",
                params![
                    thread_id.as_bytes().to_vec(),
                    cell_id.as_bytes().to_vec(),
                    bid.as_bytes().to_vec(),
                ],
            )?,
            None => self.conn.execute(
                "DELETE FROM thread_memberships \
                 WHERE thread_id = ?1 AND cell_id = ?2 AND bullet_id IS NULL",
                params![
                    thread_id.as_bytes().to_vec(),
                    cell_id.as_bytes().to_vec(),
                ],
            )?,
        };
        Ok(())
    }
}

fn split_target(target: ReferenceTarget) -> (Uuid, Option<Uuid>) {
    match target {
        ReferenceTarget::WholeCell(cell_id) => (cell_id, None),
        ReferenceTarget::Subtree { cell_id, bullet_id } => (cell_id, Some(bullet_id)),
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
    /// LEGACY archived flag. Reads only — newer code stores the
    /// equivalent state in `closed_at`. We accept old JSON via the
    /// default and we still serialize `true` (the common case) with
    /// skip-if-default so untouched data stays byte-identical. When
    /// `closed_at.is_some()` we deliberately write `active = false`
    /// so a downgrade still hides the cell in views.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    active: bool,
    /// `Some(t)` when the cell was closed at epoch ms `t`; `None`
    /// when open. Backfilled on load from legacy `active=false`
    /// rows (using `edited_at` as the imprecise close time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed_at: Option<i64>,
    /// `Some(t)` schedules the cell to appear in the Current view
    /// at epoch ms `t`. Absent for non-snoozed cells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resurface_after: Option<i64>,
    /// Inbox membership. Default false (absent from JSON for older rows).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    inbox: bool,
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
    /// Legacy field: `#tag` spans within the title. Pre-unification
    /// data stored these as a separate vector. New writes leave it
    /// empty — committed tags are emitted as `kept-tag://` link spans
    /// in `links` instead. Still read on load (and translated back
    /// into kept-tag link spans) so older databases keep working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<TagRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum CellBody {
    Plain {
        text: String,
        #[serde(default)]
        links: Vec<LinkRecord>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    /// LEGACY per-bullet archived flag. Same back-compat pattern as
    /// `PersistedCell::active` — accepted on read, derived from
    /// `closed_at` on write for downgrade-safety.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    active: bool,
    /// `Some(t)` when the bullet was closed at epoch ms `t`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed_at: Option<i64>,
    /// `Some(t)` schedules the bullet to appear in the Current view
    /// at epoch ms `t`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resurface_after: Option<i64>,
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
        // Tags now live as `kept-tag://` link spans inside `links` —
        // see `cell::common::KEPT_TAG_SCHEME`. The legacy `tags` field
        // stays in the schema for older readers but is always empty
        // on new writes (and is `skip_serializing_if = "Vec::is_empty"`,
        // so it drops out of the JSON entirely).
        tags: Vec::new(),
    });
    PersistedCell {
        title,
        // Derive the legacy bool from the new `closed_at` field so a
        // downgrade still treats closed cells as archived.
        active: cell.is_open(),
        closed_at: cell.closed_at,
        resurface_after: cell.resurface_after,
        inbox: cell.inbox,
        body: cell_to_body(cell),
    }
}

fn cell_to_body(cell: &Cell) -> CellBody {
    // Tags travel through `links` (URL `kept-tag://<name>`) since the
    // tag-unification refactor. We still emit an empty `tags` field
    // so the JSON schema stays the same shape; with
    // `skip_serializing_if = "Vec::is_empty"` it drops out of the
    // written JSON.
    let tag_records = |_tb: &TextBox| -> Vec<TagRecord> { Vec::new() };
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
                    active: b.is_open(),
                    closed_at: b.closed_at(),
                    resurface_after: b.resurface_after(),
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
            closed_at: None,
            resurface_after: None,
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
/// Pull tag names out of a record's link list — any LinkRecord whose
/// URL is `kept-tag://<name>` contributes its `<name>`. Empty tags
/// and zero-length ranges drop. This is the post-unification path:
/// committed tag spans share storage with regular link spans.
fn names_from_link_records(text: &str, links: &[LinkRecord], sink: &mut dyn FnMut(String)) {
    for l in links {
        if l.end <= l.start + 1 || l.end > text.len() {
            continue;
        }
        if let Some(name) = l.url.strip_prefix(crate::cell::KEPT_TAG_SCHEME) {
            if !name.is_empty() {
                sink(name.to_string());
            }
        }
    }
}

/// Legacy `TagRecord` list reader. Pre-unification data carried tags
/// as a separate vector. New writes leave `tags` empty (committed
/// tags live in `links` as `kept-tag://` URLs), so this path only
/// fires when reading older databases.
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
/// Reads kept-tag `LinkRecord` URLs (the post-unification path) and
/// also any legacy `TagRecord` entries (so newly-saved cells whose
/// JSON happened to still carry a non-empty `tags` field from an
/// in-flight upgrade still index correctly).
fn tag_names_from_persisted(pc: &PersistedCell) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    };
    if let Some(t) = pc.title.as_ref() {
        names_from_link_records(&t.text, &t.links, &mut |n| push(n));
        names_from_tag_ranges(&t.text, &t.tags, &mut |n| push(n));
    }
    match &pc.body {
        CellBody::Plain { text, links, tags } => {
            names_from_link_records(text, links, &mut |n| push(n));
            names_from_tag_ranges(text, tags, &mut |n| push(n));
        }
        CellBody::Outline { blocks, .. } => {
            for b in blocks {
                names_from_link_records(&b.text, &b.links, &mut |n| push(n));
                names_from_tag_ranges(&b.text, &b.tags, &mut |n| push(n));
            }
        }
        CellBody::Table { cells, .. } => {
            for row in cells {
                for c in row {
                    names_from_link_records(&c.text, &c.links, &mut |n| push(n));
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
        names_from_link_records(&t.text, &t.links, sink);
        if !t.tags.is_empty() {
            names_from_tag_ranges(&t.text, &t.tags, sink);
        } else if !t.links.iter().any(|l| l.url.starts_with(crate::cell::KEPT_TAG_SCHEME)) {
            let heading_end = t.text.find('\n').unwrap_or(t.text.len());
            for r in parse_trailing_tags(&t.text, heading_end) {
                if r.end > r.start + 1 {
                    sink(t.text[r.start + 1..r.end].to_string());
                }
            }
        }
    };
    let body_names =
        |text: &str, links: &[LinkRecord], tags: &[TagRecord], sink: &mut dyn FnMut(String)| {
            names_from_link_records(text, links, sink);
            if !tags.is_empty() {
                names_from_tag_ranges(text, tags, sink);
            } else if !links.iter().any(|l| l.url.starts_with(crate::cell::KEPT_TAG_SCHEME)) {
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
        CellBody::Plain { text, links, tags } => {
            body_names(text, links, tags, &mut |n| push(n));
        }
        CellBody::Outline { blocks, .. } => {
            for b in blocks {
                body_names(&b.text, &b.links, &b.tags, &mut |n| push(n));
            }
        }
        CellBody::Table { cells, .. } => {
            for row in cells {
                for c in row {
                    body_names(&c.text, &c.links, &c.tags, &mut |n| push(n));
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
                    // Back-compat: same migration shape as PersistedCell.
                    // Prefer explicit `closed_at`; fall back to "close at edited_at"
                    // (or "now-ish 0") for legacy `active=false` bullets. Outline
                    // rows don't carry their own edited_at, so the cell's
                    // edited_at would be ideal — but it's not in scope here.
                    // 0 is acceptable: legacy archived bullets just sort
                    // to the bottom of any future closed-bullet view.
                    let closed_at = b.closed_at.or_else(|| {
                        if !b.active { Some(0) } else { None }
                    });
                    bullet.set_closed_at(closed_at);
                    bullet.set_resurface_after(b.resurface_after);
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
        let closed_at = pc.closed_at.or_else(|| {
            if !pc.active { Some(cell.edited_at) } else { None }
        });
        let resurface_after = pc.resurface_after;
        let inbox = pc.inbox;
        let kind = body_to_kind(pc.body, typeface);
        Cell::from_parts(
            cell.id,
            kind,
            title,
            cell.timestamp,
            cell.edited_at,
            cell.context_hint_id,
            closed_at,
            resurface_after,
            cell.scratch,
            inbox,
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
            None,
            None,
            false,
            false,
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
            None,
            None,
            false,
            false,
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
    fn cell_closed_at_round_trips_through_persistence() {
        // Close a cell, run it through serialize / deserialize, and
        // assert the timestamp survives. Mirrors the snapshot test
        // but exercises the JSON path (PersistedCell.closed_at) the
        // on-disk DB uses.
        let tf = typeface();
        let mut cell = Cell::new(tf.clone(), "archived note".to_string());
        cell.closed_at = Some(987_654);
        let back = round_trip(&cell, &tf);
        assert_eq!(back.closed_at, Some(987_654), "closed_at survives round-trip");
        assert!(!back.is_open());
    }

    #[test]
    fn cell_resurface_after_round_trips_through_persistence() {
        let tf = typeface();
        let mut cell = Cell::new(tf.clone(), "snoozed note".to_string());
        cell.resurface_after = Some(2_000_000);
        let back = round_trip(&cell, &tf);
        assert_eq!(back.resurface_after, Some(2_000_000));
    }

    #[test]
    fn legacy_active_false_backfills_closed_at_to_edited_at() {
        // JSON from before this feature with `active=false` should
        // produce a `closed_at = Some(edited_at)` on load — the
        // imprecise migration explicitly approved at planning time.
        let tf = typeface();
        let mut cell = Cell::new(tf.clone(), "legacy archived".to_string());
        let edited = cell.edited_at;
        // Simulate legacy on-disk form: serialize, then mutate JSON
        // to remove closed_at and force active=false.
        let pc = persisted_cell_from(&cell);
        let mut json: serde_json::Value = serde_json::to_value(&pc).unwrap();
        let obj = json.as_object_mut().unwrap();
        obj.remove("closed_at");
        obj.insert("active".to_string(), serde_json::Value::Bool(false));
        let json_str = serde_json::to_string(&json).unwrap();
        let pc: PersistedCell = serde_json::from_str(&json_str).expect("parses");
        // Replay the load-time backfill from `round_trip`.
        let closed_at = pc.closed_at.or_else(|| {
            if !pc.active { Some(cell.edited_at) } else { None }
        });
        assert_eq!(
            closed_at,
            Some(edited),
            "legacy active=false rows produce closed_at = edited_at"
        );
    }

    #[test]
    fn legacy_persisted_cell_without_active_loads_as_active() {
        // JSON shape from before this feature — no `active` field on
        // PersistedCell. serde-default = "default_true" so legacy data
        // deserializes as active.
        let json = r##"{"kind":"plain","text":"legacy","links":[],"tags":[]}"##;
        let pc: PersistedCell = serde_json::from_str(json).expect("legacy plain parses");
        assert!(pc.active, "missing field defaults to true (active)");
        assert!(pc.closed_at.is_none());
    }

    #[test]
    fn legacy_block_record_without_active_loads_as_active() {
        // BlockRecord (outline bullet) has the same default-true
        // shape. Legacy outlines saved before this feature should
        // load with every bullet open.
        let json = r##"{"kind":"outline","blocks":[{"id":"00000000-0000-7000-8000-000000000001","depth":0,"text":"legacy bullet","links":[],"tags":[]}]}"##;
        let body: CellBody = serde_json::from_str(json).expect("legacy outline parses");
        let kind = body_to_kind(body, &typeface());
        match kind {
            CellKind::Outline(oc) => {
                assert_eq!(oc.bullets().len(), 1);
                assert!(
                    oc.bullets()[0].is_open(),
                    "legacy bullet defaults to open"
                );
            }
            _ => panic!("expected Outline kind"),
        }
    }

    #[test]
    fn bullet_closed_at_round_trips_through_persistence() {
        let tf = typeface();
        let mut oc = OutlineCell::new(tf.clone());
        let bid = oc.bullets()[0].id();
        oc.set_bullet_closed_at(bid, Some(123));
        let cell = Cell::from_parts(
            Uuid::now_v7(),
            CellKind::Outline(oc),
            None,
            now_epoch_ms(),
            now_epoch_ms(),
            None,
            None,
            None,
            false,
            false,
        );
        let back = round_trip(&cell, &tf);
        match &back.kind {
            CellKind::Outline(oc) => {
                assert_eq!(
                    oc.bullets()[0].closed_at(),
                    Some(123),
                    "bullet closed_at survives round-trip"
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

    /// In-memory `Db` for scratch-flag tests. SQLite treats
    /// `:memory:` specially — no on-disk file is created, the DB
    /// lives only for the lifetime of the connection. The migration
    /// chain runs as usual, so the resulting Db has the post-v8
    /// `scratch` column.
    fn open_in_memory_db() -> Db {
        Db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    #[test]
    fn round_trip_scratch_cell_preserves_flag() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let mut cell = Cell::new(tf.clone(), "scratch note".to_string());
        cell.scratch = true;
        let id = cell.id;
        db.save_cell(&cell).expect("save");
        let back = db
            .load_cells(&tf)
            .expect("load")
            .into_iter()
            .find(|c| c.id == id)
            .expect("cell present");
        assert!(back.scratch, "scratch flag survives a save→load round-trip");
    }

    #[test]
    fn round_trip_non_scratch_cell_defaults_to_non_scratch() {
        // Belt-and-suspenders for the column-default → live-flag
        // path: a regular cell saved without explicit scratch
        // loads back with scratch == false.
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "regular".to_string());
        let id = cell.id;
        db.save_cell(&cell).expect("save");
        let back = db
            .load_cells(&tf)
            .expect("load")
            .into_iter()
            .find(|c| c.id == id)
            .expect("cell present");
        assert!(!back.scratch, "regular cells stay non-scratch");
    }

    #[test]
    fn prune_expired_scratch_removes_old_only() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let now = now_epoch_ms();
        let day = 24 * 60 * 60 * 1000_i64;
        // Old scratch cell — should prune.
        let mut old_scratch = Cell::new(tf.clone(), "old scratch".to_string());
        old_scratch.timestamp = now - 11 * day;
        old_scratch.scratch = true;
        let old_scratch_id = old_scratch.id;
        // Fresh scratch cell — must stay.
        let mut new_scratch = Cell::new(tf.clone(), "new scratch".to_string());
        new_scratch.timestamp = now - 1 * day;
        new_scratch.scratch = true;
        let new_scratch_id = new_scratch.id;
        // Old regular cell — must stay (the TTL only fires on
        // scratch=1, not aged regular cells).
        let mut old_regular = Cell::new(tf.clone(), "old regular".to_string());
        old_regular.timestamp = now - 30 * day;
        let old_regular_id = old_regular.id;
        for c in [&old_scratch, &new_scratch, &old_regular] {
            db.save_cell(c).expect("save");
        }
        let cutoff = now - 10 * day;
        let removed = db.prune_expired_scratch(cutoff).expect("prune");
        assert_eq!(removed.len(), 1, "exactly one row removed");
        assert_eq!(removed[0], old_scratch_id);
        let remaining: Vec<Uuid> = db
            .load_cells(&tf)
            .expect("load")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(remaining.contains(&new_scratch_id), "fresh scratch survives");
        assert!(remaining.contains(&old_regular_id), "old regular cells aren't touched");
        assert!(!remaining.contains(&old_scratch_id), "expired scratch is gone");
    }

    // ----- threads -------------------------------------------------------

    fn fresh_thread(title: &str) -> Thread {
        let now = now_epoch_ms();
        Thread {
            id: Uuid::new_v4(),
            title: title.to_string(),
            created_at: now,
            edited_at: now,
            closed_at: None,
        }
    }

    #[test]
    fn round_trip_thread_create_list_delete() {
        let mut db = open_in_memory_db();
        let t = fresh_thread("alpha");
        db.upsert_thread(&t).expect("upsert");
        let listed = db.all_threads().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, t.id);
        assert_eq!(listed[0].title, "alpha");
        db.delete_thread(t.id).expect("delete");
        assert!(db.all_threads().expect("list").is_empty());
    }

    #[test]
    fn attach_detach_whole_cell_and_subtree() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "host".to_string());
        let cell_id = cell.id;
        let bullet_id = Uuid::new_v4();
        db.save_cell(&cell).expect("save");
        let t = fresh_thread("x");
        db.upsert_thread(&t).expect("upsert");
        db.attach_thread(t.id, ReferenceTarget::WholeCell(cell_id), 100)
            .expect("attach whole");
        db.attach_thread(
            t.id,
            ReferenceTarget::Subtree { cell_id, bullet_id },
            200,
        )
        .expect("attach subtree");
        let mems = db.all_thread_memberships().expect("list");
        assert_eq!(mems.len(), 2);
        let mut targets: Vec<_> = mems.iter().map(|m| m.target).collect();
        targets.sort_by_key(|t| match t {
            ReferenceTarget::WholeCell(_) => 0,
            ReferenceTarget::Subtree { .. } => 1,
        });
        assert!(matches!(targets[0], ReferenceTarget::WholeCell(c) if c == cell_id));
        assert!(matches!(
            targets[1],
            ReferenceTarget::Subtree { cell_id: c, bullet_id: b }
                if c == cell_id && b == bullet_id
        ));
        db.detach_thread(t.id, ReferenceTarget::WholeCell(cell_id))
            .expect("detach whole");
        let after = db.all_thread_memberships().expect("list");
        assert_eq!(after.len(), 1);
        assert!(matches!(after[0].target, ReferenceTarget::Subtree { .. }));
    }

    #[test]
    fn attach_is_idempotent() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "host".to_string());
        let cell_id = cell.id;
        db.save_cell(&cell).expect("save");
        let t = fresh_thread("x");
        db.upsert_thread(&t).expect("upsert");
        db.attach_thread(t.id, ReferenceTarget::WholeCell(cell_id), 100)
            .expect("attach 1");
        db.attach_thread(t.id, ReferenceTarget::WholeCell(cell_id), 200)
            .expect("attach 2");
        assert_eq!(db.all_thread_memberships().expect("list").len(), 1);
    }

    #[test]
    fn cell_delete_cascades_memberships() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "host".to_string());
        let cell_id = cell.id;
        db.save_cell(&cell).expect("save");
        let t = fresh_thread("x");
        db.upsert_thread(&t).expect("upsert");
        db.attach_thread(t.id, ReferenceTarget::WholeCell(cell_id), 1)
            .expect("attach");
        db.delete_cell(cell_id).expect("delete cell");
        assert!(
            db.all_thread_memberships().expect("list").is_empty(),
            "FK cascade should drop memberships when the host cell is deleted"
        );
    }

    #[test]
    fn thread_delete_cascades_memberships() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "host".to_string());
        let cell_id = cell.id;
        db.save_cell(&cell).expect("save");
        let t = fresh_thread("x");
        db.upsert_thread(&t).expect("upsert");
        db.attach_thread(t.id, ReferenceTarget::WholeCell(cell_id), 1)
            .expect("attach");
        db.delete_thread(t.id).expect("delete thread");
        assert!(db.all_thread_memberships().expect("list").is_empty());
    }

    #[test]
    fn reconcile_subtree_memberships_drops_only_orphans() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "host".to_string());
        let cell_id = cell.id;
        db.save_cell(&cell).expect("save");
        let t = fresh_thread("alpha");
        db.upsert_thread(&t).expect("upsert");
        let live_bullet = Uuid::new_v4();
        let dead_bullet = Uuid::new_v4();
        // Whole-cell + live-bullet membership stay; dead-bullet drops.
        db.attach_thread(t.id, ReferenceTarget::WholeCell(cell_id), 10)
            .expect("attach whole");
        db.attach_thread(
            t.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: live_bullet },
            20,
        )
        .expect("attach live");
        db.attach_thread(
            t.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: dead_bullet },
            30,
        )
        .expect("attach dead");
        let removed = db
            .reconcile_subtree_memberships(cell_id, &[live_bullet])
            .expect("reconcile");
        assert_eq!(removed, 1);
        let mems = db.all_thread_memberships().expect("list");
        assert_eq!(mems.len(), 2);
        let targets: Vec<_> = mems.iter().map(|m| m.target).collect();
        assert!(targets.iter().any(|t| matches!(t, ReferenceTarget::WholeCell(c) if *c == cell_id)));
        assert!(targets.iter().any(|t| matches!(
            t,
            ReferenceTarget::Subtree { cell_id: c, bullet_id: b }
                if *c == cell_id && *b == live_bullet
        )));
    }

    #[test]
    fn set_bullet_memberships_round_trips_via_move_then_restore() {
        // Models the undo of a bullet merge: move from->into, then
        // restore both sides from snapshots taken pre-merge.
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "host".to_string());
        let cell_id = cell.id;
        db.save_cell(&cell).expect("save");
        let t1 = fresh_thread("alpha");
        let t2 = fresh_thread("beta");
        let t3 = fresh_thread("gamma");
        db.upsert_thread(&t1).expect("upsert t1");
        db.upsert_thread(&t2).expect("upsert t2");
        db.upsert_thread(&t3).expect("upsert t3");
        let from = Uuid::new_v4();
        let into = Uuid::new_v4();
        // Pre-merge: from has t1 + t2; into has t2 + t3.
        // (t2 overlaps; dedupe will trigger on move.)
        db.attach_thread(
            t1.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: from },
            10,
        )
        .expect("attach t1@from");
        db.attach_thread(
            t2.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: from },
            20,
        )
        .expect("attach t2@from");
        db.attach_thread(
            t2.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: into },
            30,
        )
        .expect("attach t2@into");
        db.attach_thread(
            t3.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: into },
            40,
        )
        .expect("attach t3@into");
        // Snapshot pre-merge state.
        let pre_from: Vec<(Uuid, i64)> = vec![(t1.id, 10), (t2.id, 20)];
        let pre_into: Vec<(Uuid, i64)> = vec![(t2.id, 30), (t3.id, 40)];
        // Do the move.
        db.move_bullet_memberships(cell_id, from, into).expect("move");
        // Now restore both ends — undo path.
        db.set_bullet_memberships(cell_id, from, &pre_from)
            .expect("restore from");
        db.set_bullet_memberships(cell_id, into, &pre_into)
            .expect("restore into");
        let mems = db.all_thread_memberships().expect("list");
        assert_eq!(mems.len(), 4);
        let at_from: Vec<_> = mems
            .iter()
            .filter(|m| matches!(
                m.target,
                ReferenceTarget::Subtree { cell_id: c, bullet_id: b }
                    if c == cell_id && b == from
            ))
            .map(|m| (m.thread_id, m.attached_at))
            .collect();
        let mut at_from_sorted = at_from.clone();
        at_from_sorted.sort_by_key(|(_, a)| *a);
        assert_eq!(at_from_sorted, vec![(t1.id, 10), (t2.id, 20)]);
        let at_into: Vec<_> = mems
            .iter()
            .filter(|m| matches!(
                m.target,
                ReferenceTarget::Subtree { cell_id: c, bullet_id: b }
                    if c == cell_id && b == into
            ))
            .map(|m| (m.thread_id, m.attached_at))
            .collect();
        let mut at_into_sorted = at_into.clone();
        at_into_sorted.sort_by_key(|(_, a)| *a);
        assert_eq!(at_into_sorted, vec![(t2.id, 30), (t3.id, 40)]);
    }

    #[test]
    fn move_bullet_memberships_transfers_and_dedupes() {
        let tf = typeface();
        let mut db = open_in_memory_db();
        let cell = Cell::new(tf.clone(), "host".to_string());
        let cell_id = cell.id;
        db.save_cell(&cell).expect("save");
        let t1 = fresh_thread("alpha");
        let t2 = fresh_thread("beta");
        db.upsert_thread(&t1).expect("upsert t1");
        db.upsert_thread(&t2).expect("upsert t2");
        let from = Uuid::new_v4();
        let into = Uuid::new_v4();
        // t1 attached at `from` only — transfers cleanly.
        db.attach_thread(
            t1.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: from },
            10,
        )
        .expect("attach t1@from");
        // t2 attached at BOTH from and into — dedupe should drop
        // the from-side row instead of duplicating.
        db.attach_thread(
            t2.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: from },
            20,
        )
        .expect("attach t2@from");
        db.attach_thread(
            t2.id,
            ReferenceTarget::Subtree { cell_id, bullet_id: into },
            30,
        )
        .expect("attach t2@into");
        db.move_bullet_memberships(cell_id, from, into)
            .expect("move");
        let mems = db.all_thread_memberships().expect("list");
        // 2 rows expected: t1@into, t2@into. The duplicate t2@from
        // collapses into the existing t2@into.
        assert_eq!(mems.len(), 2);
        for m in &mems {
            assert!(matches!(
                m.target,
                ReferenceTarget::Subtree { cell_id: c, bullet_id: b }
                    if c == cell_id && b == into
            ));
        }
        let thread_ids: std::collections::HashSet<_> =
            mems.iter().map(|m| m.thread_id).collect();
        assert!(thread_ids.contains(&t1.id));
        assert!(thread_ids.contains(&t2.id));
    }

    #[test]
    fn set_thread_closed_at_round_trips() {
        let mut db = open_in_memory_db();
        let t = fresh_thread("x");
        db.upsert_thread(&t).expect("upsert");
        let now = now_epoch_ms();
        db.set_thread_closed_at(t.id, Some(now), now)
            .expect("close");
        let back = db.thread_by_id(t.id).expect("by id").expect("present");
        assert_eq!(back.closed_at, Some(now));
        db.set_thread_closed_at(t.id, None, now + 1)
            .expect("reopen");
        let back = db.thread_by_id(t.id).expect("by id").expect("present");
        assert!(back.closed_at.is_none());
        assert_eq!(back.edited_at, now + 1);
    }
}
