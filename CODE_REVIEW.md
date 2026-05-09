# Code review & cleanup plan

A walk through what's working, what's not, and a concrete order of operations
for reshaping the codebase. Synthesizes findings from three parallel audits
of `cell.rs`, `app.rs`, and tests/persist/cross-cutting.

## Headline numbers

| File | Lines | Structural verdict |
|---|---|---|
| `src/cell.rs` | 5,934 | Five independent UI body types stuffed into one file. Clear extraction lines. |
| `src/app.rs` | 6,991 | One God-struct (`KeptApp`, ~135 fields, ~80 methods). Several distinct subsystems intermingled. |
| `src/persist.rs` | 1,431 | Three concerns (schema, cells, entities) in one impl block. |
| `src/query.rs` | 731 | Cleanly organized; leave alone. |
| `src/main.rs` | 327 | Fine as-is. |
| **Total** | 15,414 | |

| Tests | Count | Density |
|---|---|---|
| `query.rs` | 28 | 3.8% — solid |
| `cell.rs` | 22 | 0.4% — concentrated on text/link edge cases + PopPop |
| `app.rs` | 10 | 0.14% — only pure helpers (fuzzy, tag-split) |
| `persist.rs` | 5 | 0.35% — one round-trip + four migration tests |
| **Total** | 65 | |

## What's good (don't break this stuff)

- **`CellKind` dispatch pattern.** Each body type owns its own state and renders/handles input independently; `Cell::tick`/`handle_key`/`mouse_down` etc. are thin match-and-delegate. Adding `Reference` was a one-arm extension everywhere — that's the test of a good architecture.
- **`kept://<uuid>` URL convention.** Started for `@`-mentions, transparently generalized to reference cells. Clicking a link, navigating to an entity, navigating to a referenced cell — all the same code path through `handle_link_click`.
- **Persistence schema versioning.** Five migrations (v2→v5) with thoughtful backfills. The migration helpers (`extract_title_from_body_json`, `backfill_entities_from_persons`) are idempotent and well-commented. Good discipline.
- **`query.rs` is the role model.** ~730 lines, clear sections (AST → parser → serializer → executor), 28 tests, parser/round-trip coverage tight. When the other modules grow up they should look like this.
- **Multi-pane via `Deref<Target=Pane>`.** Avoided ~309 call-site rewrites by letting `KeptApp` deref to its active `Pane`. Smart.
- **Naming consistency.** `*_id`, `*_at_doc_pos`, `last_*_rect`, `is_hovering_*` patterns all hold across files. Easy to scan.
- **Comments explain *why*, not *what*.** Migrations, multi-pane decisions, focus-mode rationale — the comments that matter are present and load-bearing.

## What's bad

### Structural

- **`cell.rs` is five files in a trench coat.** TextBox (1,859 LOC), OutlineCell (1,050), Cell aggregator (1,100), TableCell (563), PopPopCell (261), ReferenceCell (95). Each is mostly self-contained. There's no reason they share a file.
- **`app.rs` is the kitchen sink.** `KeptApp` impl (lines 874–6,587, ~5,700 LOC) blends ~15 distinct responsibilities (pane mgmt, undo, persistence flush, sidebar render, search popup, mention popup, three context menus, two custom pages, embed render, mouse dispatch, key dispatch, navigation). Most of these are independent of each other.
- **`persist.rs` mixes JSON schema, cell CRUD, entity CRUD, and migrations** in one ~1,400-line file with one big `impl Db`.
- **18 `last_*_rect(s)` fields scattered on `KeptApp`.** All are "last frame's hit-test rect for UI element X." Zero grouping.

### Duplication

- **Geometry accessor boilerplate, 6× identical.** `x_origin/y_origin/width/height/font_scale` + `set_view_geometry` + `set_font_scale` repeats verbatim across `TextBox`, `OutlineCell`, `PopPopCell`, `TableCell`, `ReferenceCell`, and `Cell`. ~100 LOC of duplication. Begs for a `BodyGeometry` trait.
- **Three context menus, three near-identical render skeletons.** `render_cell_context_menu` (177 LOC), `render_tag_context_menu` (71), `render_people_context_menu` (127). All do shadow → bg → border → row loop with hover detection. A `draw_context_menu_background` + `draw_context_menu_row` helper would collapse ~250 LOC of skeleton.
- **Mouse-down hit-test pattern, 15× repeated.** Every overlay (sidebar, search input, three context menus, entity-page buttons, people-page rows, embed cards) has the same `if rect contains point { do action; return true; }` check. Could be a small table-driven helper or at least a `point_in_rect` predicate (the inline version is 4 boolean ANDs).
- **Cell-render loop is two implementations.** Once in `tick_pane` (timeline cells), once in `render_entity_page`'s "REFERENCED IN" section (entity-page embeds). Different control flow but same shape: layout each item, draw wrapper, capture rect.

### Encapsulation breaks

- **`clone_cell_kind_for_cache` in `app.rs` reaches into `cell.rs`'s pub fields.** Walks `TextBox.text()`, `.links()`, `OutlineCell.bullets()`, `TableCell.rows_view()` to build a deep copy. Ought to be a method on `CellKind` (or a trait `BodyClone`) inside cell.rs. App layer should ask for "clone scaled to N" and not care how.
- **`build_reference_cache` similarly synthesizes `Cell` parts** (kind, title clone, scale propagation) that the cell module should own.
- **`Cell::title` is a public `Option<TextBox>`.** Mutated by app.rs in several spots. Should be behind methods (`title()`, `title_mut()`, `set_title()`).

### Inefficiencies

- **`Cell::all_link_urls() -> Vec<String>`** clones every URL across every body kind. Used per-frame on the Person page. Replace with `Cell::has_link_url(&str) -> bool` (closure or string predicate) for the hot path.
- **Per-frame cache rebuild on the Person page.** Each "REFERENCED IN" embed builds a fresh `Box<Cell>` cache via `build_reference_cache` every frame. Cheap but wasteful — and it's why selection inside Person-page embeds doesn't persist (see `FOLLOWUPS.md`). Future: persistent cache map keyed by `target_cell_id`.
- **`ViewKind::Entity` page rebuilds the mention list per frame** by walking all cells and calling `all_link_urls()` on each. O(cells × links) per frame. Cache the list and invalidate on edit.
- **Big render functions reallocate `Vec<(Uuid, Rect)>` every frame** for sidebar rects, etc. `Vec::clear` + reuse would skip the allocation.

### Big-function pile-up

| Function | Lines | Verdict |
|---|---|---|
| `KeptApp::handle_key` | 607 | Legitimately complex (15+ keybindings, modal interceptors). Split by mode. |
| `KeptApp::tick_pane` | 381 | Legitimately complex (5 view kinds × focus mode × scrollbar). Split by view kind. |
| `KeptApp::mouse_down` | 341 | **Worst offender.** ~15 copy-paste hit-test branches. Refactor before splitting. |
| `KeptApp::render_entity_page` | 309 | Embed sub-loop should extract; rest is layout. |
| `OutlineCell::handle_key` | ~300 | Selection mode + indent/outdent + split/merge dispatch. Split by sub-mode. |
| `OutlineCell::tick` | ~270 | Layout + draw decorations + bullet loop. Extract helpers. |
| `KeptApp::render_sidebar` | 244 | Repetitive rect capture. Extract `SidebarRects` substruct. |
| `TextBox::tick` | ~220 | Selection highlights + line draw + caret + tags. Extract helpers. |
| `KeptApp::render_search_popup` | 177 | Result-row loop should extract. |
| `KeptApp::render_cell_context_menu` | 177 | Skeleton + 3 rows. Helper. |
| `KeptApp::render_people_page` | 215 | Row loop + inline edit. Extract `render_people_row`. |

## What's tested vs. what isn't

### Strong coverage
- **Query parser** — every grammar path; round-trips.
- **TextBox link mechanics** — typing-after, typing-inside, undo, split-preserves-link, enter-then-backspace.
- **PopPop calc engine** — eval, comments, errors, variable threading.
- **Title parsing** — name vs. trailing tags.
- **Fuzzy match scoring** — camelCase, separators, initials.

### Critical gaps
1. **Persistence round-trips for `Plain`, `Outline`, `PopPop`, `Table`.** Only `Reference` is round-tripped (the test I added with v1). A serialization bug in any other variant silently loses data.
2. **`ReferenceCell` cache behavior.** Snapshot/restore, staleness detection on `edited_at` change, cache rebuild correctness, dangling-target handling. Untested.
3. **Query executor (`matches`, `MatchContext`)**. Parser is great; the function that actually filters cells against an AST has zero tests.
4. **App-level navigation, undo, multi-pane state, embed lifecycle.** Zero tests. Easiest to add as units under extracted modules.
5. **Title round-trip.** Migration extracts titles (covered); no test verifies edit→save→load round-trips a title.

### Test quality
- Existing tests are well-asserted and well-commented (regression-focused, e.g., `link_survives_enter_then_backspace`).
- Boilerplate (`typeface()` helper repeated everywhere) could become a fixture.
- Few negative cases (e.g., "ReferenceCell rejects edits" — that invariant has no test).

## The plan

Three phases. Each phase is a coherent body of work that ends with green tests and a runnable app. **Don't start phase N+1 until N's tests are passing.**

### Phase 0: Safety net (1–2 days)

**Goal**: tests in place before any structural moves. Refactoring without these is gambling.

1. **Persistence round-trip per CellKind variant.** Five new tests in `persist.rs::tests`: build a `Cell` of each variant (Plain, Outline, PopPop, Table, Reference), serialize via `persisted_cell_from`, deserialize via `body_to_kind`, assert deep equality (text, links, depth, bullet ids, table dims, target). Reuse the v1 reference test as the template.
2. **`ReferenceCell::cache_is_stale_for` + cache lifecycle tests.** Three tests: stale on edited_at bump, stale on missing target, stale on cache absent. Pure unit tests, no DB.
3. **`query::matches` smoke tests.** Five tests: tag-only, entity-only, time-only, combined include, exclude. Just enough to catch grammar-vs-execution drift.
4. **Title round-trip.** One test: create a Plain cell with a title, snapshot, restore, assert title text + tags survive.

End state: ~14 new tests, total ~79. Zero structural changes yet.

### Phase 1: Split `cell.rs` (3–5 days)

The cleaner split (lower coupling than app.rs). Bottom-up extraction:

1. **`cell/common.rs`** — `Affinity`, `Selection`, `Selections`, `Edit`, `LinkSpan`, `TextBoxSnapshot`, `primary_mod`/`word_mod`/`line_edge_mod`, transform-index helpers, geometry trait `BodyGeometry { fn x_origin/y_origin/width/height/font_scale + setters }`. ~150 LOC.
2. **`cell/wrap.rs`** — `wrap_text_styled`, `wrap_paragraph_into`, word-boundary helpers (`char_class`, `find_word_at`, etc.). ~150 LOC.
3. **`cell/textbox.rs`** — `TextBox` + impl, `DragKind`, `DragState`, `draw_line_with_links`. ~1,860 LOC. Implements `BodyGeometry`.
4. **`cell/outline.rs`** — `Bullet`, `BulletSnapshot`, `OutlineCell`, `OutlineSnapshot`, `OutlineDrag`, `BulletSelection`. ~1,050 LOC. Implements `BodyGeometry`.
5. **`cell/poppop.rs`** — `PopPopCell`, `compute_poppop_output`. ~260 LOC.
6. **`cell/table.rs`** — `TableCell`, `TableEntry`, `TableSnapshot`. ~565 LOC.
7. **`cell/reference.rs`** — `ReferenceCell`, `ReferenceTarget`. ~95 LOC.
8. **`cell/cell.rs`** — `Cell`, `CellKind`, `CellSnapshot`, `CellSnapshotKind`, the dispatch impl. ~1,100 LOC. Owns title-slot logic.
9. **`cell/mod.rs`** — re-exports + module organization. ~50 LOC.

**During the move, also:**
- Add the `BodyGeometry` trait, replace ~100 LOC of repeated accessors.
- Add `CellKind::clone_for_scale(typeface, scale) -> Option<CellKind>` so `app.rs` stops reaching into pub fields.
- Make `Cell::title` private; expose `title()`, `title_mut()`, `take_title()`, `set_title()`.

End state: 9 files in `src/cell/`, average ~700 LOC each. App.rs's coupling to cell.rs internals reduced.

### Phase 2: Split `persist.rs` (1 day)

Smaller, well-bounded:

1. **`persist/schema.rs`** — `CellBody`, `BlockRecord`, `LinkRecord`, `TableEntryRecord`, `ReferenceTargetRecord`, `PersistedCell`, migration helpers (`extract_title_from_body_json`, `take_heading_from_body`, etc.). ~300 LOC.
2. **`persist/cells.rs`** — `cell_to_body`, `body_to_kind`, cell load/save methods on `Db`. ~400 LOC.
3. **`persist/entities.rs`** — entity CRUD, alias index, `cell_to_entity_index`. ~300 LOC.
4. **`persist/contexts.rs`** — context CRUD. ~80 LOC. (Optional split; tiny.)
5. **`persist/mod.rs`** — `Db` struct + open/migrate, glue. ~350 LOC.

The Phase 0 round-trip tests catch any serialization breakage during the move.

### Phase 3: Tame `app.rs` (~3 weeks, multiple PRs)

The painful one. Bottom-up by isolation:

**3a. HitTestState refactor (1 day, prep work).** Consolidate the 18 `last_*_rect(s)` fields into a `HitTestState` substruct on `KeptApp`, grouped by UI surface. No behavior change; cleans up signatures everywhere.

**3b. Extract isolated overlays (3 days).**
- `app/search.rs` — `SearchState`, `render_search_popup`, `open/close_search_*`, `search_results`, `search_*_to_clipboard`. Self-contained.
- `app/mention_popup.rs` — `MentionPopup`, `MentionSource`, `filter_mentions`, render + open/sync/move/commit. Move the 5 fuzzy-matching tests with it.

**3c. Extract context menus (2 days).**
- `app/context_menus.rs` — three `*ContextMenu` structs, three render methods, common `draw_context_menu_background` + `draw_context_menu_row` helpers. Collapses ~250 LOC of duplication.

**3d. Extract sidebar (2 days).**
- `app/sidebar.rs` — `render_sidebar` + `SidebarRects`. Hit-test routing stays in mouse_down for now.

**3e. Extract embeds (1 day).**
- `app/embeds.rs` — `render_reference_cell`, `build_reference_cache`, `draw_embed_wrapper`, `render_embed_placeholder`. Depends on `cell.rs::CellKind::clone_for_scale` from Phase 1.

**3f. Extract pages (4 days).**
- `app/entity_page.rs` — `render_entity_page` + helpers. Pull the "REFERENCED IN" loop into `render_entity_page_embeds`. Depends on `app/embeds.rs`.
- `app/people_page.rs` — `render_people_page` + rename/add/delete/toggle helpers. Extract `render_people_row`.

**3g. Extract subsystems (4 days).**
- `app/nav.rs` — `Query` impl, `push_view`, `nav_back`, `nav_forward`, `restore_history_entry`, `rotate_view_to`. Pure logic, no I/O.
- `app/undo.rs` — `UndoOp`, `ContextSideEffect`, `record_edit`, `undo`, `redo`. Add tests for round-trips.
- `app/persistence.rs` — `maybe_flush_persistence`, `flush_persistence`, dirty/pending tracking.
- `app/cell_stream.rs` — `visible_cell_ids`, `insert_cell_sorted`, `mark_cell_dirty`, `touch_cell`, `prev_visible`, `next_visible`.
- `app/pane.rs` — `Pane`, `SplitDir`, layout/split/close, pane chord, `set_active_pane`.

**3h. Refactor mouse_down (3 days, hardest).**
After everything else extracted, rewrite `mouse_down` as a clean dispatch:
```rust
let route = self.hit_test(point);  // returns enum
match route { HitTest::Sidebar(...) => ..., HitTest::CellMenu(row) => ..., ... }
```
Each module exposes its hit-test contribution. Replaces 341 LOC of nested if-let with a flat match.

**3i. Final consolidation (1 day).**
- `app/mod.rs` keeps `KeptApp` struct, `tick`, `tick_pane`, `handle_key`, `new`, public API.
- Re-export submodule types.
- `tick_pane` stays for now (could split by view kind in a later phase but not urgent).
- `handle_key` stays for now (split by mode is a separate refactor).

End state: ~12 files in `src/app/`, average ~500 LOC each. `mod.rs` ~1,000 LOC of orchestration.

### Phase 4: Polish (ongoing)

Pull off the smaller smells once the big moves are done:
- Replace `all_link_urls()` with `has_link_url(&str)` predicate.
- Cache the entity-page mention list (invalidate on edit).
- Persistent cache for entity-page embeds (closes the selection gap from `FOLLOWUPS.md`).
- Test fixtures (drop `typeface()` boilerplate).
- Split `handle_key` by modal mode (search/mention/menu/normal). Optional.
- Split `tick_pane` by view kind (Ast/Context/Entity/People). Optional.
- `OutlineCell::handle_key` and `tick` extractions (helper methods for layout & draw).

## Order, scope, and risk

| Phase | Risk | Days | What ships |
|---|---|---|---|
| 0: Safety-net tests | Very low | 1–2 | ~14 new tests; no behavior change |
| 1: Split `cell.rs` | Medium | 3–5 | 9 files; trait extraction; tighter encapsulation |
| 2: Split `persist.rs` | Low | 1 | 4–5 files |
| 3a: HitTestState | Low | 1 | One field refactored, signatures cleaner |
| 3b–3f: Extract overlays/pages/embeds | Medium | ~12 | 7 new modules; context-menu duplication gone |
| 3g: Extract subsystems | Medium | ~4 | 5 more modules; testable nav/undo/persistence |
| 3h: Mouse_down rewrite | Higher | 3 | Hit-test dispatch is flat |
| 3i: Final consolidation | Low | 1 | `app/mod.rs` is 1k of orchestration |
| 4: Polish | Low | ongoing | Smell removals as opportunity arises |

**Total**: ~30 working days for a complete restructure. Realistically 3–5 weeks of part-time work.

## Don't-do list

A few things the audits flagged that I'm explicitly recommending **against**:

- **Don't split `query.rs`.** It's 731 LOC, well-organized, well-tested. Splitting would be over-engineering. Revisit only if the parser grows nested expressions or the executor gains new matchers.
- **Don't split `handle_key`/`tick_pane` by mode/view in Phase 3.** They're long but legitimately complex; their current shape is readable. Defer to Phase 4 if the file feels heavy after the rest of `app.rs` is gone.
- **Don't introduce a `BodyClone` trait if `CellKind::clone_for_scale` is enough.** A method is simpler than a trait. Add the trait only if a third caller needs the same operation.
- **Don't extract a `cell/cell.rs` "aggregator" module separately from `cell/mod.rs`.** Just make `cell/mod.rs` host `Cell` + `CellKind`; the body modules sit beneath. One fewer file.
