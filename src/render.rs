use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

pub fn convert_color(color: AnsiColor, colors: &Colors) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(index) => match colors[index as usize] {
            Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
            None => Color::Indexed(index),
        },
        AnsiColor::Named(named) => named_color(named, colors),
    }
}

fn named_color(named: NamedColor, colors: &Colors) -> Color {
    let index = match named {
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => {
            return match colors[named] {
                Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
                None => Color::Reset,
            };
        }
        NamedColor::DimBlack => 0,
        NamedColor::DimRed => 1,
        NamedColor::DimGreen => 2,
        NamedColor::DimYellow => 3,
        NamedColor::DimBlue => 4,
        NamedColor::DimMagenta => 5,
        NamedColor::DimCyan => 6,
        NamedColor::DimWhite => 7,
        other => other as usize,
    };
    match colors[index] {
        Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        None => Color::Indexed(index as u8),
    }
}

pub fn cell_style(fg: AnsiColor, bg: AnsiColor, flags: Flags, colors: &Colors) -> Style {
    let mut style = Style::default()
        .fg(convert_color(fg, colors))
        .bg(convert_color(bg, colors));
    let mut modifiers = Modifier::empty();
    if flags.contains(Flags::BOLD) {
        modifiers |= Modifier::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        modifiers |= Modifier::ITALIC;
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        modifiers |= Modifier::UNDERLINED;
    }
    if flags.contains(Flags::DIM) {
        modifiers |= Modifier::DIM;
    }
    if flags.contains(Flags::INVERSE) {
        modifiers |= Modifier::REVERSED;
    }
    if flags.contains(Flags::HIDDEN) {
        modifiers |= Modifier::HIDDEN;
    }
    if flags.contains(Flags::STRIKEOUT) {
        modifiers |= Modifier::CROSSED_OUT;
    }
    style = style.add_modifier(modifiers);
    style
}

pub fn draw_term<T: EventListener>(
    term: &Term<T>,
    area: Rect,
    buf: &mut Buffer,
) -> Option<Position> {
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let rows = term.screen_lines().min(area.height as usize);
    let cols = term.columns().min(area.width as usize);

    for y in 0..area.height {
        for x in 0..area.width {
            buf[(area.x + x, area.y + y)].reset();
        }
    }

    for indexed in content.display_iter {
        let row = indexed.point.line.0 + offset;
        let col = indexed.point.column.0;
        if row < 0 || row as usize >= rows || col >= cols {
            continue;
        }
        let cell = indexed.cell;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let target = &mut buf[(area.x + col as u16, area.y + row as u16)];
        let mut symbol = String::new();
        symbol.push(if cell.c == '\t' { ' ' } else { cell.c });
        if let Some(extra) = cell.zerowidth() {
            symbol.extend(extra);
        }
        target.set_symbol(&symbol);
        let mut style = cell_style(cell.fg, cell.bg, cell.flags, content.colors);
        if content
            .selection
            .is_some_and(|range| range.contains(indexed.point))
        {
            style = if style.add_modifier.contains(Modifier::REVERSED) {
                style.remove_modifier(Modifier::REVERSED)
            } else {
                style.add_modifier(Modifier::REVERSED)
            };
        }
        target.set_style(style);
    }

    let cursor = content.cursor.point;
    let row = cursor.line.0 + offset;
    let visible = content.mode.contains(TermMode::SHOW_CURSOR)
        && offset == 0
        && row >= 0
        && (row as usize) < rows
        && cursor.column.0 < cols;
    visible.then(|| Position::new(area.x + cursor.column.0 as u16, area.y + row as u16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::Size;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    fn render(input: &[u8], size: Size) -> (Buffer, Option<Position>) {
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, input);
        let area = Rect::new(0, 0, size.cols, size.rows);
        let mut buf = Buffer::empty(area);
        let cursor = draw_term(&term, area, &mut buf);
        (buf, cursor)
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn draws_text_colors_and_cursor() {
        let (buf, cursor) = render(
            b"ab\x1b[31;1mC\x1b[0m\r\n\x1b[38;2;1;2;3mz",
            Size { cols: 10, rows: 3 },
        );
        assert_eq!(row(&buf, 0), "abC");
        assert_eq!(row(&buf, 1), "z");
        let c = &buf[(2, 0)];
        assert_eq!(c.fg, Color::Indexed(1));
        assert!(c.modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(0, 1)].fg, Color::Rgb(1, 2, 3));
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert_eq!(cursor, Some(Position::new(1, 1)));
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        let (buf, _) = render("界x".as_bytes(), Size { cols: 6, rows: 1 });
        assert_eq!(buf[(0, 0)].symbol(), "界");
        assert_eq!(buf[(2, 0)].symbol(), "x");
    }

    #[test]
    fn hidden_cursor_is_not_reported() {
        let (_, cursor) = render(b"abc\x1b[?25l", Size { cols: 6, rows: 2 });
        assert_eq!(cursor, None);
    }

    #[test]
    fn alternate_screen_content_is_drawn() {
        let (buf, _) = render(b"main\x1b[?1049h\x1b[Halt", Size { cols: 6, rows: 2 });
        assert_eq!(row(&buf, 0), "alt");
    }

    #[test]
    fn selected_cells_are_drawn_reversed() {
        let size = Size { cols: 6, rows: 1 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"abcdef");
        use alacritty_terminal::index::{Column, Line, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(1)),
            Side::Left,
        );
        selection.update(Point::new(Line(0), Column(2)), Side::Right);
        term.selection = Some(selection);
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        draw_term(&term, area, &mut buf);
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(1, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(3, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn inverse_and_underline_flags() {
        let (buf, _) = render(b"\x1b[7;4mq", Size { cols: 3, rows: 1 });
        let c = &buf[(0, 0)];
        assert!(c.modifier.contains(Modifier::REVERSED));
        assert!(c.modifier.contains(Modifier::UNDERLINED));
    }
}
