//! `CmdCompleter` — rustyline completion for slash commands.
//!
//! Provides:
//! - `complete_pure` / `hint_pure`: pure functions over `(line, pos)` that
//!   unit tests can exercise without depending on `rustyline::Context`'s
//!   lifetime parameter.
//! - `Completer` / `Hinter` impls that delegate to those pure functions.
//!
//! Grouping D: rustyline integration. Grouping E will extend the display
//! formatting (description / arg_hint rendering).

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper, Result as RustylineResult};

/// One slash command entry. `name` is the bare command (e.g. `"model"`),
/// `description` is the short help text, `arg_hint` is the optional
/// placeholder shown next to the name (e.g. `"[model_name]"`).
#[derive(Clone, Debug)]
pub struct CmdEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub arg_hint: Option<&'static str>,
}

/// Completer that suggests commands when the input starts with `/`.
///
/// Behaviour:
/// - Input not starting with `/`: no suggestions.
/// - Input is just `/`: all registered commands.
/// - Input is `/foo`: only commands whose `name` starts with `foo`.
pub struct CmdCompleter {
    entries: Vec<CmdEntry>,
}

impl CmdCompleter {
    pub fn new(entries: Vec<CmdEntry>) -> Self {
        Self { entries }
    }

    /// Pure completion logic: returns `(replacement_start, candidates)`.
    /// Used both by the `Completer` impl and by unit tests.
    pub fn complete_pure(&self, line: &str, pos: usize) -> (usize, Vec<Pair>) {
        if !line.starts_with('/') {
            return (0, vec![]);
        }
        let prefix_end = 1;
        let prefix = &line[1..pos.min(line.len())];

        let candidates: Vec<Pair> = self
            .entries
            .iter()
            .filter(|e| e.name.starts_with(prefix))
            .map(|e| {
                let display = match e.arg_hint {
                    Some(h) => format!("/{} {}  — {}", e.name, h, e.description),
                    None => format!("/{}  — {}", e.name, e.description),
                };
                Pair {
                    display,
                    replacement: format!("/{} ", e.name),
                }
            })
            .collect();

        (prefix_end, candidates)
    }

    /// Pure hint logic. Used both by the `Hinter` impl and by unit tests.
    pub fn hint_pure(&self, line: &str, pos: usize) -> Option<String> {
        if !line.starts_with('/') {
            return None;
        }
        let prefix = &line[1..pos.min(line.len())];
        self.entries
            .iter()
            .find(|e| e.name.starts_with(prefix))
            .map(|e| format!(" — {}", e.description))
    }
}

impl Completer for CmdCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> RustylineResult<(usize, Vec<Pair>)> {
        let (end, pairs) = self.complete_pure(line, pos);
        Ok((end, pairs))
    }
}

impl Hinter for CmdCompleter {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        self.hint_pure(line, pos)
    }
}

impl Highlighter for CmdCompleter {
    // Use the default highlighter (no extra styling).
}

impl Validator for CmdCompleter {
    // Use the default validator (accept any input).
}

impl Helper for CmdCompleter {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entries() -> Vec<CmdEntry> {
        vec![
            CmdEntry {
                name: "help",
                description: "Show available commands",
                arg_hint: Some("[command]"),
            },
            CmdEntry {
                name: "model",
                description: "Switch model",
                arg_hint: Some("[model_name]"),
            },
            CmdEntry {
                name: "login",
                description: "Configure credentials",
                arg_hint: Some("[provider]"),
            },
        ]
    }

    #[test]
    fn test_complete_pure_empty_returns_empty_when_no_slash() {
        let completer = CmdCompleter::new(test_entries());
        let (start, pairs) = completer.complete_pure("hello", 5);
        assert_eq!(start, 0);
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_complete_pure_slash_returns_all_commands() {
        let completer = CmdCompleter::new(test_entries());
        let (start, pairs) = completer.complete_pure("/", 1);
        assert_eq!(start, 1);
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn test_complete_pure_prefix_match() {
        let completer = CmdCompleter::new(test_entries());
        let (start, pairs) = completer.complete_pure("/mo", 3);
        assert_eq!(start, 1);
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].display.contains("model"));
        assert_eq!(pairs[0].replacement, "/model ");
    }

    #[test]
    fn test_complete_pure_no_match() {
        let completer = CmdCompleter::new(test_entries());
        let (start, pairs) = completer.complete_pure("/xyz", 4);
        assert_eq!(start, 1);
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_hint_pure_returns_description() {
        let completer = CmdCompleter::new(test_entries());
        let hint = completer.hint_pure("/mo", 3);
        assert_eq!(hint, Some(" — Switch model".to_string()));
    }

    #[test]
    fn test_hint_pure_no_match_returns_none() {
        let completer = CmdCompleter::new(test_entries());
        let hint = completer.hint_pure("/xyz", 4);
        assert!(hint.is_none());
    }

    #[test]
    fn test_hint_pure_non_slash_returns_none() {
        let completer = CmdCompleter::new(test_entries());
        let hint = completer.hint_pure("hello", 5);
        assert!(hint.is_none());
    }

    #[test]
    fn test_complete_pure_arg_hint_included_in_display() {
        let completer = CmdCompleter::new(test_entries());
        let (_start, pairs) = completer.complete_pure("/help", 5);
        assert_eq!(pairs.len(), 1);
        // Display format includes the arg hint for /help: "/help [command]  — Show available commands"
        assert!(
            pairs[0].display.contains("[command]"),
            "display = {}",
            pairs[0].display
        );
    }
}
