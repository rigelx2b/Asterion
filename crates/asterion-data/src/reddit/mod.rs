//! Reddit submissions from the Arctic Shift archive: raw collection and daily aggregates.

pub mod api;
pub mod collect;
pub mod daily;

pub use api::*;

use crate::error::DataError;

// Reddit names are 3–21 chars of [A-Za-z0-9_]; they also become cache path segments.
const MAX_NAME_LEN: usize = 21;

/// Subreddit names from a list file: `#` starts a comment, blank lines are skipped.
pub fn parse_list(text: &str) -> Result<Vec<String>, DataError> {
    let mut names = Vec::new();
    for line in text.lines() {
        let name = line.split('#').next().unwrap_or_default().trim();
        if name.is_empty() {
            continue;
        }
        let valid = name.len() <= MAX_NAME_LEN
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(DataError::InvalidField {
                field: "subreddit",
                value: name.to_owned(),
            });
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_rejects_invalid_names() {
        assert_eq!(
            parse_list("# c\nstocks\n\n KoreanStocks # kr\n").expect("ok"),
            vec!["stocks", "KoreanStocks"]
        );
        assert!(parse_list("r/stocks").is_err());
        assert!(parse_list("wall-street").is_err());
        assert!(parse_list("a_name_that_is_longer_than_21").is_err());
    }
}
