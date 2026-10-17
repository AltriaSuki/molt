//! Signals outside the interface: the window title, the bell and desktop
//! notifications, as escape sequences the terminal understands.

/// How the terminal shows a desktop notification, beyond the bell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The bell alone: the terminal, or tmux, may not take a notification.
    Bell,
    /// OSC 9, as iTerm2, WezTerm and Ghostty show it.
    Osc9,
    /// OSC 777, as VTE terminals, foot and urxvt show it.
    Osc777,
}

impl Notice {
    /// The kind the terminal named by `var` takes. An unknown terminal gets
    /// the bell alone, since an escape it does not know may show as text.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        if var("TMUX").is_some() {
            return Self::Bell;
        }
        let program = var("TERM_PROGRAM").unwrap_or_default();
        let term = var("TERM").unwrap_or_default();
        if matches!(program.as_str(), "iTerm.app" | "WezTerm" | "ghostty") {
            Self::Osc9
        } else if var("VTE_VERSION").is_some() || term.starts_with("foot") || term.starts_with("rxvt") {
            Self::Osc777
        } else {
            Self::Bell
        }
    }

    /// The bytes that ring the bell and show `text`.
    pub fn alert(self, text: &str) -> String {
        let text = plain(text);
        match self {
            Self::Bell => "\x07".to_owned(),
            Self::Osc9 => format!("\x07\x1b]9;{text}\x07"),
            Self::Osc777 => format!("\x07\x1b]777;notify;molt;{text}\x07"),
        }
    }
}

/// Saves the window's title on the terminal's title stack, to be put back by
/// [`POP_TITLE`].
pub const PUSH_TITLE: &str = "\x1b[22;0t";
pub const POP_TITLE: &str = "\x1b[23;0t";

/// The bytes that set the window title.
pub fn title(text: &str) -> String {
    format!("\x1b]0;{}\x07", plain(text))
}

/// `text` without control characters, which could end the sequence early.
fn plain(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn the_notification_follows_the_terminal_and_tmux_gets_the_bell_alone() {
        assert_eq!(Notice::from_env(env(&[("TERM_PROGRAM", "iTerm.app")])), Notice::Osc9);
        assert_eq!(Notice::from_env(env(&[("VTE_VERSION", "7600"), ("TERM", "xterm-256color")])), Notice::Osc777);
        assert_eq!(Notice::from_env(env(&[("TERM", "foot")])), Notice::Osc777);
        assert_eq!(Notice::from_env(env(&[("TERM", "xterm-256color")])), Notice::Bell);
        assert_eq!(Notice::from_env(env(&[("TERM_PROGRAM", "WezTerm"), ("TMUX", "/tmp/t")])), Notice::Bell);
    }

    #[test]
    fn a_label_cannot_end_its_sequence_early() {
        assert_eq!(Notice::Osc9.alert("molt: check\x07 ready\x1b]"), "\x07\x1b]9;molt: check ready]\x07");
        assert_eq!(Notice::Osc777.alert("molt: passed"), "\x07\x1b]777;notify;molt;molt: passed\x07");
        assert_eq!(Notice::Bell.alert("anything"), "\x07");
        assert_eq!(title("molt · idle\n"), "\x1b]0;molt · idle\x07");
    }
}
