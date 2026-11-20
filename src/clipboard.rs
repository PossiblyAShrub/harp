//! OS clipboard access.
//!
//! Writes prefer the terminal emulator's OSC 52 (write-clipboard) support and
//! fall back to an external clipboard tool, which is what most TUI apps do.
//! Reads rely on external clipboard tools; OSC 52 *reads* are deliberately
//! skipped because terminals like kitty pop a confirmation dialog for them.

use std::{
    io::{self, Write},
    process::{Command, Stdio},
};

/// Sanitizes text about to be injected into a terminal, mirroring what kitty's
/// "sanitize" paste action does: escape sequences (CSI/OSC/DCS and friends)
/// and control codes are discarded so the clipboard can't drive the shell.
/// Newline, tab and CR are preserved.
pub fn sanitize_for_paste(text: &str) -> String {
    #[derive(Copy, Clone, Eq, PartialEq)]
    enum Esc {
        /// After a lone ESC; the next character decides the sequence type.
        Prefix,
        /// Inside a multi-character escape sequence.
        Sequence,
    }

    let mut out = String::with_capacity(text.len());
    let mut esc: Option<Esc> = None;

    for c in text.chars() {
        match esc {
            Some(Esc::Prefix) => {
                if c == '\x1b' {
                    // Doubled ESC: still a prefix.
                    continue;
                }
                if matches!(c, '\n' | '\r' | '\t') {
                    // A freshline aborts the sequence; keep it.
                    esc = None;
                    out.push(c);
                } else if matches!(c, '[' | ']' | 'P' | '^' | '_') {
                    // CSI / OSC / DCS / PM / APC: multi-character sequence.
                    esc = Some(Esc::Sequence);
                } else if ('\x40'..='\x7e').contains(&c) {
                    // Final byte of a single-character escape.
                    esc = None;
                } else {
                    // Intermediate byte: stays in the sequence.
                    esc = Some(Esc::Sequence);
                }
            }
            Some(Esc::Sequence) => match c {
                c if ('\x40'..='\x7e').contains(&c) || c == '\x07' => {
                    // Final byte, or BEL terminating an OSC sequence.
                    esc = None;
                }
                '\x1b' => esc = Some(Esc::Prefix), // ST (ESC \)
                c if matches!(c, '\n' | '\r' | '\t') => {
                    esc = None;
                    out.push(c);
                }
                _ => {}
            },
            None => match c {
                '\x1b' => esc = Some(Esc::Prefix),
                c if matches!(c, '\n' | '\r' | '\t') => out.push(c),
                c if c.is_control() => {}
                c => out.push(c),
            },
        }
    }
    out
}

/// Reads the current OS clipboard contents, if a suitable tool is available.
pub fn clipboard_get() -> Option<String> {
    const TOOLS: &[(&str, &[&str])] = &[
        ("wl-paste", &[]),                             // Wayland
        ("xclip", &["-o", "-selection", "clipboard"]), // X11
        ("xsel", &["-o", "--clipboard"]),
        ("pbpaste", &[]),              // macOS
        ("termux-clipboard-get", &[]), // Android
    ];
    for (tool, args) in TOOLS {
        let Ok(output) = Command::new(tool).args(*args).output() else {
            continue;
        };
        if output.status.success() && !output.stdout.is_empty() {
            // Lossy decode guards against non-text clipboard contents.
            return Some(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    None
}

/// Writes `text` to the OS clipboard. Sends an OSC 52 write to the terminal
/// emulator (kitty and friends place it on the OS clipboard) and also tries an
/// external clipboard tool as a fallback for emulators without OSC 52 support.
pub fn clipboard_set(text: &str) {
    let _ = write_osc52(text);
    external_set(text);
}

/// OSC 52 `Ps = c` (clipboard) write: `ESC ] 52 ; c ; <base64> ST`.
fn write_osc52(text: &str) -> io::Result<()> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\x1b]52;c;")?;
    stdout.write_all(encoded.as_bytes())?;
    stdout.write_all(b"\x07")?;
    stdout.flush()
}

/// Writes `text` to the clipboard via the first available external tool.
fn external_set(text: &str) {
    const TOOLS: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),                        // Wayland
        ("xclip", &["-selection", "clipboard"]), // X11
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),               // macOS
        ("termux-clipboard-set", &[]), // Android
    ];
    for (tool, args) in TOOLS {
        let Ok(mut child) = Command::new(tool)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let fed = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if fed {
            let _ = child.wait();
            return;
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_escape_sequences_and_control_codes() {
        assert_eq!(
            sanitize_for_paste("a\x1b[31mb\nc\t\x07d\u{9b}\x00"),
            "ab\nc\td"
        );
        assert_eq!(sanitize_for_paste("héllo world"), "héllo world");
    }
}
