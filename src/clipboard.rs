use std::io::Write;

use base64::Engine;

pub struct Clipboard {
    system: Option<arboard::Clipboard>,
    tried_system: bool,
    osc52: bool,
    pub last: Option<String>,
}

pub fn osc52_sequence(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

impl Clipboard {
    pub fn new(osc52: bool, system: bool) -> Self {
        Self {
            system: None,
            tried_system: !system,
            osc52,
            last: None,
        }
    }

    fn system(&mut self) -> Option<&mut arboard::Clipboard> {
        if !self.tried_system {
            self.tried_system = true;
            let has_display = std::env::var_os("DISPLAY").is_some()
                || std::env::var_os("WAYLAND_DISPLAY").is_some();
            if has_display || !cfg!(target_os = "linux") {
                self.system = arboard::Clipboard::new().ok();
            }
        }
        self.system.as_mut()
    }

    pub fn copy(&mut self, text: &str) -> String {
        self.last = Some(text.to_string());
        let mut targets = Vec::new();
        if let Some(system) = self.system() {
            if system.set_text(text.to_string()).is_ok() {
                targets.push("clipboard");
            }
            #[cfg(target_os = "linux")]
            {
                use arboard::{LinuxClipboardKind, SetExtLinux};
                if system
                    .set()
                    .clipboard(LinuxClipboardKind::Primary)
                    .text(text.to_string())
                    .is_ok()
                {
                    targets.push("primary selection");
                }
            }
        }
        if self.osc52 {
            let mut out = std::io::stdout();
            if out
                .write_all(osc52_sequence(text).as_bytes())
                .and_then(|_| out.flush())
                .is_ok()
            {
                targets.push("terminal (OSC 52)");
            }
        }
        let chars = text.chars().count();
        if targets.is_empty() {
            format!("could not copy {chars} characters: no clipboard available")
        } else {
            format!("copied {chars} characters to {}", targets.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encodes_text_as_base64() {
        assert_eq!(osc52_sequence("hi ż"), "\x1b]52;c;aGkgxbw=\x07");
    }
}
