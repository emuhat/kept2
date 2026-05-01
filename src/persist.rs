use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use skia_safe::Typeface;
use uuid::Uuid;

use crate::cell::{Bullet, Cell, CellKind, OutlineCell, PopPopCell, TableCell, TextBox};

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
pub struct Entity {
    pub id: Uuid,
    pub kind: String,
    pub display_name: String,
    pub primary_cell_id: Option<Uuid>,
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
            if !tag_names_from_persisted(&pc).iter().any(|n| n == "person") {
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
            let names = tag_names_from_persisted(&pc);
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
                tb
            });
            let kind = body_to_kind(pc.body, typeface);
            cells.push(Cell::from_parts(id, kind, title, timestamp, edited_at, hint));
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
    pub fn all_tags(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT name FROM tags ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

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
            "SELECT id, kind, display_name, primary_cell_id, created_at, updated_at \
             FROM entities ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let kind: String = row.get(1)?;
                let display_name: String = row.get(2)?;
                let primary_bytes: Option<Vec<u8>> = row.get(3)?;
                let created_at: i64 = row.get(4)?;
                let updated_at: i64 = row.get(5)?;
                Ok(Entity {
                    id: Uuid::from_slice(&id_bytes).unwrap_or_else(|_| Uuid::nil()),
                    kind,
                    display_name,
                    primary_cell_id: primary_bytes
                        .and_then(|b| Uuid::from_slice(&b).ok()),
                    created_at,
                    updated_at,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
        // Preserve created_at on update; bump updated_at.
        tx.execute(
            "INSERT OR REPLACE INTO entities \
                (id, kind, display_name, primary_cell_id, created_at, updated_at) \
             VALUES (?1, 'person', ?2, ?1, \
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
    #[serde(flatten)]
    body: CellBody,
}

#[derive(Serialize, Deserialize)]
struct TitleRecord {
    text: String,
    #[serde(default)]
    links: Vec<LinkRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum CellBody {
    Plain {
        text: String,
        #[serde(default)]
        links: Vec<LinkRecord>,
    },
    Outline {
        blocks: Vec<BlockRecord>,
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
}

#[derive(Serialize, Deserialize)]
struct TableEntryRecord {
    text: String,
    #[serde(default)]
    links: Vec<LinkRecord>,
    #[serde(default)]
    readonly: bool,
}

/// Build the wrapped persisted form from a live Cell. Captures the title
/// slot (if any) and the kind-tagged body.
fn persisted_cell_from(cell: &Cell) -> PersistedCell {
    let title = cell.title.as_ref().map(|tb| TitleRecord {
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
    });
    PersistedCell {
        title,
        body: cell_to_body(cell),
    }
}

fn cell_to_body(cell: &Cell) -> CellBody {
    match &cell.kind {
        CellKind::Plain(tb) => CellBody::Plain {
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
                })
                .collect(),
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
        CellBody::Plain { text, links } => take_heading_from_inline(text, links),
        CellBody::PopPop { text, links } => take_heading_from_inline(text, links),
        CellBody::Outline { blocks } => take_heading_from_outline(blocks),
        CellBody::Table { cells, .. } => take_heading_from_table(cells),
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
        });
    }
    Some(TitleRecord {
        text: title_text,
        links: title_links,
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

fn tag_names_from_persisted(pc: &PersistedCell) -> Vec<String> {
    let Some(t) = pc.title.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let heading_end = t.text.find('\n').unwrap_or(t.text.len());
    for r in parse_trailing_tags(&t.text, heading_end) {
        if r.end > r.start + 1 {
            let name = t.text[r.start + 1..r.end].to_string();
            if !out.contains(&name) {
                out.push(name);
            }
        }
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
        CellBody::Outline { blocks } => {
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
    match body {
        CellBody::Plain { text, links } => {
            let mut tb = TextBox::new(typeface.clone(), text);
            for l in links {
                tb.add_link(l.start..l.end, l.url);
            }
            CellKind::Plain(tb)
        }
        CellBody::Outline { blocks } => {
            let bullets: Vec<Bullet> = blocks
                .into_iter()
                .map(|b| {
                    let mut tb = TextBox::new(typeface.clone(), b.text);
                    for l in b.links {
                        tb.add_link(l.start..l.end, l.url);
                    }
                    Bullet::new(b.id, tb, b.depth)
                })
                .collect();
            CellKind::Outline(OutlineCell::from_bullets(typeface.clone(), bullets))
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
            let row_records: Vec<Vec<(std::ops::Range<usize>, String)>> = cells
                .iter()
                .map(|row| {
                    row.iter()
                        .flat_map(|e| {
                            e.links
                                .iter()
                                .map(|l| (l.start..l.end, l.url.clone()))
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .collect();
            // Build the (text, links, readonly) triples expected by
            // TableCell::from_records.
            let triples: Vec<Vec<(String, Vec<(std::ops::Range<usize>, String)>, bool)>> = cells
                .into_iter()
                .zip(row_records.into_iter())
                .map(|(row, _)| {
                    row.into_iter()
                        .map(|e| {
                            let links: Vec<(std::ops::Range<usize>, String)> = e
                                .links
                                .into_iter()
                                .map(|l| (l.start..l.end, l.url))
                                .collect();
                            (e.text, links, e.readonly)
                        })
                        .collect()
                })
                .collect();
            CellKind::Table(TableCell::from_records(typeface.clone(), triples))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(tag_names_from_persisted(&pc), vec!["urgent".to_string()]);
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
            CellBody::Outline { blocks } => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, "first child");
            }
            _ => panic!("kind survives migration"),
        }
        assert_eq!(tag_names_from_persisted(&pc), vec!["person".to_string()]);
    }
}
