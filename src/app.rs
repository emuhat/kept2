use skia_safe::{Canvas, Color, Font, FontMgr, Paint, Point, Typeface};

const FONT_BYTES: &[u8] = include_bytes!("../resources/fonts/Figtree.ttf");

const PARAGRAPH: &str = "Kept is a small, intentional space for the things you actually want \
to hold on to — the kind of details that drift out of inboxes and chat threads before you \
remember why they mattered. It is not a database, not a knowledge graph, not a second brain; \
it's a sturdy shelf with a few good hooks. Open it on a quiet morning, write down the name \
of someone you'd like to talk to again, the title of a book a friend mentioned, a question \
you haven't yet found the right time to ask. Close it. Come back later and find it where \
you left it, exactly as you put it down, because the only feature this app commits to is \
keeping.";

const MARGIN_X: f32 = 40.0;
const MARGIN_TOP: f32 = 60.0;

pub struct KeptApp {
    typeface: Typeface,
    body_lines: Vec<String>,
    body_lines_width: f32,
}

impl KeptApp {
    pub fn new() -> Self {
        let typeface = FontMgr::new()
            .new_from_data(FONT_BYTES, None)
            .expect("failed to load embedded TTF");
        Self {
            typeface,
            body_lines: Vec::new(),
            body_lines_width: f32::NAN,
        }
    }

    pub fn tick(&mut self, canvas: &Canvas, width: f32, _height: f32) {
        canvas.clear(Color::from_rgb(0xfa, 0xf7, 0xf2));

        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::from_rgb(0x1c, 0x1c, 0x1c));

        let title_font = Font::from_typeface(&self.typeface, 36.0);
        let body_font = Font::from_typeface(&self.typeface, 18.0);

        let max_text_width = (width - MARGIN_X * 2.0).max(80.0);
        let mut y = MARGIN_TOP;

        let (_, title_metrics) = title_font.metrics();
        y += -title_metrics.ascent;
        canvas.draw_str("Kept", Point::new(MARGIN_X, y), &title_font, &paint);
        y += title_metrics.descent + title_metrics.leading + 18.0;

        let (_, body_metrics) = body_font.metrics();
        let line_height = -body_metrics.ascent + body_metrics.descent + body_metrics.leading;

        if self.body_lines_width != max_text_width {
            self.body_lines = wrap_text(PARAGRAPH, &body_font, &paint, max_text_width);
            self.body_lines_width = max_text_width;
        }

        for line in &self.body_lines {
            y += -body_metrics.ascent;
            canvas.draw_str(line, Point::new(MARGIN_X, y), &body_font, &paint);
            y += body_metrics.descent + body_metrics.leading + line_height * 0.25;
        }
    }
}

fn wrap_text(text: &str, font: &Font, paint: &Paint, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
            continue;
        }
        let candidate_len = current.len() + 1 + word.len();
        let mut candidate = String::with_capacity(candidate_len);
        candidate.push_str(&current);
        candidate.push(' ');
        candidate.push_str(word);

        let (candidate_width, _) = font.measure_str(&candidate, Some(paint));
        if candidate_width <= max_width {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}
