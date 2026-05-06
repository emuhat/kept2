//! Query language v0.1 — parser, AST, executor.
//!
//! Token-based query language for filtering cells. Each query is a normalized
//! AST with `include` / `exclude` filter sets plus a free-text tail. Sigils:
//! `#tag`, `@person`, `-` for negation. Time expressions are a small fixed
//! grammar (`today`, `last week`, `2026-04-30`, ranges with `..`).
//!
//! The same AST powers both the search popup (typed text → AST) and the
//! sidebar's pre-built views (date click constructs `Day`-time AST, tag
//! click constructs a `tags` AST). `to_text` round-trips so a future
//! "edit this view's query" affordance can dump the AST back into the
//! popup as text.

use std::collections::HashSet;

use chrono::{Datelike, Duration, NaiveDate};
use uuid::Uuid;

use crate::cell::Cell;

// ============================================================================
// AST
// ============================================================================

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ast {
    pub include: Filters,
    pub exclude: Filters,
    pub text: String,
}

impl Ast {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty() && self.text.is_empty()
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Filters {
    pub tags: Vec<String>,
    pub entities: Vec<EntityRef>,
    pub time: Option<TimeFilter>,
}

impl Filters {
    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.entities.is_empty() && self.time.is_none()
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EntityRef {
    pub kind: EntityKind,
    /// Raw post-sigil token, lowercased. Resolution to actual person-cell
    /// UUIDs happens at execution time via `MatchContext`.
    pub id: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntityKind {
    Person,
    // Thread reserved for future; v1 parser never produces it.
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TimeFilter {
    Today,
    Yesterday,
    LastWeek,
    ThisWeek,
    NextWeek,
    /// Explicit single date (`2026-04-30` or `04-30` resolved to current year).
    Day(NaiveDate),
    /// Inclusive range (`a..b`).
    Range(NaiveDate, NaiveDate),
}

// ============================================================================
// Parser
// ============================================================================

/// Parse a query string into an AST. Tokens recognized:
/// - `#tag` / `-#tag` → include / exclude tag
/// - `@name` / `-@name` → include / exclude person entity
/// - `today` / `yesterday` → relative time
/// - `last|this|next week` (two tokens) → relative-week time
/// - `YYYY-MM-DD` → explicit date
/// - `MM-DD` → explicit date (current year)
/// - `a..b` (no spaces around `..`) → date range
/// - everything else → residual text
///
/// Multiple time tokens overwrite (last wins). Per the spec, ambiguous time
/// phrases stay as residual text.
pub fn parse(input: &str) -> Ast {
    let tokens = tokenize(input);
    let mut ast = Ast::default();
    let mut text_parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if classify_structured(tok, &mut ast) {
            i += 1;
            continue;
        }
        // Multi-word time: `last|this|next` `week`.
        if let Some(next) = tokens.get(i + 1) {
            if let Some(t) = parse_relative_week(tok, next) {
                ast.include.time = Some(t);
                i += 2;
                continue;
            }
        }
        text_parts.push(tok);
        i += 1;
    }
    ast.text = text_parts.join(" ");
    ast
}

/// Whitespace-split, keeping `..` glued. Future: respect quoted runs.
fn tokenize(input: &str) -> Vec<&str> {
    input.split_whitespace().collect()
}

/// Classify a single token. Returns true if structured (consumed).
/// Multi-word time tokens (`last week`) are handled at the call site.
fn classify_structured(tok: &str, ast: &mut Ast) -> bool {
    // Negation prefix.
    let (negated, body) = match tok.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => (true, rest),
        _ => (false, tok),
    };
    if let Some(name) = body.strip_prefix('#') {
        if name.is_empty() {
            return false; // bare `#` is not a tag
        }
        let name = name.to_lowercase();
        if negated {
            ast.exclude.tags.push(name);
        } else {
            ast.include.tags.push(name);
        }
        return true;
    }
    if let Some(name) = body.strip_prefix('@') {
        if name.is_empty() {
            return false;
        }
        let entity = EntityRef {
            kind: EntityKind::Person,
            id: name.to_lowercase(),
        };
        if negated {
            ast.exclude.entities.push(entity);
        } else {
            ast.include.entities.push(entity);
        }
        return true;
    }
    // `%name` is reserved for threads (deferred). Falls through to text.
    if body.starts_with('%') {
        return false;
    }
    if negated {
        // `-foo` without a sigil is residual text — fall through.
        return false;
    }
    // Unprefixed time tokens.
    match tok {
        "today" => {
            ast.include.time = Some(TimeFilter::Today);
            return true;
        }
        "yesterday" => {
            ast.include.time = Some(TimeFilter::Yesterday);
            return true;
        }
        _ => {}
    }
    // Range: `a..b` (where a and b each parse as a date).
    if let Some((a, b)) = tok.split_once("..") {
        if let (Some(da), Some(db)) = (parse_date(a), parse_date(b)) {
            ast.include.time = Some(TimeFilter::Range(da, db));
            return true;
        }
    }
    // Single date.
    if let Some(d) = parse_date(tok) {
        ast.include.time = Some(TimeFilter::Day(d));
        return true;
    }
    false
}

fn parse_relative_week(a: &str, b: &str) -> Option<TimeFilter> {
    if !b.eq_ignore_ascii_case("week") {
        return None;
    }
    match a.to_ascii_lowercase().as_str() {
        "last" => Some(TimeFilter::LastWeek),
        "this" => Some(TimeFilter::ThisWeek),
        "next" => Some(TimeFilter::NextWeek),
        _ => None,
    }
}

/// Parse `YYYY-MM-DD` or `MM-DD` (current year). Returns None on anything
/// else, including out-of-range dates.
fn parse_date(s: &str) -> Option<NaiveDate> {
    let parts: Vec<&str> = s.split('-').collect();
    match parts.as_slice() {
        [y, m, d] if y.len() == 4 => {
            let yi = y.parse::<i32>().ok()?;
            let mi = m.parse::<u32>().ok()?;
            let di = d.parse::<u32>().ok()?;
            NaiveDate::from_ymd_opt(yi, mi, di)
        }
        [m, d] if m.len() == 2 && d.len() == 2 => {
            let mi = m.parse::<u32>().ok()?;
            let di = d.parse::<u32>().ok()?;
            let yi = chrono::Local::now().year();
            NaiveDate::from_ymd_opt(yi, mi, di)
        }
        _ => None,
    }
}

// ============================================================================
// Serializer
// ============================================================================

/// Render an AST as a canonical query string. Round-trip property:
/// `parse(to_text(ast)) == ast` for every AST the parser can produce.
pub fn to_text(ast: &Ast) -> String {
    let mut parts: Vec<String> = Vec::new();
    for t in &ast.include.tags {
        parts.push(format!("#{t}"));
    }
    for e in &ast.include.entities {
        parts.push(entity_to_text(e, false));
    }
    if let Some(t) = &ast.include.time {
        parts.push(time_to_text(t));
    }
    for t in &ast.exclude.tags {
        parts.push(format!("-#{t}"));
    }
    for e in &ast.exclude.entities {
        parts.push(entity_to_text(e, true));
    }
    if !ast.text.is_empty() {
        parts.push(ast.text.clone());
    }
    parts.join(" ")
}

fn entity_to_text(e: &EntityRef, negated: bool) -> String {
    let sigil = match e.kind {
        EntityKind::Person => '@',
    };
    if negated {
        format!("-{sigil}{}", e.id)
    } else {
        format!("{sigil}{}", e.id)
    }
}

fn time_to_text(t: &TimeFilter) -> String {
    match t {
        TimeFilter::Today => "today".to_string(),
        TimeFilter::Yesterday => "yesterday".to_string(),
        TimeFilter::LastWeek => "last week".to_string(),
        TimeFilter::ThisWeek => "this week".to_string(),
        TimeFilter::NextWeek => "next week".to_string(),
        TimeFilter::Day(d) => d.format("%Y-%m-%d").to_string(),
        TimeFilter::Range(a, b) => format!("{}..{}", a.format("%Y-%m-%d"), b.format("%Y-%m-%d")),
    }
}

// ============================================================================
// Executor
// ============================================================================

/// State the executor needs that isn't on the AST itself: today's date for
/// relative-time resolution, and pre-resolved person-cell UUIDs for entity
/// filters (computed once per filter pass instead of per cell).
pub struct MatchContext {
    pub today: NaiveDate,
    pub person_targets: Vec<Uuid>,
    pub person_excludes: Vec<Uuid>,
}

/// Resolve a time filter to a half-open day range `[start, end)`.
fn resolve_time(t: &TimeFilter, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    let one = Duration::days(1);
    match t {
        TimeFilter::Today => (today, today + one),
        TimeFilter::Yesterday => (today - one, today),
        TimeFilter::Day(d) => (*d, *d + one),
        TimeFilter::Range(a, b) => (*a, *b + one),
        TimeFilter::ThisWeek => {
            let start = monday_of(today);
            (start, start + Duration::days(7))
        }
        TimeFilter::LastWeek => {
            let start = monday_of(today) - Duration::days(7);
            (start, start + Duration::days(7))
        }
        TimeFilter::NextWeek => {
            let start = monday_of(today) + Duration::days(7);
            (start, start + Duration::days(7))
        }
    }
}

fn monday_of(d: NaiveDate) -> NaiveDate {
    let dow = d.weekday().num_days_from_monday() as i64;
    d - Duration::days(dow)
}

/// True if `cell` matches `ast`. AND across all filters; an empty AST
/// matches every cell.
pub fn matches(ast: &Ast, cell: &Cell, ctx: &MatchContext) -> bool {
    // Time bound.
    if let Some(t) = &ast.include.time {
        let (start, end) = resolve_time(t, ctx.today);
        let cell_date = local_date_for_ms(cell.timestamp);
        if cell_date < start || cell_date >= end {
            return false;
        }
    }
    // Tag includes — cell must have ALL listed tags.
    let cell_tags = cell.heading_tag_names();
    for needed in &ast.include.tags {
        if !cell_tags.iter().any(|n| n.eq_ignore_ascii_case(needed)) {
            return false;
        }
    }
    // Tag excludes — cell must NOT have any listed tag.
    for forbidden in &ast.exclude.tags {
        if cell_tags.iter().any(|n| n.eq_ignore_ascii_case(forbidden)) {
            return false;
        }
    }
    // Person includes — cell must link to ALL person targets. Person targets
    // is the *combined* set across every `@id` in the query: since each
    // entity ref resolves independently, a cell with at least one matching
    // link per query entity satisfies it. For v1, we conservatively require
    // a link to *some* target per entity by treating the combined target
    // set as a single "any-of" check; this is fine when there's only one
    // entity in the query (the common case) and a defensible
    // interpretation of multi-entity queries until UX clarifies.
    if !ctx.person_targets.is_empty() {
        let body_links = collect_kept_link_uuids(cell);
        if !ctx.person_targets.iter().any(|u| body_links.contains(u)) {
            return false;
        }
    }
    // Person excludes.
    if !ctx.person_excludes.is_empty() {
        let body_links = collect_kept_link_uuids(cell);
        if ctx.person_excludes.iter().any(|u| body_links.contains(u)) {
            return false;
        }
    }
    // Free-text substring (case-insensitive).
    if !ast.text.is_empty() {
        let needle = ast.text.to_lowercase();
        if !cell.full_text().to_lowercase().contains(&needle) {
            return false;
        }
    }
    true
}

fn local_date_for_ms(ms: i64) -> NaiveDate {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
}

/// Walk every `kept://<uuid>` link in this cell's body and the title (if
/// any) and collect the parsed UUIDs.
fn collect_kept_link_uuids(cell: &Cell) -> HashSet<Uuid> {
    let mut out: HashSet<Uuid> = HashSet::new();
    let mut visit = |urls: &[String]| {
        for url in urls {
            if let Some(rest) = url.strip_prefix("kept://") {
                if let Ok(id) = Uuid::parse_str(rest) {
                    out.insert(id);
                }
            }
        }
    };
    let urls = cell.all_link_urls();
    visit(&urls);
    out
}

// ============================================================================
// Entity resolution helpers (used by KeptApp to build MatchContext)
// ============================================================================

/// Resolve `@<id>` refs into entity UUIDs. Output is **always** entity
/// IDs — never cell IDs (invariant #1).
///
/// Precedence (frozen):
///   1. **Alias index.** Normalize the ref's id (lowercase, strip
///      whitespace + underscores). Substring-match against normalized
///      aliases for entries with `kind == "person"`. Collect entity_ids.
///   2. **Title fallback.** Only runs for refs that scored zero hits in
///      step 1. The candidate set is `title_fallback`, a precomputed
///      list of `(entity_id, normalized_display_name)` for entities
///      that have a `primary_cell_id`. Cells without an entity are
///      *not* in this list and cannot contribute (invariant #2). The
///      matching surface is the entity's canonical `display_name`,
///      not the cell's mutable title (invariant #6 — `display_name`
///      is the identity surface).
///
/// Tags are NOT consulted at any step (invariant #4).
pub fn resolve_persons(
    refs: &[EntityRef],
    alias_index: &[(String, Uuid, String)],
    title_fallback: &[(Uuid, String)],
) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = Vec::new();
    for r in refs.iter().filter(|r| matches!(r.kind, EntityKind::Person)) {
        let needle = normalize_entity_token(&r.id);
        if needle.is_empty() {
            continue;
        }
        let before = out.len();
        // Step 1: alias index.
        for (alias, entity_id, kind) in alias_index {
            if kind != "person" {
                continue;
            }
            if normalize_entity_token(alias).contains(&needle)
                && !out.contains(entity_id)
            {
                out.push(*entity_id);
            }
        }
        if out.len() > before {
            continue;
        }
        // Step 2: title fallback (entity-derived only).
        for (entity_id, normalized_display) in title_fallback {
            if normalized_display.contains(&needle) && !out.contains(entity_id) {
                out.push(*entity_id);
            }
        }
    }
    out
}

pub(crate) fn normalize_entity_token(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::TextBox;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // ----- parser -----

    #[test]
    fn parse_empty() {
        assert_eq!(parse(""), Ast::default());
    }

    #[test]
    fn parse_tags() {
        let ast = parse("#bug -#stale");
        assert_eq!(ast.include.tags, vec!["bug".to_string()]);
        assert_eq!(ast.exclude.tags, vec!["stale".to_string()]);
        assert!(ast.text.is_empty());
    }

    #[test]
    fn parse_entities() {
        let ast = parse("@sam @mike -@bot");
        assert_eq!(
            ast.include.entities,
            vec![
                EntityRef { kind: EntityKind::Person, id: "sam".into() },
                EntityRef { kind: EntityKind::Person, id: "mike".into() },
            ]
        );
        assert_eq!(
            ast.exclude.entities,
            vec![EntityRef { kind: EntityKind::Person, id: "bot".into() }]
        );
    }

    #[test]
    fn parse_today_with_text() {
        let ast = parse("today login crash");
        assert_eq!(ast.include.time, Some(TimeFilter::Today));
        assert_eq!(ast.text, "login crash");
    }

    #[test]
    fn parse_explicit_date() {
        let ast = parse("2026-04-30");
        assert_eq!(ast.include.time, Some(TimeFilter::Day(day(2026, 4, 30))));
    }

    #[test]
    fn parse_short_date_uses_current_year() {
        let ast = parse("04-30");
        let year = chrono::Local::now().year();
        assert_eq!(ast.include.time, Some(TimeFilter::Day(day(year, 4, 30))));
    }

    #[test]
    fn parse_range() {
        let ast = parse("2026-01-01..2026-01-31");
        assert_eq!(
            ast.include.time,
            Some(TimeFilter::Range(day(2026, 1, 1), day(2026, 1, 31)))
        );
    }

    #[test]
    fn parse_last_week() {
        let ast = parse("last week");
        assert_eq!(ast.include.time, Some(TimeFilter::LastWeek));
        assert!(ast.text.is_empty());
    }

    #[test]
    fn parse_thread_sigil_falls_through_as_text() {
        let ast = parse("%foo bar");
        assert_eq!(ast.text, "%foo bar");
        assert!(ast.include.entities.is_empty());
    }

    #[test]
    fn parse_combined() {
        let ast = parse("#person today @mike crash");
        assert_eq!(ast.include.tags, vec!["person".to_string()]);
        assert_eq!(ast.include.time, Some(TimeFilter::Today));
        assert_eq!(
            ast.include.entities,
            vec![EntityRef { kind: EntityKind::Person, id: "mike".into() }]
        );
        assert_eq!(ast.text, "crash");
    }

    // ----- serializer round-trip -----

    fn roundtrip(input: &str) {
        let ast = parse(input);
        let text = to_text(&ast);
        let reparsed = parse(&text);
        assert_eq!(
            ast, reparsed,
            "round-trip failed for input {:?}: text={:?}",
            input, text
        );
    }

    #[test]
    fn roundtrip_empty() {
        roundtrip("");
    }

    #[test]
    fn roundtrip_tags() {
        roundtrip("#bug -#stale");
    }

    #[test]
    fn roundtrip_entities() {
        roundtrip("@sam -@bot");
    }

    #[test]
    fn roundtrip_time_today() {
        roundtrip("today");
    }

    #[test]
    fn roundtrip_time_explicit() {
        roundtrip("2026-04-30");
    }

    #[test]
    fn roundtrip_time_range() {
        roundtrip("2026-01-01..2026-01-31");
    }

    #[test]
    fn roundtrip_last_week() {
        roundtrip("last week");
    }

    #[test]
    fn roundtrip_combined() {
        roundtrip("#person today @mike login crash");
    }

    // ----- time resolution -----

    #[test]
    fn resolve_today() {
        let t = day(2026, 4, 30);
        assert_eq!(resolve_time(&TimeFilter::Today, t), (t, day(2026, 5, 1)));
    }

    #[test]
    fn resolve_yesterday() {
        let t = day(2026, 4, 30);
        assert_eq!(
            resolve_time(&TimeFilter::Yesterday, t),
            (day(2026, 4, 29), t)
        );
    }

    #[test]
    fn resolve_this_week_starts_monday() {
        // 2026-04-30 is a Thursday → Monday is 2026-04-27.
        let t = day(2026, 4, 30);
        assert_eq!(
            resolve_time(&TimeFilter::ThisWeek, t),
            (day(2026, 4, 27), day(2026, 5, 4))
        );
    }

    #[test]
    fn resolve_range_inclusive() {
        let t = day(2026, 4, 30);
        assert_eq!(
            resolve_time(&TimeFilter::Range(day(2026, 4, 1), day(2026, 4, 3)), t),
            (day(2026, 4, 1), day(2026, 4, 4))
        );
    }

    // ----- entity resolution -----

    fn person_ref(id: &str) -> EntityRef {
        EntityRef {
            kind: EntityKind::Person,
            id: id.to_string(),
        }
    }

    #[test]
    fn resolve_alias_match_returns_entity_id() {
        let entity_id = Uuid::now_v7();
        let cell_id = Uuid::now_v7(); // distinct from entity_id — invariant #1/3
        let alias_index = vec![("patrick_foy".to_string(), entity_id, "person".to_string())];
        // title_fallback contains a different entity to prove the alias path
        // wins and returns its own entity_id, never the cell_id.
        let title_fallback = vec![(cell_id, "patrickfoy".to_string())];
        let got = resolve_persons(&[person_ref("Patrick_Foy")], &alias_index, &title_fallback);
        assert_eq!(got, vec![entity_id]);
    }

    #[test]
    fn resolve_excludes_non_person_kinds() {
        let thread_id = Uuid::now_v7();
        let alias_index = vec![("login_bug".to_string(), thread_id, "thread".to_string())];
        let got = resolve_persons(&[person_ref("login_bug")], &alias_index, &[]);
        assert!(got.is_empty());
    }

    #[test]
    fn resolve_title_fallback_returns_entity_id() {
        let entity_id = Uuid::now_v7();
        // Empty alias index forces the fallback.
        let title_fallback = vec![(entity_id, "patrickfoy".to_string())];
        let got = resolve_persons(&[person_ref("patrick")], &[], &title_fallback);
        assert_eq!(got, vec![entity_id]);
    }

    #[test]
    fn resolve_title_fallback_only_has_entities() {
        // Cells without an entity row contribute nothing to the fallback —
        // the fallback corpus is entity-derived. Empty fallback → no match.
        let got = resolve_persons(&[person_ref("patrick")], &[], &[]);
        assert!(got.is_empty());
    }

    #[test]
    fn resolve_alias_wins_over_fallback() {
        let alias_entity = Uuid::now_v7();
        let fallback_entity = Uuid::now_v7();
        let alias_index = vec![
            ("patrick_foy".to_string(), alias_entity, "person".to_string()),
        ];
        let title_fallback = vec![(fallback_entity, "patrickfoy".to_string())];
        let got = resolve_persons(&[person_ref("patrick")], &alias_index, &title_fallback);
        assert_eq!(got, vec![alias_entity]);
    }

    // ----- executor (matches) -----
    //
    // Smoke coverage for `matches` — the function the search popup and the
    // active view both rely on to filter cells. Each test exercises one
    // filter dimension at a time so a regression in any one of them can't
    // hide behind another passing.

    fn typeface() -> skia_safe::Typeface {
        skia_safe::FontMgr::new()
            .new_from_data(include_bytes!("../resources/fonts/Figtree.ttf"), None)
            .expect("font loads")
    }

    /// Build a Plain cell with the given body text + an optional title.
    /// The title forces heading mode so `heading_tag_names` will see the
    /// trailing `#tag`s as tags.
    fn make_cell(body: &str, title: Option<&str>, ts_ms: i64) -> Cell {
        let tf = typeface();
        let mut cell = Cell::new(tf.clone(), body.to_string());
        cell.timestamp = ts_ms;
        cell.edited_at = ts_ms;
        if let Some(t) = title {
            let mut tb = TextBox::new(tf, t.to_string());
            tb.set_force_heading(true);
            cell.set_title(Some(tb));
        }
        cell
    }

    fn empty_ctx(today: NaiveDate) -> MatchContext {
        MatchContext {
            today,
            person_targets: Vec::new(),
            person_excludes: Vec::new(),
        }
    }

    #[test]
    fn matches_tag_include_requires_all_listed_tags() {
        let cell = make_cell("body", Some("Standup #urgent #planning"), 0);
        let ctx = empty_ctx(day(2026, 1, 1));
        assert!(matches(&parse("#urgent"), &cell, &ctx), "single tag hit");
        assert!(
            matches(&parse("#urgent #planning"), &cell, &ctx),
            "all tags present → match"
        );
        assert!(
            !matches(&parse("#urgent #missing"), &cell, &ctx),
            "missing one tag → no match"
        );
    }

    #[test]
    fn matches_tag_exclude_filters_out_cells_with_listed_tag() {
        let cell = make_cell("body", Some("Note #stale"), 0);
        let ctx = empty_ctx(day(2026, 1, 1));
        assert!(
            !matches(&parse("-#stale"), &cell, &ctx),
            "excluded tag present → no match"
        );
        let untagged = make_cell("body", Some("Note"), 0);
        assert!(
            matches(&parse("-#stale"), &untagged, &ctx),
            "no excluded tag → match"
        );
    }

    #[test]
    fn matches_time_filter_day_constrains_to_that_local_date() {
        // Build cells timestamped on two different local-time dates.
        // Use noon UTC to dodge timezone edge cases — the local date for
        // both is the same calendar day no matter the host TZ.
        use chrono::TimeZone;
        let jan1_noon = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap().timestamp_millis();
        let jan2_noon = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap().timestamp_millis();
        let cell1 = make_cell("note", None, jan1_noon);
        let cell2 = make_cell("note", None, jan2_noon);
        // The cell's local date may differ from UTC date in some TZs.
        // Compute it via the same local_date_for_ms the executor uses.
        let cell1_local = local_date_for_ms(jan1_noon);
        let ast = Ast {
            include: Filters {
                time: Some(TimeFilter::Day(cell1_local)),
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = empty_ctx(cell1_local);
        assert!(matches(&ast, &cell1, &ctx), "cell on the day matches");
        assert!(
            !matches(&ast, &cell2, &ctx),
            "cell on different day excluded"
        );
    }

    #[test]
    fn matches_entity_include_requires_kept_link_to_person() {
        // Build a cell whose body has a `kept://<uuid>` link to Alice.
        let alice_id = Uuid::now_v7();
        let bob_id = Uuid::now_v7();
        let tf = typeface();
        let mut cell = Cell::new(tf, "hi @Alice".to_string());
        cell.add_link_to_first(3..9, format!("kept://{}", alice_id));
        // AST asks for cells linking to Alice.
        let ast = Ast {
            include: Filters {
                entities: vec![EntityRef {
                    kind: EntityKind::Person,
                    id: "alice".to_string(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx_alice = MatchContext {
            today: day(2026, 1, 1),
            person_targets: vec![alice_id],
            person_excludes: Vec::new(),
        };
        let ctx_bob = MatchContext {
            today: day(2026, 1, 1),
            person_targets: vec![bob_id],
            person_excludes: Vec::new(),
        };
        assert!(matches(&ast, &cell, &ctx_alice), "kept link to Alice → match");
        assert!(
            !matches(&ast, &cell, &ctx_bob),
            "kept link is to Alice, not Bob → no match"
        );
    }

    #[test]
    fn matches_free_text_substring_is_case_insensitive() {
        let cell = make_cell("Some Body Of Text", None, 0);
        let ctx = empty_ctx(day(2026, 1, 1));
        assert!(matches(&parse("body of"), &cell, &ctx), "substring hit");
        assert!(
            matches(&parse("BODY OF"), &cell, &ctx),
            "case-insensitive"
        );
        assert!(
            !matches(&parse("missing"), &cell, &ctx),
            "no substring → no match"
        );
    }

    #[test]
    fn resolve_falls_back_only_for_refs_with_zero_alias_hits() {
        // Two refs: `@patrick` (alias-hits), `@dana` (alias-misses → fallback).
        let patrick_id = Uuid::now_v7();
        let dana_id = Uuid::now_v7();
        let alias_index = vec![
            ("patrick_foy".to_string(), patrick_id, "person".to_string()),
        ];
        let title_fallback = vec![(dana_id, "danadoe".to_string())];
        let got = resolve_persons(
            &[person_ref("patrick"), person_ref("dana")],
            &alias_index,
            &title_fallback,
        );
        assert!(got.contains(&patrick_id));
        assert!(got.contains(&dana_id));
    }
}
