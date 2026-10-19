//! What ⌘P found per node type while the palette is open (spec 002). Typing more narrows a
//! search: when a type's last answer held every match (fewer rows than the limit) and the new
//! text only narrows the old (`search_narrows`), the new matches are among the old rows and are
//! filtered here instead of asked for again. Text typed back to (after a backspace) shows its
//! answer again. Most types answer every keystroke after the first few without a read.

use std::collections::HashMap;

use gq_language::builder::{search_matches, search_narrows};

/// A node a search found: its id, and its label when the type has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub id: String,
    pub label: Option<String>,
}

#[derive(Default)]
pub(crate) struct Answers {
    /// Every answer while the palette is open, by type and text: text typed back to shows
    /// again without a read.
    exact: HashMap<(String, String), Vec<Found>>,
    /// Each type's widest answer that held every match (fewer rows than were asked for), with
    /// its text; narrower text filters it. Kept while the text narrows and widens again, so a
    /// typo corrected answers from it.
    complete: HashMap<String, (String, Vec<Found>)>,
}

impl Answers {
    /// `type_name`'s matches for `text` without a read, when an earlier answer holds them all.
    pub(crate) fn known(&mut self, type_name: &str, text: &str) -> Option<Vec<Found>> {
        let key = (type_name.to_owned(), text.to_owned());
        if let Some(rows) = self.exact.get(&key) {
            return Some(rows.clone());
        }
        let (earlier, rows) = self.complete.get(type_name)?;
        if !search_narrows(earlier, text) {
            return None;
        }
        let rows: Vec<Found> = rows
            .iter()
            .filter(|found| search_matches(&found.id, found.label.as_deref(), text))
            .cloned()
            .collect();
        self.exact.insert(key, rows.clone());
        Some(rows)
    }

    /// Keeps what a read for `type_name` at `text` answered, asked with `limit`.
    pub(crate) fn keep(&mut self, type_name: String, text: String, rows: Vec<Found>, limit: usize) {
        // An answer the held one covers is narrower: the held one stays.
        let covered = self
            .complete
            .get(&type_name)
            .is_some_and(|(held, _)| search_narrows(held, &text));
        if rows.len() < limit && !covered {
            self.complete
                .insert(type_name.clone(), (text.clone(), rows.clone()));
        }
        self.exact.insert((type_name, text), rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(id: &str, label: &str) -> Found {
        Found {
            id: id.to_owned(),
            label: Some(label.to_owned()),
        }
    }

    #[test]
    fn a_complete_answer_narrows_without_a_read_and_a_full_one_asks_again() {
        let mut answers = Answers::default();
        answers.keep(
            "Person".into(),
            "ad".into(),
            vec![found("p-1", "Ada Lovelace"), found("p-2", "Adam Smith")],
            5,
        );
        assert_eq!(
            answers.known("Person", "ada l"),
            Some(vec![found("p-1", "Ada Lovelace")])
        );
        assert_eq!(answers.known("Person", "ada lx"), Some(Vec::new()));
        assert_eq!(
            answers.known("Person", "ad").map(|rows| rows.len()),
            Some(2),
            "text typed back to shows its answer again"
        );
        assert_eq!(answers.known("Person", "a"), None, "shorter text asks");
        // A typo corrected: what "ad" held answers "adam" though "ada lx" found nothing.
        assert_eq!(
            answers.known("Person", "adam"),
            Some(vec![found("p-2", "Adam Smith")])
        );
        assert_eq!(answers.known("Company", "ada"), None, "no answer yet");

        // As many rows as the limit: there may be more, so a narrower search still asks.
        answers.keep(
            "Company".into(),
            "a".into(),
            vec![found("c-1", "Acme"), found("c-2", "Ada Corp")],
            2,
        );
        assert_eq!(answers.known("Company", "ada"), None);
    }
}
