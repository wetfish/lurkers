//! IRC compares nicks and channel names without regard to case. Servers
//! announce their rules as CASEMAPPING: `ascii` folds only A to Z, and
//! `rfc1459` also treats `[]\~` as the uppercase forms of `{}|^`. Folding
//! with rfc1459 matches every name that either rule matches, and the only
//! extra matches are names that differ by those four characters alone.

/// Lowercases a nick or channel name so it can be compared with another.
pub fn fold(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            '[' => '{',
            ']' => '}',
            '\\' => '|',
            '~' => '^',
            _ => c,
        })
        .collect()
}

/// Whether two nicks or channel names are the same to the server.
pub fn eq(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ascii_letters() {
        assert_eq!(fold("#FBI_Watchlist"), "#fbi_watchlist");
        assert!(eq("Lurker", "lURKER"));
    }

    #[test]
    fn folds_rfc1459_brackets() {
        assert_eq!(fold("[a]\\~"), "{a}|^");
        assert!(eq("rachel[away]", "rachel{away}"));
    }

    #[test]
    fn keeps_other_characters() {
        assert_eq!(fold("lurker_-^`|1"), "lurker_-^`|1");
        assert!(!eq("lurker", "lurker_"));
    }
}
