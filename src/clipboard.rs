//! Clipboard serialization for cross-app + Kept↔Kept round-trip.
//!
//! Three formats live on the OS clipboard for every copy:
//!
//! 1. **HTML** — the only structured format `arboard` exposes
//!    cross-platform. Google Docs / browsers / rich-text editors
//!    consume this. Embedded inside the HTML is a hidden
//!    `<span data-kept-payload="…base64 JSON…">` marker carrying
//!    the full `KeptPayload`. Kept reads this back for byte-perfect
//!    round-trip; other apps just ignore the unknown attribute.
//! 2. **Plain text** — `arboard`'s `set_html(html, alt_text)` writes
//!    plain text as the alt; this is what receives in apps that
//!    don't honor HTML clipboards.
//!
//! On read, priority is: embedded marker → HTML structure parse →
//! plain text fallback. Each step is a graceful degrade.
//!
//! The module is self-contained — no UI dependencies — so it can
//! be unit-tested without standing up a window / Skia.

use base64::Engine;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cell::{LinkSpan, ReferenceTarget};

/// The structured form of "what's on the clipboard".
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum KeptPayload {
    /// A text run + link spans. From a TextBox selection (Plain
    /// cell, PopPop, single-bullet outline copy).
    Text { text: String, links: Vec<SerLink> },
    /// Multi-bullet selection from an Outline cell. Each bullet
    /// has its own depth + text + links. Pasting into an outline
    /// expands these as siblings of the focused bullet; pasting
    /// into a plain text context flattens with leading whitespace.
    Outline { bullets: Vec<BulletPayload> },
    /// "Copy reference" → paste-as-link or paste-as-cell.
    Reference {
        target: SerTarget,
        snippet: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerLink {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BulletPayload {
    pub depth: u32,
    pub text: String,
    pub links: Vec<SerLink>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "target_kind", rename_all = "lowercase")]
pub enum SerTarget {
    Cell { cell_id: Uuid },
    Subtree { cell_id: Uuid, bullet_id: Uuid },
}

impl SerLink {
    pub fn from_span(s: &LinkSpan) -> Self {
        Self {
            start: s.range.start,
            end: s.range.end,
            url: s.url.clone(),
        }
    }
    pub fn into_span(self) -> LinkSpan {
        LinkSpan {
            range: self.start..self.end,
            url: self.url,
        }
    }
    pub fn spans_to_ser(spans: &[LinkSpan]) -> Vec<Self> {
        spans.iter().map(Self::from_span).collect()
    }
    pub fn ser_to_spans(spans: Vec<Self>) -> Vec<LinkSpan> {
        spans.into_iter().map(Self::into_span).collect()
    }
}

impl SerTarget {
    pub fn from_target(t: ReferenceTarget) -> Self {
        match t {
            ReferenceTarget::WholeCell(cell_id) => SerTarget::Cell { cell_id },
            ReferenceTarget::Subtree { cell_id, bullet_id } => {
                SerTarget::Subtree { cell_id, bullet_id }
            }
        }
    }
    pub fn into_target(self) -> ReferenceTarget {
        match self {
            SerTarget::Cell { cell_id } => ReferenceTarget::WholeCell(cell_id),
            SerTarget::Subtree { cell_id, bullet_id } => {
                ReferenceTarget::Subtree { cell_id, bullet_id }
            }
        }
    }

    /// `kept://<uuid>` for a whole cell; `kept://<cell>#<bullet>`
    /// for a subtree. The fragment form isn't yet consumed by
    /// click navigation (which currently strips the fragment), but
    /// future code can use it to scroll to a specific bullet.
    pub fn to_url(&self) -> String {
        match self {
            SerTarget::Cell { cell_id } => format!("kept://{}", cell_id),
            SerTarget::Subtree { cell_id, bullet_id } => {
                format!("kept://{}#{}", cell_id, bullet_id)
            }
        }
    }
}

const PAYLOAD_MARKER_ATTR: &str = "data-kept-payload";

// ---------------------------------------------------------------------------
// Serialize
// ---------------------------------------------------------------------------

/// Plain-text rendering. Always-works fallback for apps that
/// don't honor HTML clipboards.
pub fn to_plain_text(p: &KeptPayload) -> String {
    match p {
        KeptPayload::Text { text, .. } => text.clone(),
        KeptPayload::Outline { bullets } => bullets
            .iter()
            .map(|b| format!("{}{}", "    ".repeat(b.depth as usize), b.text))
            .collect::<Vec<_>>()
            .join("\n"),
        KeptPayload::Reference { target, snippet } => {
            format!("↗ {} ({})", snippet, target.to_url())
        }
    }
}

/// HTML rendering. Starts with a hidden `<span data-kept-payload="…">`
/// carrying the base64-encoded JSON so Kept can round-trip the
/// payload byte-perfectly. Subsequent content is the visible HTML
/// representation (lists, paragraphs, anchors).
pub fn to_html(p: &KeptPayload) -> String {
    let mut out = String::new();
    let json = serde_json::to_string(p).unwrap_or_default();
    let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    out.push_str(&format!(
        "<span {}=\"{}\" style=\"display:none\"></span>",
        PAYLOAD_MARKER_ATTR, b64
    ));
    match p {
        KeptPayload::Text { text, links } => {
            out.push_str("<p>");
            out.push_str(&render_text_with_links(text, links));
            out.push_str("</p>");
        }
        KeptPayload::Outline { bullets } => {
            out.push_str(&render_outline(bullets));
        }
        KeptPayload::Reference { target, snippet } => {
            out.push_str(&format!(
                "<p><a href=\"{}\">↗ {}</a></p>",
                escape_attr(&target.to_url()),
                escape_text(snippet),
            ));
        }
    }
    out
}

fn render_text_with_links(text: &str, links: &[SerLink]) -> String {
    let mut sorted = links.to_vec();
    sorted.sort_by_key(|l| l.start);
    let mut out = String::new();
    let mut cursor = 0;
    for l in &sorted {
        if l.start > cursor && l.start <= text.len() {
            out.push_str(&escape_text(&text[cursor..l.start]));
        }
        let lo = l.start.min(text.len());
        let hi = l.end.min(text.len());
        if lo < hi {
            out.push_str(&format!(
                "<a href=\"{}\">{}</a>",
                escape_attr(&l.url),
                escape_text(&text[lo..hi]),
            ));
        }
        cursor = hi;
    }
    if cursor < text.len() {
        out.push_str(&escape_text(&text[cursor..]));
    }
    out
}

fn render_outline(bullets: &[BulletPayload]) -> String {
    // Nest <ul>/<li> properly so a depth jump of +1 emits a fresh
    // inner <ul>, and a jump back closes the right number of
    // </li></ul> pairs. State machine:
    //   `open_uls`   – count of currently-open <ul> elements
    //                  (depth N requires open_uls == N + 1).
    //   `has_open_li` – true if the innermost <ul> currently has
    //                  an open <li>. We close it when emitting a
    //                  sibling, leave it open when nesting deeper.
    let mut out = String::new();
    let mut open_uls: usize = 0;
    let mut has_open_li = false;
    for b in bullets {
        let target_uls = (b.depth + 1) as usize;
        if open_uls == 0 {
            out.push_str("<ul>");
            open_uls = 1;
        }
        // Going shallower: close </li></ul> pairs until we're at
        // the right depth. After each </ul>, the enclosing <li>
        // (one level up) is the most-recently-open <li> — set
        // `has_open_li` so the next sibling at THAT depth closes
        // it before opening its own <li>.
        while open_uls > target_uls {
            if has_open_li {
                out.push_str("</li>");
            }
            out.push_str("</ul>");
            open_uls -= 1;
            has_open_li = true;
        }
        // Going deeper: open <ul>s. We DON'T close the parent
        // <li> on the way down — its content is the new <ul>.
        while open_uls < target_uls {
            out.push_str("<ul>");
            open_uls += 1;
            has_open_li = false;
        }
        // Sibling at the current depth: close the prior <li>.
        if has_open_li {
            out.push_str("</li>");
        }
        out.push_str("<li>");
        out.push_str(&render_text_with_links(&b.text, &b.links));
        has_open_li = true;
    }
    // Close every remaining <li>/<ul>.
    while open_uls > 0 {
        if has_open_li {
            out.push_str("</li>");
            has_open_li = false;
        }
        out.push_str("</ul>");
        open_uls -= 1;
        if open_uls > 0 {
            has_open_li = true;
        }
    }
    out
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

/// Reconstruct a payload from the clipboard contents. Priority:
/// embedded marker (perfect round-trip) → HTML structure (lossy
/// but preserves lists + links) → plain text (lossiest).
pub fn from_clipboard(html: Option<&str>, text: &str) -> KeptPayload {
    if let Some(h) = html {
        if let Some(p) = extract_payload_marker(h) {
            return p;
        }
        if let Some(p) = parse_html(h) {
            return p;
        }
    }
    from_plain_text(text)
}

/// Scan for our hidden `data-kept-payload="…"` marker. Doesn't
/// require a full DOM walk — the attribute is always at the top
/// of our-generated HTML and the value is base64 (no quotes), so
/// a substring scan is robust enough.
fn extract_payload_marker(html: &str) -> Option<KeptPayload> {
    let needle = concat_marker();
    let start = html.find(&needle)? + needle.len();
    let end = html[start..].find('"')? + start;
    let b64 = &html[start..end];
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()?;
    let json = std::str::from_utf8(&bytes).ok()?;
    serde_json::from_str::<KeptPayload>(json).ok()
}

fn concat_marker() -> String {
    format!("{}=\"", PAYLOAD_MARKER_ATTR)
}

fn parse_html(html: &str) -> Option<KeptPayload> {
    let dom: RcDom = html5ever::parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .ok()?;
    let mut bullets: Vec<BulletPayload> = Vec::new();
    let mut plain = String::new();
    let mut plain_links: Vec<SerLink> = Vec::new();
    walk_html(&dom.document, -1, &mut bullets, &mut plain, &mut plain_links);

    if !bullets.is_empty() {
        Some(KeptPayload::Outline { bullets })
    } else {
        let trimmed = plain.trim();
        if trimmed.is_empty() {
            None
        } else {
            // Trim leading/trailing whitespace; adjust link ranges.
            let leading = plain.len() - plain.trim_start().len();
            let trailing_cut = plain.len() - plain.trim_end().len();
            let new_len = plain.len() - leading - trailing_cut;
            let adjusted: Vec<SerLink> = plain_links
                .into_iter()
                .filter_map(|l| {
                    let s = l.start.saturating_sub(leading);
                    let e = l.end.saturating_sub(leading);
                    if e <= new_len && s < e {
                        Some(SerLink {
                            start: s,
                            end: e,
                            url: l.url,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            Some(KeptPayload::Text {
                text: trimmed.to_string(),
                links: adjusted,
            })
        }
    }
}

fn element_name(handle: &Handle) -> Option<String> {
    if let NodeData::Element { name, .. } = &handle.data {
        Some(name.local.as_ref().to_ascii_lowercase())
    } else {
        None
    }
}

fn get_attr(handle: &Handle, key: &str) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &handle.data {
        for a in attrs.borrow().iter() {
            if a.name.local.as_ref().eq_ignore_ascii_case(key) {
                return Some(a.value.to_string());
            }
        }
    }
    None
}

fn walk_html(
    handle: &Handle,
    depth: i32,
    bullets: &mut Vec<BulletPayload>,
    plain: &mut String,
    plain_links: &mut Vec<SerLink>,
) {
    // Depth contract: `depth` is the bullet depth for any <li>
    // encountered directly inside `handle`. A <ul>/<ol> entered at
    // depth N records its <li>s at depth N (it doesn't bump itself
    // — the caller chose N). Generic non-list parents (root, body,
    // div, …) detect a <ul>/<ol> child and pass `depth + 1`.
    match element_name(handle).as_deref() {
        Some("ul") | Some("ol") => {
            // Children at this list's depth are <li>s; children
            // that are themselves <ul>/<ol> are nested (Google
            // Docs writes the nested list as a SIBLING of the
            // parent <li> rather than its child, so we have to
            // detect that shape and bump depth here).
            for child in handle.children.borrow().iter() {
                let next_depth = if matches!(
                    element_name(child).as_deref(),
                    Some("ul") | Some("ol")
                ) {
                    depth + 1
                } else {
                    depth
                };
                walk_html(child, next_depth, bullets, plain, plain_links);
            }
        }
        Some("li") => {
            let mut text = String::new();
            let mut links: Vec<SerLink> = Vec::new();
            collect_li(handle, &mut text, &mut links);
            let trimmed = trim_normalize(&text);
            if !trimmed.is_empty() && depth >= 0 {
                let adjusted = rebase_links(&trimmed, &text, &links);
                bullets.push(BulletPayload {
                    depth: depth as u32,
                    text: trimmed,
                    links: adjusted,
                });
            }
            // Nested <ul>/<ol> inside this <li> are one level deeper.
            for child in handle.children.borrow().iter() {
                let n = element_name(child);
                if matches!(n.as_deref(), Some("ul") | Some("ol")) {
                    walk_html(child, depth + 1, bullets, plain, plain_links);
                }
            }
        }
        Some("br") => {
            plain.push('\n');
        }
        Some("p") => {
            if !plain.is_empty() && !plain.ends_with('\n') {
                plain.push('\n');
            }
            for child in handle.children.borrow().iter() {
                collect_plain(child, plain, plain_links);
            }
        }
        Some("a") => {
            // Top-level anchor outside <li>/<p>: still record link.
            let url = get_attr(handle, "href").unwrap_or_default();
            let start = plain.len();
            for child in handle.children.borrow().iter() {
                collect_plain(child, plain, plain_links);
            }
            let end = plain.len();
            if end > start && !url.is_empty() {
                plain_links.push(SerLink { start, end, url });
            }
        }
        _ => {
            // Generic descent. Bump depth when a child is a list
            // root (so the list's <li>s come in at depth+1).
            if let NodeData::Text { contents } = &handle.data {
                plain.push_str(&contents.borrow());
            }
            for child in handle.children.borrow().iter() {
                let next_depth = if matches!(
                    element_name(child).as_deref(),
                    Some("ul") | Some("ol")
                ) {
                    depth + 1
                } else {
                    depth
                };
                walk_html(child, next_depth, bullets, plain, plain_links);
            }
        }
    }
}

/// Collect <li> content into `text` + `links`, ignoring nested
/// <ul>/<ol> (those are recorded as their own bullets).
fn collect_li(handle: &Handle, text: &mut String, links: &mut Vec<SerLink>) {
    match element_name(handle).as_deref() {
        Some("ul") | Some("ol") => return,
        Some("a") => {
            let url = get_attr(handle, "href").unwrap_or_default();
            let start = text.len();
            for child in handle.children.borrow().iter() {
                collect_li(child, text, links);
            }
            let end = text.len();
            if end > start && !url.is_empty() {
                links.push(SerLink { start, end, url });
            }
        }
        Some("br") => {
            text.push('\n');
        }
        _ => {
            if let NodeData::Text { contents } = &handle.data {
                text.push_str(&contents.borrow());
            }
            for child in handle.children.borrow().iter() {
                collect_li(child, text, links);
            }
        }
    }
}

/// Plain-text collector — same shape as `collect_li` but appends
/// to the plain buffer (with link tracking).
fn collect_plain(handle: &Handle, plain: &mut String, plain_links: &mut Vec<SerLink>) {
    match element_name(handle).as_deref() {
        Some("a") => {
            let url = get_attr(handle, "href").unwrap_or_default();
            let start = plain.len();
            for child in handle.children.borrow().iter() {
                collect_plain(child, plain, plain_links);
            }
            let end = plain.len();
            if end > start && !url.is_empty() {
                plain_links.push(SerLink { start, end, url });
            }
        }
        Some("br") => plain.push('\n'),
        _ => {
            if let NodeData::Text { contents } = &handle.data {
                plain.push_str(&contents.borrow());
            }
            for child in handle.children.borrow().iter() {
                collect_plain(child, plain, plain_links);
            }
        }
    }
}

/// Light text normalization for HTML-derived <li> content:
/// collapse runs of internal whitespace into single spaces and
/// trim edges. HTML parsers tend to emit lots of whitespace from
/// indented source.
fn trim_normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true; // suppress leading ws
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// After we normalize the bullet text (trim + collapse
/// whitespace), the byte ranges of the original link spans no
/// longer line up. Recompute them by scanning the normalized
/// text for each link's original text content. Imperfect (if the
/// same substring appears twice we pick the first), but good
/// enough for the typical case of one link per bullet with
/// distinct anchor text.
fn rebase_links(
    normalized: &str,
    raw: &str,
    raw_links: &[SerLink],
) -> Vec<SerLink> {
    let mut out = Vec::new();
    for l in raw_links {
        if l.start >= raw.len() || l.end > raw.len() || l.start >= l.end {
            continue;
        }
        let anchor_raw = &raw[l.start..l.end];
        let anchor_norm = trim_normalize(anchor_raw);
        if anchor_norm.is_empty() {
            continue;
        }
        if let Some(start) = normalized.find(&anchor_norm) {
            out.push(SerLink {
                start,
                end: start + anchor_norm.len(),
                url: l.url.clone(),
            });
        }
    }
    out
}

/// Plain-text → payload. If the text looks indented-as-outline
/// (multiple lines with leading-space patterns), parse as
/// Outline; otherwise as Text. URL auto-detection is left to the
/// existing paste path — `KeptPayload::Text` here carries no
/// links because plain text doesn't preserve span metadata.
pub fn from_plain_text(text: &str) -> KeptPayload {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() >= 2 && lines.iter().any(|l| l.starts_with("    ") || l.starts_with("\t")) {
        // Each line becomes a bullet at depth = floor(leading_spaces / 4)
        // (or count tab characters).
        let bullets: Vec<BulletPayload> = lines
            .iter()
            .filter_map(|l| {
                let (depth, rest) = strip_indent(l);
                let trimmed = rest.trim_end();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(BulletPayload {
                        depth,
                        text: strip_bullet_prefix(trimmed).to_string(),
                        links: Vec::new(),
                    })
                }
            })
            .collect();
        if !bullets.is_empty() {
            return KeptPayload::Outline { bullets };
        }
    }
    KeptPayload::Text {
        text: text.to_string(),
        links: Vec::new(),
    }
}

fn strip_indent(line: &str) -> (u32, &str) {
    let mut depth_spaces = 0u32;
    let mut idx = 0;
    let bytes = line.as_bytes();
    while idx < bytes.len() {
        match bytes[idx] {
            b' ' => {
                depth_spaces += 1;
                idx += 1;
            }
            b'\t' => {
                depth_spaces += 4;
                idx += 1;
            }
            _ => break,
        }
    }
    (depth_spaces / 4, &line[idx..])
}

fn strip_bullet_prefix(s: &str) -> &str {
    // Strip Markdown-ish bullet markers ("- ", "* ", "• ") that
    // commonly precede outline-style text.
    for prefix in ["- ", "* ", "• "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest;
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ser_link(start: usize, end: usize, url: &str) -> SerLink {
        SerLink {
            start,
            end,
            url: url.to_string(),
        }
    }

    #[test]
    fn text_roundtrip_via_marker() {
        let p = KeptPayload::Text {
            text: "hello world".to_string(),
            links: vec![ser_link(6, 11, "https://example.com/")],
        };
        let html = to_html(&p);
        // The marker should be at the start.
        assert!(html.contains("data-kept-payload="));
        let back = from_clipboard(Some(&html), "");
        assert_eq!(back, p);
    }

    #[test]
    fn outline_roundtrip_via_marker() {
        let p = KeptPayload::Outline {
            bullets: vec![
                BulletPayload {
                    depth: 0,
                    text: "alpha".into(),
                    links: vec![],
                },
                BulletPayload {
                    depth: 1,
                    text: "beta".into(),
                    links: vec![ser_link(0, 4, "https://x.com/")],
                },
                BulletPayload {
                    depth: 0,
                    text: "gamma".into(),
                    links: vec![],
                },
            ],
        };
        let html = to_html(&p);
        let back = from_clipboard(Some(&html), "");
        assert_eq!(back, p);
    }

    #[test]
    fn reference_roundtrip_via_marker() {
        let cell_id = Uuid::now_v7();
        let p = KeptPayload::Reference {
            target: SerTarget::Cell { cell_id },
            snippet: "Notes on Tuesday".into(),
        };
        let html = to_html(&p);
        let back = from_clipboard(Some(&html), "");
        assert_eq!(back, p);
        // Plain-text fallback contains the URL.
        let text = to_plain_text(&p);
        assert!(text.contains(&format!("kept://{}", cell_id)));
    }

    /// REGRESSION: Google Docs serializes nested lists with the
    /// inner `<ul>` as a SIBLING of the parent `<li>` inside the
    /// outer `<ul>` (rather than a child of `<li>`), with `<p>` +
    /// `<span>` wrappers around the text. Our parser must
    /// recognize the sibling form as "one level deeper" — without
    /// this, every bullet collapses to depth 0 on paste.
    #[test]
    fn html_parses_google_docs_sibling_nested_list() {
        // Trimmed but structure-faithful sample of what Docs writes
        // for a 3-bullet outline at depths [0, 1, 0].
        let html = r##"<meta charset="utf-8"><b style="font-weight:normal" id="docs-internal-guid-x">
<ul style="margin-top:0;margin-bottom:0">
<li dir="ltr" aria-level="1"><p dir="ltr" role="presentation"><span>alpha</span></p></li>
<ul style="margin-top:0;margin-bottom:0">
<li dir="ltr" aria-level="2"><p dir="ltr" role="presentation"><span>beta</span></p></li>
</ul>
<li dir="ltr" aria-level="1"><p dir="ltr" role="presentation"><span>gamma</span></p></li>
</ul>
</b>"##;
        match from_clipboard(Some(html), "") {
            KeptPayload::Outline { bullets } => {
                let depths: Vec<u32> = bullets.iter().map(|b| b.depth).collect();
                let texts: Vec<String> =
                    bullets.iter().map(|b| b.text.clone()).collect();
                assert_eq!(depths, vec![0, 1, 0], "depths from sibling-nested Docs HTML");
                assert_eq!(texts, vec!["alpha", "beta", "gamma"]);
            }
            other => panic!("expected Outline, got {:?}", other),
        }
    }

    #[test]
    fn html_parses_nested_list_to_outline() {
        // Synthetic — no marker, so the parser walks the HTML.
        let html = "<ul><li>top<ul><li>nested</li></ul></li><li>second</li></ul>";
        match from_clipboard(Some(html), "") {
            KeptPayload::Outline { bullets } => {
                assert_eq!(bullets.len(), 3);
                assert_eq!(bullets[0].depth, 0);
                assert_eq!(bullets[0].text, "top");
                assert_eq!(bullets[1].depth, 1);
                assert_eq!(bullets[1].text, "nested");
                assert_eq!(bullets[2].depth, 0);
                assert_eq!(bullets[2].text, "second");
            }
            other => panic!("expected Outline, got {:?}", other),
        }
    }

    #[test]
    fn html_parses_paragraph_with_link_to_text() {
        let html = r#"<p>hello <a href="https://x.com/">there</a> world</p>"#;
        match from_clipboard(Some(html), "") {
            KeptPayload::Text { text, links } => {
                assert_eq!(text, "hello there world");
                assert_eq!(links.len(), 1);
                assert_eq!(links[0].url, "https://x.com/");
                assert_eq!(&text[links[0].start..links[0].end], "there");
            }
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn plain_text_with_indents_parses_as_outline() {
        let text = "alpha\n    beta\n        gamma\n    delta";
        match from_plain_text(text) {
            KeptPayload::Outline { bullets } => {
                assert_eq!(bullets.len(), 4);
                assert_eq!(bullets[0].depth, 0);
                assert_eq!(bullets[1].depth, 1);
                assert_eq!(bullets[2].depth, 2);
                assert_eq!(bullets[3].depth, 1);
            }
            other => panic!("expected Outline, got {:?}", other),
        }
    }

    #[test]
    fn plain_text_no_indent_is_text() {
        let text = "just one line of text";
        match from_plain_text(text) {
            KeptPayload::Text { text: t, .. } => assert_eq!(t, text),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn marker_wins_over_html_structure() {
        // Manually craft HTML where the marker says Reference but
        // the visible content looks like an Outline. Marker
        // wins — that's the round-trip contract.
        let cell_id = Uuid::now_v7();
        let p = KeptPayload::Reference {
            target: SerTarget::Cell { cell_id },
            snippet: "X".into(),
        };
        let html = format!("{}<ul><li>fake</li></ul>", to_html(&p));
        let back = from_clipboard(Some(&html), "");
        match back {
            KeptPayload::Reference { .. } => {} // ✓
            other => panic!("marker should have won, got {:?}", other),
        }
    }

    /// REGRESSION: emitted HTML for a nested outline must encode
    /// the nesting via nested `<ul>` elements so Google Docs (and
    /// any other rich-text receiver) reconstructs the hierarchy.
    /// The earlier `render_outline` left siblings flat at depth 1
    /// when going from depth=0 to depth=1, dropping the nest.
    #[test]
    fn html_outline_emits_nested_ul_for_nested_bullets() {
        let p = KeptPayload::Outline {
            bullets: vec![
                BulletPayload { depth: 0, text: "alpha".into(), links: vec![] },
                BulletPayload { depth: 1, text: "beta".into(), links: vec![] },
                BulletPayload { depth: 0, text: "gamma".into(), links: vec![] },
            ],
        };
        let html = to_html(&p);
        // After the hidden marker span, the visible content
        // should be: <ul><li>alpha<ul><li>beta</li></ul></li><li>gamma</li></ul>
        let visible_idx = html
            .find("<ul>")
            .expect("emit at least one <ul>");
        let visible = &html[visible_idx..];
        assert_eq!(
            visible,
            "<ul><li>alpha<ul><li>beta</li></ul></li><li>gamma</li></ul>",
            "outline must round-trip through a nested-ul HTML emission"
        );
    }

    /// REGRESSION: round-tripping the same nested outline (HTML
    /// only, no marker) through the parser must give back the
    /// same depths.
    #[test]
    fn html_outline_roundtrip_without_marker_preserves_depths() {
        let original = vec![
            BulletPayload { depth: 0, text: "alpha".into(), links: vec![] },
            BulletPayload { depth: 1, text: "beta".into(), links: vec![] },
            BulletPayload { depth: 0, text: "gamma".into(), links: vec![] },
        ];
        let p = KeptPayload::Outline { bullets: original.clone() };
        let html = to_html(&p);
        // Strip the marker span so we exercise the HTML parser, not
        // the marker fast-path.
        let visible_idx = html.find("<ul>").unwrap();
        let visible = &html[visible_idx..];
        match parse_html(visible).unwrap() {
            KeptPayload::Outline { bullets } => {
                let depths: Vec<u32> = bullets.iter().map(|b| b.depth).collect();
                let texts: Vec<String> = bullets.iter().map(|b| b.text.clone()).collect();
                assert_eq!(depths, vec![0, 1, 0]);
                assert_eq!(texts, vec!["alpha", "beta", "gamma"]);
            }
            other => panic!("expected Outline, got {:?}", other),
        }
    }

    #[test]
    fn bullet_prefix_stripped_in_plain_text_parse() {
        let text = "- alpha\n    - beta";
        match from_plain_text(text) {
            KeptPayload::Outline { bullets } => {
                assert_eq!(bullets[0].text, "alpha");
                assert_eq!(bullets[1].text, "beta");
            }
            other => panic!("expected Outline, got {:?}", other),
        }
    }
}
