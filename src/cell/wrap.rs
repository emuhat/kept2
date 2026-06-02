//! Pure text wrapping + word/character/line helpers shared by every body
//! type that lays out wrapped text. Nothing here mutates cell state; all
//! functions take whatever they need by reference.
//!
//! Visibility is `pub(super)` (visible to `cell.rs` and its sibling
//! body modules) — none of these helpers are part of the crate-public
//! surface.

use std::ops::Range;

use skia_safe::{Canvas, Font, Paint, Point};

use super::common::{Affinity, LinkSpan, is_kept_tag_url};

/// Draw the visible portion of `line` from `text` at `(text_x, baseline)`,
/// switching paints per span:
/// - `kept-tag://` URLs render in the dim ghost color used for committed
///   `#tag` tokens. No underline.
/// - any other URL renders in `link_paint` with an underline.
/// Plain text uses `text_paint`.
pub(super) fn draw_line_with_links(
    canvas: &Canvas,
    text: &str,
    line: &Range<usize>,
    links: &[LinkSpan],
    text_x: f32,
    baseline: f32,
    font: &Font,
    text_paint: &Paint,
    link_paint: &Paint,
    underline_paint: &Paint,
) {
    let mut tag_paint = Paint::default();
    tag_paint.set_anti_alias(true);
    tag_paint.set_color(crate::color::text_ghost());

    let mut pos = line.start;
    let mut x = text_x;
    while pos < line.end {
        let in_link = links
            .iter()
            .find(|l| pos >= l.range.start && pos < l.range.end);
        let run_end = match in_link {
            Some(l) => l.range.end.min(line.end),
            None => links
                .iter()
                .filter(|l| l.range.start > pos && l.range.start < line.end)
                .map(|l| l.range.start)
                .min()
                .unwrap_or(line.end),
        };
        let segment = &text[pos..run_end];
        let is_tag = in_link.map(|l| is_kept_tag_url(&l.url)).unwrap_or(false);
        let paint = match in_link {
            Some(_) if is_tag => &tag_paint,
            Some(_) => link_paint,
            None => text_paint,
        };
        canvas.draw_str(segment, Point::new(x, baseline), font, paint);
        let w = font.measure_str(segment, Some(paint)).0;
        if in_link.is_some() && !is_tag {
            canvas.draw_line(
                (x, baseline + 2.0),
                (x + w, baseline + 2.0),
                underline_paint,
            );
        }
        x += w;
        pos = run_end;
    }
}

/// X-pixel offset within a line up to byte `target_offset`. Heading
/// and body lines now use a uniform main-font model — tags are
/// painted as an overdraw in the original line font, just with a
/// dimmer color, so position computations don't need to be tag-aware.
pub(super) fn line_x_at_offset(
    line_text: &str,
    target_offset: usize,
    main_font: &Font,
    paint: &Paint,
) -> f32 {
    let target = target_offset.min(line_text.len());
    main_font.measure_str(&line_text[..target], Some(paint)).0
}

/// Parse trailing `#tag` tokens from a heading paragraph. `text` is the
/// full source text; `heading_end` is the byte offset where the heading
/// paragraph ends (exclusive). Returns absolute-in-`text` byte ranges for
/// each tag, in source order. Tags are whitespace-separated tokens
/// starting with `#`. A bare `#` (length 1) counts as a tag-in-progress
/// so the user gets stable rendering as they type out the next tag's name.
/// Operates on title text only (no leading `# ` marker), so any
/// `#`-prefixed token — including the very first one — is a tag candidate.
pub(super) fn parse_heading_tags(text: &str, heading_end: usize) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut tags: Vec<Range<usize>> = Vec::new();
    let mut end = heading_end;
    loop {
        // Skip trailing whitespace.
        while end > 0 && (bytes[end - 1] as char).is_whitespace() {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        // Find start of last word.
        let mut start = end;
        while start > 0 && !(bytes[start - 1] as char).is_whitespace() {
            start -= 1;
        }
        // Any `#`-prefixed word is a tag — even a bare `#` (the user is
        // mid-typing a new tag).
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

/// Parse `#tag` tokens anywhere in `text`. A token is `#` followed by
/// zero or more non-whitespace, non-`#` chars; the `#` must be preceded
/// by whitespace or sit at the start of the text (so `abc#xyz` and
/// embedded fragments like `http://x.com#y` are NOT tags). A bare `#`
/// counts (matches `parse_heading_tags`'s "tag-in-progress" rule). Tag
/// ranges include the leading `#`. Returned in source order.
pub fn parse_inline_tags(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut tags: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let prev_is_boundary = i == 0 || (bytes[i - 1] as char).is_whitespace();
        if bytes[i] == b'#' && prev_is_boundary {
            let start = i;
            let mut end = i + 1;
            // Walk forward: tag body is non-whitespace, non-`#` chars.
            // A second `#` ends the tag (so `#a#b` parses as `#a` and
            // then `#b` doesn't qualify because it's not whitespace-led).
            while end < bytes.len() {
                let c = bytes[end] as char;
                if c.is_whitespace() || c == '#' {
                    break;
                }
                end += 1;
            }
            tags.push(start..end);
            i = end;
        } else {
            i += 1;
        }
    }
    tags
}

/// End byte of the visible portion of a line: drops a single trailing
/// `'\n'` (which lives at the end of the line range but isn't drawn).
pub(super) fn trim_nl_end(text: &str, line: &Range<usize>) -> usize {
    if line.end > line.start && text.as_bytes().get(line.end - 1) == Some(&b'\n') {
        line.end - 1
    } else {
        line.end
    }
}

pub(super) fn locate_caret(
    lines: &[Range<usize>],
    idx: usize,
    affinity: Affinity,
) -> (usize, usize) {
    if lines.is_empty() {
        return (0, 0);
    }
    for (i, line) in lines.iter().enumerate() {
        if idx <= line.end {
            if idx == line.end && affinity == Affinity::Downstream && i + 1 < lines.len() {
                return (i + 1, 0);
            }
            let local = idx.saturating_sub(line.start).min(line.end - line.start);
            return (i, local);
        }
    }
    let last = lines.len() - 1;
    let line = &lines[last];
    (last, line.end - line.start)
}

pub(super) fn is_wrap_boundary(body_lines: &[Range<usize>], idx: usize) -> bool {
    if body_lines.len() < 2 {
        return false;
    }
    body_lines[..body_lines.len() - 1]
        .iter()
        .any(|line| line.end == idx)
}

pub(super) fn step_horizontal(
    text: &str,
    body_lines: &[Range<usize>],
    head: usize,
    affinity: Affinity,
    dir: i32,
) -> (usize, Affinity) {
    if dir > 0 {
        if affinity == Affinity::Upstream && is_wrap_boundary(body_lines, head) {
            return (head, Affinity::Downstream);
        }
        let next = next_char_boundary(text, head);
        let new_aff = if is_wrap_boundary(body_lines, next) {
            Affinity::Upstream
        } else {
            Affinity::Downstream
        };
        (next, new_aff)
    } else {
        if affinity == Affinity::Downstream && is_wrap_boundary(body_lines, head) {
            return (head, Affinity::Upstream);
        }
        let prev = prev_char_boundary(text, head);
        (prev, Affinity::Downstream)
    }
}

pub(super) fn hit_affinity(line_idx: usize, idx: usize, body_lines: &[Range<usize>]) -> Affinity {
    let line = &body_lines[line_idx];
    if idx == line.start && line_idx > 0 {
        Affinity::Downstream
    } else if idx == line.end && line_idx + 1 < body_lines.len() {
        Affinity::Upstream
    } else {
        Affinity::Downstream
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CharClass {
    Word,
    Whitespace,
    Other,
}

pub(super) fn char_class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

pub(super) fn find_word_at(text: &str, idx: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let class_at = |i: usize| -> Option<CharClass> {
        text.get(i..).and_then(|s| s.chars().next()).map(char_class)
    };
    let class_left = |i: usize| -> Option<CharClass> {
        if i == 0 {
            None
        } else {
            class_at(prev_char_boundary(text, i))
        }
    };

    let here = class_at(idx);
    let left = class_left(idx);

    let (probe, class) = match (left, here) {
        (Some(l), Some(h)) if l != h => {
            if l == CharClass::Word {
                (prev_char_boundary(text, idx), CharClass::Word)
            } else {
                (idx, h)
            }
        }
        (_, Some(h)) => (idx, h),
        (Some(l), None) => (prev_char_boundary(text, idx), l),
        (None, None) => return idx..idx,
    };

    let mut start = probe;
    while start > 0 {
        let p = prev_char_boundary(text, start);
        if class_at(p) == Some(class) {
            start = p;
        } else {
            break;
        }
    }
    let mut end = next_char_boundary(text, probe);
    while end < text.len() && class_at(end) == Some(class) {
        end = next_char_boundary(text, end);
    }
    start..end
}

pub(super) fn class_at_byte(text: &str, i: usize) -> Option<CharClass> {
    text.get(i..).and_then(|s| s.chars().next()).map(char_class)
}

/// Range from the start of the run containing the char just before `idx`
/// up to `idx`. For Ctrl+Backspace: peek left, walk back through the
/// same-class run. Empty range if `idx == 0`.
pub(super) fn find_word_left_of(text: &str, idx: usize) -> Range<usize> {
    if idx == 0 {
        return 0..0;
    }
    let prev = prev_char_boundary(text, idx);
    let target = class_at_byte(text, prev);
    let mut start = prev;
    while start > 0 {
        let p = prev_char_boundary(text, start);
        if class_at_byte(text, p) == target {
            start = p;
        } else {
            break;
        }
    }
    start..idx
}

/// Range from `idx` to the end of the run starting at `idx`. For
/// Ctrl+Delete. Empty range if `idx >= text.len()`.
pub(super) fn find_word_right_of(text: &str, idx: usize) -> Range<usize> {
    if idx >= text.len() {
        return idx..idx;
    }
    let target = class_at_byte(text, idx);
    let mut end = next_char_boundary(text, idx);
    while end < text.len() && class_at_byte(text, end) == target {
        end = next_char_boundary(text, end);
    }
    idx..end
}

pub(super) fn find_line_at(
    body_lines: &[Range<usize>],
    idx: usize,
    affinity: Affinity,
) -> Range<usize> {
    if body_lines.is_empty() {
        return 0..0;
    }
    let (line_idx, _) = locate_caret(body_lines, idx, affinity);
    body_lines[line_idx].clone()
}

pub(super) fn next_char_boundary(text: &str, idx: usize) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    let mut i = idx + 1;
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

pub(super) fn prev_char_boundary(text: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx - 1;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Wrap into a contiguous partition of the text, treating `'\n'` as a
/// hard paragraph break. Each visual line's range includes its trailing
/// `'\n'` (if any). Empty paragraphs produce empty visual lines.
///
/// Wraps `text` paragraph-by-paragraph, using `heading_font` for the
/// first paragraph if `force_heading` is set, and `body_font` otherwise.
/// Returns `(lines, is_heading, is_comment)` aligned by index. Comment
/// classification per source paragraph: leading `#` (after optional
/// whitespace) marks the whole paragraph (including wrapped continuations)
/// as a comment when `enable_comment_coloring` is set.
pub(super) fn wrap_text_styled(
    text: &str,
    body_font: &Font,
    heading_font: &Font,
    paint: &Paint,
    max_width: f32,
    force_heading: bool,
    enable_comment_coloring: bool,
) -> (Vec<Range<usize>>, Vec<bool>, Vec<bool>) {
    if text.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let mut lines: Vec<Range<usize>> = Vec::new();
    let mut is_heading: Vec<bool> = Vec::new();
    let mut is_comment: Vec<bool> = Vec::new();
    let mut start = 0usize;
    loop {
        let nl = text[start..].find('\n').map(|p| start + p);
        let para_end = nl.unwrap_or(text.len());
        let font = if force_heading {
            heading_font
        } else {
            body_font
        };
        let para_is_comment =
            enable_comment_coloring && text[start..para_end].trim_start().starts_with('#');
        let prev = lines.len();
        wrap_paragraph_into(text, start, para_end, font, paint, max_width, &mut lines);
        let consumed_to = nl.map(|i| i + 1).unwrap_or(text.len());
        if let Some(last) = lines.last_mut() {
            last.end = consumed_to;
        }
        for _ in prev..lines.len() {
            is_heading.push(force_heading);
            is_comment.push(para_is_comment);
        }
        match nl {
            Some(i) => start = i + 1,
            None => break,
        }
    }
    (lines, is_heading, is_comment)
}

/// Word-wrap a single paragraph (no `'\n'` inside) and append its lines
/// to `out`. Empty paragraphs emit one empty range so each paragraph
/// contributes ≥ 1 line.
pub(super) fn wrap_paragraph_into(
    text: &str,
    start: usize,
    end: usize,
    font: &Font,
    paint: &Paint,
    max_width: f32,
    out: &mut Vec<Range<usize>>,
) {
    if start >= end {
        out.push(start..end);
        return;
    }
    // Collect word ranges within [start, end) (whitespace-delimited).
    let mut words: Vec<Range<usize>> = Vec::new();
    let mut word_start: Option<usize> = None;
    for (i, c) in text[start..end].char_indices() {
        let abs = start + i;
        if c.is_whitespace() {
            if let Some(s) = word_start.take() {
                words.push(s..abs);
            }
        } else if word_start.is_none() {
            word_start = Some(abs);
        }
    }
    if let Some(s) = word_start {
        words.push(s..end);
    }
    if words.is_empty() {
        out.push(start..end);
        return;
    }
    let mut line_start = start;
    let mut have_word = false;
    for word in &words {
        if have_word {
            let candidate = &text[line_start..word.end];
            if font.measure_str(candidate, Some(paint)).0 > max_width {
                out.push(line_start..word.start);
                line_start = word.start;
            }
        }
        have_word = true;
        // If this word starts the line and is itself wider than the
        // viewport (e.g., a long URL with no spaces), break inside it
        // at char boundaries — otherwise the line would render outside
        // the cell. Each emitted slice is the largest char-bounded
        // prefix that fits; the trailing remainder stays on the
        // current line for the next word check to extend.
        if line_start == word.start
            && font.measure_str(&text[word.start..word.end], Some(paint)).0 > max_width
        {
            let mut cursor = word.start;
            while font.measure_str(&text[cursor..word.end], Some(paint)).0 > max_width {
                let take = largest_prefix_fitting(&text[cursor..word.end], font, paint, max_width);
                let next = if take == 0 {
                    // Even one char overflows (max_width is sub-glyph).
                    // Force one char so we don't loop forever.
                    text[cursor..word.end]
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1)
                } else {
                    take
                };
                if cursor + next >= word.end {
                    break;
                }
                out.push(line_start..cursor + next);
                cursor += next;
                line_start = cursor;
            }
        }
    }
    out.push(line_start..end);
}

/// Largest byte length `n` such that `s[..n]` ends on a char boundary
/// and `font.measure_str(&s[..n])` ≤ `max_width`. Returns 0 if even the
/// first character doesn't fit. O(n) measure calls in the worst case.
fn largest_prefix_fitting(s: &str, font: &Font, paint: &Paint, max_width: f32) -> usize {
    let mut last_fit = 0;
    for (i, _) in s.char_indices() {
        if i == 0 {
            continue;
        }
        if font.measure_str(&s[..i], Some(paint)).0 > max_width {
            return last_fit;
        }
        last_fit = i;
    }
    s.len()
}
