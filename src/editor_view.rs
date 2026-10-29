//! Visible rows only: editing uses the same GPU text and rectangle renderer.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    editor::Document,
    layout::Bounds,
    renderer::{ColorSpan, Rect, TextRun},
    theme::{BORDER, FG, GREEN, MUTED, SELECTION, SURFACE},
};

pub struct EditorFrame {
    pub rects: Vec<Rect>,
    pub texts: Vec<TextRun>,
    pub text_bounds: Bounds,
    pub cursor: Option<(f32, f32)>,
}

pub fn visual_width(text: &str) -> usize {
    text.graphemes(true).fold(0, |col, glyph| {
        col + if glyph == "\t" {
            4 - col % 4
        } else {
            glyph.width()
        }
    })
}

/// Map document byte colors onto a clipped row without splitting grapheme clusters.
pub(crate) fn visible_line(
    text: &str,
    byte_offset: usize,
    spans: &[ColorSpan],
    start: usize,
    columns: usize,
) -> (String, Vec<ColorSpan>) {
    let mut result = String::new();
    let mut colors: Vec<ColorSpan> = Vec::new();
    let mut span_index = spans.partition_point(|span| span.range.end <= byte_offset);
    let mut col = 0;
    let end = start.saturating_add(columns);
    for (byte, glyph) in text.grapheme_indices(true) {
        let width = if glyph == "\t" {
            4 - col % 4
        } else {
            glyph.width()
        };
        let next = col + width;
        if next > start && col < end {
            let displayed_start = result.len();
            if glyph == "\t" || col < start || next > end {
                result.extend(std::iter::repeat_n(' ', next.min(end) - col.max(start)));
            } else {
                result.push_str(glyph);
            }
            let absolute = byte_offset + byte;
            while spans
                .get(span_index)
                .is_some_and(|span| span.range.end <= absolute)
            {
                span_index += 1;
            }
            if let Some(span) = spans
                .get(span_index)
                .filter(|span| span.range.contains(&absolute))
            {
                if let Some(previous) = colors.last_mut()
                    && previous.range.end == displayed_start
                    && previous.color == span.color
                {
                    previous.range.end = result.len();
                } else {
                    colors.push(ColorSpan {
                        range: displayed_start..result.len(),
                        color: span.color,
                    });
                }
            }
        }
        col = next;
        if col >= end {
            break;
        }
    }
    (result, colors)
}

pub fn paint(
    document: &Document,
    spans: &[ColorSpan],
    bounds: Bounds,
    offset: usize,
    column: usize,
    font: f32,
    cell: (f32, f32),
) -> EditorFrame {
    let lines = document.text().split('\n').count();
    let digits = lines.max(1).to_string().len();
    let gutter = (digits.max(3) + 2) as f32 * cell.0;
    let text_bounds = Bounds {
        x: bounds.x + gutter,
        w: (bounds.w - gutter - 8.0).max(1.0),
        ..bounds
    };
    let rows = (bounds.h / cell.1).floor().max(1.0) as usize;
    let columns = (text_bounds.w / cell.0).floor().max(1.0) as usize;
    let mut frame = EditorFrame {
        rects: Vec::new(),
        texts: Vec::new(),
        text_bounds,
        cursor: None,
    };
    let (cursor_row, cursor_col) = document.cursor_position();
    let selection = document.selection_range();
    frame.rects.push(rect(
        Bounds {
            x: text_bounds.x - 6.0,
            w: 1.0,
            ..bounds
        },
        BORDER,
    ));
    let mut byte = 0;
    for (row, raw) in document.text().split('\n').enumerate() {
        if row >= offset + rows {
            break;
        }
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if row >= offset {
            let y = bounds.y + (row - offset) as f32 * cell.1;
            let row_bounds = Bounds {
                y,
                h: cell.1,
                ..text_bounds
            };
            if row == cursor_row {
                frame.rects.push(rect(row_bounds, SURFACE));
            }
            let start = selection.start.max(byte).min(byte + line.len()) - byte;
            let finish = selection.end.max(byte).min(byte + line.len()) - byte;
            if selection.start < byte + raw.len() + 1 && selection.end > byte {
                let left = visual_width(&line[..start]);
                let mut right = visual_width(&line[..finish]);
                if selection.end > byte + raw.len() {
                    right += 1;
                }
                let visible_left = left.max(column).min(column + columns);
                let visible_right = right.max(column).min(column + columns);
                if visible_right > visible_left {
                    frame.rects.push(rect(
                        Bounds {
                            x: text_bounds.x + (visible_left - column) as f32 * cell.0,
                            w: (visible_right - visible_left) as f32 * cell.0,
                            ..row_bounds
                        },
                        SELECTION,
                    ));
                }
            }
            frame.texts.push(text(
                bounds.x,
                y,
                format!("{:>width$}", row + 1, width = digits.max(3)),
                font,
                cell.1,
                if row == cursor_row { GREEN } else { MUTED },
                Bounds {
                    w: gutter - 8.0,
                    ..bounds
                },
            ));
            let (visible, colors) = visible_line(line, byte, spans, column, columns);
            let mut run = text(text_bounds.x, y, visible, font, cell.1, FG, text_bounds);
            run.colors = colors;
            frame.texts.push(run);
            if row == cursor_row && (column..=column + columns).contains(&cursor_col) {
                let x = text_bounds.x + (cursor_col - column) as f32 * cell.0;
                frame.rects.push(rect(
                    Bounds {
                        x,
                        y,
                        w: 2.0,
                        h: cell.1,
                    },
                    GREEN,
                ));
                frame.cursor = Some((x, y));
            }
        }
        byte += raw.len() + 1;
    }
    frame
}

fn rect(bounds: Bounds, color: [u8; 4]) -> Rect {
    Rect {
        x: bounds.x,
        y: bounds.y,
        width: bounds.w,
        height: bounds.h,
        color,
    }
}

#[allow(clippy::too_many_arguments)]
fn text(
    x: f32,
    y: f32,
    text: String,
    font_size: f32,
    line_height: f32,
    color: [u8; 4],
    clip: Bounds,
) -> TextRun {
    TextRun {
        x,
        y,
        text,
        font_size,
        line_height,
        color,
        colors: Vec::new(),
        bold: false,
        italic: false,
        clip: [clip.x, clip.y, clip.x + clip.w, clip.y + clip.h],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_clipping_preserves_graphemes_and_tab_columns() {
        assert_eq!(visual_width("a\t中文e\u{301}"), 9);
        assert_eq!(visible_line("a\t中文e\u{301}", 0, &[], 1, 6).0, "   中 ");
        assert_eq!(visible_line("中文e\u{301}", 0, &[], 1, 4).0, " 文e\u{301}");
    }

    #[test]
    fn clipped_colors_follow_tabs_unicode_and_absolute_document_offsets() {
        let spans = [
            ColorSpan {
                range: 100..102,
                color: GREEN,
            },
            ColorSpan {
                range: 102..108,
                color: MUTED,
            },
            ColorSpan {
                range: 108..111,
                color: GREEN,
            },
        ];
        let (text, colors) = visible_line("a\t中文e\u{301}", 100, &spans, 1, 8);
        assert_eq!(text, "   中文e\u{301}");
        assert_eq!(
            colors,
            vec![
                ColorSpan {
                    range: 0..3,
                    color: GREEN
                },
                ColorSpan {
                    range: 3..9,
                    color: MUTED
                },
                ColorSpan {
                    range: 9..12,
                    color: GREEN
                },
            ]
        );
        let (text, colors) = visible_line("a\t中文e\u{301}", 100, &spans, 5, 2);
        assert_eq!(text, "  ");
        assert_eq!(
            colors,
            vec![ColorSpan {
                range: 0..2,
                color: MUTED
            }]
        );
    }

    #[test]
    fn colors_keep_gaps_and_never_split_combining_clusters() {
        let spans = [
            ColorSpan {
                range: 0..1,
                color: GREEN,
            },
            ColorSpan {
                range: 1..3,
                color: MUTED,
            },
            ColorSpan {
                range: 4..5,
                color: GREEN,
            },
        ];
        let (text, colors) = visible_line("e\u{301} x", 0, &spans, 0, 10);
        assert_eq!(text, "e\u{301} x");
        assert_eq!(
            colors,
            vec![
                ColorSpan {
                    range: 0..3,
                    color: GREEN
                },
                ColorSpan {
                    range: 4..5,
                    color: GREEN
                },
            ]
        );
        assert!(
            colors
                .iter()
                .all(|span| text.is_char_boundary(span.range.start)
                    && text.is_char_boundary(span.range.end))
        );
    }
}
