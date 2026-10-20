use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn modifier_param(mods: KeyModifiers) -> u8 {
    let mut value = 1;
    if mods.contains(KeyModifiers::SHIFT) {
        value += 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        value += 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        value += 4;
    }
    value
}

fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn csi_letter(letter: char, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let param = modifier_param(mods);
    if param > 1 {
        format!("\x1b[1;{param}{letter}").into_bytes()
    } else if app_cursor {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[{letter}").into_bytes()
    }
}

fn csi_tilde(code: u8, mods: KeyModifiers) -> Vec<u8> {
    let param = modifier_param(mods);
    if param > 1 {
        format!("\x1b[{code};{param}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

fn function_key(n: u8, mods: KeyModifiers) -> Option<Vec<u8>> {
    let param = modifier_param(mods);
    match n {
        1..=4 => {
            let letter = (b'P' + n - 1) as char;
            Some(if param > 1 {
                format!("\x1b[1;{param}{letter}").into_bytes()
            } else {
                format!("\x1bO{letter}").into_bytes()
            })
        }
        5 => Some(csi_tilde(15, mods)),
        6..=10 => Some(csi_tilde(11 + n, mods)),
        11 | 12 => Some(csi_tilde(12 + n, mods)),
        _ => None,
    }
}

pub fn encode(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let mods = key.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let with_alt = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    let bytes = match key.code {
        KeyCode::Char(c) if ctrl => with_alt(vec![control_byte(c)?]),
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            with_alt(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter if mods.contains(KeyModifiers::SHIFT) => b"\x1b\r".to_vec(),
        KeyCode::Enter => with_alt(b"\r".to_vec()),
        KeyCode::Backspace if ctrl => with_alt(vec![0x08]),
        KeyCode::Backspace => with_alt(vec![0x7f]),
        KeyCode::Tab => with_alt(b"\t".to_vec()),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => with_alt(vec![0x1b]),
        KeyCode::Up => csi_letter('A', mods, app_cursor),
        KeyCode::Down => csi_letter('B', mods, app_cursor),
        KeyCode::Right => csi_letter('C', mods, app_cursor),
        KeyCode::Left => csi_letter('D', mods, app_cursor),
        KeyCode::Home => csi_letter('H', mods, app_cursor),
        KeyCode::End => csi_letter('F', mods, app_cursor),
        KeyCode::Insert => csi_tilde(2, mods),
        KeyCode::Delete => csi_tilde(3, mods),
        KeyCode::PageUp => csi_tilde(5, mods),
        KeyCode::PageDown => csi_tilde(6, mods),
        KeyCode::F(n) => function_key(n, mods)?,
        _ => return None,
    };
    Some(bytes)
}

pub fn bracketed_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n");
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(normalized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        normalized.replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn enc(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode(&key(code, mods), false).unwrap()
    }

    #[test]
    fn plain_and_unicode_chars() {
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(enc(KeyCode::Char('ż'), KeyModifiers::NONE), "ż".as_bytes());
    }

    #[test]
    fn control_and_alt_chars() {
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(enc(KeyCode::Char('4'), KeyModifiers::CONTROL), [0x1c]);
        assert_eq!(enc(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(
            enc(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            [0x1b, 0x18]
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r");
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(enc(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(enc(KeyCode::Esc, KeyModifiers::NONE), [0x1b]);
    }

    #[test]
    fn cursor_keys_follow_app_cursor_mode() {
        assert_eq!(enc(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(
            encode(&key(KeyCode::Up, KeyModifiers::NONE), true).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(enc(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(
            encode(&key(KeyCode::Right, KeyModifiers::SHIFT), true).unwrap(),
            b"\x1b[1;2C"
        );
    }

    #[test]
    fn tilde_and_function_keys() {
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(enc(KeyCode::PageUp, KeyModifiers::SHIFT), b"\x1b[5;2~");
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(6), KeyModifiers::NONE), b"\x1b[17~");
        assert_eq!(enc(KeyCode::F(10), KeyModifiers::NONE), b"\x1b[21~");
        assert_eq!(enc(KeyCode::F(11), KeyModifiers::NONE), b"\x1b[23~");
        assert_eq!(enc(KeyCode::F(12), KeyModifiers::NONE), b"\x1b[24~");
    }

    #[test]
    fn release_events_are_ignored() {
        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(encode(&release, false).is_none());
    }

    #[test]
    fn paste_wrapping() {
        assert_eq!(bracketed_paste("a\r\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(bracketed_paste("a\nb", false), b"a\rb");
    }
}
