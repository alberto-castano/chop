use std::io::{self, IsTerminal};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if force_color("CLICOLOR_FORCE") || force_color("FORCE_COLOR") {
        return true;
    }
    if std::env::var_os("CLICOLOR").is_some_and(|value| value == "0") {
        return false;
    }
    io::stdout().is_terminal() && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

pub fn supports_tui() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

fn force_color(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| value != "0")
}

pub fn bold(enabled: bool, value: &str) -> String {
    paint(enabled, "1", value)
}

pub fn muted(enabled: bool, value: &str) -> String {
    paint(enabled, "2", value)
}

pub fn good(enabled: bool, value: &str) -> String {
    paint(enabled, "1;32", value)
}

pub fn warn(enabled: bool, value: &str) -> String {
    paint(enabled, "1;33", value)
}

pub fn info(enabled: bool, value: &str) -> String {
    paint(enabled, "1;36", value)
}

pub fn danger(enabled: bool, value: &str) -> String {
    paint(enabled, "1;31", value)
}

fn paint(enabled: bool, code: &str, value: &str) -> String {
    if enabled {
        format!("\x1b[{code}m{value}\x1b[0m")
    } else {
        value.to_owned()
    }
}

pub fn path(path: &Path) -> String {
    let mut escaped = String::new();
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b' '..=b'~' if *byte != b'\\' => escaped.push(char::from(*byte)),
            b'\\' => escaped.push_str("\\\\"),
            byte => escaped.push_str(&format!("\\x{byte:02x}")),
        }
    }
    escaped
}

pub fn text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{1b}' => escaped.push_str("\\x1b"),
            character if character.is_control() => {
                for part in character.escape_default() {
                    escaped.push(part);
                }
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{good, path, text};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    #[test]
    fn escapes_terminal_control_characters() {
        assert_eq!(text("one\n\x1b]52;bad\u{7}"), "one\\n\\x1b]52;bad\\u{7}");
    }

    #[test]
    fn path_escape_is_injective_for_invalid_bytes_and_backslashes() {
        let invalid = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        let literal = PathBuf::from(r"/\xff");

        assert_eq!(path(&invalid), r"/\xff");
        assert_eq!(path(&literal), r"/\\xff");
    }

    #[test]
    fn color_can_be_enabled_or_disabled() {
        assert_eq!(good(false, "Remove"), "Remove");
        assert_eq!(good(true, "Remove"), "\x1b[1;32mRemove\x1b[0m");
    }
}
