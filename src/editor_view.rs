//! Visible rows only: editing uses the same GPU text and rectangle renderer.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    editor::Document,
    layout::Bounds,
    renderer::{Rect, TextRun},
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

fn visible_line(text: &str, start: usize, columns: usize) -> String {
    let mut result = String::new();
    let mut col = 0;
    let end = start.saturating_add(columns);
    for glyph in text.graphemes(true) {
        let width = if glyph == "\t" {
            4 - col % 4
        } else {
            glyph.width()
        };
        let next = col + width;
        if next > start && col < end {
            if glyph == "\t" || col < start || next > end {
                result.extend(std::iter::repeat_n(' ', next.min(end) - col.max(start)));
            } else {
                result.push_str(glyph);
            }
        }
        col = next;
        if col >= end {
            break;
        }
    }
    result
}

pub fn paint(
    document: &Document,
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
            frame.texts.push(text(
                text_bounds.x,
                y,
                visible_line(line, column, columns),
                font,
                cell.1,
                FG,
                text_bounds,
            ));
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
        assert_eq!(visible_line("a\t中文e\u{301}", 1, 6), "   中 ");
        assert_eq!(visible_line("中文e\u{301}", 1, 4), " 文e\u{301}");
    }
}
