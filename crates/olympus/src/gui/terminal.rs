use super::{Root, TermGrid, TermSelection, FONT};
use crate::session::Pane;
use crate::theme;
use gpui::prelude::*;
use gpui::{canvas, div, fill, font, point, px, size, AnyElement, Bounds, Context, CursorStyle, FontStyle, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Point, ScrollWheelEvent, SharedString, TextRun, Window};
use std::sync::{Arc, Mutex};

const CELL_FONT: f32 = 13.0;
const CELL_LINE: f32 = 18.0;

fn indexed(index: u8) -> Hsla {
    match index {
        0..=15 => theme::ansi(index),
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| if v == 0 { 0.0 } else { (55.0 + 40.0 * f32::from(v)) / 255.0 };
            gpui::Rgba { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6), a: 1.0 }.into()
        }
        _ => {
            let v = (8.0 + 10.0 * f32::from(index - 232)) / 255.0;
            gpui::Rgba { r: v, g: v, b: v, a: 1.0 }.into()
        }
    }
}

fn color(value: vt100::Color, fallback: Hsla) -> Hsla {
    match value {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(index) => indexed(index),
        vt100::Color::Rgb(r, g, b) => gpui::Rgba { r: f32::from(r) / 255.0, g: f32::from(g) / 255.0, b: f32::from(b) / 255.0, a: 1.0 }.into(),
    }
}

/// One stretch of same-looking cells to draw at a column.
#[derive(Debug, PartialEq)]
pub struct CellRun {
    pub column: u16,
    pub width: u16,
    pub text: String,
    pub fg: Hsla,
    pub bg: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

// Non-ASCII glyphs can fall back to fonts of another width, so each gets its own run to stay on the grid.
pub fn row_runs(screen: &vt100::Screen, row: u16, cols: u16, cursor: Option<u16>) -> Vec<CellRun> {
    let mut runs: Vec<CellRun> = Vec::new();
    for column in 0..cols {
        let Some(cell) = screen.cell(row, column) else { continue };
        if cell.is_wide_continuation() {
            continue;
        }
        let mut fg = color(cell.fgcolor(), theme::text());
        let mut bg = match cell.bgcolor() {
            vt100::Color::Default => None,
            other => Some(color(other, theme::bg())),
        };
        if cell.inverse() {
            let old_fg = fg;
            fg = bg.unwrap_or_else(theme::bg);
            bg = Some(old_fg);
        }
        let at_cursor = cursor == Some(column);
        if at_cursor {
            fg = theme::bg();
            bg = Some(theme::accent());
        }
        let contents = cell.contents();
        let text = if contents.is_empty() { " " } else { contents };
        let width = if cell.is_wide() { 2 } else { 1 };
        let (bold, italic, underline) = (cell.bold(), cell.italic(), cell.underline());
        if text.is_ascii() && !at_cursor {
            if let Some(last) = runs.last_mut() {
                if last.column + last.width == column && last.fg == fg && last.bg == bg && last.bold == bold && last.italic == italic && last.underline == underline && last.text.is_ascii() && cursor != Some(last.column) {
                    last.text.push_str(text);
                    last.width += width;
                    continue;
                }
            }
        }
        runs.push(CellRun { column, width, text: text.to_string(), fg, bg, bold, italic, underline });
    }
    for run in &mut runs {
        if run.bg.is_none() {
            let trimmed = run.text.trim_end_matches(' ').len();
            run.text.truncate(trimmed);
        }
    }
    runs.retain(|run| !run.text.is_empty() || run.bg.is_some());
    runs
}

fn paint_grid(parser: &Mutex<vt100::Parser>, grid: TermGrid, selection: Option<TermSelection>, focused: bool, window: &mut Window, cx: &mut gpui::App) {
    let Ok(parser) = parser.lock() else { return };
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor_position();
    let show_cursor = focused && !screen.hide_cursor() && screen.scrollback() == 0;
    let line_height = px(CELL_LINE);
    for row in 0..rows {
        let y = grid.origin.y + grid.cell.height * f32::from(row);
        let cursor = (show_cursor && row == cursor_row).then_some(cursor_col);
        let runs = row_runs(screen, row, cols, cursor);
        for run in &runs {
            if let Some(bg) = run.bg {
                let x = grid.origin.x + grid.cell.width * f32::from(run.column);
                window.paint_quad(fill(Bounds::new(point(x, y), size(grid.cell.width * f32::from(run.width), grid.cell.height)), bg));
            }
        }
        if let Some(selection) = selection {
            let (a, b) = (selection.start.min(selection.end), selection.start.max(selection.end));
            if row >= a.0 && row <= b.0 {
                let from = if row == a.0 { a.1 } else { 0 };
                let to = if row == b.0 { b.1 } else { cols };
                let x = grid.origin.x + grid.cell.width * f32::from(from);
                window.paint_quad(fill(Bounds::new(point(x, y), size(grid.cell.width * f32::from(to.saturating_sub(from)), grid.cell.height)), theme::tint(theme::accent(), 0.3)));
            }
        }
        for run in runs {
            if run.text.is_empty() {
                continue;
            }
            let mut face = font(FONT);
            if run.bold {
                face.weight = FontWeight::BOLD;
            }
            if run.italic {
                face.style = FontStyle::Italic;
            }
            let shaped_run = TextRun {
                len: run.text.len(),
                font: face,
                color: run.fg,
                background_color: None,
                underline: run.underline.then(|| gpui::UnderlineStyle { thickness: px(1.), color: None, wavy: false }),
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(SharedString::from(run.text), px(CELL_FONT), &[shaped_run], None);
            let x = grid.origin.x + grid.cell.width * f32::from(run.column);
            let _ = shaped.paint(point(x, y), line_height, window, cx);
        }
    }
}

impl Root {
    fn cell_at(&self, window: u64, position: Point<Pixels>) -> Option<(u16, u16)> {
        let grid = self.term_grids.get(&window)?;
        let local = position - grid.origin;
        let column = (f32::from(local.x) / f32::from(grid.cell.width)).max(0.0) as u16;
        let row = (f32::from(local.y) / f32::from(grid.cell.height)).max(0.0) as u16;
        Some((row, column))
    }

    pub(super) fn term_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(selection) = self.term_selection.filter(|s| s.dragging) else { return };
        if let Some(cell) = self.cell_at(selection.window, position) {
            if let Some(current) = &mut self.term_selection {
                current.end = cell;
            }
            cx.notify();
        }
    }

    fn term_wheel(&mut self, window: u64, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let lines = (f32::from(event.delta.pixel_delta(px(CELL_LINE)).y) / CELL_LINE).round() as i32;
        let cell = self.cell_at(window, event.position).unwrap_or((0, 0));
        let Some(found) = self.app.window_mut(window) else { return };
        let Pane::Term(term) = &mut found.pane else { return };
        if lines == 0 {
            return;
        }
        let (protocol, alternate) = match term.pty.parser.lock() {
            Ok(parser) => (parser.screen().mouse_protocol_mode(), parser.screen().alternate_screen()),
            Err(_) => return,
        };
        let count = lines.unsigned_abs() as usize;
        if protocol != vt100::MouseProtocolMode::None {
            let button = if lines > 0 { 64 } else { 65 };
            let report = format!("\x1b[<{button};{};{}M", cell.1 + 1, cell.0 + 1);
            term.pty.send(report.repeat(count).as_bytes());
        } else if alternate {
            term.pty.send((if lines > 0 { "\x1b[A" } else { "\x1b[B" }).repeat(count).as_bytes());
        } else if let Ok(mut parser) = term.pty.parser.lock() {
            let current = parser.screen().scrollback();
            parser.screen_mut().set_scrollback(current.saturating_add_signed(lines as isize));
        }
        cx.notify();
    }

    pub(super) fn term_view(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(found) = self.app.windows.iter().find(|w| w.id == id) else { return div().into_any_element() };
        let Pane::Term(term) = &found.pane else { return div().into_any_element() };
        let parser: Arc<Mutex<vt100::Parser>> = term.pty.parser.clone();
        let exited = term.pty.exited.load(std::sync::atomic::Ordering::Relaxed);
        let focused = self.app.active_id() == id;
        let selection = self.term_selection.filter(|s| s.window == id);
        let root = cx.entity().downgrade();
        let grid = canvas(
            move |bounds, window, cx| {
                let face = window.text_system().resolve_font(&font(FONT));
                let width = window.text_system().advance(face, px(CELL_FONT), 'm').map_or(px(8.), |a| a.width);
                let cell = size(width, px(CELL_LINE));
                let rows = (f32::from(bounds.size.height) / CELL_LINE).floor().max(2.0) as u16;
                let cols = (f32::from(bounds.size.width) / f32::from(width)).floor().max(2.0) as u16;
                let grid = TermGrid { origin: bounds.origin, cell };
                if let Some(root) = root.upgrade() {
                    root.update(cx, |root, _| {
                        root.term_grids.insert(id, grid);
                        if let Some(Pane::Term(term)) = root.app.window_mut(id).map(|w| &mut w.pane) {
                            term.pty.resize(rows, cols);
                        }
                    });
                }
                grid
            },
            move |_, grid, window, cx| paint_grid(&parser, grid, selection, focused, window, cx),
        )
        .size_full();
        div()
            .id(SharedString::from(format!("term-{id}")))
            .relative()
            .size_full()
            .p(px(8.))
            .bg(theme::bg())
            .cursor(CursorStyle::IBeam)
            .on_scroll_wheel(cx.listener(move |root, event: &ScrollWheelEvent, _, cx| root.term_wheel(id, event, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |root, event: &MouseDownEvent, _, cx| {
                    root.selection = None;
                    root.term_selection = root.cell_at(id, event.position).map(|cell| TermSelection { window: id, start: cell, end: cell, dragging: true });
                    cx.notify();
                }),
            )
            .child(grid)
            .when(exited, |el| {
                el.child(div().absolute().bottom(px(10.)).left_0().right_0().flex().justify_center().child(div().px(px(12.)).py(px(4.)).rounded(px(8.)).bg(theme::overlay()).border_1().border_color(theme::line()).child("The program ended. Alt+W closes this window.")))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(input: &str) -> vt100::Parser {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(input.as_bytes());
        parser
    }

    #[test]
    fn plain_text_becomes_one_run_and_trailing_blanks_are_dropped() {
        let parser = screen("hello");
        let runs = row_runs(parser.screen(), 0, 20, None);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].column, runs[0].text.as_str()), (0, "hello"));
    }

    #[test]
    fn color_changes_split_runs_and_inverse_swaps() {
        let parser = screen("a\x1b[31mb\x1b[0m\x1b[7mc\x1b[0m");
        let runs = row_runs(parser.screen(), 0, 20, None);
        assert_eq!(runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(runs[1].fg, theme::ansi(1));
        assert_eq!(runs[2].bg, Some(theme::text()));
    }

    #[test]
    fn wide_and_box_characters_each_keep_their_grid_column() {
        let parser = screen("日本─x");
        let runs = row_runs(parser.screen(), 0, 20, None);
        assert_eq!(runs.iter().map(|r| (r.column, r.text.as_str())).collect::<Vec<_>>(), [(0, "日"), (2, "本"), (4, "─"), (5, "x")]);
    }

    #[test]
    fn the_cursor_cell_is_inverted_even_on_a_blank() {
        let parser = screen("ab");
        let runs = row_runs(parser.screen(), 0, 20, Some(5));
        let cursor = runs.iter().find(|r| r.column == 5).expect("cursor run");
        assert_eq!(cursor.bg, Some(theme::accent()));
    }

    #[test]
    fn color_cube_and_grays_map_to_real_colors() {
        let red: gpui::Rgba = indexed(196).into();
        assert!(red.r > 0.9 && red.g < 0.05);
        let gray: gpui::Rgba = indexed(244).into();
        assert!((gray.r - gray.b).abs() < 0.01);
    }
}
