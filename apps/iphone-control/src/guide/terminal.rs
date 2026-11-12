//! Small, local menus for the guide. Text and secret input stay with the CLI.
//!
//! termion decodes complete key sequences and restores termios after each read.
//! We never hide the cursor or enter an alternate screen.
use std::fmt;
use std::io::{self, IsTerminal, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use termion::raw::IntoRawMode;

use console::{Key, Term};

// ESC alone is ambiguous on a byte stream. Allow up to 250ms for a whole
// sequence (including fragmented UTF-8), never an unbounded wait or allocation.
const SEQUENCE_WAIT: Duration = Duration::from_millis(250);
const INPUT_LIMIT: usize = 256;

fn read_byte(input: &mut std::fs::File, deadline: Instant) -> io::Result<Option<u8>> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        // select works for terminal descriptors on macOS as well as Linux.
        let fd = input.as_raw_fd();
        if fd < 0 || fd as usize >= libc::FD_SETSIZE {
            return Err(io::Error::other("terminal descriptor exceeds select capacity"));
        }
        let mut timeout = libc::timeval {
            tv_sec: remaining.as_secs() as libc::time_t,
            tv_usec: remaining.subsec_micros().max(1) as libc::suseconds_t,
        };
        // SAFETY: fd is live and checked against FD_SETSIZE; the initialized
        // set and timeout remain valid for the duration of select.
        let ready = unsafe {
            let mut set = std::mem::zeroed::<libc::fd_set>();
            libc::FD_ZERO(&mut set);
            libc::FD_SET(fd, &mut set);
            libc::select(fd + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &mut timeout)
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(None);
        }
        let mut byte = [0];
        match input.read(&mut byte) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")),
            Ok(_) => return Ok(Some(byte[0])),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn decode(bytes: &[u8]) -> Key {
    use termion::event::{Event, Key as Parsed};
    if bytes == b"\x1b" {
        return Key::Escape;
    }
    // termion's legacy mouse/numeric branches assume valid complete input.
    // Pass only bounded, complete keyboard sequences with valid u8 parameters.
    if bytes.starts_with(b"\x1b[") {
        let Some((&last, params)) = bytes[2..].split_last() else { return Key::Unknown };
        if !matches!(last, b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'Z' | b'~')
            || (!params.is_empty()
                && (!params.iter().all(|b| b.is_ascii_digit() || *b == b';')
                    || !params
                        .split(|b| *b == b';')
                        .all(|p| std::str::from_utf8(p).ok().and_then(|s| s.parse::<u8>().ok()).is_some())))
        {
            return Key::Unknown;
        }
    }
    // Application cursor mode uses SS3 rather than CSI.
    let normalized;
    let bytes = if bytes.len() == 3 && bytes.starts_with(b"\x1bO") && b"ABCDHF".contains(&bytes[2]) {
        normalized = [0x1b, b'[', bytes[2]];
        &normalized[..]
    } else {
        bytes
    };
    match termion::event::parse_event(bytes[0], &mut bytes[1..].iter().copied().map(Ok)) {
        Ok(Event::Key(key)) => match key {
            Parsed::Up => Key::ArrowUp,
            Parsed::Down => Key::ArrowDown,
            Parsed::Left => Key::ArrowLeft,
            Parsed::Right => Key::ArrowRight,
            Parsed::Home => Key::Home,
            Parsed::End => Key::End,
            Parsed::PageUp => Key::PageUp,
            Parsed::PageDown => Key::PageDown,
            Parsed::Backspace => Key::Backspace,
            Parsed::Char('\n') => Key::Enter,
            Parsed::Ctrl('c') => Key::CtrlC,
            Parsed::Char(c) => Key::Char(c),
            _ => Key::Unknown,
        },
        _ => Key::Unknown,
    }
}

/// Returns `Ok(None)` when no byte arrives within the first-read window: the
/// terminal is simply idle, so the caller can skip repainting an unchanged
/// frame. A byte that arrives but forms an unknown/partial sequence still
/// returns `Some(Key::Unknown)`, as does every later timeout.
fn read_key() -> io::Result<Option<Key>> {
    // Duplicate stdin, with no read-ahead, just like crate::read_line. A raw
    // guard per read preserves normal output newline handling between frames.
    let mut input = std::fs::File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let _raw = std::io::stdout().into_raw_mode()?;
    // Periodic wakeups let the caller notice a resize, even without keys.
    let Some(first) = read_byte(&mut input, Instant::now() + Duration::from_millis(100))? else {
        return Ok(None);
    };
    let deadline = Instant::now() + SEQUENCE_WAIT;
    let mut bytes = vec![first];
    if first == 0x1b {
        let Some(second) = read_byte(&mut input, deadline)? else { return Ok(Some(Key::Escape)) };
        bytes.push(second);
        if matches!(second, b'[' | b'O') {
            let mut overflow = false;
            loop {
                let Some(next) = read_byte(&mut input, deadline)? else {
                    return Ok(Some(Key::Unknown));
                };
                if bytes.len() < 32 {
                    bytes.push(next);
                } else {
                    overflow = true;
                }
                if (0x40..=0x7e).contains(&next) {
                    if overflow {
                        return Ok(Some(Key::Unknown));
                    }
                    break;
                }
            }
        }
    }
    // Complete UTF-8 (also Alt+UTF-8) before handing it to the parser.
    while std::str::from_utf8(&bytes).is_err_and(|error| error.error_len().is_none()) && bytes.len() < 6 {
        let Some(next) = read_byte(&mut input, deadline)? else { return Ok(Some(Key::Unknown)) };
        bytes.push(next);
    }
    Ok(Some(decode(&bytes)))
}

#[derive(Debug)]
pub(super) struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("setup cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn supports_menu(stdin: bool, stdout: bool, term: Option<&str>, size: (u16, u16)) -> bool {
    stdin && stdout && term.is_some_and(|t| !t.is_empty() && t != "dumb") && size.0 >= 6 && size.1 >= 40
}

fn interactive(term: &Term) -> bool {
    supports_menu(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::env::var("TERM").ok().as_deref(),
        term.size(),
    )
}

pub(super) fn choose(prompt: &str, options: &[&str], default: usize) -> anyhow::Result<usize> {
    anyhow::ensure!(!options.is_empty() && default < options.len(), "invalid menu default");
    let term = Term::stdout();
    if interactive(&term) {
        menu(&term, prompt, options, default, false)
    } else {
        choose_line(prompt, options, default)
    }
}

pub(super) fn ask_yes(prompt: &str, default: bool) -> anyhow::Result<bool> {
    let term = Term::stdout();
    if interactive(&term) {
        let options = [ictl_l10n::tr!("是", "Yes"), ictl_l10n::tr!("否", "No")];
        Ok(menu(&term, prompt, &options, usize::from(!default), true)? == 0)
    } else {
        ask_yes_line(prompt, default)
    }
}

fn ask_yes_line(prompt: &str, default: bool) -> anyhow::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        let a = crate::read_line(&format!("{prompt} {hint} "))?.to_ascii_lowercase();
        match a.as_str() {
            "" => return Ok(default),
            "y" | "yes" | "是" | "好" => return Ok(true),
            "n" | "no" | "否" | "不" => return Ok(false),
            _ => println!("{}", ictl_l10n::tr!("  输入 y 或 n", "  Enter y or n")),
        }
    }
}

fn choose_line(prompt: &str, options: &[&str], default: usize) -> anyhow::Result<usize> {
    println!("{prompt}");
    for (i, option) in options.iter().enumerate() {
        println!("  {}) {option}{}", i + 1, if i == default { ictl_l10n::tr!("（默认）", " (default)") } else { "" });
    }
    loop {
        let a = crate::read_line(ictl_l10n::tr!("选择: ", "Choice: "))?;
        if a.is_empty() {
            return Ok(default);
        }
        match a.parse::<usize>() {
            Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
            _ => println!("{}", ictl_l10n::trf!("  输入 1 到 {}", "  Enter 1 to {}", options.len())),
        }
    }
}

struct Selection {
    selected: usize,
    count: usize,
    input: String,
    overflow: bool,
    confirmation: bool,
}

impl Selection {
    fn new(default: usize, count: usize, confirmation: bool) -> Self {
        Self { selected: default, count, input: String::new(), overflow: false, confirmation }
    }

    fn answer(&self) -> Option<usize> {
        if self.overflow {
            return None;
        }
        let input = self.input.trim();
        if self.confirmation {
            match input.to_ascii_lowercase().as_str() {
                "y" | "yes" | "是" | "好" => return Some(0),
                "n" | "no" | "否" | "不" => return Some(1),
                _ => {}
            }
        }
        input.parse::<usize>().ok().filter(|n| (1..=self.count).contains(n)).map(|n| n - 1)
    }

    fn key(&mut self, key: Key, page: usize) -> Result<Option<usize>, Cancelled> {
        let next = match key {
            Key::Escape | Key::CtrlC => return Err(Cancelled),
            Key::Enter => {
                return Ok(if self.input.is_empty() && !self.overflow { Some(self.selected) } else { self.answer() });
            }
            Key::ArrowUp => Some((self.selected + self.count - 1) % self.count),
            Key::ArrowDown => Some((self.selected + 1) % self.count),
            Key::ArrowLeft if self.confirmation => Some(0),
            Key::ArrowRight if self.confirmation => Some(1),
            Key::Home => Some(0),
            Key::End => Some(self.count - 1),
            Key::PageUp => Some(self.selected.saturating_sub(page)),
            Key::PageDown => Some(self.selected.saturating_add(page).min(self.count - 1)),
            Key::Char(c) if !c.is_control() => {
                // Never discard an unrecognized character: "1x" must not
                // confirm option 1, and a mistyped No must not accept Yes.
                if self.input.len() + c.len_utf8() <= INPUT_LIMIT && !self.overflow {
                    self.input.push(c);
                } else {
                    self.overflow = true;
                }
                if let Some(index) = self.answer() {
                    self.selected = index;
                }
                None
            }
            Key::Backspace => {
                self.input.pop();
                if let Some(index) = self.answer() {
                    self.selected = index;
                }
                None
            }
            _ => None,
        };
        if let Some(index) = next {
            self.selected = index;
            self.input.clear();
            self.overflow = false;
        }
        Ok(None)
    }
}

fn clean(text: &str) -> String {
    // Labels may eventually contain external device names. Never let them move
    // the cursor, create extra lines or inject terminal escape sequences.
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn fit(text: &str, columns: usize) -> String {
    // console::truncate_str subtracts the tail width ("..." = 3 columns) from
    // the available width; a width below that underflows usize in debug builds
    // and wraps in release. A window this narrow cannot show the tail anyway, so
    // clamp to the smallest width truncate_str handles instead of panicking.
    let columns = columns.max(4);
    console::truncate_str(&clean(text), columns, "...").into_owned()
}

fn capacity(rows: u16) -> usize {
    usize::from(rows.saturating_sub(4)).clamp(1, 12)
}

fn frame(prompt: &str, options: &[&str], state: &Selection, size: (u16, u16)) -> Vec<String> {
    let page = capacity(size.0);
    let start = state.selected / page * page;
    let end = (start + page).min(options.len());
    // Leave the final column unused: terminals disagree about wrapping exactly
    // at the margin, especially when the last character occupies two columns.
    let columns = usize::from(size.1.saturating_sub(1));
    let help = if state.confirmation {
        ictl_l10n::tr!("↑↓/←→ 选择 · Enter 确认 · Esc 取消", "↑↓/←→ select · Enter confirm · Esc cancel")
    } else {
        ictl_l10n::tr!("↑↓ 选择 · Enter 确认 · Esc 取消", "↑↓ select · Enter confirm · Esc cancel")
    };
    let mut lines = vec![fit(prompt, columns), fit(help, columns)];
    for (index, option) in options.iter().enumerate().take(end).skip(start) {
        lines.push(fit(
            &format!("{} {}) {option}", if index == state.selected { ">" } else { " " }, index + 1),
            columns,
        ));
    }
    let status = if state.overflow {
        ictl_l10n::tr!("输入过长 · 用方向键重新选择", "Input too long · use arrows to select again").to_owned()
    } else if state.input.is_empty() {
        ictl_l10n::trf!(
            "{}–{}/{} · 数字 + Enter · Ctrl+C 取消",
            "{}–{}/{} · number + Enter · Ctrl+C cancel",
            start + 1,
            end,
            options.len()
        )
    } else if state.answer().is_some() {
        ictl_l10n::trf!("输入: {} · Enter 确认", "Input: {} · Enter confirm", state.input)
    } else if state.confirmation {
        ictl_l10n::trf!(
            "无效输入 · 输入 是/否、yes/no 或 1/2: {}",
            "Invalid input · use yes/no or 1/2: {}",
            state.input
        )
    } else {
        ictl_l10n::trf!("无效输入 · 输入 1 到 {}: {}", "Invalid input · enter 1 to {}: {}", options.len(), state.input)
    };
    lines.push(fit(&status, columns));
    lines
}

fn menu(term: &Term, prompt: &str, options: &[&str], default: usize, confirmation: bool) -> anyhow::Result<usize> {
    let mut state = Selection::new(default, options.len(), confirmation);
    // (size, rendered line count) for the frame currently shown on screen.
    let mut previous = None;
    let mut needs_render = true;
    loop {
        let size = term.size();
        let resized = previous.is_some_and(|(old_size, _)| old_size != size);
        if needs_render || resized {
            // A resize can reflow every previous line. Only erase a frame while
            // its dimensions are unchanged; otherwise leave history alone and append.
            if let Some((old_size, lines)) = previous
                && old_size == size
            {
                term.clear_last_lines(lines)?;
            }
            if size.0 < 6 || size.1 < 40 {
                // Initial small terminals use line prompts; shrinking an active
                // menu must never discard invalid input or accept a stale default.
                // The prompt is written once per size; idle wakeups must not spam.
                if resized {
                    term.write_line(&fit(
                        ictl_l10n::tr!("放大窗口；Esc 取消", "Enlarge window; Esc cancels"),
                        usize::from(size.1.saturating_sub(1)),
                    ))?;
                }
                previous = Some((size, 0));
            } else {
                let lines = frame(prompt, options, &state, size);
                for line in &lines {
                    term.write_line(line)?;
                }
                previous = Some((size, lines.len()));
            }
            needs_render = false;
        }
        // An idle wait (no key) leaves the on-screen frame untouched; the next
        // iteration still re-checks term.size(), so resizes keep working.
        let Some(key) = read_key()? else { continue };
        needs_render = true;
        let current_size = term.size();
        if (current_size.0 < 6 || current_size.1 < 40) && !matches!(key, Key::Escape | Key::CtrlC) {
            continue;
        }
        if let Some(index) = state.key(key, capacity(current_size.0))? {
            term.write_line(&format!("{}: {}", clean(prompt), clean(options[index])))?;
            return Ok(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_rejects_malformed_and_unsupported_sequences() {
        for bytes in [b"\x1b[".as_slice(), b"\x1b[999~", b"\x1b[;~", b"\x1b[<0;1;1M", b"\x1b[M", b"\xff"] {
            assert_eq!(decode(bytes), Key::Unknown, "{bytes:?}");
        }
        assert_eq!(decode(b"\x1b[6~"), Key::PageDown);
        assert_eq!(decode(b"\x1bOA"), Key::ArrowUp);
        assert_eq!(decode("否".as_bytes()), Key::Char('否'));
    }

    #[test]
    fn oversized_input_cannot_accept_a_truncated_valid_answer() {
        let mut state = Selection::new(0, 2, true);
        state.key(Key::Char('1'), 2).unwrap();
        for _ in 0..INPUT_LIMIT * 2 {
            state.key(Key::Char(' '), 2).unwrap();
        }
        assert!(state.input.len() <= INPUT_LIMIT);
        assert_eq!(state.key(Key::Enter, 2).unwrap(), None);
        for _ in 0..INPUT_LIMIT {
            state.key(Key::Backspace, 2).unwrap();
        }
        assert_eq!(state.key(Key::Enter, 2).unwrap(), None);
        state.key(Key::ArrowRight, 2).unwrap();
        assert_eq!(state.key(Key::Enter, 2).unwrap(), Some(1));
    }

    #[test]
    fn arrows_defaults_and_confirmation() {
        let mut state = Selection::new(1, 3, false);
        assert_eq!(state.key(Key::Enter, 2).unwrap(), Some(1));
        state.key(Key::ArrowDown, 2).unwrap();
        state.key(Key::ArrowDown, 2).unwrap();
        assert_eq!(state.selected, 0);
        state.key(Key::ArrowUp, 2).unwrap();
        assert_eq!(state.selected, 2);
        let mut confirm = Selection::new(1, 2, true);
        confirm.key(Key::ArrowLeft, 2).unwrap();
        assert_eq!(confirm.key(Key::Enter, 2).unwrap(), Some(0));
        confirm.key(Key::ArrowRight, 2).unwrap();
        assert_eq!(confirm.key(Key::Enter, 2).unwrap(), Some(1));
        assert!(confirm.key(Key::Escape, 2).is_err());
        assert!(confirm.key(Key::CtrlC, 2).is_err());
    }

    #[test]
    fn numeric_input_is_buffered_validated_and_editable() {
        let mut state = Selection::new(0, 12, false);
        state.key(Key::Char('1'), 3).unwrap();
        state.key(Key::Char('2'), 3).unwrap();
        assert_eq!(state.key(Key::Enter, 3).unwrap(), Some(11));
        state.key(Key::Char('3'), 3).unwrap();
        assert_eq!(state.key(Key::Enter, 3).unwrap(), None);
        state.key(Key::Backspace, 3).unwrap();
        assert_eq!(state.key(Key::Enter, 3).unwrap(), Some(11));
        state.key(Key::Home, 3).unwrap();
        assert!(state.input.is_empty());
        state.key(Key::PageDown, 3).unwrap();
        assert_eq!(state.selected, 3);
        state.key(Key::End, 3).unwrap();
        assert_eq!(state.selected, 11);
    }

    #[test]
    fn confirmation_text_never_silently_accepts_the_default() {
        for (input, expected) in [("否", 1), ("不", 1), ("no", 1), ("NO", 1), ("是", 0), ("好", 0), ("yes", 0)] {
            let mut state = Selection::new(0, 2, true);
            for c in input.chars() {
                state.key(Key::Char(c), 2).unwrap();
            }
            assert_eq!(state.key(Key::Enter, 2).unwrap(), Some(expected), "{input}");
        }
        for confirmation in [false, true] {
            let mut state = Selection::new(0, 2, confirmation);
            for c in "1x".chars() {
                state.key(Key::Char(c), 2).unwrap();
            }
            assert_eq!(state.key(Key::Enter, 2).unwrap(), None);
            state.key(Key::Backspace, 2).unwrap();
            assert_eq!(state.key(Key::Enter, 2).unwrap(), Some(0));
            state.key(Key::Char('x'), 2).unwrap();
            state.key(Key::ArrowDown, 2).unwrap();
            assert!(state.input.is_empty());
            assert_eq!(state.key(Key::Enter, 2).unwrap(), Some(1));
        }
    }

    #[test]
    fn terminal_capability_requires_both_ttys_and_enough_space() {
        assert!(supports_menu(true, true, Some("xterm"), (24, 80)));
        for (input, output, term, size) in [
            (false, true, Some("xterm"), (24, 80)),
            (true, false, Some("xterm"), (24, 80)),
            (true, true, Some("dumb"), (24, 80)),
            (true, true, None, (24, 80)),
            (true, true, Some("xterm"), (5, 80)),
            (true, true, Some("xterm"), (24, 39)),
        ] {
            assert!(!supports_menu(input, output, term, size));
        }
    }

    #[test]
    fn fit_survives_widths_below_the_ellipsis_without_panicking() {
        let label = "中文很长的菜单选项 with extra text to truncate";
        for columns in 0..=4usize {
            let fitted = fit(label, columns);
            // Widths below the "..." tail (3) are clamped to the smallest
            // width truncate_str handles, so the result stays bounded and clean.
            assert!(console::measure_text_width(&fitted) <= 4);
            assert!(!fitted.chars().any(char::is_control));
        }
        assert!(fit("abcdefghij", 4).ends_with("..."));
    }

    #[test]
    fn frames_fit_chinese_long_labels_and_scroll_without_color() {
        let labels = vec!["中文很长的菜单选项\n\x1b[2J\t带有控制字符和额外文本"; 30];
        let state = Selection::new(29, labels.len(), false);
        for size in [(6, 40), (24, 80), (10, 50)] {
            let lines = frame("中文标题", &labels, &state, size);
            assert!(lines.len() < usize::from(size.0));
            assert!(lines.iter().all(|line| console::measure_text_width(line) < usize::from(size.1)));
            assert!(lines.iter().all(|line| !line.chars().any(char::is_control)));
            assert!(lines.iter().any(|line| line.starts_with("> 30)")));
        }
    }

    /// Driven by tests/terminal.py under a PTY. No setup/service/device effects.
    #[test]
    #[ignore = "interactive PTY harness; run tests/terminal.py"]
    fn pty_menu() {
        if std::env::var("ICTL_TEST_MENU").as_deref() == Ok("text-then-menu") {
            println!("TEXT_RESULT={}", crate::read_line("Text: ").unwrap());
        }
        let result = if std::env::var("ICTL_TEST_MENU").as_deref() == Ok("confirm") {
            ask_yes("Confirm?", std::env::var("ICTL_TEST_DEFAULT").as_deref() == Ok("yes")).map(usize::from)
        } else {
            let labels: Vec<_> =
                (1..=30).map(|i| format!("中文 menu item {i} with a long label for narrow windows")).collect();
            let options: Vec<_> = labels.iter().map(String::as_str).collect();
            choose("Menu test", &options, 1)
        };
        match result {
            Ok(index) => println!("MENU_RESULT={index}"),
            Err(error) if error.is::<Cancelled>() => println!("MENU_CANCELLED"),
            Err(error) => panic!("menu error: {error}"),
        }
    }
}
