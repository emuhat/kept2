//! Shared types and helpers used by every cell-body module.
//!
//! Lives at the bottom of the dependency graph: nothing in here imports
//! from sibling cell modules. Everything else in `cell::*` may import from
//! here.

use std::ops::Range;
use std::time::Duration;

// ----- Shared typography & interaction constants -----

pub(crate) const BODY_FONT_SIZE: f32 = 18.0;
pub(crate) const HEADING_FONT_SCALE: f32 = 1.12;
/// Vertical breathing room between a cell's title slot and its body.
/// Logical pixels; scaled with `font_scale`.
pub(crate) const TITLE_BODY_GAP: f32 = 6.0;
pub(crate) const CARET_WIDTH: f32 = 1.5;
/// Alpha applied via `Canvas::save_layer_alpha` to inactive
/// ("archived") cells and bullets when they're surfaced by the
/// global "Show archived" toggle. Lives here so both the app-layer
/// cell-wrap and the bullet-wrap inside `OutlineCell::tick` share a
/// single value. ~0.4 (66/255).
pub(crate) const INACTIVE_ALPHA: u8 = 0x66;
pub(crate) const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const MULTI_CLICK_DIST: f32 = 5.0;

/// Which side of a wrap boundary a caret sits on. At a soft wrap, byte
/// index `i` equals both `line[i].end` and `line[i+1].start`; affinity
/// picks which side the caret is on for rendering and "current line"
/// lookups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Affinity {
    Upstream,
    #[default]
    Downstream,
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    pub affinity: Affinity,
}

impl Selection {
    pub fn caret(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
            affinity: Affinity::Downstream,
        }
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.head
    }

    pub fn range(&self) -> Range<usize> {
        let lo = self.anchor.min(self.head);
        let hi = self.anchor.max(self.head);
        lo..hi
    }
}

#[derive(Clone)]
pub struct Selections {
    pub items: Vec<Selection>,
    pub primary: usize,
}

impl Selections {
    pub fn single_caret(at: usize) -> Self {
        Self {
            items: vec![Selection::caret(at)],
            primary: 0,
        }
    }

    pub fn normalize(&mut self) {
        if self.items.is_empty() {
            self.primary = 0;
            return;
        }
        let primary_head = self.items[self.primary].head;

        self.items.sort_by_key(|s| s.range().start);

        let mut merged: Vec<Selection> = Vec::with_capacity(self.items.len());
        for sel in self.items.drain(..) {
            if let Some(last) = merged.last_mut() {
                if sel.range().start <= last.range().end {
                    let new_lo = last.range().start.min(sel.range().start);
                    let new_hi = last.range().end.max(sel.range().end);
                    if last.anchor <= last.head {
                        last.anchor = new_lo;
                        last.head = new_hi;
                    } else {
                        last.anchor = new_hi;
                        last.head = new_lo;
                    }
                    continue;
                }
            }
            merged.push(sel);
        }
        self.items = merged;

        self.primary = self
            .items
            .iter()
            .position(|s| {
                let r = s.range();
                r.start <= primary_head && primary_head <= r.end
            })
            .unwrap_or(0);
    }
}

pub struct Edit {
    pub range: Range<usize>,
    pub replacement: String,
}

#[derive(Clone, Debug)]
pub struct LinkSpan {
    pub range: Range<usize>,
    pub url: String,
}

/// A `#tag` token marked for tag treatment. Pure byte range — the tag's
/// name is the substring at `range` minus the leading `#`. Tag spans
/// only exist when the user explicitly committed a tag through the
/// mention popup (or when they were migrated in from legacy text on
/// first load): typing `#X` without commit leaves no span and no tag.
#[derive(Clone, Debug)]
pub struct TagSpan {
    pub range: Range<usize>,
}

/// A point-in-time clone of a cell's document state. Used by undo/redo to
/// roll a cell back to a previous text + selection + zoom configuration.
/// View-only state (drag, click count, line cache, geometry) is excluded.
#[derive(Clone)]
pub struct TextBoxSnapshot {
    pub text: String,
    pub sels: Selections,
    pub font_scale: f32,
    pub links: Vec<LinkSpan>,
    pub tags: Vec<TagSpan>,
}

/// Whether the platform "primary" modifier is held — Cmd on macOS, Ctrl
/// everywhere else. Use this for app shortcuts (Ctrl+Enter / Cmd+Enter, etc.)
/// instead of reading `control_key()` directly so Mac builds feel native.
pub(crate) fn primary_mod(state: winit::keyboard::ModifiersState) -> bool {
    #[cfg(target_os = "macos")]
    {
        state.super_key()
    }
    #[cfg(not(target_os = "macos"))]
    {
        state.control_key()
    }
}

/// Whether the "word-navigation" modifier is held — Option on macOS (Mac
/// convention is Option+Arrow for word jumps; Cmd+Arrow is line/doc edges)
/// and Ctrl elsewhere. Use this for word-nav and word-delete only; app
/// shortcuts should use `primary_mod`.
pub(crate) fn word_mod(state: winit::keyboard::ModifiersState) -> bool {
    #[cfg(target_os = "macos")]
    {
        state.alt_key()
    }
    #[cfg(not(target_os = "macos"))]
    {
        state.control_key()
    }
}

/// Mac-only: Cmd+Left / Cmd+Right move to start / end of the current visual
/// line (with Shift, extend the selection there). Linux/Windows already
/// have dedicated Home/End keys, so there's no equivalent on those
/// platforms — this returns false off-Mac and the existing Home/End keys
/// stay the canonical line-edge gesture.
pub(crate) fn line_edge_mod(state: winit::keyboard::ModifiersState) -> bool {
    #[cfg(target_os = "macos")]
    {
        state.super_key()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state;
        false
    }
}

/// Index transform after replacing `text[start..start+del]` with `ins`
/// bytes. Right-gravity at the boundary: an insertion exactly at `i`
/// pushes `i` forward. Used for caret positions that should ride with
/// inserted text.
pub(crate) fn transform_index(i: usize, start: usize, del: usize, ins: usize) -> usize {
    if i < start {
        i
    } else if i == start && del == 0 {
        i + ins
    } else if i >= start + del {
        i - del + ins
    } else {
        start + ins
    }
}

/// Like `transform_index`, but with left-gravity at the boundary: an
/// insertion exactly at `i` does not push `i` forward. Used for link end
/// positions so typing right after a link doesn't extend it.
pub(crate) fn transform_index_closed_end(
    i: usize,
    start: usize,
    del: usize,
    ins: usize,
) -> usize {
    if i < start {
        i
    } else if i == start && del == 0 {
        i
    } else if i >= start + del {
        i - del + ins
    } else {
        start + ins
    }
}

pub(crate) fn open_url(url: &str) {
    eprintln!("Opening link: {url}");
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
