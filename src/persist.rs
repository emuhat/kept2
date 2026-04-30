use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use skia_safe::Typeface;
use uuid::Uuid;

use crate::cell::{Bullet, Cell, CellKind, OutlineCell, TextBox};

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
        Ok(())
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
            let body: CellBody = match serde_json::from_str(&body_json) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let names = tag_names_from_body(&body);
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
            let body: CellBody = serde_json::from_str(&body_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let kind = body_to_kind(body, typeface);
            cells.push(Cell::from_parts(id, kind, timestamp, edited_at, hint));
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
        let body = cell_to_body(cell);
        let body_json = serde_json::to_string(&body)
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
        let names = tag_names_from_body(&body);
        self.write_cell_tags(&cell_id_bytes, &names)?;
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
    #[allow(dead_code)]
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

    pub fn delete_cell(&mut self, id: Uuid) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM cells WHERE id = ?1",
            params![id.as_bytes().to_vec()],
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
    }
}

/// Extract distinct, source-ordered tag names from a cell body. For Plain
/// cells, parses the heading paragraph. For Outline cells, every bullet
/// whose text starts with `# ` contributes its trailing tags. The leading
/// `#` is stripped so names match the canonical form stored in `tags.name`.
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
        if start >= 2 && start < end && bytes[start] == b'#' {
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
    }
}
