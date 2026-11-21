// Rust build: East Asian consoles draw "ambiguous width" characters (box drawing, ●, →, ·, …) two cells wide, while
// the layout counts them as one, so every row after one of them is pushed right ("歪一邊"). The Windows console host
// with a Chinese, Japanese or Korean code page does this; a finished frame is rewritten there with narrow look-alikes.
use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthChar;

// Characters one cell wide everywhere but two on an East Asian console.
pub fn ambiguous(c: char) -> bool {
    !c.is_ascii() && c.width() == Some(1) && c.width_cjk() == Some(2)
}

// A one-cell ASCII stand-in that keeps the shape: lines, corners, dots, arrows, quotes.
pub fn narrow(c: char) -> char {
    match c {
        '╭' | '┌' | '┏' | '╔' | '╮' | '┐' | '┓' | '╗' => '.',
        '╰' | '└' | '┗' | '╚' | '╯' | '┘' | '┛' | '╝' => '\'',
        '\u{2500}'..='\u{257f}' => {
            let horizontal = matches!(c, '─' | '━' | '┄' | '┅' | '┈' | '┉' | '╌' | '╍' | '═' | '╴' | '╶' | '╸' | '╺' | '╼' | '╾');
            let vertical = matches!(c, '│' | '┃' | '┆' | '┇' | '┊' | '┋' | '╎' | '╏' | '║' | '╵' | '╷' | '╹' | '╻' | '╽' | '╿');
            if horizontal {
                '-'
            } else if vertical {
                '|'
            } else {
                '+'
            }
        }
        '\u{2580}'..='\u{259f}' => ' ',
        '●' | '•' | '◆' | '◇' | '○' | '◎' | '◉' | '■' | '□' | '▪' | '▫' | '★' | '☆' | '※' => '*',
        '→' | '⇒' | '▶' | '►' | '▸' | '›' => '>',
        '←' | '⇐' | '◀' | '◄' | '◂' | '‹' => '<',
        '↑' | '▲' | '△' => '^',
        '↓' | '▼' | '▽' => 'v',
        '↔' | '⇆' | '⇄' | '–' | '—' | '―' => '-',
        '·' | '…' | '‥' | '∙' => '.',
        '‘' | '’' | '′' => '\'',
        '“' | '”' | '″' => '"',
        '×' => 'x',
        '÷' => '/',
        '±' => '+',
        '°' => 'o',
        '§' => 'S',
        _ => '?',
    }
}

// Full blocks keep their colour as a background; every other ambiguous cell becomes its narrow stand-in.
pub fn narrow_buffer(buffer: &mut Buffer) {
    for cell in buffer.content.iter_mut() {
        let mut chars = cell.symbol().chars();
        let (Some(c), None) = (chars.next(), chars.next()) else { continue };
        if !ambiguous(c) {
            continue;
        }
        if c == '█' {
            let fg = cell.fg;
            cell.set_bg(fg);
        }
        cell.set_char(narrow(c));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    #[test]
    fn only_east_asian_ambiguous_characters_change() {
        for c in ['─', '│', '╭', '●', '→', '·', '…', '▀', '█'] {
            assert!(ambiguous(c), "{c}");
        }
        for c in ['a', '-', '中', '，', '⎿', '✓', '⠋'] {
            assert!(!ambiguous(c), "{c}");
        }
        let line: String = "╭──╮│●→·…╰╯".chars().map(narrow).collect();
        assert_eq!(line, ".--.|*>..''");
    }

    #[test]
    fn rewrites_a_frame_in_place() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        buffer.set_string(0, 0, "● 中█", ratatui::style::Style::default().fg(Color::Red));
        narrow_buffer(&mut buffer);
        let symbols: Vec<&str> = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert_eq!((symbols[0], symbols[2], symbols[4]), ("*", "中", " "));
        assert_eq!(buffer.content[4].bg, Color::Red);
    }
}
