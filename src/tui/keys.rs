use std::time::Duration;

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

pub const PASTE_LINE_GAP: Duration = Duration::from_millis(20);
pub const NEXT_WAITING_KEY: char = 'n';

enum BurstKey {
    Text(char),
    Release,
}

fn burst_key(event: &Event) -> Option<BurstKey> {
    let Event::Key(key) = event else { return None };
    if key.kind == KeyEventKind::Release {
        return Some(BurstKey::Release);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Enter | KeyCode::Char('\r' | '\n') if !ctrl && !alt => Some(BurstKey::Text('\n')),
        KeyCode::Tab if !ctrl && !alt => Some(BurstKey::Text('\t')),
        KeyCode::Char(c) if !ctrl || alt => Some(BurstKey::Text(c)),
        _ => None,
    }
}

pub fn coalesce_paste(first: Event, mut pending: impl FnMut(Duration) -> Option<Event>) -> Vec<Event> {
    if burst_key(&first).is_none() {
        return vec![first];
    }
    let mut after_line_break = matches!(burst_key(&first), Some(BurstKey::Text('\n')));
    let mut burst = vec![first];
    let mut tail = None;
    while let Some(event) = pending(if after_line_break { PASTE_LINE_GAP } else { Duration::ZERO }) {
        match burst_key(&event) {
            Some(key) => {
                if let BurstKey::Text(c) = key {
                    after_line_break = c == '\n';
                }
                burst.push(event);
            }
            None => {
                tail = Some(event);
                break;
            }
        }
    }
    let text: String = burst
        .iter()
        .filter_map(|event| match burst_key(event) {
            Some(BurstKey::Text(c)) => Some(c),
            _ => None,
        })
        .collect();
    let pasted = text.trim_end_matches('\n').contains('\n');
    let mut events = if pasted { vec![Event::Paste(text)] } else { burst };
    events.extend(tail);
    events
}

pub fn is_detach_key(key: &KeyEvent) -> bool {
    if key.code == KeyCode::Char('\u{1d}') {
        return true;
    }
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(
            key.code,
            KeyCode::Char(']') | KeyCode::Char('5') | KeyCode::Char('ç') | KeyCode::Char('Ç')
        )
}

pub fn is_next_waiting_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char(NEXT_WAITING_KEY)
        && key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
}

pub fn is_press(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
}

fn modifier_param(mods: KeyModifiers) -> u8 {
    1 + u8::from(mods.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(mods.contains(KeyModifiers::ALT))
        + 4 * u8::from(mods.contains(KeyModifiers::CONTROL))
}

fn ctrl_byte(c: char) -> Option<u8> {
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

fn cursor_key(letter: char, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let param = modifier_param(mods);
    if param > 1 {
        format!("\x1b[1;{param}{letter}").into_bytes()
    } else if app_cursor {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[{letter}").into_bytes()
    }
}

fn tilde_key(number: u8, mods: KeyModifiers) -> Vec<u8> {
    let param = modifier_param(mods);
    if param > 1 { format!("\x1b[{number};{param}~").into_bytes() } else { format!("\x1b[{number}~").into_bytes() }
}

fn with_alt(mods: KeyModifiers, mut bytes: Vec<u8>) -> Vec<u8> {
    if mods.contains(KeyModifiers::ALT) {
        bytes.insert(0, 0x1b);
    }
    bytes
}

pub fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    if !is_press(key) {
        return None;
    }
    let mods = key.modifiers;
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl && alt && !c.is_ascii_alphabetic() {
                c.to_string().into_bytes()
            } else if ctrl {
                let byte = ctrl_byte(c)?;
                with_alt(mods, vec![byte])
            } else {
                with_alt(mods, c.to_string().into_bytes())
            }
        }
        KeyCode::Enter => {
            if mods.contains(KeyModifiers::SHIFT) || alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        KeyCode::Backspace => {
            if ctrl {
                vec![0x08]
            } else {
                with_alt(mods, vec![0x7f])
            }
        }
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => cursor_key('A', mods, app_cursor),
        KeyCode::Down => cursor_key('B', mods, app_cursor),
        KeyCode::Right => cursor_key('C', mods, app_cursor),
        KeyCode::Left => cursor_key('D', mods, app_cursor),
        KeyCode::Home => cursor_key('H', mods, app_cursor),
        KeyCode::End => cursor_key('F', mods, app_cursor),
        KeyCode::Insert => tilde_key(2, mods),
        KeyCode::Delete => tilde_key(3, mods),
        KeyCode::PageUp => tilde_key(5, mods),
        KeyCode::PageDown => tilde_key(6, mods),
        KeyCode::F(n @ 1..=4) => {
            let letter = char::from(b'P' + (n - 1));
            let param = modifier_param(mods);
            if param > 1 {
                format!("\x1b[1;{param}{letter}").into_bytes()
            } else {
                format!("\x1bO{letter}").into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            tilde_key(code, mods)
        }
        KeyCode::Null => vec![0],
        _ => return None,
    };
    Some(bytes)
}

pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
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

pub fn encode_mouse(
    event: &MouseEvent,
    col: u16,
    row: u16,
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use vt100::MouseProtocolMode as Mode;
    if mode == Mode::None {
        return None;
    }
    let button_code = |button: MouseButton| match button {
        MouseButton::Left => 0u16,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut code, release) = match event.kind {
        MouseEventKind::Down(button) => (button_code(button), false),
        MouseEventKind::Up(button) => {
            if mode == Mode::Press {
                return None;
            }
            (button_code(button), true)
        }
        MouseEventKind::Drag(button) => {
            if !matches!(mode, Mode::ButtonMotion | Mode::AnyMotion) {
                return None;
            }
            (button_code(button) + 32, false)
        }
        MouseEventKind::Moved => {
            if mode != Mode::AnyMotion {
                return None;
            }
            (35, false)
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        _ => return None,
    };
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let x = u32::from(col) + 1;
    let y = u32::from(row) + 1;
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            let suffix = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{x};{y}{suffix}").into_bytes())
        }
        _ => {
            let code = if release { 3 } else { code };
            if x > 223 || y > 223 {
                return None;
            }
            let to_byte = |v: u32| u8::try_from(v + 32).unwrap_or(255);
            Some(vec![0x1b, b'[', b'M', to_byte(u32::from(code)), to_byte(x), to_byte(y)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn typed(text: &str) -> Vec<Event> {
        let mut events = Vec::new();
        for c in text.chars() {
            let code = if c == '\r' { KeyCode::Enter } else { KeyCode::Char(c) };
            events.push(Event::Key(key(code, KeyModifiers::NONE)));
            let mut release = key(code, KeyModifiers::NONE);
            release.kind = KeyEventKind::Release;
            events.push(Event::Key(release));
        }
        events
    }

    fn coalesce(events: Vec<Event>) -> Vec<Event> {
        let mut queue = events.into_iter();
        let first = queue.next().unwrap();
        coalesce_paste(first, |_| queue.next())
    }

    #[test]
    fn queued_keys_with_a_line_break_become_one_paste() {
        let mut events = typed("fix\rthe bug\r");
        events.push(Event::Key(key(KeyCode::Up, KeyModifiers::NONE)));
        let out = coalesce(events);
        assert_eq!(out, vec![Event::Paste("fix\nthe bug\n".into()), Event::Key(key(KeyCode::Up, KeyModifiers::NONE))]);
    }

    #[test]
    fn keys_without_line_breaks_pass_through_unchanged() {
        let events = typed("dd");
        assert_eq!(coalesce(events.clone()), events);
        let enter = typed("\r");
        assert_eq!(coalesce(enter.clone()), enter);
        let ctrl = vec![Event::Key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))];
        assert_eq!(coalesce_paste(ctrl[0].clone(), |_| panic!("must not read ahead")), ctrl);
    }

    #[test]
    fn typing_that_queued_up_before_enter_still_submits() {
        let events = typed("fix the bug\r");
        assert_eq!(coalesce(events.clone()), events);
    }

    #[test]
    fn a_lone_key_is_not_buffered() {
        let first = Event::Key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(coalesce_paste(first.clone(), |_| None), vec![first]);
    }

    #[test]
    fn a_line_break_waits_briefly_for_the_rest_of_a_split_paste() {
        let mut queue = typed("next line").into_iter();
        let mut waits = Vec::new();
        let first = Event::Key(key(KeyCode::Enter, KeyModifiers::NONE));
        let out = coalesce_paste(first, |wait| {
            waits.push(wait);
            queue.next()
        });
        assert_eq!(out, vec![Event::Paste("\nnext line".into())]);
        assert_eq!(waits[0], PASTE_LINE_GAP);
        assert!(waits[1..].iter().all(|wait| wait.is_zero()));
    }

    fn enc(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode_key(&key(code, mods), false).unwrap()
    }

    #[test]
    fn printable_and_control_characters() {
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(enc(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(enc(KeyCode::Char('ç'), KeyModifiers::NONE), "ç".as_bytes());
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(enc(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::CONTROL | KeyModifiers::ALT), b"\x1b\x01");
    }

    #[test]
    fn altgr_characters_are_sent_verbatim() {
        assert_eq!(enc(KeyCode::Char('/'), KeyModifiers::CONTROL | KeyModifiers::ALT), b"/");
        assert_eq!(enc(KeyCode::Char('?'), KeyModifiers::CONTROL | KeyModifiers::ALT), b"?");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r");
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::NONE), b"\x7f");
        assert_eq!(enc(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(enc(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(enc(KeyCode::Esc, KeyModifiers::NONE), b"\x1b");
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(enc(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
    }

    #[test]
    fn cursor_keys_respect_application_mode_and_modifiers() {
        assert_eq!(enc(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(encode_key(&key(KeyCode::Up, KeyModifiers::NONE), true).unwrap(), b"\x1bOA");
        assert_eq!(enc(KeyCode::Right, KeyModifiers::CONTROL), b"\x1b[1;5C");
        assert_eq!(enc(KeyCode::Left, KeyModifiers::SHIFT | KeyModifiers::ALT), b"\x1b[1;4D");
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(12), KeyModifiers::NONE), b"\x1b[24~");
        assert_eq!(enc(KeyCode::F(2), KeyModifiers::SHIFT), b"\x1b[1;2Q");
    }

    #[test]
    fn release_events_are_ignored() {
        let mut k = key(KeyCode::Char('a'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert_eq!(encode_key(&k, false), None);
    }

    #[test]
    fn detach_key_matches_ctrl_bracket_variants() {
        assert!(is_detach_key(&key(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(is_detach_key(&key(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(is_detach_key(&key(KeyCode::Char('ç'), KeyModifiers::CONTROL)));
        assert!(is_detach_key(&key(KeyCode::Char('Ç'), KeyModifiers::CONTROL)));
        assert!(!is_detach_key(&key(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert!(!is_detach_key(&key(KeyCode::Char('ç'), KeyModifiers::NONE)));
        assert!(!is_detach_key(&key(KeyCode::Char(']'), KeyModifiers::CONTROL | KeyModifiers::ALT)));
    }

    #[test]
    fn next_waiting_key_is_alt_n_only() {
        assert!(is_next_waiting_key(&key(KeyCode::Char('n'), KeyModifiers::ALT)));
        assert!(!is_next_waiting_key(&key(KeyCode::Char('n'), KeyModifiers::NONE)));
        assert!(!is_next_waiting_key(&key(KeyCode::Char('n'), KeyModifiers::CONTROL)));
        assert!(!is_next_waiting_key(&key(KeyCode::Char('n'), KeyModifiers::CONTROL | KeyModifiers::ALT)));
        assert!(!is_next_waiting_key(&key(KeyCode::Char('N'), KeyModifiers::ALT | KeyModifiers::SHIFT)));
    }

    #[test]
    fn paste_encoding() {
        assert_eq!(encode_paste("a\r\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
    }

    #[test]
    fn mouse_encoding_sgr_and_default() {
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let sgr = vt100::MouseProtocolEncoding::Sgr;
        let mode = vt100::MouseProtocolMode::PressRelease;
        assert_eq!(encode_mouse(&down, 4, 2, mode, sgr).unwrap(), b"\x1b[<0;5;3M");
        let up = MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), ..down };
        assert_eq!(encode_mouse(&up, 4, 2, mode, sgr).unwrap(), b"\x1b[<0;5;3m");
        let default = vt100::MouseProtocolEncoding::Default;
        assert_eq!(encode_mouse(&down, 0, 0, mode, default).unwrap(), vec![0x1b, b'[', b'M', 32, 33, 33]);
        assert_eq!(encode_mouse(&down, 0, 0, vt100::MouseProtocolMode::None, sgr), None);
        let scroll = MouseEvent { kind: MouseEventKind::ScrollDown, ..down };
        assert_eq!(encode_mouse(&scroll, 0, 0, mode, sgr).unwrap(), b"\x1b[<65;1;1M");
        let drag = MouseEvent { kind: MouseEventKind::Drag(MouseButton::Left), ..down };
        assert_eq!(encode_mouse(&drag, 0, 0, mode, sgr), None);
    }
}
