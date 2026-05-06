//! Shared "calc grid" visual styling — row stripes + vertical dividers.
//! Used by both `PopPopCell` (its two-column layout) and `TableCell`
//! (its rows × cols grid). One source of truth so the two stay coherent.

use skia_safe::{Canvas, Color, Paint, Rect};

/// Alternating row stripe color (very pale calc-blue). Painted on
/// odd-indexed bands to give the spreadsheet feel.
pub(super) const GRID_STRIPE_RGB: (u8, u8, u8) = (0xed, 0xf3, 0xfa);

/// Muted gray used for vertical column dividers.
pub(super) const GRID_DIVIDER_ARGB: (u8, u8, u8, u8) = (0x40, 0x60, 0x60, 0x60);

/// Padding on each side of a vertical divider line, in logical pixels.
pub(super) const GRID_DIVIDER_PAD: f32 = 8.0;

/// Paint odd-indexed bands of `bands` with the calc-blue stripe color,
/// spanning `[left, right]`. Bands are `(top, bottom)` in display order.
pub(super) fn draw_alternating_row_stripes(
    canvas: &Canvas,
    bands: &[(f32, f32)],
    left: f32,
    right: f32,
) {
    let (r, g, b) = GRID_STRIPE_RGB;
    let mut stripe = Paint::default();
    stripe.set_anti_alias(true);
    stripe.set_color(Color::from_rgb(r, g, b));
    for (i, &(top, bot)) in bands.iter().enumerate() {
        if i % 2 == 1 {
            canvas.draw_rect(Rect::new(left, top, right, bot), &stripe);
        }
    }
}

/// Draw a single muted-gray vertical divider from `(x, y_top)` to
/// `(x, y_bot)`.
pub(super) fn draw_vertical_divider(canvas: &Canvas, x: f32, y_top: f32, y_bot: f32) {
    let (a, r, g, b) = GRID_DIVIDER_ARGB;
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(Color::from_argb(a, r, g, b));
    paint.set_stroke_width(1.0);
    canvas.draw_line((x, y_top), (x, y_bot), &paint);
}
