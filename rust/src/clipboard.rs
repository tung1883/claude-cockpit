//! Copy text to the system clipboard from inside the TUI, where mouse capture
//! means the terminal's own selection is not available.
//!
//! Two routes, both tried: OSC 52 (the terminal sets its clipboard, which also
//! works over SSH; written to the outer terminal) and the OS clipboard tool.

use std::io::Write;
use std::process::{Command, Stdio};

/// `ESC ] 52 ; c ; <base64> BEL`
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

pub fn copy(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
    let text = text.to_string();
    std::thread::spawn(move || native(&text));
}

fn pipe_to(mut cmd: Command, text: &str) -> bool {
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = cmd.spawn() else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(windows)]
fn native(text: &str) {
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::InputEncoding=[Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
    ]);
    pipe_to(cmd, text);
}

#[cfg(target_os = "macos")]
fn native(text: &str) {
    pipe_to(Command::new("pbcopy"), text);
}

#[cfg(all(unix, not(target_os = "macos")))]
fn native(text: &str) {
    let tools: [(&str, &[&str]); 3] = [("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])];
    for (prog, args) in tools {
        let mut cmd = Command::new(prog);
        cmd.args(args);
        if pipe_to(cmd, text) {
            return;
        }
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo".as_bytes()), "aMOpbGxv");
    }

    #[test]
    fn osc52_wraps_the_payload() {
        assert_eq!(osc52("foo"), "\x1b]52;c;Zm9v\x07");
    }
}
