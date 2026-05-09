# Followups & known limitations

Captures things we know are missing / sub-optimal but didn't ship in v1.
Each item should be self-contained enough that we can pick it up cold.

---

## References / embeds

The reference-cell feature (top-level read-only embeds in the timeline,
plus the "REFERENCED IN" list on the Person page) ships with two known
gaps and one deferred phase.

### Selection on Person-page embeds

**State today:** Timeline reference cells own a persistent
`Option<Box<Cell>>` cache on `ReferenceCell`. The cache survives across
frames and owns its own selection state, so drag-select inside a
timeline embed works like in any other cell, and `Cmd+A` / `Cmd+C`
forward through `Cell::select_all_focused` and `Cell::copy_text`.

The Person page's "REFERENCED IN" embeds **don't** have a persistent
cache — `render_entity_page` builds a fresh cache via
`build_reference_cache` for each embed every frame, ticks it, drops it.
Visual fidelity is identical (warm-tan dashed wrapper, body, footer),
but a drag-select inside one of these embeds paints highlights as you
drag and disappears on release. Click-on-embed navigates to the source
cell — that's the main interaction.

**Fix sketch:** Add `entity_page_refs: Vec<EntityPageRef>` and
`entity_page_refs_for: Option<Uuid>` fields on `KeptApp`. Each entry
holds `(target_cell_id, cache, cache_edited_at)`. Refresh the list when
`entity_page_refs_for != Some(current_entity)` or when the set of
mentions changes; preserve caches by `target_cell_id` across rebuilds.
Render iterates the list and ticks each cache (same machinery as
timeline references). Sized at ~50 lines on top of what's already there.

### Right-click + `Cmd+X` on a focused reference

`Cmd+X` is currently a no-op (returns empty string from
`Cell::cut_text`). Should probably forward to `cache.copy_text` so
"cut" on a read-only cell behaves like "copy" — fewer surprises. One
match arm.

### Phase 2: bullet-as-reference inside an outline

Documented in detail in `~/.claude/plans/ok-i-want-to-soft-breeze.md`
under "Out of scope (Stage 2)." Short version:

- `Bullet` becomes `enum BulletKind { Text(TextBox), Reference(ReferenceTarget) }`
  (or grows an optional `ReferenceTarget` discriminator field).
- All outline mutators (`indent_focused`, `outdent_focused`,
  `split_focused`, `merge_focused_into_prev`) get arm checks — reference
  bullets participate in subtree ranges via depth but aren't text-mutable.
- `BlockRecord` in `persist.rs` gains an optional reference-target
  field. Schema is additive — existing rows still load as text bullets.
- `OutlineCell::tick`'s per-bullet branch dispatches reference bullets
  through the same cache+wrapper machinery v1 built.

Creation UX (the missing piece): right-click a bullet inside an outline
→ "Copy as embed" (puts target on an internal clipboard). Right-click
another bullet → "Paste embed here" (inserts a reference bullet at
that position). This is the copy/paste flow we removed for the
timeline-level case — it actually fits the in-cell case naturally,
because there the paste target (a specific bullet position) is the
meaningful anchor.

---

## Cell content & search

### `all_link_urls()` allocations

`Cell::all_link_urls() -> Vec<String>` clones every URL across all body
shapes. Used per-frame on the Person page to find mentioning cells.
Cheap for now; if it shows up, swap for a closure-based predicate
(`Cell::has_link_url(&str) -> bool`) so the hot path doesn't allocate.

---

## Larger structural concerns

These are addressed in detail in the code-review document
(`CODE_REVIEW.md`); listed here for cross-reference.

- `cell.rs` (~5934 lines) and `app.rs` (~6991 lines) are too big.
  UI-element bodies (TextBox, OutlineCell, PopPopCell, TableCell,
  Bullet) all live in `cell.rs`; their natural homes are
  `cell/textbox.rs`, `cell/outline.rs`, etc.
- `app.rs` mixes app-level concerns (KeptApp, undo, persistence,
  navigation) with rendering of specific UI surfaces (sidebar, search
  popup, entity page, people page, context menus). Each render-X
  function is a candidate for extraction.
- Test coverage is concentrated in `cell.rs::tests` (text/outline/
  poppop edge cases). App-level navigation, persistence round-trips
  beyond the schema test, and the new reference machinery have minimal
  coverage.
