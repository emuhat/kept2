//! Search executor: free functions used by the pane URL-bar pill
//! to populate its suggestion dropdown. Pre-pill this module also
//! owned a standalone Ctrl+K popup (`SearchState`); now the pill
//! plays that role and only the search executor + result-snippet
//! helpers live here.

use uuid::Uuid;

use crate::cell::{Cell, now_epoch_ms};
use crate::document::Document;
use crate::entity_cache::EntityCache;
use crate::query;

use super::local_date_for_ms;

const SEARCH_SNIPPET_LEN: usize = 80;

/// Top-N matching cell IDs for the suggestion dropdown. Parses
/// `query` through the language, runs the executor, and sorts
/// most-recent first. Free function so anyone who needs cell search
/// (URL-bar dropdown today; previously the Ctrl+K popup) can call
/// it without owning an input.
pub(super) fn search_results(
    query: &str,
    document: &Document,
    entities: &EntityCache,
    show_inactive_cells: bool,
) -> Vec<Uuid> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let ast = query::parse(query);
    let ctx = query::MatchContext {
        today: local_date_for_ms(now_epoch_ms()),
        person_targets: query::resolve_persons(
            &ast.include.entities,
            &entities.alias_index,
            &entities.title_fallback,
        ),
        person_excludes: query::resolve_persons(
            &ast.exclude.entities,
            &entities.alias_index,
            &entities.title_fallback,
        ),
    };
    // Inactive cells drop out of search unless the global "Show
    // archived" toggle is on, mirroring `is_visible_for_view`.
    // Otherwise an archived cell could surface here, the user clicks
    // it, and navigates to a single-cell view that has it filtered
    // out — a dead-end click. Scratch cells are excluded
    // unconditionally — they're transient by design and shouldn't
    // surface in the URL-bar dropdown.
    let mut hits: Vec<&Cell> = document
        .cells
        .iter()
        .filter(|c| {
            (c.is_open() || show_inactive_cells)
                && !c.scratch
                && query::matches(&ast, c, &ctx)
        })
        .collect();
    hits.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    hits.into_iter().map(|c| c.id).collect()
}

/// Build a result-row snippet centered on the residual text portion
/// of `query` (so `#tag` / `@person` / time tokens don't drive
/// centering). Falls back to the head of the cell text when the
/// residual is empty.
pub(super) fn result_snippet(text: &str, query: &str) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let residual = query::parse(query).text;
    let lower = flat.to_lowercase();
    let needle = residual.to_lowercase();
    let center = if needle.is_empty() {
        0
    } else {
        lower.find(&needle).unwrap_or(0)
    };
    let pre = SEARCH_SNIPPET_LEN / 2;
    let start_chars = center.saturating_sub(pre);
    let end_chars = (start_chars + SEARCH_SNIPPET_LEN).min(flat.chars().count());
    let mut iter = flat.chars();
    let snippet: String = iter
        .by_ref()
        .skip(start_chars)
        .take(end_chars - start_chars)
        .collect();
    let prefix = if start_chars > 0 { "…" } else { "" };
    let suffix = if end_chars < flat.chars().count() { "…" } else { "" };
    format!("{prefix}{snippet}{suffix}")
}
