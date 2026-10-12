//! Names and ranking for the palette: humanized action names, a fuzzy matcher that prefers word
//! starts and runs of characters, and the order rows appear in (spec 002).

/// `studio_graph::OpenType` → `Graph: open type`; `studio::ToggleSidebar` → `Studio: toggle
/// sidebar`; `editor::SelectAll` → `Editor: select all`. Acronyms stay one word
/// (`editor::OpenURL` → `Editor: open url`).
pub fn humanize_action_name(name: &str) -> String {
    let (namespace, action) = match name.rsplit_once("::") {
        Some((namespace, action)) => (namespace, action),
        None => ("", name),
    };
    let namespace = namespace.replace("::", " ");
    let namespace = namespace
        .strip_prefix("studio_")
        .unwrap_or(&namespace)
        .replace('_', " ");
    let words = split_words(action).join(" ");
    if namespace.is_empty() {
        capitalize(&words)
    } else {
        format!("{}: {words}", capitalize(&namespace))
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Splits `CamelCase`, `snake_case` and `ACRONYMWords` into lowercase words.
fn split_words(action: &str) -> Vec<String> {
    let chars: Vec<char> = action.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == ' ' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if c.is_uppercase() && !current.is_empty() {
            let previous = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if previous.is_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_uppercase() && next_lower)
            {
                words.push(std::mem::take(&mut current));
            }
        }
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Folds what a person types into the shape of humanized names: `studio_graph::open` and
/// `graph: open` both search `Graph: open …`.
pub fn normalize_query(query: &str) -> String {
    let mut normalized = String::new();
    for c in query.trim().chars() {
        let c = if c == '_' { ' ' } else { c };
        if c == ':' && normalized.ends_with(':') {
            continue;
        }
        if c.is_whitespace() && normalized.ends_with(char::is_whitespace) {
            continue;
        }
        if !c.is_whitespace() && c != ':' && normalized.ends_with(':') {
            normalized.push(' ');
        }
        normalized.push(c);
    }
    normalized
        .strip_prefix("studio ")
        .map(str::to_owned)
        .unwrap_or(normalized)
}

/// A fuzzy match of a query against one label: the score (higher is better) and the byte
/// offsets of the label's characters that matched, for highlighting.
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatch {
    pub score: f32,
    pub positions: Vec<usize>,
}

const MATCH: f32 = 1.0;
const WORD_START: f32 = 6.0;
const CONSECUTIVE: f32 = 4.0;
const GAP: f32 = 0.2;

/// Matches `query` as a case-insensitive subsequence of `label`, choosing the alignment with
/// the best score. Spaces in the query are skipped, so `open type` matches `Graph: open type`.
/// `None` when some query character does not appear in order.
pub fn fuzzy_match(query: &str, label: &str) -> Option<FuzzyMatch> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    let label_chars: Vec<char> = label.chars().collect();
    let lower: Vec<char> = label_chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0.0,
            positions: Vec::new(),
        });
    }
    let n = lower.len();
    let m = query.len();
    if m > n {
        return None;
    }
    let word_start = |j: usize| {
        j == 0 || {
            let p = label_chars[j - 1];
            !p.is_alphanumeric() || (p.is_lowercase() && label_chars[j].is_uppercase())
        }
    };
    const NONE: f32 = f32::NEG_INFINITY;
    // score[i][j]: best score with query[i] matched at label[j]; back[i][j]: where query[i-1] is.
    let mut score = vec![vec![NONE; n]; m];
    let mut back = vec![vec![usize::MAX; n]; m];
    for j in 0..n {
        if lower[j] == query[0] {
            score[0][j] =
                MATCH + if word_start(j) { WORD_START } else { 0.0 } - GAP * j as f32 * 0.1;
        }
    }
    for i in 1..m {
        // Best of score[i-1][k] + GAP * k over k <= j - 2, for non-adjacent predecessors.
        let mut best: (f32, usize) = (NONE, usize::MAX);
        for j in 1..n {
            if j >= 2 {
                let k = j - 2;
                let candidate = score[i - 1][k] + GAP * k as f32;
                if candidate > best.0 {
                    best = (candidate, k);
                }
            }
            if lower[j] != query[i] {
                continue;
            }
            let bonus = MATCH + if word_start(j) { WORD_START } else { 0.0 };
            let adjacent = score[i - 1][j - 1] + CONSECUTIVE;
            let apart = best.0 - GAP * (j - 1) as f32;
            if adjacent >= apart && adjacent > NONE {
                score[i][j] = adjacent + bonus;
                back[i][j] = j - 1;
            } else if apart > NONE {
                score[i][j] = apart + bonus;
                back[i][j] = best.1;
            }
        }
    }
    let (mut j, best) = score[m - 1]
        .iter()
        .enumerate()
        .filter(|(_, s)| **s > NONE)
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(j, s)| (j, *s))?;
    let bytes: Vec<usize> = label.char_indices().map(|(at, _)| at).collect();
    let mut positions = vec![0; m];
    for i in (0..m).rev() {
        positions[i] = bytes[j];
        if i > 0 {
            j = back[i][j];
        }
    }
    // Among equal matches, prefer the shorter label.
    Some(FuzzyMatch {
        score: best - n as f32 * 0.01,
        positions,
    })
}

/// Which group a row belongs to; earlier groups come first when scores tie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// Studio's own actions.
    Studio,
    /// Quick-open rows for node and edge types.
    Type,
    /// Actions from Zed's crates, such as the editor's.
    Other,
}

/// The facts ranking needs about one row.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub label: String,
    /// The stable key recency is remembered by: the action name, or `type:<name>`.
    pub key: String,
    pub group: Group,
}

/// A row to show: the candidate's index and the label positions to highlight.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub index: usize,
    pub positions: Vec<usize>,
}

/// Orders candidates for a query. With no query: recently run first (most recent first), then
/// Studio's actions, types and the rest, each alphabetically. With a query: matches only, by
/// score plus a bonus for recent use, then the same tie-breaks.
pub fn rank(candidates: &[Candidate], query: &str, recent: &[String]) -> Vec<Ranked> {
    let query = normalize_query(query);
    let recency = |key: &str| recent.iter().position(|r| r == key);
    let mut rows: Vec<(f32, Option<usize>, usize, Vec<usize>)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let found = fuzzy_match(&query, &candidate.label)?;
            let recent_at = recency(&candidate.key);
            let bonus = recent_at.map_or(0.0, |at| 4.0 * (1.0 - (at.min(20) as f32) / 20.0));
            Some((found.score + bonus, recent_at, index, found.positions))
        })
        .collect();
    rows.sort_by(|a, b| {
        let (ca, cb) = (&candidates[a.2], &candidates[b.2]);
        let by_score = if query.is_empty() {
            std::cmp::Ordering::Equal
        } else {
            b.0.total_cmp(&a.0)
        };
        by_score
            .then_with(|| match (a.1, b.1) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| ca.group.cmp(&cb.group))
            .then_with(|| ca.label.cmp(&cb.label))
    });
    rows.into_iter()
        .map(|(_, _, index, positions)| Ranked { index, positions })
        .collect()
}

/// Puts `key` at the front of the recent list, without duplicates, keeping at most `cap`.
pub fn remember(recent: &mut Vec<String>, key: &str, cap: usize) {
    recent.retain(|r| r != key);
    recent.insert(0, key.to_owned());
    recent.truncate(cap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_are_humanized() {
        assert_eq!(
            humanize_action_name("studio_graph::OpenType"),
            "Graph: open type"
        );
        assert_eq!(
            humanize_action_name("studio_graph::NewQuery"),
            "Graph: new query"
        );
        assert_eq!(
            humanize_action_name("studio::ToggleCommandPalette"),
            "Studio: toggle command palette"
        );
        assert_eq!(humanize_action_name("studio_query::Run"), "Query: run");
        assert_eq!(
            humanize_action_name("editor::SelectAll"),
            "Editor: select all"
        );
        assert_eq!(humanize_action_name("editor::OpenURL"), "Editor: open url");
        assert_eq!(
            humanize_action_name("editor::ConvertToUpperCase"),
            "Editor: convert to upper case"
        );
        assert_eq!(humanize_action_name("git_ui::Commit"), "Git ui: commit");
        assert_eq!(humanize_action_name("Quit"), "Quit");
    }

    #[test]
    fn queries_are_normalized() {
        assert_eq!(normalize_query("studio_graph::open"), "graph: open");
        assert_eq!(normalize_query("  new   query "), "new query");
    }

    #[test]
    fn fuzzy_matches_prefer_word_starts_and_runs() {
        let found = fuzzy_match("nq", "Graph: new query").unwrap();
        assert_eq!(found.positions, vec![7, 11]);
        assert!(fuzzy_match("xyz", "Graph: new query").is_none());
        let run = fuzzy_match("query", "Query: run").unwrap();
        let scattered = fuzzy_match("query", "Graph: quit everything you").unwrap();
        assert!(run.score > scattered.score);
        // Case does not matter and spaces in the query are skipped.
        assert!(fuzzy_match("OPEN type", "Graph: open type").is_some());
        // The best alignment wins over the first one found: `t` and `y` of `type`, not the
        // `t` of `toggle` then the `y` of `type`.
        let found = fuzzy_match("ty", "Studio: toggle type").unwrap();
        assert_eq!(found.positions, vec![15, 16]);
        // Positions are byte offsets, for highlighting.
        let found = fuzzy_match("q", "Café: query").unwrap();
        assert_eq!(found.positions, vec![7]);
    }

    fn candidates() -> Vec<Candidate> {
        vec![
            Candidate {
                label: "Editor: select all".into(),
                key: "editor::SelectAll".into(),
                group: Group::Other,
            },
            Candidate {
                label: "Graph: new query".into(),
                key: "studio_graph::NewQuery".into(),
                group: Group::Studio,
            },
            Candidate {
                label: "Studio: toggle sidebar".into(),
                key: "studio::ToggleSidebar".into(),
                group: Group::Studio,
            },
            Candidate {
                label: "Person".into(),
                key: "type:Person".into(),
                group: Group::Type,
            },
        ]
    }

    fn labels(ranked: &[Ranked], candidates: &[Candidate]) -> Vec<String> {
        ranked
            .iter()
            .map(|r| candidates[r.index].label.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_lists_recent_then_studio_then_types_then_the_rest() {
        let candidates = candidates();
        let recent = vec!["studio::ToggleSidebar".to_owned()];
        assert_eq!(
            labels(&rank(&candidates, "", &recent), &candidates),
            [
                "Studio: toggle sidebar",
                "Graph: new query",
                "Person",
                "Editor: select all"
            ]
        );
    }

    #[test]
    fn a_query_ranks_by_score_with_a_bonus_for_recent_use() {
        let candidates = candidates();
        assert_eq!(
            labels(&rank(&candidates, "se", &[]), &candidates)[0],
            "Editor: select all"
        );
        let recent = vec!["studio::ToggleSidebar".to_owned()];
        let ranked = labels(&rank(&candidates, "s", &recent), &candidates);
        assert_eq!(ranked[0], "Studio: toggle sidebar");
        assert_eq!(
            labels(&rank(&candidates, "pers", &[]), &candidates),
            ["Person"]
        );
    }

    #[test]
    fn recent_keys_move_to_the_front() {
        let mut recent = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        remember(&mut recent, "c", 3);
        assert_eq!(recent, ["c", "a", "b"]);
        remember(&mut recent, "d", 3);
        assert_eq!(recent, ["d", "c", "a"]);
    }
}
