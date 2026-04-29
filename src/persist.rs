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
        if version < 1 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE entries (
                     id          BLOB PRIMARY KEY,
                     title       TEXT NOT NULL,
                     created_at  INTEGER NOT NULL,
                     edited_at   INTEGER NOT NULL
                 );
                 CREATE TABLE cells (
                     id          BLOB PRIMARY KEY,
                     entry_id    BLOB NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
                     order_index INTEGER NOT NULL,
                     kind        TEXT NOT NULL,
                     body        TEXT NOT NULL,
                     created_at  INTEGER NOT NULL,
                     edited_at   INTEGER NOT NULL
                 );
                 CREATE INDEX cells_by_entry ON cells (entry_id, order_index);
                 PRAGMA user_version = 1;
                 COMMIT;",
            )?;
        }
        Ok(())
    }

    pub fn load_entries(&self, typeface: &Typeface) -> rusqlite::Result<Vec<EntryRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, title, created_at, edited_at FROM entries ORDER BY edited_at DESC")?;
        let rows: Vec<(Uuid, String, i64, i64)> = stmt
            .query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let id = Uuid::from_slice(&id_bytes).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?;
                Ok((id, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut entries = Vec::with_capacity(rows.len());
        for (id, title, created_at, edited_at) in rows {
            let cells = self.load_cells_for_entry(id, typeface)?;
            entries.push(EntryRow {
                id,
                title,
                cells,
                created_at,
                edited_at,
            });
        }
        Ok(entries)
    }

    fn load_cells_for_entry(
        &self,
        entry_id: Uuid,
        typeface: &Typeface,
    ) -> rusqlite::Result<Vec<Cell>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, body, created_at, edited_at FROM cells \
             WHERE entry_id = ?1 ORDER BY order_index ASC",
        )?;
        let rows: Vec<(Uuid, String, i64, i64)> = stmt
            .query_map(params![entry_id.as_bytes().to_vec()], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let id = Uuid::from_slice(&id_bytes).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?;
                Ok((id, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut cells = Vec::with_capacity(rows.len());
        for (id, body_json, created_at, edited_at) in rows {
            let body: CellBody = serde_json::from_str(&body_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let kind = body_to_kind(body, typeface);
            cells.push(Cell::from_parts(id, kind, created_at, edited_at));
        }
        Ok(cells)
    }

    pub fn save_entry(&mut self, entry: &EntryRef<'_>) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO entries (id, title, created_at, edited_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                entry.id.as_bytes().to_vec(),
                entry.title,
                entry.created_at,
                entry.edited_at,
            ],
        )?;
        tx.execute(
            "DELETE FROM cells WHERE entry_id = ?1",
            params![entry.id.as_bytes().to_vec()],
        )?;
        for (idx, cell) in entry.cells.iter().enumerate() {
            let body = cell_to_body(cell);
            let body_json = serde_json::to_string(&body).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            })?;
            let kind_str = match &cell.kind {
                CellKind::Plain(_) => "plain",
                CellKind::Outline(_) => "outline",
            };
            tx.execute(
                "INSERT INTO cells (id, entry_id, order_index, kind, body, created_at, edited_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    cell.id.as_bytes().to_vec(),
                    entry.id.as_bytes().to_vec(),
                    idx as i64,
                    kind_str,
                    body_json,
                    cell.created_at,
                    cell.edited_at,
                ],
            )?;
        }
        tx.commit()
    }
}

/// Loaded entry shape — matches `Entry` in app.rs but lives here so the
/// persistence module doesn't depend on the app's runtime types.
pub struct EntryRow {
    pub id: Uuid,
    pub title: String,
    pub cells: Vec<Cell>,
    pub created_at: i64,
    pub edited_at: i64,
}

/// Borrowed view passed to `save_entry`.
pub struct EntryRef<'a> {
    pub id: Uuid,
    pub title: &'a str,
    pub cells: &'a [Cell],
    pub created_at: i64,
    pub edited_at: i64,
}

// ---------------------------------------------------------------------------
// Serde shape — kept private to this module. Cell body fields land here.
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
