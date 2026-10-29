//! Compact conflict comparison above the existing editable result buffer.

use crate::{
    editor_view::visible_line,
    layout::Bounds,
    merge::{Choice, Conflict},
    renderer::{ColorSpan, Rect, TextRun},
    theme::{AMBER, BG, BORDER, FG, GREEN, MUTED, PANEL, RED, SURFACE},
};

#[derive(Clone, Copy, Debug)]
pub enum MergeAction {
    Previous,
    Next,
    Accept(Choice),
}

pub struct MergeFrame {
    pub rects: Vec<Rect>,
    pub texts: Vec<TextRun>,
    pub buttons: Vec<(Bounds, MergeAction)>,
    pub editor_bounds: Bounds,
    pub comparison_bounds: Option<Bounds>,
}

#[allow(clippy::too_many_arguments)]
pub fn paint(
    text: &str,
    spans: &[ColorSpan],
    conflicts: &[Conflict],
    selected: usize,
    error: Option<&str>,
    bounds: Bounds,
    font: f32,
    cell: (f32, f32),
    offset: usize,
    column: usize,
) -> MergeFrame {
    let mut frame = MergeFrame {
        rects: Vec::new(),
        texts: Vec::new(),
        buttons: Vec::new(),
        editor_bounds: bounds,
        comparison_bounds: None,
    };
    let header = Bounds {
        h: 32.0_f32.min(bounds.h),
        ..bounds
    };
    frame.rect(header, PANEL);
    let comparison_height = if let Some(error) = error {
        frame.label(header, "Conflict markers need attention", 12.0, RED);
        frame.label(
            Bounds {
                y: header.y + 30.0,
                h: 24.0,
                ..header
            },
            error,
            11.0,
            MUTED,
        );
        58.0_f32.min(bounds.h)
    } else if conflicts.is_empty() {
        frame.label(
            header,
            "No conflict blocks · review and save the result",
            12.0,
            GREEN,
        );
        header.h
    } else {
        let selected = selected.min(conflicts.len() - 1);
        let conflict = &conflicts[selected];
        let comparison_height = (bounds.h * 0.4).min(250.0);
        let right = bounds.x + bounds.w - 8.0;
        let both = Bounds {
            x: right - 108.0,
            y: header.y + 3.0,
            w: 108.0,
            h: 26.0,
        };
        let next = Bounds {
            x: both.x - 34.0,
            w: 28.0,
            ..both
        };
        let previous = Bounds {
            x: next.x - 34.0,
            ..next
        };
        frame.label(
            Bounds {
                w: (previous.x - bounds.x - 8.0).max(0.0),
                ..header
            },
            &format!(
                "Conflict {}/{} · {} left",
                selected + 1,
                conflicts.len(),
                conflicts.len()
            ),
            11.0,
            FG,
        );
        frame.button(previous, "←", MergeAction::Previous, MUTED, false);
        frame.button(next, "→", MergeAction::Next, MUTED, false);
        frame.button(
            both,
            "Accept Both",
            MergeAction::Accept(Choice::Both),
            GREEN,
            true,
        );
        let comparison = Bounds {
            y: header.y + header.h,
            h: (comparison_height - header.h).max(0.0),
            ..bounds
        };
        frame.comparison_bounds = Some(comparison);
        let half = ((bounds.w - 8.0) / 2.0).max(0.0);
        for (incoming, source, label) in [
            (false, &conflict.current, &conflict.current_label),
            (true, &conflict.incoming, &conflict.incoming_label),
        ] {
            let side = Bounds {
                x: bounds.x + if incoming { half + 8.0 } else { 0.0 },
                w: half,
                ..comparison
            };
            let color = if incoming { AMBER } else { GREEN };
            frame.rect(side, PANEL);
            frame.rect(Bounds { w: 2.0, ..side }, color);
            let accept = Bounds {
                x: side.x + side.w - 120.0,
                y: side.y + 4.0,
                w: 114.0,
                h: 25.0,
            };
            frame.label(
                Bounds {
                    w: (side.w - 126.0).max(0.0),
                    h: 32.0,
                    ..side
                },
                if incoming { "INCOMING" } else { "CURRENT" },
                10.0,
                color,
            );
            frame.button(
                accept,
                if incoming {
                    "Accept Incoming"
                } else {
                    "Accept Current"
                },
                MergeAction::Accept(if incoming {
                    Choice::Incoming
                } else {
                    Choice::Current
                }),
                color,
                false,
            );
            frame.label(
                Bounds {
                    y: side.y + 30.0,
                    h: 18.0,
                    ..side
                },
                if label.is_empty() {
                    "(unlabelled marker)"
                } else {
                    label
                },
                10.0,
                MUTED,
            );
            let code = Bounds {
                x: side.x + 8.0,
                y: side.y + 52.0,
                w: (side.w - 16.0).max(0.0),
                h: (side.h - 58.0).max(0.0),
            };
            frame.source(
                text.get(source.clone()).unwrap_or(""),
                source.start,
                spans,
                code,
                font,
                cell,
                offset,
                column,
            );
        }
        comparison_height
    };
    let result = Bounds {
        y: bounds.y + comparison_height,
        h: 24.0_f32.min((bounds.h - comparison_height).max(0.0)),
        ..bounds
    };
    frame.rect(result, BG);
    frame.rect(Bounds { h: 1.0, ..result }, BORDER);
    frame.label(result, "RESULT · Editable", 10.0, GREEN);
    frame.editor_bounds = Bounds {
        y: result.y + result.h,
        h: (bounds.h - comparison_height - result.h).max(0.0),
        ..bounds
    };
    frame
}

impl MergeFrame {
    fn rect(&mut self, bounds: Bounds, color: [u8; 4]) {
        if bounds.w > 0.0 && bounds.h > 0.0 {
            self.rects.push(Rect {
                x: bounds.x,
                y: bounds.y,
                width: bounds.w,
                height: bounds.h,
                color,
            });
        }
    }

    fn label(&mut self, bounds: Bounds, label: &str, font: f32, color: [u8; 4]) {
        let text: String = label
            .chars()
            .filter(|c| !c.is_control())
            .take(180)
            .collect();
        self.texts.push(TextRun {
            x: bounds.x + 8.0,
            y: bounds.y + (bounds.h - font * 1.4).max(0.0) / 2.0,
            text,
            font_size: font,
            line_height: font * 1.4,
            color,
            colors: Vec::new(),
            bold: false,
            italic: false,
            clip: [bounds.x, bounds.y, bounds.x + bounds.w, bounds.y + bounds.h],
        });
    }

    fn button(
        &mut self,
        bounds: Bounds,
        label: &str,
        action: MergeAction,
        color: [u8; 4],
        primary: bool,
    ) {
        self.rect(bounds, if primary { color } else { BORDER });
        if !primary {
            self.rect(
                Bounds {
                    x: bounds.x + 1.0,
                    y: bounds.y + 1.0,
                    w: bounds.w - 2.0,
                    h: bounds.h - 2.0,
                },
                SURFACE,
            );
        }
        self.label(bounds, label, 10.5, if primary { BG } else { color });
        self.buttons.push((bounds, action));
    }

    #[allow(clippy::too_many_arguments)]
    fn source(
        &mut self,
        source: &str,
        byte_offset: usize,
        spans: &[ColorSpan],
        bounds: Bounds,
        font: f32,
        cell: (f32, f32),
        offset: usize,
        column: usize,
    ) {
        let rows = (bounds.h / cell.1.max(1.0)).floor().max(0.0) as usize;
        let columns = (bounds.w / cell.0.max(1.0)).floor().max(0.0) as usize;
        let mut byte = byte_offset;
        for (row, raw) in source.split('\n').enumerate() {
            if row >= offset + rows {
                break;
            }
            if row < offset {
                byte += raw.len() + 1;
                continue;
            }
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let (text, colors) = visible_line(line, byte, spans, column, columns);
            self.texts.push(TextRun {
                x: bounds.x,
                y: bounds.y + (row - offset) as f32 * cell.1,
                text,
                font_size: font,
                line_height: cell.1,
                color: FG,
                colors,
                bold: false,
                italic: false,
                clip: [bounds.x, bounds.y, bounds.x + bounds.w, bounds.y + bounds.h],
            });
            byte += raw.len() + 1;
        }
        if source.is_empty() && rows > 0 {
            self.label(
                Bounds {
                    h: cell.1,
                    ..bounds
                },
                "Empty section",
                font - 1.0,
                MUTED,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_colors_use_document_offsets_after_scrolling_and_crlf() {
        let bounds = Bounds {
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 40.0,
        };
        let mut frame = MergeFrame {
            rects: Vec::new(),
            texts: Vec::new(),
            buttons: Vec::new(),
            editor_bounds: bounds,
            comparison_bounds: None,
        };
        let spans = [
            ColorSpan {
                range: 106..112,
                color: GREEN,
            },
            ColorSpan {
                range: 113..114,
                color: AMBER,
            },
        ];
        frame.source(
            "skip\r\n中文\t1\n",
            100,
            &spans,
            bounds,
            12.0,
            (8.0, 16.0),
            1,
            0,
        );
        assert_eq!(frame.texts[0].text, "中文    1");
        assert_eq!(
            frame.texts[0].colors,
            vec![
                ColorSpan {
                    range: 0..6,
                    color: GREEN
                },
                ColorSpan {
                    range: 10..11,
                    color: AMBER
                },
            ]
        );
        assert_eq!(
            frame.texts.len(),
            2,
            "one text buffer per visible source row"
        );
    }
}
