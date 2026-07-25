use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;

use crate::color::blend;
use crate::terminal_palette::default_bg;
use crate::terminal_palette::default_fg;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();
const MOTION_GREEN_RGB: (u8, u8, u8) = (32, 160, 80);

fn elapsed_since_start() -> Duration {
    let start = PROCESS_START.get_or_init(Instant::now);
    start.elapsed()
}

pub(crate) fn shimmer_spans(text: &str) -> Vec<Span<'static>> {
    let base_color = default_fg().unwrap_or((128, 128, 128));
    let highlight_color = default_bg().unwrap_or((255, 255, 255));
    shimmer_spans_with_palette(
        text,
        base_color,
        highlight_color,
        ShimmerFallback::Intensity,
    )
}

pub(crate) fn green_shimmer_spans(text: &str) -> Vec<Span<'static>> {
    shimmer_spans_with_palette(
        text,
        MOTION_GREEN_RGB,
        (160, 255, 190),
        ShimmerFallback::Solid(Color::Green),
    )
}

pub(crate) fn white_green_shimmer_spans(text: &str) -> Vec<Span<'static>> {
    let grapheme_count = text.graphemes(true).count();
    white_green_shimmer_spans_at(text, sweep_position(grapheme_count), motion_green_style())
}

pub(crate) fn motion_green_style() -> Style {
    let color = if supports_color::on_cached(supports_color::Stream::Stdout)
        .map(|level| level.has_16m)
        .unwrap_or(false)
    {
        Color::Rgb(MOTION_GREEN_RGB.0, MOTION_GREEN_RGB.1, MOTION_GREEN_RGB.2)
    } else {
        Color::Green
    };
    Style::default().fg(color)
}

#[derive(Clone, Copy)]
enum ShimmerFallback {
    Intensity,
    Solid(Color),
}

fn shimmer_spans_with_palette(
    text: &str,
    base_color: (u8, u8, u8),
    highlight_color: (u8, u8, u8),
    fallback: ShimmerFallback,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    // Use time-based sweep synchronized to process start.
    let padding = 10usize;
    let pos = sweep_position(chars.len());
    let has_true_color = supports_color::on_cached(supports_color::Stream::Stdout)
        .map(|level| level.has_16m)
        .unwrap_or(false);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chars.len());
    for (i, ch) in chars.iter().enumerate() {
        let t = band_intensity(i, pos, padding);
        let style = if has_true_color {
            let highlight = t.clamp(0.0, 1.0);
            let (r, g, b) = blend(highlight_color, base_color, highlight * 0.9);
            // Allow custom RGB colors, as the implementation is thoughtfully
            // adjusting the level of the default foreground color.
            #[allow(clippy::disallowed_methods)]
            let style = Style::default().fg(Color::Rgb(r, g, b));
            style.add_modifier(Modifier::BOLD)
        } else {
            fallback_style(t, fallback)
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    spans
}

fn white_green_shimmer_spans_at(text: &str, pos: usize, green_style: Style) -> Vec<Span<'static>> {
    let padding = 10usize;
    style_runs(text, |index| {
        let intensity = band_intensity(index, pos, padding);
        if intensity < 0.2 {
            Style::default()
        } else if intensity < 0.6 {
            green_style
        } else {
            green_style.add_modifier(Modifier::BOLD)
        }
    })
}

fn style_runs(text: &str, style_at: impl Fn(usize) -> Style) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(5);
    let mut current_style = None;
    let mut current_text = String::new();

    for (index, grapheme) in text.graphemes(true).enumerate() {
        let style = style_at(index);
        if current_style.is_some_and(|current| current != style) {
            spans.push(Span::styled(
                std::mem::take(&mut current_text),
                current_style.expect("style exists"),
            ));
        }
        if current_style != Some(style) {
            current_style = Some(style);
        }
        current_text.push_str(grapheme);
    }

    if let Some(style) = current_style {
        spans.push(Span::styled(current_text, style));
    }
    spans
}

fn sweep_position(text_len: usize) -> usize {
    let padding = 10usize;
    let period = text_len + padding * 2;
    let sweep_seconds = 2.0f32;
    let pos_f =
        (elapsed_since_start().as_secs_f32() % sweep_seconds) / sweep_seconds * period as f32;
    pos_f as usize
}

fn band_intensity(index: usize, pos: usize, padding: usize) -> f32 {
    let dist = ((index + padding) as isize - pos as isize).unsigned_abs() as f32;
    let band_half_width = 5.0;
    if dist <= band_half_width {
        let x = std::f32::consts::PI * (dist / band_half_width);
        0.5 * (1.0 + x.cos())
    } else {
        0.0
    }
}

fn fallback_style(intensity: f32, fallback: ShimmerFallback) -> Style {
    match fallback {
        ShimmerFallback::Intensity => color_for_level(intensity),
        ShimmerFallback::Solid(color) => color_for_level(intensity).fg(color),
    }
}

fn color_for_level(intensity: f32) -> Style {
    // Tune fallback styling so the shimmer band reads even without RGB support.
    if intensity < 0.2 {
        Style::default().add_modifier(Modifier::DIM)
    } else if intensity < 0.6 {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_green_sweep_uses_bounded_style_runs() {
        let text = "A long active summary remains white outside its green sweep";
        let green = Style::default().fg(Color::Rgb(
            MOTION_GREEN_RGB.0,
            MOTION_GREEN_RGB.1,
            MOTION_GREEN_RGB.2,
        ));
        let spans = white_green_shimmer_spans_at(text, 30, green);

        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            text
        );
        assert!(spans.len() <= 5, "{spans:#?}");
        assert!(spans.iter().any(|span| span.style == Style::default()));
        assert!(spans.iter().any(|span| span.style.fg == green.fg));
        assert!(
            spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn white_green_sweep_preserves_grapheme_boundaries() {
        let text = "A e\u{301} 👩‍💻 Z";
        let green = Style::default().fg(Color::Green);
        let spans = white_green_shimmer_spans_at(text, 14, green);
        let grapheme_boundaries = text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .collect::<std::collections::HashSet<_>>();
        let mut byte_offset = 0;

        for span in &spans {
            byte_offset += span.content.len();
            assert!(
                grapheme_boundaries.contains(&byte_offset),
                "span boundary {byte_offset} splits a grapheme in {text:?}"
            );
        }
        assert_eq!(byte_offset, text.len());
        assert!(white_green_shimmer_spans_at("", 0, green).is_empty());
    }
}
