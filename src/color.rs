//! Centralized color palette. Every paint in the app should pull its
//! color from a named constant in this file rather than spelling the hex
//! inline. Lets us audit the whole palette in one place and reskin
//! later by editing values, not 90 call sites.
//!
//! Names describe the *role* (`TEXT_PRIMARY`, `EMBED_BORDER`,
//! `DIVIDER_PANE`) not the value (`DARK_GREY`, `WARM_TAN`). If we
//! redesign, the role stays the same and only the hex changes.
//!
//! Active palette: realtimecolors.com green-on-pale-green light theme.
//! Five ramps × 11 shades each:
//!   - `text` / `bg` / `primary` — green; text and bg share hue.
//!   - `secondary` — blue; used for links, focus, selection.
//!   - `accent`    — purple; used for reference-embed chrome to keep
//!                   embeds visibly distinct from links.
//! The palette has no red, so destructive actions and PopPop-error
//! text retain their original reds.

use skia_safe::Color;

// ---------------------------------------------------------------------------
// Page / surface backgrounds
// ---------------------------------------------------------------------------

/// Page background (bg-50). Very pale green; the canvas is cleared to
/// this every frame.
pub const BG_PAGE: Color = Color::from_rgb(233, 252, 239);

/// Slightly more saturated panel background (bg-100). Search popup /
/// tag-menu panel fills, where we want a tiny bit of contrast against
/// `BG_PAGE`.
#[allow(dead_code)]
pub const BG_PANEL: Color = Color::from_rgb(210, 249, 223);

/// White card backdrop behind the focused cell + context menus + search
/// popup + scrollbar knob.
pub const BG_CARD: Color = Color::WHITE;

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Primary body / heading / cell text (text-950). Near-black with a
/// green tint.
pub const TEXT_PRIMARY: Color = Color::from_rgb(3, 23, 12);

/// Slightly lighter text for context-menu rows other than "Delete"
/// (text-800).
pub const TEXT_MENU_ROW: Color = Color::from_rgb(11, 91, 47);

/// Generic muted text (text-700) — embed footer text, deleted-cell
/// placeholder, search-popup secondary text.
pub const TEXT_MUTED_GREY: Color = Color::from_rgb(16, 137, 70);

/// Three muted gradations used in the sidebar + entity-page headers.
/// Slight steps communicate hierarchy. text-800 → text-700 → text-600.
pub const TEXT_MUTED_WARM_DEEP: Color = Color::from_rgb(11, 91, 47);
pub const TEXT_MUTED_WARM: Color = Color::from_rgb(16, 137, 70);
pub const TEXT_MUTED_WARM_SOFT: Color = Color::from_rgb(21, 183, 94);
/// Section-header tone (`BACKING CELL`, `REFERENCED IN`, sidebar
/// section labels) and entity metadata. text-600.
pub const TEXT_SECTION_HEADER: Color = Color::from_rgb(21, 183, 94);

/// Outline-cell bullet marker dot (text-700).
pub const BULLET_MARKER: Color = Color::from_rgb(16, 137, 70);

/// Lightest tints, for tertiary info / placeholder labels. Pale green
/// shades from the bg ramp so they sit just above the page background
/// without going neon. bg-200 / bg-300 territory.
pub const TEXT_GHOST: Color = Color::from_rgb(121, 236, 159); // bg-300
pub const TEXT_DISABLED: Color = Color::from_rgb(166, 242, 191); // bg-200
pub const TEXT_GHOST_WARM: Color = Color::from_rgb(166, 242, 191); // bg-200

// ---------------------------------------------------------------------------
// Inline links + Pop-Pop syntax coloring
// ---------------------------------------------------------------------------

/// Underlined link text + the underline itself (secondary-600). Mid-blue.
pub const LINK_TEXT: Color = Color::from_rgb(24, 115, 180);

/// Pop-Pop computed-output column text. Darker blue (secondary-700) so
/// it reads as data rather than as a link.
pub const POPPOP_OUTPUT: Color = Color::from_rgb(18, 86, 135);

/// Pop-Pop comment-line text (rows starting with `#`). Mid-deep green
/// from the primary ramp (primary-700).
pub const POPPOP_COMMENT: Color = Color::from_rgb(18, 135, 71);

/// Pop-Pop error-line text. Kept red — the palette has no red ramp,
/// and "error = red" is a near-universal convention.
pub const POPPOP_ERROR: Color = Color::from_rgb(0x9a, 0x1e, 0x1e);

// ---------------------------------------------------------------------------
// Accent (focus / selection)
// ---------------------------------------------------------------------------

/// Base accent (secondary-500). Most call sites reach for one of the
/// alpha variants below; the bare opaque form is here for future use
/// and for symmetry.
#[allow(dead_code)]
pub const ACCENT_BLUE: Color = Color::from_rgb(31, 144, 224);

/// `ACCENT_BLUE` with a custom alpha — convenience for the few places
/// (scrollbar fade, focus ring) that need to scale it from a runtime
/// value rather than a fixed constant.
#[inline]
pub const fn accent_blue_alpha(a: u8) -> Color {
    Color::from_argb(a, 31, 144, 224)
}

/// Bullet-range / TextBox selection highlight (40/255).
pub const ACCENT_BLUE_SELECTION: Color = accent_blue_alpha(0x40);
/// Active pane border (80/255).
pub const ACCENT_BLUE_PANE_BORDER: Color = accent_blue_alpha(0x80);
/// Focus ring in edit mode (full opacity).
pub const ACCENT_BLUE_FOCUS_EDIT: Color = accent_blue_alpha(0xff);

// ---------------------------------------------------------------------------
// Reference embeds
// ---------------------------------------------------------------------------

/// Dashed border around a reference cell. Uses the accent (purple-blue)
/// ramp so embeds read as visually distinct from inline links, which
/// use the secondary (blue) ramp.
pub const EMBED_BORDER: Color = Color::from_rgb(24, 58, 180); // accent-600
/// Faint accent tint filling the embed body's background.
pub const EMBED_TINT: Color = Color::from_argb(0x0c, 24, 58, 180);
/// Accent tint for hovering an embed-related menu row.
pub const EMBED_HOVER: Color = Color::from_argb(0x20, 24, 58, 180);

// ---------------------------------------------------------------------------
// Destructive (delete, red)
// ---------------------------------------------------------------------------

pub const DELETE_TEXT: Color = Color::from_rgb(0xc0, 0x30, 0x30);
pub const DELETE_HOVER_BG: Color = Color::from_argb(0x20, 0xc0, 0x30, 0x30);

// ---------------------------------------------------------------------------
// Borders, dividers, hairlines
// ---------------------------------------------------------------------------

/// 1 px border around context menus / search popup. bg-200.
pub const MENU_BORDER: Color = Color::from_rgb(166, 242, 191);

/// Search popup outer border. Same ramp position as `MENU_BORDER`,
/// keeps the parameter slot for future re-skinning. primary-200.
pub const PANEL_BORDER_WARM: Color = Color::from_rgb(165, 243, 200);

/// Pane divider — gutter between left/right panes. bg-200 default,
/// bg-300 on hover.
pub const DIVIDER_PANE: Color = Color::from_rgb(166, 242, 191);
pub const DIVIDER_PANE_HOVER: Color = Color::from_rgb(121, 236, 159);

/// Hairline divider above the action rows in the cell context menu
/// (28/255 over text-950).
pub const HAIRLINE_DIVIDER: Color = Color::from_argb(0x28, 3, 23, 12);

/// Hover tint for sidebar rows and entity-page button backgrounds
/// (24/255 over text-950).
pub const HOVER_FAINT: Color = Color::from_argb(0x18, 3, 23, 12);

/// `TEXT_PRIMARY` (the deep text-950 green-near-black) with a runtime
/// alpha. Used for the cell outline (`CELL_OUTLINE_ALPHA` in app.rs),
/// the scrollbar thumb (alpha fades with idle time), and the
/// entity-page "Create backing cell" button background.
#[inline]
pub const fn dark_alpha(a: u8) -> Color {
    Color::from_argb(a, 3, 23, 12)
}

/// Pure black with a runtime alpha. Drop shadows that scale with the
/// element they sit beneath (focus card / panel).
#[inline]
pub const fn black_alpha(a: u8) -> Color {
    Color::from_argb(a, 0, 0, 0)
}

/// Border around the "+ Create backing cell" button on entity pages
/// (64/255 over text-950).
pub const BUTTON_BORDER_FAINT: Color = Color::from_argb(0x40, 3, 23, 12);

/// Toggle pill background when off (entity active/inactive, People
/// "Show inactive"). Mid-green at 96/255.
pub const TOGGLE_OFF_BG: Color = Color::from_argb(0x60, 21, 183, 94);

/// Background of the inactive-toggle indicator pill (entity page).
/// Deeper green at 48/255.
pub const TOGGLE_INACTIVE_BG: Color = Color::from_argb(0x30, 11, 91, 47);

/// Heading-rule under date / context section labels in the cell loop
/// (128/255 over text-700).
pub const HEADING_RULE: Color = Color::from_argb(0x80, 16, 137, 70);

// ---------------------------------------------------------------------------
// Drop shadows
// ---------------------------------------------------------------------------

/// Soft shadow under floating UI (focus card, etc.).
pub const SHADOW_SOFT: Color = Color::from_argb(0x30, 0, 0, 0);
/// Slightly darker shadow under context menus + search popup.
pub const SHADOW_MENU: Color = Color::from_argb(0x40, 0, 0, 0);

// ---------------------------------------------------------------------------
// Calculator-style grid (Pop-Pop, Table)
// ---------------------------------------------------------------------------

/// Alternating row stripe. Pulled from the secondary (blue) ramp so the
/// calc grid keeps a "data-y" feel against the green page bg.
/// secondary-50.
pub const GRID_STRIPE: Color = Color::from_rgb(233, 244, 252);
/// Vertical column dividers (64/255 over text-700).
pub const GRID_DIVIDER: Color = Color::from_argb(0x40, 16, 137, 70);
