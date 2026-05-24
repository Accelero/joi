//! The slash-command catalog: the single source of truth for the `/` commands, shared by the
//! prompt's autosuggest (see [`AppModel::slash_suggestions`](crate::app::AppModel::slash_suggestions))
//! and the help overlay. A leading `/` at the start of the prompt opens the suggester; it
//! substring-matches the typed text against these names (see [`matching`]).

/// One slash command the user can run from the prompt.
pub struct SlashCommand {
    /// The canonical command, including the leading slash (e.g. `/resume`).
    pub name: &'static str,
    /// One-line description, shown in the suggester and the help overlay.
    pub help: &'static str,
}

/// Every slash command, in display order. `/exit` also accepts the `/quit` and `/q` aliases at
/// submit time (see [`AppModel::submit`](crate::app::AppModel)), but only the canonical names are
/// suggested.
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/resume",
        help: "list & resume a session",
    },
    SlashCommand {
        name: "/new",
        help: "start a fresh session",
    },
    SlashCommand {
        name: "/exit",
        help: "quit (or /quit, /q)",
    },
];

/// The commands matching `query`: a case-insensitive substring match of the text *after* the leading
/// slash against each command's body. A bare `/` matches everything. Prefix matches sort before
/// looser substring matches; catalog order breaks ties. Returns empty when `query` (ignoring leading
/// whitespace) doesn't start with `/`, so the suggester only opens on a slash prompt.
#[must_use]
pub fn matching(query: &str) -> Vec<&'static SlashCommand> {
    let Some(body) = query.trim_start().strip_prefix('/') else {
        return Vec::new();
    };
    let needle = body.trim().to_ascii_lowercase();
    let (mut prefix, mut substr) = (Vec::new(), Vec::new());
    for cmd in SLASH_COMMANDS {
        let hay = cmd.name.trim_start_matches('/').to_ascii_lowercase();
        if needle.is_empty() || hay.starts_with(&needle) {
            prefix.push(cmd);
        } else if hay.contains(&needle) {
            substr.push(cmd);
        }
    }
    prefix.extend(substr);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(query: &str) -> Vec<&'static str> {
        matching(query).iter().map(|c| c.name).collect()
    }

    #[test]
    fn non_slash_text_matches_nothing() {
        assert!(matching("hello").is_empty());
        assert!(matching("").is_empty());
    }

    #[test]
    fn bare_slash_lists_every_command_in_catalog_order() {
        assert_eq!(names("/"), vec!["/resume", "/new", "/exit"]);
    }

    #[test]
    fn substring_matches_the_command_body() {
        // "ew" is inside "new"; "sum" is inside "resume"; "x" is inside "exit".
        assert_eq!(names("/ew"), vec!["/new"]);
        assert_eq!(names("/sum"), vec!["/resume"]);
        assert_eq!(names("/x"), vec!["/exit"]);
    }

    #[test]
    fn prefix_matches_sort_before_looser_substring_matches() {
        // "e" prefixes "exit" but only appears mid-word in "resume"/"new".
        assert_eq!(names("/e"), vec!["/exit", "/resume", "/new"]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(names("/RES"), vec!["/resume"]);
    }
}
