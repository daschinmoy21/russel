//! Terminal output helpers: color, tables, status highlighting.
//!
//! Color is on when stdout is a TTY and `NO_COLOR` is unset. Control characters
//! in server-supplied strings are stripped before print.

use std::io::{IsTerminal, stdout};

pub fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && stdout().is_terminal()
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}

pub fn green(s: &str) -> String {
    paint("1;32", s)
}

pub fn red(s: &str) -> String {
    paint("1;31", s)
}

pub fn yellow(s: &str) -> String {
    paint("1;33", s)
}

pub fn cyan(s: &str) -> String {
    paint("1;36", s)
}

/// Strip C0/C1 controls except tab/newline/CR so a hostile payload cannot
/// hijack the terminal.
pub fn sanitize(s: &str) -> String {
    s.chars().filter(|&c| !is_terminal_control(c)).collect()
}

fn is_terminal_control(c: char) -> bool {
    let u = c as u32;
    if u <= 0x1F && u != 0x09 && u != 0x0A && u != 0x0D {
        return true;
    }
    u == 0x7F || (0x80..=0x9F).contains(&u)
}

/// Color a lifecycle / deploy status word.
pub fn status_style(status: &str) -> String {
    let pretty = sanitize(status);
    match pretty.as_str() {
        "deployed" | "running" | "active" => green(&pretty),
        "failed" | "error" | "destroyed" => red(&pretty),
        "stopped" | "stopping" | "rolled_back" | "previous" => yellow(&pretty),
        "building" | "starting" => cyan(&pretty),
        _ => dim(&pretty),
    }
}

pub fn format_uptime(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        let h = seconds / 3600;
        let m = (seconds % 3600) / 60;
        format!("{h}h {m:02}m")
    }
}

/// One labeled row: dim label (width 10) then value.
pub fn kv(label: &str, value: &str) {
    println!("  {}  {value}", dim(&format!("{label:>10}")));
}

pub fn heading(title: &str) {
    println!();
    println!("  {}", bold(title));
}

/// Print an aligned table. `headers` and each row must have the same length.
pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("  {}", dim("(none)"));
        return;
    }
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(visible_width(cell));
        }
    }
    print!("  ");
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            print!("  ");
        }
        print!("{}", dim(&format!("{h:<width$}", width = widths[i])));
    }
    println!();
    for row in rows {
        print!("  ");
        for (i, cell) in row.iter().enumerate().take(cols) {
            if i > 0 {
                print!("  ");
            }
            let pad = widths[i].saturating_sub(visible_width(cell));
            print!("{cell}{}", " ".repeat(pad));
        }
        println!();
    }
}

/// Display width ignoring ANSI CSI sequences.
fn visible_width(s: &str) -> usize {
    let mut n = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for x in chars.by_ref() {
                    if x.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        n += 1;
    }
    n
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn uptime_buckets() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(13), "13s");
        assert_eq!(format_uptime(75), "1m 15s");
        assert_eq!(format_uptime(3661), "1h 01m");
    }

    #[test]
    fn sanitize_strips_escapes() {
        assert_eq!(sanitize("ok\x1b[31mRED"), "ok[31mRED");
        assert_eq!(sanitize("a\nb"), "a\nb");
    }

    #[test]
    fn visible_width_skips_ansi() {
        assert_eq!(visible_width("hello"), 5);
        assert_eq!(visible_width("\x1b[1;32mhello\x1b[0m"), 5);
    }
}
