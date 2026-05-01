#!/usr/bin/env python3
"""Import a LogSeq markdown export into a Kept SQLite DB.

Pages are classified by filename:
  - `@FirstLast.md`           -> a `#person` cell.
  - `<Mon> <Day><suf>, YYYY.md` -> N journal cells (one per top-level bullet).
  - anything else             -> a single topic outline cell.

`[[…]]` references resolve to `kept://<uuid>` links pointing at the imported
target page. Broken references are left as plain text.

Cells go in unattached (`context_hint_id = NULL`) and are visible via Kept's
date sidebar by their `timestamp` alone. The user's existing contexts are
not touched.

Example:
  python scripts/import_logseq.py \\
      --src ~/Downloads/WorkDB_markdown_1777487321 \\
      --db  ~/.local/share/kept/notes.db
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import sys
import time
import uuid
from datetime import date, datetime, time as dt_time
from pathlib import Path
from typing import Iterable, NamedTuple


# ---------------------------------------------------------------------------
# UUIDv7
# ---------------------------------------------------------------------------

def uuid7() -> uuid.UUID:
    """Generate a UUIDv7 per RFC 9562: 48-bit ms timestamp, version, random."""
    ms = int(time.time() * 1000) & ((1 << 48) - 1)
    rand_a = int.from_bytes(os.urandom(2), "big") & 0x0FFF
    rand_b = int.from_bytes(os.urandom(8), "big") & ((1 << 62) - 1)
    ms_bytes = ms.to_bytes(6, "big")
    byte6_7 = ((0x7 << 12) | rand_a).to_bytes(2, "big")
    byte8_15 = ((0x2 << 62) | rand_b).to_bytes(8, "big")
    return uuid.UUID(bytes=ms_bytes + byte6_7 + byte8_15)


# ---------------------------------------------------------------------------
# Page classification + dates
# ---------------------------------------------------------------------------

MONTHS = {
    "Jan": 1, "Feb": 2, "Mar": 3, "Apr": 4, "May": 5, "Jun": 6,
    "Jul": 7, "Aug": 8, "Sep": 9, "Oct": 10, "Nov": 11, "Dec": 12,
}
JOURNAL_RE = re.compile(r"^([A-Z][a-z]{2}) (\d{1,2})(?:st|nd|rd|th), (\d{4})$")
PERSON_RE = re.compile(r"^@(.+)$")
LINK_RE = re.compile(r"\[\[([^\]]+)\]\]")
BULLET_RE = re.compile(r"^(\t*)- ?(.*)$")
PROPERTY_RE = re.compile(r"^[a-zA-Z][\w-]*::")

PERSON_TYPE = "person"
JOURNAL_TYPE = "journal"
TOPIC_TYPE = "topic"


class Page(NamedTuple):
    path: Path | None    # None for synthesized-from-references person pages
    name: str            # filename without `.md` (or `@Name` for synthetic)
    kind: str            # "person" | "journal" | "topic"
    journal_date: date | None  # for journals: source date; for synthetic
                               # persons: earliest journal that mentions them
    cell_uuid: uuid.UUID  # one assigned per page (first cell for journals)
    is_existing: bool   # True iff this page reuses an existing Kept cell;
                        # we use its UUID for [[…]] resolution but skip the
                        # INSERT during the write pass.


def classify(path: Path, existing_persons: dict[str, uuid.UUID]) -> Page:
    name = path.stem
    if PERSON_RE.match(name):
        key = normalize_person_key(name)
        if key in existing_persons:
            return Page(path, name, PERSON_TYPE, None, existing_persons[key], True)
        return Page(path, name, PERSON_TYPE, None, uuid7(), False)
    m = JOURNAL_RE.match(name)
    if m:
        mon_abbr, day, year = m.group(1), int(m.group(2)), int(m.group(3))
        mon = MONTHS.get(mon_abbr)
        if mon is not None:
            return Page(path, name, JOURNAL_TYPE, date(year, mon, day), uuid7(), False)
    return Page(path, name, TOPIC_TYPE, None, uuid7(), False)


def split_camel(name: str) -> str:
    """`MikeFitzpatrick` -> `Mike Fitzpatrick`. Idempotent on already-spaced."""
    return re.sub(r"(?<!^)(?=[A-Z])", " ", name)


def normalize_person_key(name: str) -> str:
    """Canonical key for de-duping person references. Strips a leading `@`,
    removes all whitespace, lowercases. So `@MicahFry`, `Micah Fry`,
    `micahfry`, and `MicahFry` all map to `micahfry`. Used both as the
    `page_index` lookup key and for matching against existing `#person`
    cells in the target DB."""
    s = name[1:] if name.startswith("@") else name
    s = re.sub(r"\s+", "", s)
    return s.lower()


def display_name(page: Page) -> str:
    if page.kind == PERSON_TYPE:
        bare = page.name[1:] if page.name.startswith("@") else page.name
        # If the source already has spaces (e.g. a `[[Micah Fry]]` reference
        # with the spelled-out form), keep them; otherwise CamelCase split.
        if " " in bare:
            return bare
        return split_camel(bare)
    if page.kind == TOPIC_TYPE:
        if " " in page.name:
            return page.name
        return split_camel(page.name)
    return page.name  # journals not currently surfaced as link targets


# ---------------------------------------------------------------------------
# Bullet parsing
# ---------------------------------------------------------------------------

class Bullet(NamedTuple):
    depth: int
    text: str


def parse_bullets(content: str) -> list[Bullet]:
    """Walk lines, return tab-indented bullets in source order. Lines that
    aren't bullets are either dropped (properties, code fences, blanks) or
    appended to the most recent bullet's text (continuation paragraphs)."""
    out: list[list] = []   # mutable while building, then frozen
    in_fence = False
    for raw in content.splitlines():
        stripped = raw.strip()
        if stripped.startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            if out:
                out[-1][1] = out[-1][1] + ("\n" if out[-1][1] else "") + raw
            continue
        if PROPERTY_RE.match(stripped):
            continue
        if not stripped:
            continue
        m = BULLET_RE.match(raw)
        if m:
            depth = len(m.group(1))
            text = m.group(2).rstrip()
            out.append([depth, text])
            continue
        # Non-bullet, non-empty line. If we've started a bullet, attach as a
        # continuation; otherwise drop it.
        if out:
            out[-1][1] = out[-1][1] + " " + stripped
    return [Bullet(d, t) for d, t in out]


# ---------------------------------------------------------------------------
# Link resolution + body conversion
# ---------------------------------------------------------------------------

class LinkRecord(NamedTuple):
    start: int
    end: int
    url: str


def resolve_links(
    raw: str, page_index: dict[str, Page], broken: list[str]
) -> tuple[str, list[LinkRecord]]:
    """Replace each `[[X]]` in `raw` with X's display text + a link entry.
    Unknown targets are left as plain `[[X]]` and recorded in `broken`."""
    out_text_parts: list[str] = []
    links: list[LinkRecord] = []
    cursor = 0
    out_len = 0  # current byte length of out_text_parts when joined
    for m in LINK_RE.finditer(raw):
        # Append the literal slice before this link.
        prefix = raw[cursor:m.start()]
        out_text_parts.append(prefix)
        out_len += len(prefix.encode("utf-8"))

        target = m.group(1).strip()
        page = lookup_target(target, page_index)
        if page is None:
            # Broken — keep the literal `[[X]]`.
            literal = m.group(0)
            out_text_parts.append(literal)
            out_len += len(literal.encode("utf-8"))
            broken.append(target)
        else:
            disp = display_name(page)
            disp_bytes = len(disp.encode("utf-8"))
            link_start = out_len
            link_end = link_start + disp_bytes
            out_text_parts.append(disp)
            out_len += disp_bytes
            links.append(LinkRecord(
                start=link_start,
                end=link_end,
                url=f"kept://{page.cell_uuid}",
            ))
        cursor = m.end()
    # Trailing slice.
    out_text_parts.append(raw[cursor:])
    return "".join(out_text_parts), links


def lookup_target(target: str, page_index: dict[str, Page]) -> Page | None:
    """Look up via the normalized form. `@MicahFry`, `Micah Fry`,
    `MicahFry`, and `micah fry` all hit the same entry."""
    return page_index.get(normalize_person_key(target))


# ---------------------------------------------------------------------------
# Body JSON construction
# ---------------------------------------------------------------------------

def plain_body(text: str, links: list[LinkRecord]) -> str:
    return json.dumps(
        {"kind": "plain", "text": text, "links": [link_dict(l) for l in links]},
        ensure_ascii=False,
        separators=(",", ":"),
    )


def outline_body(blocks: list[dict]) -> str:
    return json.dumps(
        {"kind": "outline", "blocks": blocks},
        ensure_ascii=False,
        separators=(",", ":"),
    )


def link_dict(l: LinkRecord) -> dict:
    return {"start": l.start, "end": l.end, "url": l.url}


def block_dict(uuid_str: str, depth: int, text: str, links: list[LinkRecord]) -> dict:
    return {
        "id": uuid_str,
        "depth": depth,
        "text": text,
        "links": [link_dict(l) for l in links],
    }


# ---------------------------------------------------------------------------
# Heading-tag extraction (mirror of src/persist.rs::tag_names_from_body)
# ---------------------------------------------------------------------------

def heading_tag_names(text: str) -> list[str]:
    """If `text` starts with `# `, return tag names (no leading `#`) trailing
    the heading paragraph. Mirrors `parse_trailing_tags` in persist.rs."""
    if not text.startswith("# "):
        return []
    nl = text.find("\n")
    heading_end = nl if nl >= 0 else len(text)
    out: list[str] = []
    end = heading_end
    while True:
        while end > 0 and text[end - 1].isspace():
            end -= 1
        if end == 0:
            break
        start = end
        while start > 0 and not text[start - 1].isspace():
            start -= 1
        if start >= 2 and start < end and text[start] == "#":
            tag = text[start + 1:end]
            if tag and tag not in out:
                out.append(tag)
            end = start
        else:
            break
    out.reverse()
    return out


# ---------------------------------------------------------------------------
# Cell construction per page
# ---------------------------------------------------------------------------

class Cell(NamedTuple):
    cell_id: uuid.UUID
    timestamp_ms: int
    edited_at_ms: int
    body_json: str
    tag_names: list[str]


def file_mtime_ms(p: Path) -> int:
    return int(p.stat().st_mtime * 1000)


def journal_timestamp_ms(d: date, idx: int) -> int:
    """Noon-local on `d` plus `idx` ms so cells within a day keep order."""
    dt = datetime.combine(d, dt_time(12, 0))
    return int(dt.timestamp() * 1000) + idx


def build_person_cell(
    page: Page, bullets: list[Bullet], page_index: dict[str, Page], broken: list[str]
) -> Cell | None:
    """Person page → one Plain cell with `# {Name} #person` heading + any body.
    Returns None for pages flagged `is_existing` (we already have a Kept
    person cell with that name; the import reuses its UUID for `[[…]]`
    resolution rather than creating a duplicate)."""
    if page.is_existing:
        return None
    name = display_name(page)
    title_line = f"# {name} #person"

    body_lines: list[str] = []
    for b in bullets:
        body_lines.append(b.text)
    body_raw = "\n".join(body_lines)
    text_raw = title_line if not body_raw else f"{title_line}\n\n{body_raw}"
    text, links = resolve_links(text_raw, page_index, broken)

    if page.path is not None:
        ts = file_mtime_ms(page.path)
    elif page.journal_date is not None:
        ts = journal_timestamp_ms(page.journal_date, 0)
    else:
        ts = int(time.time() * 1000)
    return Cell(
        cell_id=page.cell_uuid,
        timestamp_ms=ts,
        edited_at_ms=ts,
        body_json=plain_body(text, links),
        tag_names=heading_tag_names(text),
    )


def build_topic_cell(
    page: Page, bullets: list[Bullet], page_index: dict[str, Page], broken: list[str]
) -> Cell:
    """Topic page → one Outline cell. First bullet is `# {Title}`, then the
    page's bullets at depths +1 (so they sit under the title)."""
    title = display_name(page)
    blocks: list[dict] = []

    # Title bullet at depth 0.
    title_text_raw = f"# {title}"
    title_text, title_links = resolve_links(title_text_raw, page_index, broken)
    blocks.append(block_dict(str(uuid7()), 0, title_text, title_links))

    # Source bullets.
    for b in bullets:
        text, links = resolve_links(b.text, page_index, broken)
        blocks.append(block_dict(str(uuid7()), b.depth + 1, text, links))

    ts = file_mtime_ms(page.path)
    all_text_for_tags = title_text  # tags only parsed from heading line
    return Cell(
        cell_id=page.cell_uuid,
        timestamp_ms=ts,
        edited_at_ms=ts,
        body_json=outline_body(blocks),
        tag_names=heading_tag_names(all_text_for_tags),
    )


def build_journal_cells(
    page: Page, bullets: list[Bullet], page_index: dict[str, Page], broken: list[str]
) -> list[Cell]:
    """Journal page → one cell per top-level bullet. Plain if the bullet has
    no children, Outline otherwise."""
    cells: list[Cell] = []
    # Group bullets into top-level groups (depth 0 with subsequent deeper
    # bullets attached to that group).
    groups: list[list[Bullet]] = []
    for b in bullets:
        if b.depth == 0:
            groups.append([b])
        elif groups:
            groups[-1].append(b)
        # else: stray non-zero-depth bullet before any depth-0; drop.

    assert page.journal_date is not None
    for idx, group in enumerate(groups):
        ts = journal_timestamp_ms(page.journal_date, idx)
        cell_id = page.cell_uuid if idx == 0 else uuid7()
        if len(group) == 1:
            text, links = resolve_links(group[0].text, page_index, broken)
            cells.append(Cell(
                cell_id=cell_id,
                timestamp_ms=ts,
                edited_at_ms=ts,
                body_json=plain_body(text, links),
                tag_names=heading_tag_names(text),
            ))
        else:
            # Outline cell. Re-base depths so the top-level is 0.
            base_depth = group[0].depth  # always 0 here, but be explicit
            blocks: list[dict] = []
            for b in group:
                text, links = resolve_links(b.text, page_index, broken)
                blocks.append(
                    block_dict(str(uuid7()), b.depth - base_depth, text, links)
                )
            # Tags come from the first bullet's heading (if any).
            cells.append(Cell(
                cell_id=cell_id,
                timestamp_ms=ts,
                edited_at_ms=ts,
                body_json=outline_body(blocks),
                tag_names=heading_tag_names(blocks[0]["text"]),
            ))
    return cells


# ---------------------------------------------------------------------------
# DB I/O
# ---------------------------------------------------------------------------

def load_existing_persons(conn: sqlite3.Connection) -> dict[str, uuid.UUID]:
    """Return `{normalized_name → cell_uuid}` for every `#person` cell
    already in the DB. Used to dedupe so the import reuses existing UUIDs
    when a person already lives in Kept (possibly with spaces in the name)."""
    out: dict[str, uuid.UUID] = {}
    cur = conn.execute(
        "SELECT c.id, c.body FROM cells c "
        "JOIN cell_tags ct ON ct.cell_id = c.id "
        "JOIN tags t ON t.id = ct.tag_id "
        "WHERE t.name = 'person'"
    )
    for cell_id_bytes, body_json in cur:
        try:
            body = json.loads(body_json)
        except json.JSONDecodeError:
            continue
        title = extract_heading_title(body)
        if not title:
            continue
        out[normalize_person_key(title)] = uuid.UUID(bytes=cell_id_bytes)
    return out


def extract_heading_title(body: dict) -> str | None:
    """Mirror of `Cell::heading_title` in src/cell.rs. For Plain/PopPop:
    parse the cell text. For Outline: parse the first bullet's text."""
    kind = body.get("kind")
    if kind in ("plain", "poppop"):
        return parse_heading_title(body.get("text", ""))
    if kind == "outline":
        blocks = body.get("blocks", [])
        if blocks:
            return parse_heading_title(blocks[0].get("text", ""))
    return None


def parse_heading_title(text: str) -> str | None:
    """If `text` starts with `# `, return the title portion — text between
    `# ` and the first trailing tag — trimmed. Else None. Mirrors the
    title-end detection in `recompute_line_tag_layout`."""
    if not text.startswith("# "):
        return None
    nl = text.find("\n")
    heading_end = nl if nl >= 0 else len(text)
    end = heading_end
    while True:
        while end > 0 and text[end - 1].isspace():
            end -= 1
        if end == 0:
            break
        start = end
        while start > 0 and not text[start - 1].isspace():
            start -= 1
        if start >= 2 and start < end and text[start] == "#":
            end = start
        else:
            break
    title = text[2:end].strip()
    return title or None


def open_and_validate_db(db_path: Path) -> sqlite3.Connection:
    if not db_path.exists():
        sys.exit(f"error: DB not found at {db_path}")
    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA foreign_keys = ON")
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    if version != 3:
        sys.exit(
            f"error: DB schema is user_version={version}, expected 3. "
            f"Open the app once to migrate, then re-run."
        )
    # Sanity: required tables present.
    tables = {
        r[0] for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table'"
        )
    }
    for required in ("cells", "contexts", "tags", "cell_tags"):
        if required not in tables:
            sys.exit(f"error: required table '{required}' missing")
    return conn


def backup_db(db_path: Path) -> Path:
    ts = datetime.now().strftime("%Y%m%d-%H%M%S")
    bak = db_path.with_suffix(db_path.suffix + f".bak.{ts}")
    shutil.copy2(db_path, bak)
    return bak


def insert_cell(conn: sqlite3.Connection, c: Cell) -> None:
    """Insert one cell row + its tag links. Mirrors `Db::save_cell` and
    `write_cell_tags` in src/persist.rs."""
    cell_id_bytes = c.cell_id.bytes
    conn.execute(
        "INSERT INTO cells (id, timestamp, body, edited_at, context_hint_id) "
        "VALUES (?, ?, ?, ?, NULL)",
        (cell_id_bytes, c.timestamp_ms, c.body_json, c.edited_at_ms),
    )
    for name in c.tag_names:
        new_tag_id = uuid7().bytes
        conn.execute(
            "INSERT OR IGNORE INTO tags (id, name) VALUES (?, ?)",
            (new_tag_id, name),
        )
        tag_id = conn.execute(
            "SELECT id FROM tags WHERE name = ?", (name,)
        ).fetchone()[0]
        conn.execute(
            "INSERT OR IGNORE INTO cell_tags (cell_id, tag_id) VALUES (?, ?)",
            (cell_id_bytes, tag_id),
        )


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def discover_pages(
    src: Path, existing_persons: dict[str, uuid.UUID]
) -> list[Page]:
    return [classify(p, existing_persons) for p in sorted(src.glob("*.md"))]


def build_page_index(pages: Iterable[Page]) -> dict[str, Page]:
    """Normalized-name → Page. Same key form as `lookup_target`."""
    out: dict[str, Page] = {}
    for p in pages:
        out[normalize_person_key(p.name)] = p
    return out


def synthesize_missing_persons(
    pages: list[Page],
    page_index: dict[str, Page],
    existing_persons: dict[str, uuid.UUID],
) -> list[Page]:
    """Scan every page's content for `[[…]]` references that don't already
    have a corresponding `.md` file in the index. Synthesize a person Page
    for each such target so the import produces a real `#person` cell that
    the @-mention popup can find. Tag each synthetic page with the earliest
    journal date that mentioned it. If an existing `#person` cell in the
    target DB already matches the normalized name, reuse its UUID and mark
    the synthesized page `is_existing=True` so the write pass skips it."""
    # Per-target collected info: earliest journal date + best display name.
    # Display preference: a spaced form ("Micah Fry") wins over CamelCase
    # ("@MicahFry") so we keep the user's original spelling when it exists.
    earliest_by_key: dict[str, date | None] = {}
    name_by_key: dict[str, str] = {}
    for p in pages:
        if p.path is None:
            continue
        try:
            content = p.path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for m in LINK_RE.finditer(content):
            target = m.group(1).strip()
            if lookup_target(target, page_index) is not None:
                continue
            key = normalize_person_key(target)
            jdate = p.journal_date
            if key in earliest_by_key:
                cur = earliest_by_key[key]
                if jdate is not None and (cur is None or jdate < cur):
                    earliest_by_key[key] = jdate
            else:
                earliest_by_key[key] = jdate
            # Pick the prettier of the candidate names: prefer one with
            # spaces; otherwise prefer CamelCase (capitalized) over lower.
            chosen_name = target if target.startswith("@") else "@" + target
            existing_name = name_by_key.get(key)
            if existing_name is None or " " in chosen_name and " " not in existing_name:
                name_by_key[key] = chosen_name

    new_pages: list[Page] = []
    for key, jdate in earliest_by_key.items():
        name = name_by_key[key]
        if key in existing_persons:
            synthetic = Page(
                path=None,
                name=name,
                kind=PERSON_TYPE,
                journal_date=jdate,
                cell_uuid=existing_persons[key],
                is_existing=True,
            )
        else:
            synthetic = Page(
                path=None,
                name=name,
                kind=PERSON_TYPE,
                journal_date=jdate,
                cell_uuid=uuid7(),
                is_existing=False,
            )
        new_pages.append(synthetic)
        page_index[key] = synthetic
    return new_pages


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.strip().splitlines()[0])
    parser.add_argument("--src", type=Path, required=True,
                        help="LogSeq export directory")
    parser.add_argument("--db", type=Path, default=None,
                        help="Path to the Kept SQLite DB. Defaults to "
                             "$KEPT_DB_PATH or ~/.local/share/kept/notes.db")
    parser.add_argument("--dry-run", action="store_true",
                        help="Parse + classify + summarize without writing")
    args = parser.parse_args()

    src: Path = args.src.expanduser()
    if not src.is_dir():
        sys.exit(f"error: --src {src} is not a directory")

    db_path: Path
    if args.db is not None:
        db_path = args.db.expanduser()
    elif "KEPT_DB_PATH" in os.environ:
        db_path = Path(os.environ["KEPT_DB_PATH"])
    else:
        db_path = Path.home() / ".local/share/kept/notes.db"

    print(f"opening DB at {db_path}")
    conn = open_and_validate_db(db_path)
    existing_persons = load_existing_persons(conn)
    print(f"existing #person cells in DB: {len(existing_persons)}")

    pages = discover_pages(src, existing_persons)
    page_index = build_page_index(pages)

    synthetic = synthesize_missing_persons(pages, page_index, existing_persons)
    pages.extend(synthetic)

    person_pages = [p for p in pages if p.kind == PERSON_TYPE]
    journal_pages = [p for p in pages if p.kind == JOURNAL_TYPE]
    topic_pages = [p for p in pages if p.kind == TOPIC_TYPE]
    reused = sum(1 for p in person_pages if p.is_existing)

    print(
        f"discovered {len(pages)} pages: "
        f"{len(person_pages)} person "
        f"({len(synthetic)} synthesized from [[@…]] refs, "
        f"{reused} dedup-merged into existing Kept persons), "
        f"{len(journal_pages)} journal, "
        f"{len(topic_pages)} topic"
    )

    # Build all cells in memory first so we can report counts before touching
    # the DB. Broken links accumulate across all pages.
    broken: list[str] = []
    all_cells: list[Cell] = []
    journal_cell_total = 0
    for p in pages:
        if p.path is not None:
            content = p.path.read_text(encoding="utf-8", errors="replace")
            bullets = parse_bullets(content)
        else:
            bullets = []  # synthesized page; no source file
        if p.kind == PERSON_TYPE:
            cell = build_person_cell(p, bullets, page_index, broken)
            if cell is not None:
                all_cells.append(cell)
        elif p.kind == JOURNAL_TYPE:
            cells = build_journal_cells(p, bullets, page_index, broken)
            journal_cell_total += len(cells)
            all_cells.extend(cells)
        else:
            all_cells.append(build_topic_cell(p, bullets, page_index, broken))

    print(
        f"prepared {len(all_cells)} cells "
        f"({len(person_pages)} person + "
        f"{journal_cell_total} journal + "
        f"{len(topic_pages)} topic)"
    )
    print(f"links: {sum(1 for _ in [c for c in all_cells])} cells, "
          f"{len(broken)} broken `[[…]]` references")
    if broken:
        unique_broken = sorted(set(broken))
        sample = ", ".join(unique_broken[:5])
        more = "" if len(unique_broken) <= 5 else f" (+ {len(unique_broken) - 5} more)"
        print(f"  broken targets: {sample}{more}")

    if args.dry_run:
        print("dry-run: no DB writes")
        conn.close()
        return

    bak = backup_db(db_path)
    print(f"backup: {bak}")

    try:
        conn.execute("BEGIN")
        for c in all_cells:
            insert_cell(conn, c)
        conn.commit()
    except Exception:
        conn.rollback()
        raise
    finally:
        conn.close()

    print(f"inserted {len(all_cells)} cells")


if __name__ == "__main__":
    main()
