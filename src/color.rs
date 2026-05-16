//! Centralized color palette. Every paint in the app should pull its
//! color from a function in this file rather than spelling the hex
//! inline. Lets us audit the whole palette in one place and reskin
//! later by editing values, not 90 call sites.
//!
//! Names describe the *role* (`text_primary`, `embed_border`,
//! `divider_pane`) not the value (`dark_grey`, `warm_tan`). If we
//! redesign, the role stays the same and only the hex changes.
//!
//! ## Live editing
//!
//! Values come from a YAML file the app polls every few hundred ms.
//! Path resolution:
//!   1. `KEPT_COLORS_PATH` env var if set
//!   2. `dirs::config_dir().join("kept/colors.yaml")`
//!   3. `colors.yaml` in the current working directory
//!
//! On first launch the defaults are written to that path so the user
//! has a starting point. After that, editing the file changes the
//! running app's colors on the next frame after the file's mtime
//! advances. Malformed YAML or missing keys leave the palette
//! untouched and print a warning to stderr — the app stays usable.

use std::fs;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use skia_safe::Color;

// ---------------------------------------------------------------------------
// Palette struct
// ---------------------------------------------------------------------------

/// One field per role. `Copy` so accessor functions can return a
/// `Color` cheaply by copying out of the global lock.
#[derive(Clone, Copy)]
pub struct Palette {
    // Page surfaces
    pub bg_page: Color,
    pub bg_panel: Color,
    pub bg_card: Color,

    // Text — `text_primary` and `text_section_header` paint against
    // the page bg; the `sidebar_*` variants paint against `bg_panel`
    // (the sidebar surface, which can have its own bg). Default to
    // the same values so a palette that doesn't override them looks
    // identical.
    pub text_primary: Color,
    pub text_menu_row: Color,
    pub text_muted_grey: Color,
    pub text_muted_warm_deep: Color,
    pub text_muted_warm: Color,
    pub text_muted_warm_soft: Color,
    pub text_section_header: Color,
    pub sidebar_text_primary: Color,
    pub sidebar_section_header: Color,
    /// Background for the per-pane URL-bar header band. Distinct
    /// from `panel_border_warm` so the header can be tuned in
    /// isolation without affecting the URL-bar dropdown chrome
    /// that historically shared the hue.
    pub pane_header_bg: Color,
    /// Text-selection background when the focused cell is in **view**
    /// mode (read-only). Fully opaque — the original text color
    /// renders on top, so contrast doesn't depend on alpha blending.
    pub text_selection_view: Color,
    /// Text-selection background when the focused cell is in **edit**
    /// mode. Distinct hue from `text_selection_view` so the user
    /// can tell view vs. edit at a glance from any selection alone.
    /// Same opacity rule as the view variant — opaque, text drawn
    /// on top.
    pub text_selection_edit: Color,
    pub bullet_marker: Color,
    pub text_ghost: Color,
    pub text_disabled: Color,
    pub text_ghost_warm: Color,

    // Inline links + Pop-Pop syntax
    pub link_text: Color,
    pub poppop_output: Color,
    pub poppop_comment: Color,
    pub poppop_error: Color,

    // Accent (focus / selection). The base + three pre-baked alpha
    // variants. `accent_blue_alpha(a)` composes a runtime-alpha value
    // off `accent_blue`.
    pub accent_blue: Color,
    pub accent_blue_selection: Color,
    pub accent_blue_pane_border: Color,
    pub accent_blue_focus_edit: Color,

    // Reference embeds
    pub embed_border: Color,
    pub embed_tint: Color,
    pub embed_hover: Color,

    // Cell left-edge bars — slot for click-to-select + state color
    // coding. `default` is the always-present muted slot; the rest
    // are state overrides (snoozed cells get amber, reference /
    // envelope cells get the warm-tan accent, closed cells go dim).
    pub cell_bar_default: Color,
    pub cell_bar_snoozed: Color,
    pub cell_bar_resurfaced: Color,
    pub cell_bar_closed: Color,

    // Destructive
    pub delete_text: Color,
    pub delete_hover_bg: Color,

    // Borders / dividers / hairlines
    pub menu_border: Color,
    pub panel_border_warm: Color,
    pub divider_pane: Color,
    pub divider_pane_hover: Color,
    pub hairline_divider: Color,
    pub hover_faint: Color,
    pub button_border_faint: Color,

    // Toggles + heading rule
    pub toggle_off_bg: Color,
    pub toggle_inactive_bg: Color,
    pub heading_rule: Color,

    // Drop shadows
    pub shadow_soft: Color,
    pub shadow_menu: Color,

    // Calc grid (Pop-Pop, Table)
    pub grid_stripe: Color,
    pub grid_divider: Color,

    // Base used by `dark_alpha(a)` runtime composer (cell outline,
    // scrollbar thumb, button bg). Defaults to `text_primary` if not
    // set explicitly in YAML.
    pub dark_alpha_base: Color,
}

impl Palette {
    /// Hardcoded defaults — match the in-tree values prior to YAML
    /// loading. Used as the initial palette and as the "factory
    /// reset" target written to disk on first launch.
    pub fn defaults() -> Self {
        Self {
            bg_page: Color::from_rgb(233, 252, 239),
            bg_panel: Color::from_rgb(210, 249, 223),
            bg_card: Color::WHITE,

            text_primary: Color::from_rgb(3, 23, 12),
            text_menu_row: Color::from_rgb(11, 91, 47),
            text_muted_grey: Color::from_rgb(16, 137, 70),
            text_muted_warm_deep: Color::from_rgb(11, 91, 47),
            text_muted_warm: Color::from_rgb(16, 137, 70),
            text_muted_warm_soft: Color::from_rgb(21, 183, 94),
            text_section_header: Color::from_rgb(21, 183, 94),
            sidebar_text_primary: Color::from_rgb(3, 23, 12),
            sidebar_section_header: Color::from_rgb(21, 183, 94),
            pane_header_bg: Color::from_rgb(0x5e, 0x00, 0x80),
            text_selection_view: Color::from_rgb(0xc7, 0xe3, 0xf7),
            text_selection_edit: Color::from_rgb(0xc4, 0xf7, 0xe2),
            bullet_marker: Color::from_rgb(16, 137, 70),
            text_ghost: Color::from_rgb(121, 236, 159),
            text_disabled: Color::from_rgb(166, 242, 191),
            text_ghost_warm: Color::from_rgb(166, 242, 191),

            link_text: Color::from_rgb(24, 115, 180),
            poppop_output: Color::from_rgb(18, 86, 135),
            poppop_comment: Color::from_rgb(18, 135, 71),
            poppop_error: Color::from_rgb(0x9a, 0x1e, 0x1e),

            accent_blue: Color::from_rgb(31, 144, 224),
            accent_blue_selection: Color::from_argb(0x40, 31, 144, 224),
            accent_blue_pane_border: Color::from_argb(0x80, 31, 144, 224),
            accent_blue_focus_edit: Color::from_argb(0xff, 31, 144, 224),

            embed_border: Color::from_rgb(24, 58, 180),
            embed_tint: Color::from_argb(0x0c, 24, 58, 180),
            embed_hover: Color::from_argb(0x20, 24, 58, 180),

            // Cell bars: default at ~28% alpha over text_primary so
            // it reads as a present-but-quiet column without
            // competing with cell text. Snoozed = warm amber
            // (waiting). Resurfaced = the same blue accent the
            // reference border uses, so the surfacing origin reads
            // visually. Closed = neutral dim.
            cell_bar_default: Color::from_argb(0x48, 3, 23, 12),
            cell_bar_snoozed: Color::from_rgb(0xd8, 0x91, 0x2a),
            cell_bar_resurfaced: Color::from_rgb(24, 58, 180),
            cell_bar_closed: Color::from_argb(0x70, 3, 23, 12),

            delete_text: Color::from_rgb(0xc0, 0x30, 0x30),
            delete_hover_bg: Color::from_argb(0x20, 0xc0, 0x30, 0x30),

            menu_border: Color::from_rgb(166, 242, 191),
            panel_border_warm: Color::from_rgb(165, 243, 200),
            divider_pane: Color::from_rgb(166, 242, 191),
            divider_pane_hover: Color::from_rgb(121, 236, 159),
            hairline_divider: Color::from_argb(0x28, 3, 23, 12),
            hover_faint: Color::from_argb(0x18, 3, 23, 12),
            button_border_faint: Color::from_argb(0x40, 3, 23, 12),

            toggle_off_bg: Color::from_argb(0x60, 21, 183, 94),
            toggle_inactive_bg: Color::from_argb(0x30, 11, 91, 47),
            heading_rule: Color::from_argb(0x80, 16, 137, 70),

            shadow_soft: Color::from_argb(0x30, 0, 0, 0),
            shadow_menu: Color::from_argb(0x40, 0, 0, 0),

            grid_stripe: Color::from_rgb(233, 244, 252),
            grid_divider: Color::from_argb(0x40, 16, 137, 70),

            dark_alpha_base: Color::from_rgb(3, 23, 12),
        }
    }

    /// Apply a single `key: "#hex"` pair to the appropriate field.
    /// Unknown keys are ignored (with a stderr warning); duplicates
    /// take the last-wins value. Hex parsing accepts `#rrggbb` and
    /// `#rrggbbaa`.
    fn set(&mut self, key: &str, value: Color) {
        match key {
            "bg_page" => self.bg_page = value,
            "bg_panel" => self.bg_panel = value,
            "bg_card" => self.bg_card = value,
            "text_primary" => self.text_primary = value,
            "text_menu_row" => self.text_menu_row = value,
            "text_muted_grey" => self.text_muted_grey = value,
            "text_muted_warm_deep" => self.text_muted_warm_deep = value,
            "text_muted_warm" => self.text_muted_warm = value,
            "text_muted_warm_soft" => self.text_muted_warm_soft = value,
            "text_section_header" => self.text_section_header = value,
            "sidebar_text_primary" => self.sidebar_text_primary = value,
            "sidebar_section_header" => self.sidebar_section_header = value,
            "pane_header_bg" => self.pane_header_bg = value,
            "text_selection_view" => self.text_selection_view = value,
            "text_selection_edit" => self.text_selection_edit = value,
            "bullet_marker" => self.bullet_marker = value,
            "text_ghost" => self.text_ghost = value,
            "text_disabled" => self.text_disabled = value,
            "text_ghost_warm" => self.text_ghost_warm = value,
            "link_text" => self.link_text = value,
            "poppop_output" => self.poppop_output = value,
            "poppop_comment" => self.poppop_comment = value,
            "poppop_error" => self.poppop_error = value,
            "accent_blue" => self.accent_blue = value,
            "accent_blue_selection" => self.accent_blue_selection = value,
            "accent_blue_pane_border" => self.accent_blue_pane_border = value,
            "accent_blue_focus_edit" => self.accent_blue_focus_edit = value,
            "embed_border" => self.embed_border = value,
            "embed_tint" => self.embed_tint = value,
            "embed_hover" => self.embed_hover = value,
            "cell_bar_default" => self.cell_bar_default = value,
            "cell_bar_snoozed" => self.cell_bar_snoozed = value,
            "cell_bar_resurfaced" => self.cell_bar_resurfaced = value,
            "cell_bar_closed" => self.cell_bar_closed = value,
            "delete_text" => self.delete_text = value,
            "delete_hover_bg" => self.delete_hover_bg = value,
            "menu_border" => self.menu_border = value,
            "panel_border_warm" => self.panel_border_warm = value,
            "divider_pane" => self.divider_pane = value,
            "divider_pane_hover" => self.divider_pane_hover = value,
            "hairline_divider" => self.hairline_divider = value,
            "hover_faint" => self.hover_faint = value,
            "button_border_faint" => self.button_border_faint = value,
            "toggle_off_bg" => self.toggle_off_bg = value,
            "toggle_inactive_bg" => self.toggle_inactive_bg = value,
            "heading_rule" => self.heading_rule = value,
            "shadow_soft" => self.shadow_soft = value,
            "shadow_menu" => self.shadow_menu = value,
            "grid_stripe" => self.grid_stripe = value,
            "grid_divider" => self.grid_divider = value,
            "dark_alpha_base" => self.dark_alpha_base = value,
            other => {
                eprintln!("kept: colors.yaml — unknown key `{other}` ignored");
            }
        }
    }

    /// Parse a YAML-ish file (a tiny subset: `key: "#hex"` per line,
    /// `#`-comments, blank lines). Quotes are optional. Unknown keys
    /// are warned about but don't fail the parse — the rest of the
    /// file still applies. Returns the palette mutated from
    /// `Self::defaults()` plus a count of applied keys.
    pub fn from_yaml(text: &str) -> Self {
        let mut p = Self::defaults();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(colon) = line.find(':') else {
                eprintln!(
                    "kept: colors.yaml line {}: missing `:`, skipping",
                    lineno + 1,
                );
                continue;
            };
            let key = line[..colon].trim();
            let raw_value = line[colon + 1..].trim();
            // Quoted values: drop quotes first; whatever's inside is
            // the value verbatim (may contain `#` as part of a hex
            // color). Unquoted values: strip a trailing `# comment`.
            let value = if let Some(rest) = raw_value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| raw_value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            {
                rest
            } else {
                match raw_value.find('#') {
                    Some(0) | None => raw_value,
                    Some(hash) => raw_value[..hash].trim(),
                }
            };
            match parse_hex_color(value) {
                Some(c) => p.set(key, c),
                None => eprintln!(
                    "kept: colors.yaml line {}: bad color `{}` for `{}`",
                    lineno + 1,
                    value,
                    key,
                ),
            }
        }
        p
    }

    /// Default YAML written to disk on first launch. Each line names
    /// a role and gives its `#rrggbb` (or `#rrggbbaa`) value. Ordered
    /// by category so a human editor sees logical grouping.
    pub fn defaults_yaml() -> String {
        let p = Self::defaults();
        let mut out = String::new();
        out.push_str("# kept color palette — edit this file and the running app picks up changes\n");
        out.push_str("# within ~250ms. Hex colors: `#rrggbb` (opaque) or `#rrggbbaa` (with alpha).\n");
        out.push_str("# Lines starting with `#` are comments. Blank lines are fine.\n");
        out.push_str("# Unknown keys are warned about on stderr; malformed lines are skipped.\n\n");

        let row = |out: &mut String, key: &str, c: Color| {
            out.push_str(&format!("{}: \"{}\"\n", key, color_to_hex(c)));
        };

        out.push_str("# Page surfaces\n");
        row(&mut out, "bg_page", p.bg_page);
        row(&mut out, "bg_panel", p.bg_panel);
        row(&mut out, "bg_card", p.bg_card);

        out.push_str("\n# Text\n");
        row(&mut out, "text_primary", p.text_primary);
        row(&mut out, "text_menu_row", p.text_menu_row);
        row(&mut out, "text_muted_grey", p.text_muted_grey);
        row(&mut out, "text_muted_warm_deep", p.text_muted_warm_deep);
        row(&mut out, "text_muted_warm", p.text_muted_warm);
        row(&mut out, "text_muted_warm_soft", p.text_muted_warm_soft);
        row(&mut out, "text_section_header", p.text_section_header);
        row(&mut out, "sidebar_text_primary", p.sidebar_text_primary);
        row(&mut out, "sidebar_section_header", p.sidebar_section_header);
        row(&mut out, "pane_header_bg", p.pane_header_bg);
        row(&mut out, "text_selection_view", p.text_selection_view);
        row(&mut out, "text_selection_edit", p.text_selection_edit);
        row(&mut out, "bullet_marker", p.bullet_marker);
        row(&mut out, "text_ghost", p.text_ghost);
        row(&mut out, "text_disabled", p.text_disabled);
        row(&mut out, "text_ghost_warm", p.text_ghost_warm);

        out.push_str("\n# Inline links + Pop-Pop\n");
        row(&mut out, "link_text", p.link_text);
        row(&mut out, "poppop_output", p.poppop_output);
        row(&mut out, "poppop_comment", p.poppop_comment);
        row(&mut out, "poppop_error", p.poppop_error);

        out.push_str("\n# Accent (focus / selection)\n");
        row(&mut out, "accent_blue", p.accent_blue);
        row(&mut out, "accent_blue_selection", p.accent_blue_selection);
        row(&mut out, "accent_blue_pane_border", p.accent_blue_pane_border);
        row(&mut out, "accent_blue_focus_edit", p.accent_blue_focus_edit);

        out.push_str("\n# Reference embeds\n");
        row(&mut out, "embed_border", p.embed_border);
        row(&mut out, "embed_tint", p.embed_tint);
        row(&mut out, "embed_hover", p.embed_hover);

        out.push_str("\n# Cell left-edge state bars\n");
        row(&mut out, "cell_bar_default", p.cell_bar_default);
        row(&mut out, "cell_bar_snoozed", p.cell_bar_snoozed);
        row(&mut out, "cell_bar_resurfaced", p.cell_bar_resurfaced);
        row(&mut out, "cell_bar_closed", p.cell_bar_closed);

        out.push_str("\n# Destructive\n");
        row(&mut out, "delete_text", p.delete_text);
        row(&mut out, "delete_hover_bg", p.delete_hover_bg);

        out.push_str("\n# Borders / dividers / hairlines\n");
        row(&mut out, "menu_border", p.menu_border);
        row(&mut out, "panel_border_warm", p.panel_border_warm);
        row(&mut out, "divider_pane", p.divider_pane);
        row(&mut out, "divider_pane_hover", p.divider_pane_hover);
        row(&mut out, "hairline_divider", p.hairline_divider);
        row(&mut out, "hover_faint", p.hover_faint);
        row(&mut out, "button_border_faint", p.button_border_faint);

        out.push_str("\n# Toggles + heading rule\n");
        row(&mut out, "toggle_off_bg", p.toggle_off_bg);
        row(&mut out, "toggle_inactive_bg", p.toggle_inactive_bg);
        row(&mut out, "heading_rule", p.heading_rule);

        out.push_str("\n# Drop shadows\n");
        row(&mut out, "shadow_soft", p.shadow_soft);
        row(&mut out, "shadow_menu", p.shadow_menu);

        out.push_str("\n# Calc grid (Pop-Pop, Table)\n");
        row(&mut out, "grid_stripe", p.grid_stripe);
        row(&mut out, "grid_divider", p.grid_divider);

        out.push_str("\n# Base used by the `dark_alpha(a)` runtime composer (cell outline,\n");
        out.push_str("# scrollbar thumb, button background).\n");
        row(&mut out, "dark_alpha_base", p.dark_alpha_base);

        out
    }
}

// ---------------------------------------------------------------------------
// Hex parsing / formatting
// ---------------------------------------------------------------------------

/// Parse `#rrggbb` or `#rrggbbaa` (case-insensitive) into a `Color`.
/// Returns `None` for any other shape.
fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim();
    let hex = s.strip_prefix('#')?;
    let bytes = hex.as_bytes();
    if !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let parse_u8 = |a: u8, b: u8| -> Option<u8> {
        let hi = (a as char).to_digit(16)? as u8;
        let lo = (b as char).to_digit(16)? as u8;
        Some(hi * 16 + lo)
    };
    match bytes.len() {
        6 => {
            let r = parse_u8(bytes[0], bytes[1])?;
            let g = parse_u8(bytes[2], bytes[3])?;
            let b = parse_u8(bytes[4], bytes[5])?;
            Some(Color::from_rgb(r, g, b))
        }
        8 => {
            let r = parse_u8(bytes[0], bytes[1])?;
            let g = parse_u8(bytes[2], bytes[3])?;
            let b = parse_u8(bytes[4], bytes[5])?;
            let a = parse_u8(bytes[6], bytes[7])?;
            Some(Color::from_argb(a, r, g, b))
        }
        _ => None,
    }
}

/// Render a `Color` as `#rrggbb` (alpha 0xff) or `#rrggbbaa`. Used by
/// `defaults_yaml()` so the file mirrors the parser's input format.
fn color_to_hex(c: Color) -> String {
    let r = c.r();
    let g = c.g();
    let b = c.b();
    let a = c.a();
    if a == 0xff {
        format!("#{:02x}{:02x}{:02x}", r, g, b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", r, g, b, a)
    }
}

// ---------------------------------------------------------------------------
// Global storage + hot reload
// ---------------------------------------------------------------------------

static PALETTE: OnceLock<RwLock<Palette>> = OnceLock::new();

fn palette_lock() -> &'static RwLock<Palette> {
    PALETTE.get_or_init(|| RwLock::new(Palette::defaults()))
}

/// The active palette. Cheap — copies a `Palette` (≈170 bytes) out
/// from under a read lock. Each accessor function below calls this
/// and pulls a single field.
pub fn palette() -> Palette {
    *palette_lock().read().expect("color palette poisoned")
}

/// Path the palette is loaded from. Resolution: `KEPT_COLORS_PATH`,
/// then `dirs::config_dir().join("kept/colors.yaml")`, then
/// `colors.yaml` in cwd. The path is decided once per process.
pub fn colors_path() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Ok(p) = std::env::var("KEPT_COLORS_PATH") {
            return PathBuf::from(p);
        }
        if let Some(dir) = dirs::config_dir() {
            return dir.join("kept").join("colors.yaml");
        }
        PathBuf::from("colors.yaml")
    })
    .clone()
}

/// Write the default YAML to `colors_path()` if it doesn't exist.
/// Creates parent directories. No-op if the file is already present.
/// Called once at app startup; the path is printed to stderr so the
/// user knows where to edit.
pub fn ensure_colors_file_exists() {
    let path = colors_path();
    if path.exists() {
        eprintln!("kept: editing colors at {}", path.display());
        return;
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "kept: couldn't create {}: {} — falling back to defaults",
                parent.display(),
                e,
            );
            return;
        }
    }
    match fs::write(&path, Palette::defaults_yaml()) {
        Ok(_) => eprintln!("kept: wrote default colors to {}", path.display()),
        Err(e) => eprintln!(
            "kept: couldn't write {}: {} — falling back to defaults",
            path.display(),
            e,
        ),
    }
}

/// Reload the palette from disk if the YAML's mtime has advanced
/// since the last successful load. Throttled to ~250ms so the per-
/// frame cost is one cheap `fs::metadata` call most of the time.
/// Errors (missing file, parse failures) leave the active palette
/// untouched.
pub fn maybe_reload() {
    static LAST_POLL: OnceLock<RwLock<Instant>> = OnceLock::new();
    static LAST_MTIME: OnceLock<RwLock<Option<SystemTime>>> = OnceLock::new();

    let poll = LAST_POLL.get_or_init(|| RwLock::new(Instant::now() - Duration::from_secs(60)));
    {
        let last = *poll.read().expect("poll lock");
        if last.elapsed() < Duration::from_millis(250) {
            return;
        }
    }
    *poll.write().expect("poll lock") = Instant::now();

    let path = colors_path();
    let Ok(meta) = fs::metadata(&path) else {
        return;
    };
    let Ok(mtime) = meta.modified() else {
        return;
    };
    let mtime_lock =
        LAST_MTIME.get_or_init(|| RwLock::new(None));
    if Some(mtime) == *mtime_lock.read().expect("mtime lock") {
        return;
    }
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "kept: couldn't read {}: {} — keeping current palette",
                path.display(),
                e,
            );
            return;
        }
    };
    let new_pal = Palette::from_yaml(&text);
    *palette_lock().write().expect("palette lock") = new_pal;
    *mtime_lock.write().expect("mtime lock") = Some(mtime);
}

// ---------------------------------------------------------------------------
// Accessor functions — one per role.
// ---------------------------------------------------------------------------

#[inline]
pub fn bg_page() -> Color {
    palette().bg_page
}
#[inline]
#[allow(dead_code)]
pub fn bg_panel() -> Color {
    palette().bg_panel
}
#[inline]
pub fn bg_card() -> Color {
    palette().bg_card
}

#[inline]
pub fn text_primary() -> Color {
    palette().text_primary
}
#[inline]
pub fn text_menu_row() -> Color {
    palette().text_menu_row
}
#[inline]
pub fn text_muted_grey() -> Color {
    palette().text_muted_grey
}
#[inline]
pub fn text_muted_warm_deep() -> Color {
    palette().text_muted_warm_deep
}
#[inline]
pub fn text_muted_warm() -> Color {
    palette().text_muted_warm
}
#[inline]
pub fn text_muted_warm_soft() -> Color {
    palette().text_muted_warm_soft
}
#[inline]
pub fn text_section_header() -> Color {
    palette().text_section_header
}
#[inline]
pub fn sidebar_text_primary() -> Color {
    palette().sidebar_text_primary
}
#[inline]
pub fn sidebar_section_header() -> Color {
    palette().sidebar_section_header
}
#[inline]
pub fn bullet_marker() -> Color {
    palette().bullet_marker
}
#[inline]
pub fn text_ghost() -> Color {
    palette().text_ghost
}
#[inline]
pub fn text_disabled() -> Color {
    palette().text_disabled
}
#[inline]
pub fn text_ghost_warm() -> Color {
    palette().text_ghost_warm
}

#[inline]
pub fn link_text() -> Color {
    palette().link_text
}
#[inline]
pub fn poppop_output() -> Color {
    palette().poppop_output
}
#[inline]
pub fn poppop_comment() -> Color {
    palette().poppop_comment
}
#[inline]
pub fn poppop_error() -> Color {
    palette().poppop_error
}

#[inline]
#[allow(dead_code)]
pub fn accent_blue() -> Color {
    palette().accent_blue
}
#[inline]
pub fn accent_blue_selection() -> Color {
    palette().accent_blue_selection
}
#[inline]
pub fn accent_blue_pane_border() -> Color {
    palette().accent_blue_pane_border
}
#[inline]
pub fn accent_blue_focus_edit() -> Color {
    palette().accent_blue_focus_edit
}

/// `accent_blue` with a runtime alpha. Currently unused (the
/// text-selection path moved to dedicated opaque colors), kept as
/// a generic helper for any future caller that needs a fade-in
/// blue overlay.
#[allow(dead_code)]
#[inline]
pub fn accent_blue_alpha(a: u8) -> Color {
    let c = palette().accent_blue;
    Color::from_argb(a, c.r(), c.g(), c.b())
}

/// Sidebar section-header color composed with a runtime alpha.
/// Used by the focus ring so the active cell pops in the same hue
/// as the WHAT / WHEN section labels.
#[inline]
pub fn sidebar_section_header_alpha(a: u8) -> Color {
    let c = palette().sidebar_section_header;
    Color::from_argb(a, c.r(), c.g(), c.b())
}

/// Background fill for the pane URL-bar header band.
#[inline]
pub fn pane_header_bg() -> Color {
    palette().pane_header_bg
}

/// Text-selection background colors, opaque. The view/edit pair
/// reads from the palette and the call site picks one based on
/// the focused cell's `editing` flag (via the `show_caret`
/// surrogate). Opaque on purpose: text is drawn on top, so the
/// contrast doesn't depend on alpha blending.
#[inline]
pub fn text_selection_view() -> Color {
    palette().text_selection_view
}

#[inline]
pub fn text_selection_edit() -> Color {
    palette().text_selection_edit
}

#[inline]
pub fn embed_border() -> Color {
    palette().embed_border
}
#[inline]
pub fn embed_tint() -> Color {
    palette().embed_tint
}
#[inline]
pub fn embed_hover() -> Color {
    palette().embed_hover
}
#[inline]
pub fn cell_bar_default() -> Color {
    palette().cell_bar_default
}
#[inline]
pub fn cell_bar_snoozed() -> Color {
    palette().cell_bar_snoozed
}
#[inline]
pub fn cell_bar_resurfaced() -> Color {
    palette().cell_bar_resurfaced
}
#[inline]
pub fn cell_bar_closed() -> Color {
    palette().cell_bar_closed
}

#[inline]
pub fn delete_text() -> Color {
    palette().delete_text
}
#[inline]
pub fn delete_hover_bg() -> Color {
    palette().delete_hover_bg
}

#[inline]
pub fn menu_border() -> Color {
    palette().menu_border
}
#[inline]
pub fn panel_border_warm() -> Color {
    palette().panel_border_warm
}
#[inline]
pub fn divider_pane() -> Color {
    palette().divider_pane
}
#[inline]
pub fn divider_pane_hover() -> Color {
    palette().divider_pane_hover
}
#[inline]
pub fn hairline_divider() -> Color {
    palette().hairline_divider
}
#[inline]
pub fn hover_faint() -> Color {
    palette().hover_faint
}
#[inline]
pub fn button_border_faint() -> Color {
    palette().button_border_faint
}

/// `dark_alpha_base` (defaults to `text_primary`) with a runtime
/// alpha. Used by the cell outline, scrollbar thumb, and the
/// entity-page "Create backing cell" button background.
#[inline]
pub fn dark_alpha(a: u8) -> Color {
    let c = palette().dark_alpha_base;
    Color::from_argb(a, c.r(), c.g(), c.b())
}

/// Pure black with a runtime alpha. Drop shadows that scale with the
/// element they sit beneath.
#[inline]
pub fn black_alpha(a: u8) -> Color {
    Color::from_argb(a, 0, 0, 0)
}

#[inline]
pub fn toggle_off_bg() -> Color {
    palette().toggle_off_bg
}
#[inline]
pub fn toggle_inactive_bg() -> Color {
    palette().toggle_inactive_bg
}
#[inline]
pub fn heading_rule() -> Color {
    palette().heading_rule
}

#[inline]
pub fn shadow_soft() -> Color {
    palette().shadow_soft
}
#[inline]
pub fn shadow_menu() -> Color {
    palette().shadow_menu
}

#[inline]
pub fn grid_stripe() -> Color {
    palette().grid_stripe
}
#[inline]
pub fn grid_divider() -> Color {
    palette().grid_divider
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_six_and_eight_digit() {
        let c6 = parse_hex_color("#1c1c1c").expect("6-digit");
        assert_eq!(c6.r(), 0x1c);
        assert_eq!(c6.a(), 0xff);
        let c8 = parse_hex_color("#1f90e040").expect("8-digit");
        assert_eq!(c8.r(), 0x1f);
        assert_eq!(c8.a(), 0x40);
    }

    #[test]
    fn parse_hex_rejects_garbage() {
        assert!(parse_hex_color("nope").is_none());
        assert!(parse_hex_color("#zzzzzz").is_none());
        assert!(parse_hex_color("#12345").is_none());
    }

    #[test]
    fn from_yaml_overrides_named_keys_only() {
        let yaml = r##"
            # comment
            text_primary: "#ff0000"
            bg_page: '#00ff00'
            unknown_key: "#abcdef"
        "##;
        let p = Palette::from_yaml(yaml);
        assert_eq!(p.text_primary, Color::from_rgb(255, 0, 0));
        assert_eq!(p.bg_page, Color::from_rgb(0, 255, 0));
        // Defaults preserved for keys not in the YAML.
        assert_eq!(p.bg_card, Palette::defaults().bg_card);
    }

    #[test]
    fn defaults_yaml_round_trips() {
        let yaml = Palette::defaults_yaml();
        let parsed = Palette::from_yaml(&yaml);
        let defaults = Palette::defaults();
        // Spot-check a handful of fields — round-trip is exact for
        // every #rrggbb / #rrggbbaa value the defaults produce.
        assert_eq!(parsed.text_primary, defaults.text_primary);
        assert_eq!(parsed.embed_tint, defaults.embed_tint);
        assert_eq!(parsed.bg_card, defaults.bg_card);
    }
}

