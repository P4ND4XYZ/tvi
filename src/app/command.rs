#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Quit,
    Help,
    HideHighlights,
}

pub(super) fn parse(input: &str) -> Option<Command> {
    match input.trim() {
        "q" | "quit" => Some(Command::Quit),
        "help" => Some(Command::Help),
        "noh" | "nohlsearch" => Some(Command::HideHighlights),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_aliases() {
        for (input, expected) in [
            ("q", Command::Quit),
            ("quit", Command::Quit),
            ("help", Command::Help),
            ("noh", Command::HideHighlights),
            ("nohlsearch", Command::HideHighlights),
        ] {
            assert_eq!(parse(input), Some(expected));
        }
        assert_eq!(parse("qa"), None);
    }
}
