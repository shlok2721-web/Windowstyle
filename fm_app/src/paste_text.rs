//! Recognise plain text on the clipboard that is clearly a Python script, a shell script or
//! ordinary text, so Ctrl+V in a folder can turn it into a new file with the right extension.
//!
//! Only these few kinds are recognised, and only on strong signals (a shebang, or several
//! unmistakable lines): a wrong guess would create a wrongly named file, so anything unsure
//! falls back to plain text rather than to a code type.

/// What the pasted text should be saved as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastedKind {
    Python,
    Shell,
    Text,
}

impl PastedKind {
    /// Base name of the new file (made unique by the caller).
    pub fn default_name(self) -> &'static str {
        match self {
            PastedKind::Python => "pasted.py",
            PastedKind::Shell => "pasted.sh",
            PastedKind::Text => "Pasted text.txt",
        }
    }

    /// Scripts get their execute bit only when the text itself starts with a shebang.
    pub fn is_script(self) -> bool {
        !matches!(self, PastedKind::Text)
    }
}

/// Largest clipboard text turned into a file (guards against pasting huge selections).
pub const MAX_PASTE_BYTES: usize = 8 * 1024 * 1024;

/// Classify `text`. `None` for empty / whitespace-only / oversized text, which is never pasted.
pub fn detect(text: &str) -> Option<PastedKind> {
    if text.trim().is_empty() || text.len() > MAX_PASTE_BYTES || text.contains('\0') {
        return None;
    }
    let first = text.lines().next().unwrap_or("").trim();
    if let Some(interpreter) = first.strip_prefix("#!") {
        let lc = interpreter.to_ascii_lowercase();
        if lc.contains("python") {
            return Some(PastedKind::Python);
        }
        if ["sh", "bash", "zsh", "dash", "ksh", "ash"]
            .iter()
            .any(|shell| lc.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == *shell))
        {
            return Some(PastedKind::Shell);
        }
    }

    let (mut py, mut sh) = (0u32, 0u32);
    for raw in text.lines().take(400) {
        let line = raw.trim();
        if line.is_empty() || (line.starts_with('#') && !line.starts_with("#!")) {
            continue;
        }
        if python_line(line) {
            py += 1;
        }
        if shell_line(line) {
            sh += 1;
        }
    }
    // Two independent signals are required, and the winner must clearly lead.
    if py >= 2 && py > sh {
        Some(PastedKind::Python)
    } else if sh >= 2 && sh > py {
        Some(PastedKind::Shell)
    } else {
        Some(PastedKind::Text)
    }
}

fn starts_with_word(line: &str, word: &str) -> bool {
    line.strip_prefix(word)
        .is_some_and(|rest| rest.starts_with(|c: char| c == ' ' || c == '\t' || c == '('))
}

/// A line only Python would have: block openers ending in `:`, imports, `print(`...
fn python_line(line: &str) -> bool {
    if line.starts_with("if __name__") {
        return true;
    }
    if line.ends_with(':')
        && ["def", "class", "if", "elif", "else", "for", "while", "try", "except", "finally", "with", "async"]
            .iter()
            .any(|kw| line == format!("{kw}:") || starts_with_word(line, kw))
    {
        // `else:` / `try:` / `finally:` alone count; the others need their keyword up front.
        return true;
    }
    if let Some(rest) = line.strip_prefix("import ") {
        return !rest.contains(';') && !rest.is_empty();
    }
    if let Some(rest) = line.strip_prefix("from ") {
        return rest.contains(" import ");
    }
    line.starts_with("print(") || line.starts_with("return ") && !line.contains(';')
}

/// A line only a shell would have.
fn shell_line(line: &str) -> bool {
    const COMMANDS: [&str; 16] = [
        "echo", "cd", "export", "source", "alias", "sudo", "chmod", "chown", "mkdir", "rm", "cp",
        "mv", "apt", "apt-get", "pacman", "systemctl",
    ];
    if COMMANDS
        .iter()
        .any(|cmd| line == *cmd || line.strip_prefix(cmd).is_some_and(|r| r.starts_with(' ')))
    {
        return true;
    }
    if ["fi", "done", "esac", "then", "do"].contains(&line) || line.starts_with("set -e") {
        return true;
    }
    if (line.starts_with("if [") || line.starts_with("while [") || line.starts_with("until ["))
        && (line.ends_with("; then") || line.ends_with("]") || line.ends_with("; do"))
    {
        return true;
    }
    if line.starts_with("for ") && (line.ends_with("; do") || line.ends_with(" do")) {
        return true;
    }
    // `NAME=value` and `$(...)` / `${...}` expansions
    let assignment = line.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    });
    assignment || line.contains("$(") || line.contains("${")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shebangs_decide_immediately() {
        assert_eq!(detect("#!/usr/bin/env python3\nx = 1\n"), Some(PastedKind::Python));
        assert_eq!(detect("#!/bin/bash\nls\n"), Some(PastedKind::Shell));
        assert_eq!(detect("#!/usr/bin/env sh\n"), Some(PastedKind::Shell));
        assert_eq!(detect("#!/usr/bin/zsh\n"), Some(PastedKind::Shell));
    }

    #[test]
    fn python_structure() {
        let src = "import os\n\ndef main():\n    print(os.getcwd())\n\nif __name__ == \"__main__\":\n    main()\n";
        assert_eq!(detect(src), Some(PastedKind::Python));
        assert_eq!(detect("from a import b\nclass X:\n    pass\n"), Some(PastedKind::Python));
    }

    #[test]
    fn shell_structure() {
        let src = "set -e\nNAME=world\necho \"hello $NAME\"\nif [ -d /tmp ]; then\n  cd /tmp\nfi\n";
        assert_eq!(detect(src), Some(PastedKind::Shell));
        assert_eq!(detect("sudo apt install git\nsudo apt install curl\n"), Some(PastedKind::Shell));
    }

    #[test]
    fn everything_else_is_plain_text() {
        assert_eq!(detect("Hello, this is a note.\nSecond line."), Some(PastedKind::Text));
        assert_eq!(detect("import is a word I use; import it"), Some(PastedKind::Text));
        // one lone signal is not enough
        assert_eq!(detect("echo hi"), Some(PastedKind::Text));
        assert_eq!(detect("def f(): pass"), Some(PastedKind::Text));
        assert_eq!(detect("a = 1\nb = 2\n"), Some(PastedKind::Text));
    }

    #[test]
    fn nothing_to_paste() {
        assert_eq!(detect(""), None);
        assert_eq!(detect("   \n\t\n"), None);
        assert_eq!(detect("a\0b"), None);
    }
}
