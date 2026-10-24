//! `orion …` typed into the chat: a command for a terminal, not a message.
//! Those with a chat counterpart run it; the rest get a word on where they
//! belong, instead of going to the model.

/// What to do with a typed `orion …` line.
#[derive(Debug, PartialEq)]
pub(super) enum Typed {
    /// Run this slash command (without the `/`).
    Run(String),
    /// Show this instead.
    Explain(String),
}

const SUBCOMMANDS: &[&str] = &[
    "chat", "shell", "listen", "memory", "voice", "conversations", "serve", "daemon", "helper", "approvals",
    "status", "tools", "call", "setup", "doctor", "audit", "help",
];

/// Flags whose value is the next word.
const VALUED: &[&str] =
    &["--config", "--person", "--provider", "--model", "--base-url", "--port", "--socket", "--caller", "-n", "--lines"];

/// `Some` when `input` is an `orion` command line; anything else (“Orion,
/// what's using my disk?”) is a message, and `None`.
pub(super) fn typed(input: &str) -> Option<Typed> {
    let input = input.trim().trim_start_matches("$ ");
    let mut words = input.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("orion") {
        return None;
    }
    let words: Vec<&str> = words.collect();
    let (sub, rest) = match words.split_first() {
        None => ("help", &[][..]),
        Some((&("--help" | "-h"), rest)) => ("help", rest),
        Some((sub, rest)) => (*sub, rest),
    };
    if !SUBCOMMANDS.contains(&sub) {
        return None;
    }
    // Flags are fine anywhere; other words only where the command takes them,
    // so “orion status of my disks please” stays a message.
    let mut args = Vec::new();
    let mut rest = rest.iter();
    while let Some(&w) = rest.next() {
        if VALUED.contains(&w) {
            rest.next();
        } else if !w.starts_with('-') {
            args.push(w);
        }
    }
    let terminal = |hint: &str| {
        Some(Typed::Explain(format!(
            "`orion {sub}` is a command for a terminal. Here, what you type goes to the AI, \
             and slash commands (/help lists them) control the chat. {hint}"
        )))
    };
    match (sub, args.as_slice()) {
        ("help", []) => Some(Typed::Run("help".into())),
        ("status", []) => Some(Typed::Run("status".into())),
        ("tools", []) => Some(Typed::Run("tools".into())),
        ("memory", [] | ["list"]) => Some(Typed::Run("memory".into())),
        // Forgetting stays something you type on purpose.
        ("memory", ["forget", id]) => terminal(&format!("Type /memory forget {id} to forget it here.")),
        ("memory", ["clear"]) => terminal("Type /memory clear to clear it here."),
        ("conversations", [] | ["list"]) => Some(Typed::Run("history".into())),
        ("conversations", ["show", id]) => terminal(&format!("Type /resume {id} to continue it here.")),
        ("conversations", ["delete", _]) => terminal("Deleting one needs the terminal."),
        ("chat" | "shell" | "listen", []) => Some(Typed::Explain(
            "You're already in Orion's chat. /use auto|claude|local picks the model; /resume continues a saved conversation."
                .into(),
        )),
        ("doctor", []) => terminal(
            "Run `orion doctor` in a terminal to check and fix the setup (it may ask for your password), \
             or ask me here, such as “is my microphone working?”",
        ),
        ("setup", []) => terminal("Run `orion setup` in a terminal to connect a model; /model shows the ones in use."),
        ("voice", [_, ..]) => terminal("Voice enrollment runs in a terminal; /voice shows the voice setup here."),
        ("call", [] | [_] | [_, _]) if args.first().is_none_or(|t| t.contains('_')) => {
            terminal("Here, ask for it in words and the AI calls the tool for you.")
        }
        ("serve" | "daemon" | "helper" | "approvals" | "audit", []) => terminal("Run it in a terminal."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &str) -> Option<Typed> {
        Some(Typed::Run(s.into()))
    }

    #[test]
    fn commands_with_a_slash_counterpart_run_it() {
        assert_eq!(typed("orion status"), run("status"));
        assert_eq!(typed("  Orion tools "), run("tools"));
        assert_eq!(typed("$ orion memory list"), run("memory"));
        assert_eq!(typed("orion memory --person sam"), run("memory"));
        assert_eq!(typed("orion conversations"), run("history"));
        assert_eq!(typed("orion"), run("help"));
        assert_eq!(typed("orion --help"), run("help"));
    }

    #[test]
    fn the_rest_are_explained() {
        for line in ["orion doctor", "orion doctor --no-fix", "orion setup --provider claude", "orion chat --local",
            "orion memory clear", "orion memory forget 3", "orion voice enroll", "orion audit -n 5",
            "orion call system_info", "orion conversations show abc"]
        {
            assert!(matches!(typed(line), Some(Typed::Explain(_))), "{line}");
        }
        let Some(Typed::Explain(text)) = typed("orion memory forget 3") else { panic!() };
        assert!(text.contains("/memory forget 3"), "{text}");
    }

    #[test]
    fn messages_that_start_with_the_name_go_to_the_model() {
        for line in ["Orion, status?", "orion status of my disks please", "orion help me find my photos",
            "orion what time is it", "orion call mom", "orion doctor this file", "hello orion status", "orionstatus"]
        {
            assert_eq!(typed(line), None, "{line}");
        }
    }
}
