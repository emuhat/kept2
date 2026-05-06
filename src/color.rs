//! Centralized color palette. Every paint in the app should pull its
//! color from a named constant in this file rather than spelling the hex
//! inline. Lets us audit the whole palette in one place and reskin
//! later by editing values, not 90 call sites.
//!
//! Names describe the *role* (`TEXT_PRIMARY`, `EMBED_BORDER`,
//! `DIVIDER_PANE`) not the value (`DARK_GREY`, `WARM_TAN`). If we
//! redesign, the role stays the same and only the hex changes.

use skia_safe::Color;

// ---------------------------------------------------------------------------
// Page / surface backgrounds
// ---------------------------------------------------------------------------

/// Cream page background; the canvas is cleared to this every frame.
pub const BG_PAGE: Color = Color::from_rgb(0xfa, 0xf7, 0xf2);

/// Slightly darker cream — search popup / tag-menu panel fills (where
/// the page already is `BG_PAGE` and we want a tiny bit of contrast).
#[allow(dead_code)]
pub const BG_PANEL: Color = Color::from_rgb(0xf2, 0xee, 0xe6);

/// White card backdrop behind the focused cell + context menus + search
/// popup + scrollbar knob.
pub const BG_CARD: Color = Color::WHITE;

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Primary body / heading / cell text. Near-black with a tiny warm cast.
pub const TEXT_PRIMARY: Color = Color::from_rgb(0x1c, 0x1c, 0x1c);

/// Slightly desaturated text for context-menu rows other than "Delete."
pub const TEXT_MENU_ROW: Color = Color::from_rgb(0x40, 0x40, 0x40);

/// Generic muted grey — embed footer text, deleted-cell placeholder,
/// search-popup secondary text.
pub const TEXT_MUTED_GREY: Color = Color::from_rgb(0x80, 0x80, 0x80);

/// Warmer muted variants used in the sidebar + entity-page headers and
/// info lines. Slight gradations communicate hierarchy.
pub const TEXT_MUTED_WARM_DEEP: Color = Color::from_rgb(0x60, 0x58, 0x48);
pub const TEXT_MUTED_WARM: Color = Color::from_rgb(0x70, 0x68, 0x58);
pub const TEXT_MUTED_WARM_SOFT: Color = Color::from_rgb(0x80, 0x78, 0x68);
/// Section-header tone (`BACKING CELL`, `REFERENCED IN`) and entity
/// metadata. Two byte-equivalent shades exist in the wild
/// (`0x88_78` vs `0x88_7a`); we collapse to `0x7a` here.
pub const TEXT_SECTION_HEADER: Color = Color::from_rgb(0x90, 0x88, 0x7a);

/// Outline-cell bullet marker dot. Mid-grey.
pub const BULLET_MARKER: Color = Color::from_rgb(0x60, 0x60, 0x60);

/// Lightest muted greys, for tertiary info / placeholder labels.
pub const TEXT_GHOST: Color = Color::from_rgb(0x90, 0x90, 0x90);
pub const TEXT_DISABLED: Color = Color::from_rgb(0xa0, 0x9a, 0x90);
pub const TEXT_GHOST_WARM: Color = Color::from_rgb(0xa8, 0xa0, 0x90);

// ---------------------------------------------------------------------------
// Inline links + Pop-Pop syntax coloring
// ---------------------------------------------------------------------------

/// Underlined link text + the underline itself. Mid-blue.
pub const LINK_TEXT: Color = Color::from_rgb(0x1a, 0x66, 0xc4);

/// Pop-Pop computed-output column text. Darker blue than `LINK_TEXT` so
/// it reads as data rather than as a link.
pub const POPPOP_OUTPUT: Color = Color::from_rgb(0x18, 0x3a, 0x9c);

/// Pop-Pop comment-line text (rows starting with `#`). Dark green.
pub const POPPOP_COMMENT: Color = Color::from_rgb(0x1f, 0x6b, 0x2a);

/// Pop-Pop error-line text (rendered beneath the failing input row).
pub const POPPOP_ERROR: Color = Color::from_rgb(0x9a, 0x1e, 0x1e);

// ---------------------------------------------------------------------------
// Accent (focus / selection)
// ---------------------------------------------------------------------------

/// Base accent blue. Most call sites reach for one of the alpha variants
/// below; the bare opaque form is here for future use and for symmetry.
#[allow(dead_code)]
pub const ACCENT_BLUE: Color = Color::from_rgb(0x4a, 0x90, 0xe2);

/// `ACCENT_BLUE` with a custom alpha — convenience for the few places
/// (scrollbar fade, focus ring) that need to scale it from a runtime
/// value rather than a fixed constant.
#[inline]
pub const fn accent_blue_alpha(a: u8) -> Color {
    Color::from_argb(a, 0x4a, 0x90, 0xe2)
}

/// Bullet-range / TextBox selection highlight (40/255).
pub const ACCENT_BLUE_SELECTION: Color = accent_blue_alpha(0x40);
/// Active pane border (80/255).
pub const ACCENT_BLUE_PANE_BORDER: Color = accent_blue_alpha(0x80);
/// Focus ring in edit mode (full opacity).
pub const ACCENT_BLUE_FOCUS_EDIT: Color = accent_blue_alpha(0xff);

// ---------------------------------------------------------------------------
// Reference embeds (warm tan)
// ---------------------------------------------------------------------------

/// Warm-tan dashed border around a reference cell.
pub const EMBED_BORDER: Color = Color::from_rgb(0xb3, 0x92, 0x60);
/// Faint warm-tan tint filling the embed body's background.
pub const EMBED_TINT: Color = Color::from_argb(0x0c, 0xb3, 0x92, 0x60);
/// Warm-tan tint for hovering an embed-related menu row.
pub const EMBED_HOVER: Color = Color::from_argb(0x20, 0xb3, 0x92, 0x60);

// ---------------------------------------------------------------------------
// Destructive (delete, red)
// ---------------------------------------------------------------------------

pub const DELETE_TEXT: Color = Color::from_rgb(0xc0, 0x30, 0x30);
pub const DELETE_HOVER_BG: Color = Color::from_argb(0x20, 0xc0, 0x30, 0x30);

// ---------------------------------------------------------------------------
// Borders, dividers, hairlines
// ---------------------------------------------------------------------------

/// Light-grey 1 px border around context menus / search popup.
pub const MENU_BORDER: Color = Color::from_rgb(0xc0, 0xc0, 0xc0);

/// Search popup outer border (slightly warmer than `MENU_BORDER`).
pub const PANEL_BORDER_WARM: Color = Color::from_rgb(0xc8, 0xc0, 0xb0);

/// Pane divider — gutter between left/right panes. Default + hovered.
pub const DIVIDER_PANE: Color = Color::from_rgb(0xdc, 0xd4, 0xc6);
pub const DIVIDER_PANE_HOVER: Color = Color::from_rgb(0xc8, 0xbf, 0xb0);

/// Hairline divider above the action rows in the cell context menu
/// (28/255).
pub const HAIRLINE_DIVIDER: Color = Color::from_argb(0x28, 0x1c, 0x1c, 0x1c);

/// Hover tint for sidebar rows and entity-page button backgrounds.
pub const HOVER_FAINT: Color = Color::from_argb(0x18, 0x1c, 0x1c, 0x1c);

/// `TEXT_PRIMARY` (the dark `0x1c` near-black) with a runtime alpha.
/// Used for the cell outline (`CELL_OUTLINE_ALPHA` in app.rs), the
/// scrollbar thumb (alpha fades with idle time), and the entity-page
/// "Create backing cell" button background.
#[inline]
pub const fn dark_alpha(a: u8) -> Color {
    Color::from_argb(a, 0x1c, 0x1c, 0x1c)
}

/// Pure black with a runtime alpha. Drop shadows that scale with the
/// element they sit beneath (focus card / panel).
#[inline]
pub const fn black_alpha(a: u8) -> Color {
    Color::from_argb(a, 0, 0, 0)
}

/// Border around the "+ Create backing cell" button on entity pages.
pub const BUTTON_BORDER_FAINT: Color = Color::from_argb(0x40, 0x1c, 0x1c, 0x1c);

/// Toggle pill background when off (entity active/inactive, People
/// "Show inactive"). Warm muted.
pub const TOGGLE_OFF_BG: Color = Color::from_argb(0x60, 0x90, 0x88, 0x7a);

/// Background of the inactive-toggle indicator pill (entity page).
pub const TOGGLE_INACTIVE_BG: Color = Color::from_argb(0x30, 0x40, 0x40, 0x40);

/// Heading-rule under date / context section labels in the cell loop.
pub const HEADING_RULE: Color = Color::from_argb(0x80, 0x70, 0x68, 0x58);

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

/// Pale calc-blue alternating row stripe.
pub const GRID_STRIPE: Color = Color::from_rgb(0xed, 0xf3, 0xfa);
/// Muted gray vertical column dividers.
pub const GRID_DIVIDER: Color = Color::from_argb(0x40, 0x60, 0x60, 0x60);
