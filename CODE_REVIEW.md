# Code review & cleanup plan (v2)

A fresh structural read of the codebase, contrasted against the v1 review.
The v1 plan moved several pieces, but the central debt — a god-object
`KeptApp` and a hand-rolled 5-arm dispatch over `CellKind` — is still here
and has compounded as the app gained features (people page, envelope
outlines, multi-pane, span tags, active/inactive flags, toasts).

Headline: **the structural problems aren't where the file boundaries are.**
Splitting files without splitting *responsibility* hasn't helped, and in
two cases made the picture harder to read.

## Headline numbers

| File | Lines | Verdict |
|---|---|---|
| `src/app/mod.rs` | 7,839 | God object. Grew despite four submodules being broken out. |
| `src/cell.rs` | 2,654 | Pure dispatcher: 217 `CellKind::` references, 43 match blocks, ~61 tests. |
| `src/persist.rs` | 2,068 | Schema + cells + entities + contexts + migrations in one `impl Db`. |
| `src/cell/textbox.rs` | 1,930 | 29-field god widget; one impl block. |
| `src/cell/outline.rs` | 1,617 | Multi-bullet container; reimplements much of TextBox's selection/copy plumbing. |
| `src/app/mention_popup.rs` | 914 | `impl KeptApp` block + state struct in the same file. |
| `src/query.rs` | 887 | Still the role model. Leave alone. |
| `src/color.rs` | 792 | Live-reload color config. Self-contained. |
| `src/cell/table.rs` | 688 | Grid container. Parallel boilerplate with outline. |
| `src/app/context_menus.rs` | 565 | Three menus + shared `draw_menu_card`/`draw_menu_row`. Still `impl KeptApp`. |
| `src/cell/wrap.rs` | 519 | Pure layout/parse utilities. Clean. |
| `src/cell/poppop.rs` | 369 | Calc cell. Self-contained. |
| `src/app/search.rs` | 361 | `impl KeptApp` extension. |
| `src/main.rs` | 351 | OpenGL/winit setup. Fine. |
| `src/app/sidebar.rs` | 344 | `impl KeptApp` extension. |
| `src/cell/common.rs` | 244 | Shared types + helpers. Clean. |
| `src/cell/reference.rs` | 232 | Pointer + cache forwarder. |
| `src/cell/grid.rs` | 36 | Shared stripe/divider helpers. |
| **Total** | **22,410** | (was 15,414 in v1; +45% over ~6 weeks of feature work) |

| Tests | Count | Notes |
|---|---|---|
| `src/cell.rs` | 61 | Strong coverage of TextBox link/tag/edit edge cases, outline split/merge, active-flag round-trips |
| `src/query.rs` | 33 | Parser + executor |
| `src/persist.rs` | 17 | Round-trips for every `CellKind` variant — v1 Phase-0 work landed |
| `src/app/mod.rs` | 5 | Helper-level only |
| `src/app/mention_popup.rs` | 5 | Fuzzy match |
| **Total** | **121** | (was 65 in v1; nearly doubled — most of the gain in `cell.rs`) |

## What got done since v1

Concrete progress — worth naming so we don't re-litigate it.

- **Phase 0 safety net.** Round-trip tests for every persisted CellKind variant landed (persist.rs:1662-2068, ~17 tests). This is the floor that makes the rest of the refactor safe.
- **Phase 1: `cell.rs` split.** `cell/{common,wrap,textbox,outline,poppop,table,reference,grid}.rs` all exist. Parent `cell.rs` is the dispatcher only.
- **Phase 3a: `HitTestState`.** The 18 scattered `last_*_rect` fields collapsed into `HitTestState` (app/mod.rs:112).
- **Phase 3c: context-menu skeleton helpers.** `draw_menu_card` + `draw_menu_row` (context_menus.rs:87, :117) collapsed the duplicated render skeletons across cell / tag / people menus.
- **Encapsulation cleanup.** `Cell::title` is private with accessors; `CellKind::clone_for_scale` (cell.rs:163) replaced the app's reach-in clone helper.
- **`grid.rs` shared visual helpers** between Table and PopPop (the "1 source of truth for the calc-blue stripe" line is exactly the right pattern).

## What didn't get done — and why it still matters

These are the v1 items that are still on the table. They've gotten worse, not better, because the app grew around them.

- **Phase 2: `persist.rs` split.** Still one 2,068-line file with schema, cell CRUD, entity CRUD, context CRUD, migrations, helpers all in one `impl Db`. Lower risk than ever now that round-trip tests exist — should be cheap to do.
- **Phase 3b/3d "submodule" extractions.** `search.rs`, `mention_popup.rs`, `sidebar.rs`, `context_menus.rs` were created — but each file just opens `impl KeptApp { ... }` and bolts more methods onto the same god struct. State is not isolated; method bodies can still touch any field on `self`. **This is the worst-of-both-worlds outcome:** the file is split (you have to jump around to read flow) but the dependency graph isn't reduced (any method can still touch any state).
- **Phase 3e–g: embeds, pages, subsystems.** None extracted. `tick_pane` (1,105 lines), `render_entity_page` (~305), `render_people_page` (~215), `render_envelope_outline_cell` (~200) all still live on `KeptApp`.
- **Phase 3h: `mouse_down` rewrite.** Still 15+ hit-test branches in `dispatch_doc_click` (~250 lines) and `dispatch_sidebar_click` (~85 lines).
- **`all_link_urls()` per-frame allocation.** Still allocates a `Vec<String>` of every URL across every cell each frame on the Entity page. (cell.rs:881; consumers at app/mod.rs:4979, 5353.)

## What's new since v1 — and the debt it added

The feature work between v1 and now landed a lot of value, but several pieces accreted new structural problems:

- **Multi-pane (`Pane`, `KeptApp: Deref<Target=Pane>`).** v1 praised this as smart; six weeks later it's migration scar tissue. 50+ implicit derefs in `tick_pane` alone, and `Pane` itself derefs to `Scroller`. Readers can't tell from a call site which fields are per-pane vs. global without checking the struct. Was right at the time; is friction now.
- **Entity caches** (`entities`, `entity_alias_index`, `cell_to_entity`, `entity_title_fallback` on KeptApp:1322-1336). Invariants are documented as #1-#7 in a comment but enforced nowhere. `refresh_entities` (app/mod.rs:2691) is called manually from ~15 sites. Forgetting one = silently stale UI.
- **Span-based tags.** Replaced the parse-from-text approach. Right call architecturally, but `TextBox::apply_edit` (textbox.rs:1784) now has *parallel* transform loops over `links` and `tags` with near-identical gravity logic. Span maintenance is now a 4-concern choke point: text mutate → selections → links → tags → rewrap.
- **Active/inactive flags.** Three more `UndoOp` variants (`SetCellActive`, `SetBulletActive`, `SetEntityActive`); the `undo`/`redo` match blocks grew to 13 arms each, ~200 lines apiece, fully duplicated in structure.
- **Envelope outlines & embed depth.** `Reference` cells now sometimes contain a cached `Cell` that contains a `CellKind::Outline` whose `reference_header` is itself a `ReferenceTarget`. Recursive embed depth cap (`MAX_EMBED_DEPTH = 4`) exists, but the cache lifecycle and staleness checks are still repeated across `render_reference_cell` and `render_envelope_outline_cell`.
- **Toasts, people-rename, people-add, people-context-menu, tag-context-menu.** Five more `Option<...>` fields on `KeptApp` for transient UI overlays.

The pattern across all of these: **new feature → new field on `KeptApp` → new arm in undo/redo → new render code in `tick_pane` → new branch in `mouse_down` dispatch.** Nothing forces (or even encourages) features to land as cohesive units; they smear across the god object.

## What's good (preserve)

- **`query.rs`** is still the role model. AST → parser → serializer → executor, 33 tests, parser/round-trip coverage tight.
- **`persist.rs` migrations.** Seven versioned, idempotent migrations with thoughtful backfills. The discipline is excellent even if the file got long.
- **`kept://<uuid>` URL convention.** Started for @-mentions, now generalizes to entity nav, reference cells, embed clicks. One code path.
- **Persistence round-trip tests** added in Phase 0. They're the safety net the rest of the plan rests on.
- **Per-cell-type files** post-split. `cell/textbox.rs`, `cell/outline.rs`, etc. are big but self-contained — the body types don't reach into each other.
- **`cell/grid.rs`** and `cell/common.rs` — the right shape for shared helpers.
- **`MentionPopup` state struct** (mention_popup.rs:21). Owns its own data; just needs its methods to follow.
- **Comments explain *why*, not *what*.** Migration intent, multi-pane Deref rationale, focus-mode invariants, embed cache staleness — the load-bearing comments are present.

## What's bad — the current diagnosis

### Structural

- **`KeptApp` is a textbook god object.** ~40 fields: cells, contexts, panes, undo/redo, dirty sets, pending deletes (×2), six overlay `Option`s, sidebar scroll, hit-tests, four entity caches, mouse pos, alt-pan state, toast, clipboard, db. (app/mod.rs:1238-1355.)
- **The `app/` submodule split is cosmetic.** `search.rs:35`, `sidebar.rs:62`, `context_menus.rs:156`, `mention_popup.rs:183` all do `impl KeptApp { ... }`. The file boundary tells you nothing about which state a method touches.
- **`CellKind` dispatch is hand-coded ~200 times.** `cell.rs`: 217 `CellKind::` references, 43 match blocks. `app/mod.rs`: 50 more references, 30 more matches. v1 considered this acceptable because "adding `Reference` was a one-arm extension"; that's still true *per dispatch site*, but the cost is now spread across hundreds of sites. Adding a sixth kind today is a search-and-replace exercise.
- **`TextBox` is a 29-field accretion.** Text + multi-cursor selections + wrap cache + geometry + mouse drag + click-count ladder + styling flags (`force_heading`, `enable_comment_coloring`, `text_color`, `line_extra_below`) + link spans + tag spans + undo/redo + pending-click-link/tag buffers. One impl block from textbox.rs:127 to ~1850.

### Weird logic flow

- **Render mutates input state.** `hit_tests` is populated *during* `tick_pane` rendering (30+ mutation sites in a 1,105-line method) and consumed by `mouse_down` next frame. Single biggest "weird flow" smell. No invalidation if rendering is skipped or geometry changes mid-frame.
- **`TextBox::apply_edit`** (textbox.rs:1784) mutates text → transforms every selection index → transforms every link span (closed-right gravity) → transforms every tag span (closed-right gravity + `#` revalidation) → invalidates wrap cache. Four parallel transform loops; adding a fifth span type means a fifth loop. Should be a single `Vec<Span>` with a kind tag.
- **`TextBox::paste`** (textbox.rs:736) conflates text insertion with URL auto-detection and link rebase. Edit semantics and content-classification semantics in the same method.
- **`TextBox::mouse_down`** (textbox.rs:1199, ~105 lines) handles link/tag hit-detection, click-count ladder, drag state setup, AND multi-cursor add — four interaction modes resolved in one function.
- **Undo and redo are parallel 13-arm beasts.** `undo` (app/mod.rs:5593, 206 lines) and `redo` (5799, 186 lines) each match every `UndoOp` variant and hand-code the inverse mutation across cells, dirty sets, focus, contexts, entities, DB. Adding an op = touching both in lockstep.
- **Dirty-marking is inconsistent.** Some mutation paths call `mark_cell_dirty` + `touch_cell`; some only one; `insert_cell_after_focused` (app/mod.rs:6150) relies on a side effect of `insert_cell_sorted`. A silent miss = a persistence bug.
- **Entity cache invariants are documented in a comment, not in types.** `refresh_entities` (app/mod.rs:2691) called from ~15 sites; nothing stops a stale read between mutations and refresh.

### Duplication

- **Multi-textbox containers reimplement parallel boilerplate** (outline.rs / table.rs / poppop.rs): `mouse_drag_to`, `mouse_up`, `clear_all_selections`, `link_at_doc_pos`, `tag_at_doc_pos`, `take_pending_link_url`, `take_pending_tag_name`. Each is "loop over inner textboxes and OR the results", coded three times. ~150–200 LOC.
- **Reference cache lifecycle** is hand-rolled in two places: `render_reference_cell` (app/mod.rs:2015) and `render_envelope_outline_cell` (app/mod.rs:2138). Both do staleness check → detach → rebuild → attach. No shared helper.
- **Hit-test dispatch pattern** repeated ~15× in `dispatch_doc_click` and `dispatch_sidebar_click`: clone-vec-of-rects → iterate → point-in-rect → dispatch. Each overlay surfaces does it independently.
- **Snapshot/restore loop-and-call** is reimplemented per cell type (bullet vector, row-major grid, etc.) with identical iteration shape.

### Big-function pile-up (updated from v1)

| Function | Lines | Verdict |
|---|---|---|
| `KeptApp::tick_pane` | ~1,105 | Was 381 in v1 — has tripled. Now does: doc area, focus card, focus ring, cell loop, sidebar, entity page, people page, toast, context menus, hit-test mutation. **Worst single function in the codebase.** |
| `KeptApp::handle_key` | ~686 | Was 607. Pane chords + search + mention popup + cell creation + context rotation + focus nav + undo/redo + link navigation, interleaved. |
| `KeptApp::dispatch_doc_click` | ~250 | The post-`mouse_down`-rewrite-that-wasn't. 15+ branches. |
| `KeptApp::undo` | 206 | 13-arm match; mirrors `redo`. |
| `KeptApp::redo` | 186 | Mirror of `undo`. |
| `KeptApp::render_entity_page` | ~305 | Entity title + meta + references list + toggle buttons + context-menu hit-test recording. |
| `KeptApp::render_envelope_outline_cell` | ~204 | Embed render + bullet body, mirrors `Cell::tick`. |
| `KeptApp::render_people_page` | ~215 | Row loop + rename/add inputs + context menus. |
| `TextBox::tick` | ~210 | Layout + selection paint + link/tag overdraw + caret. |
| `OutlineCell::tick` | ~135 | Multi-bullet layout + tag filter + active state + overlay. |
| `KeptApp::delete_cell_by_id` | ~115 | Five-arm CellKind match + three deletion paths + context side effects. |

## Recommended structural seams

In priority order. The numbering picks up from v1 since some items there are still live.

### High leverage, low risk

**S1. `CellKind` → `CellBody` trait.** The single highest-ROI move.

```rust
trait CellBody {
    fn tick(&mut self, canvas: &Canvas, x: f32, y: f32, w: f32, focused: bool, show_caret: bool) -> f32;
    fn handle_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool;
    fn mouse_down(&mut self, abs_x: f32, abs_y: f32, modifiers: &Modifiers, editing: bool) -> bool;
    fn mouse_drag_to(&mut self, x: f32, y: f32) -> bool;
    fn mouse_up(&mut self) -> bool;
    fn copy_text(&self) -> String;
    fn cut_text(&mut self) -> String;
    fn paste_text(&mut self, s: &str);
    fn link_at_doc_pos(&self, x: f32, y: f32) -> bool;
    fn tag_at_doc_pos(&self, x: f32, y: f32) -> bool;
    fn take_pending_link_url(&mut self) -> Option<String>;
    fn take_pending_tag_name(&mut self) -> Option<String>;
    fn all_link_urls_into(&self, out: &mut Vec<String>); // no Vec allocation
    fn iter_textboxes<'a>(&'a self) -> Box<dyn Iterator<Item = &'a TextBox> + 'a>;
    fn iter_textboxes_mut<'a>(&'a mut self) -> Box<dyn Iterator<Item = &'a mut TextBox> + 'a>;
    fn snapshot(&self) -> CellBodySnapshot;
    fn restore(&mut self, snap: CellBodySnapshot);
    // ...
}
```

- Default trait methods for `mouse_drag_to`, `mouse_up`, `clear_all_selections`, `link_at_doc_pos`, `tag_at_doc_pos`, `take_pending_link_url`, `take_pending_tag_name` — all expressible in terms of `iter_textboxes_mut`. Collapses ~150–200 LOC of multi-textbox boilerplate.
- `CellKind` can stay an enum (one match, dispatching to the trait) or become `Box<dyn CellBody>`. Either way, the host stops re-listing five variants per method.
- `Reference`'s "I wrap a cached Cell" structure works fine — `Reference::iter_textboxes` returns the cache's.
- Cost: a few weekend's worth of mechanical change; the round-trip tests will catch breakage.

**S2. Hit-tests as a frame snapshot, not a render side effect.**

- During `tick_pane`, append rects to a `HitTestBuilder`. At end-of-frame finalize it into a frozen `HitTests` value. `mouse_down` reads only from the frozen value.
- If a frame's render is skipped, the previous frame's hit-tests are valid (or explicitly cleared on focus loss).
- Cheap to implement; opens the door to splitting `tick_pane`.

**S3. Unify `TextBox` spans.**

- Replace `links: Vec<LinkSpan>` + `tags: Vec<TagSpan>` with `spans: Vec<Span>` where `Span { range, kind: SpanKind, gravity, payload }`.
- The four transform loops in `apply_edit` collapse to one. Adding highlights / footnotes / future mention types becomes a kind variant.
- ~50 LOC reduction; bigger win is conceptual.

**S4. Centralize dirty discipline.**

- A `Document` struct (or just a `DocMut` newtype around `&mut KeptApp`) where every cell mutation goes through a path that flips dirty bits + touches `edited_at` + (optionally) records undo. Today the rules are sprinkled across each call site.
- One source of the rule is also the right hook to fire entity-cache invalidation (S6) and pending-link-url draining.

### Medium leverage, moderate risk

**S5. Carve `KeptApp` into named subsystems.** The shape from the new review:

- `Document { cells, contexts, dirty_cells, pending_deletes, dirty_contexts, pending_context_deletes }`. Mutation API enforces dirty discipline (S4).
- `EntityCache { entities, alias_index, cell_to_entity, title_fallback }`. `refresh(&db, &cells)` is the only entry point. Replaces the ~15 manual `refresh_entities` calls with explicit invalidation hooks.
- `PaneTree { panes, active, split_ratio, dragging_divider, split_dir }`. Pane split/close/layout/focus lives here. **Drops the `Deref<Target=Pane>` magic** — call sites become `panes.active().view` (explicit about scope).
- `Overlays { mention_popup, search, cell_menu, tag_menu, people_menu, people_rename, people_add, toast }`. Six `Option<...>` fields become one struct field.
- `UndoLog { undo, redo }` with `record(&mut self, op)` and `apply(&mut self, dir, doc, entities, ...)`. Houses the dispatch.
- `KeptApp` shrinks to `Document`, `EntityCache`, `PaneTree`, `Overlays`, `UndoLog`, `HitTests`, `Clipboard`, `Db`, plus render/input glue.

**S6. Make undo apply data-driven.**

- Give each `UndoOp` variant an `fn apply(&self, ctx: &mut AppMut, direction: Dir)` (one match, two directions), or define each as a `(do, undo)` pair of named helpers.
- Today the two 200-line `match` blocks have to be kept in sync by hand. Adding a new op = three places to remember.

**S7. Break up `tick_pane`.**

- Now feasible because hit-tests are decoupled (S2). Split into `render_cell_stream`, `render_focus_card`, `render_scrollbar`, `render_sidebar_pass`, `render_overlay_pass`. Each takes a minimum slice of state.
- The cell-rendering helper is reused by entity-page and people-page (which today re-derive a lot of the same geometry).

**S8. Move per-subsystem methods off `impl KeptApp` and onto their state structs.**

- The `impl KeptApp for ... ` blocks in `search.rs`, `sidebar.rs`, `context_menus.rs`, `mention_popup.rs` get rewritten as methods on `SearchState`, `MentionPopup`, etc. The state structs already exist (S5 organizes them); this is making the methods follow.
- Each method then takes only the slice of context it needs (`&Document`, `&EntityCache`, `&mut HitTestBuilder`, etc.). Compile-time enforcement of scope.

### Cleanup (Phase 4 from v1, still relevant)

**S9. `all_link_urls()` → `has_link_url(&str)` predicate.** Hot path on Entity page. Per-frame allocation gone. (Or move to S1's `all_link_urls_into(&mut Vec<String>)`.)

**S10. Entity-page "REFERENCED IN" list cache.** Currently O(cells × links) per frame. Cache + invalidate on edit; entity-cache invalidation already exists post-S6.

**S11. Persistent embed cache.** Closes the selection-doesn't-persist-in-embeds gap noted in FOLLOWUPS.md.

**S12. Stratify `TextBox` styling vs. semantics.** Bundle `force_heading`, `enable_comment_coloring`, `text_color`, `line_extra_below` into a `TextBoxStyle` field — or pass style as a parameter to `tick()` rather than persisting it on the widget. Today setting any of them silently re-wraps.

**S13. `persist.rs` split.** v1 Phase 2 — never done. `schema.rs` / `cells.rs` / `entities.rs` / `contexts.rs` / `mod.rs`. Round-trip tests make it safe.

**S14. Drop the `KeptApp: Deref<Target=Pane>` once subsystems own their methods.** Mechanical sweep at the end of S5+S8.

## The plan (revised)

| Step | Risk | Effort | Unblocks |
|---|---|---|---|
| **S1**: CellBody trait | Low (round-trip tests cover) | ~3 days | Cuts 200+ dispatch sites; collapses multi-textbox boilerplate |
| **S2**: Hit-tests as snapshot | Low | ~1 day | Decouples render from input; prereq for S7 |
| **S3**: Unify TextBox spans | Low | ~1 day | Simplifies `apply_edit`; future-proofs span types |
| **S4**: Centralize dirty discipline | Low | ~1 day | Kills inconsistency footgun |
| **S5**: Carve subsystems | Medium | ~5 days | Foundation for S6/S7/S8; biggest readability win |
| **S6**: Data-driven undo dispatch | Medium | ~2 days | Halves the undo/redo file size |
| **S7**: Break up `tick_pane` | Medium | ~3 days | Readable render layer |
| **S8**: Methods onto subsystem structs | Medium | ~3 days | Actually achieves what the "app/ split" tried to |
| **S9–S12**: Cleanup pass | Low | ~3 days | Polish |
| **S13**: `persist.rs` split | Low | ~1 day | Symmetry with cell/ split |
| **S14**: Drop Deref-to-Pane | Low | ~1 day | Removes residual magic |

Total: ~24 working days. Doable in 3–4 weeks part-time.

**Order matters.** S1+S2+S3+S4 are independent and can land in any order; together they make S5 dramatically easier. S5 unblocks the rest. S7 needs S2; S6 needs S5; S8 needs S5.

**Suggested first PR:** S1 alone (CellBody trait). It's the biggest single readability win, has the strongest test coverage backing it, and doesn't touch app.rs at all. Worth doing solo to confirm the trait shape before stacking the rest on top.

## Don't-do list (updated)

- **Don't split `query.rs`.** Still the role model.
- **Don't split `handle_key` / `tick_pane` by mode/view.** Long but legitimately complex; their shape is readable. Defer if needed.
- **Don't add new feature surfaces during the refactor.** Each new `Option<...>` overlay field on `KeptApp` (toasts, people-add, people-rename, tag-context-menu, etc.) compounds the god-object problem. Land new features through whatever subsystem they belong to once S5 is in.
- **Don't refactor `mention_popup.rs` internals.** The 914-line file is busy but the logic is genuinely complex (fuzzy match + sync + commit + render). S8 moves its methods onto `MentionPopup`; that's enough.
- **Don't introduce a deeper Cell hierarchy.** S1's trait is enough. Resist the urge to make `OutlineCell` and `TableCell` share a `MultiTextBoxContainer` supertype — `iter_textboxes` on the body trait covers 90% of what that would buy.

## Verification checklist

After each step, the safety net to keep green:

- `cargo test` — 121+ tests must pass throughout.
- Manual smoke: open a date view, type in a cell, undo, redo, search, mention, switch panes, archive a cell, restart and verify it loaded.
- Compile-time: `cargo clippy` to catch newly-dead code.

The persistence round-trip tests in `persist.rs` are the keystone — any structural move that doesn't break them is almost certainly safe.
