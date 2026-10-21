//! The commits History draws (spec 009).
//!
//! OmniGraph's `GET /commits?branch=` lists one branch's first-parent chain, newest first, a
//! page at a time: a merged branch's commits appear only as a `merged_parent_commit_id`.
//! Without a lineage route, History reads each merged-in commit alone by its id
//! (`GET /commits/{id}`), then the parents that commit names, down the merged branch until its
//! chain meets a commit History holds, so merges draw as lanes.
//!
//! By id rather than from the other branches' listings: a listing starts at its branch's head
//! now, so finding a merged commit there means paging down past every commit made since the
//! merge, on every branch that might hold it, and a deleted branch has no listing at all. A
//! commit read by its id costs that one commit, whichever branch made it, and only the commits
//! the rows drawn name are read.
//!
//! The walk goes only as far as the rows read: a merged-in commit older than the oldest of the
//! branch's own commits read would sort below them, so it is held back, its parents unread,
//! until the page that reaches its time is read. Once their merged-in commits are read, the rows
//! drawn are the newest of the whole history, and a page read adds rows below them. A merged
//! parent not read yet, one the server does not have (a deleted branch's commit, cleaned up),
//! or, for a token refused commits by id, one the other branches' pages read do not hold
//! (perhaps on a branch whose page could not be read), draws as a stub, which says which
//! ([`Absent`]).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use graph_client::api::CommitOutput;

/// What History has read of its branch's history: the pages of its own chain, and the commits
/// merged into it, each read by id.
#[derive(Debug, Clone, Default)]
pub struct HistoryRead {
    /// The branch's first-parent chain as far as its pages were read, newest first.
    pub own: Vec<CommitOutput>,
    /// The listing has no page after the last read: every commit of the chain is in `own`.
    pub complete: bool,
    /// Merged-in commits read by id. A commit never changes, so they are kept when the first
    /// page is read again, for as long as a commit read reaches them.
    merged: HashMap<String, CommitOutput>,
    /// Merged-in commits not had, and why: drawn as stubs, and not asked for again while a
    /// commit read names them.
    missing: HashMap<String, Absent>,
}

/// Why a merged-in commit History asked for is not drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    /// The server does not have it: a deleted branch's latest commits, or history cleaned up.
    NotHad,
    /// The other branches' pages read for a token refused commits by id do not hold it.
    NotListed,
    /// Those pages do not hold it, and some branch's page could not be read: it may be there,
    /// so it is answered again once that page is read.
    Unlisted,
}

/// What History draws from what it has read ([`HistoryRead::draw`]).
#[derive(Debug, Default)]
pub struct Drawn {
    /// The branch's own commits and the merged-in commits they reach, newest first.
    pub commits: Vec<CommitOutput>,
    /// The merged-in commits the commits drawn name and History has not read, in the order of
    /// the newest commit naming each: read next, the top rows' first.
    pub wanted: Vec<String>,
}

impl HistoryRead {
    /// Takes the listing's first page (`page`; the last when `complete`). With `keep`, a page
    /// that reaches the head the pages held were read from puts the commits above that head on
    /// top of them and keeps them, so they go on continuing their listing: commits landing on
    /// the branch cost one page, and the rows read stay. Returns whether they were kept;
    /// otherwise (a Refresh, a page holding the whole history, more commits landed than a page
    /// holds, or a branch made again) the page replaces them.
    pub fn first_page(&mut self, page: Vec<CommitOutput>, complete: bool, keep: bool) -> bool {
        let reached = keep && !complete && !self.own.is_empty();
        let at = reached
            .then(|| {
                let head = &self.own[0].graph_commit_id;
                page.iter().position(|c| &c.graph_commit_id == head)
            })
            .flatten();
        match at {
            Some(at) => {
                let held = std::mem::take(&mut self.own);
                self.own = page;
                self.own.truncate(at);
                self.own.extend(held);
                true
            }
            None => {
                self.own = page;
                self.complete = complete;
                false
            }
        }
    }

    /// Takes the listing's next page, continuing the pages held (the last when `complete`).
    pub fn next_page(&mut self, page: Vec<CommitOutput>, complete: bool) {
        self.own.extend(page);
        self.complete = complete;
    }

    /// Takes the merged-in commit read for `id`, or `None` when the server does not have it.
    pub fn merged_read(&mut self, id: String, commit: Option<CommitOutput>) {
        match commit {
            Some(commit) => {
                // An answer for another id leaves the one asked for unread for good.
                if commit.graph_commit_id != id {
                    self.missing.insert(id, Absent::NotHad);
                }
                self.merged.insert(commit.graph_commit_id.clone(), commit);
            }
            None => {
                self.missing.insert(id, Absent::NotHad);
            }
        }
    }

    /// Forgets which merged-in commits the server did not have, so they are asked for again; the
    /// commits read are kept, since they never change.
    pub fn forget_missing(&mut self) {
        self.missing.clear();
    }

    /// Forgets which merged-in commits a listing that could not be read left unanswered
    /// ([`Absent::Unlisted`]), so they are asked for again.
    pub fn forget_unlisted(&mut self) {
        self.missing.retain(|_, absent| *absent != Absent::Unlisted);
    }

    /// Answers the merged-in commits wanted from `listed`, commits by id from the other
    /// branches' listings, then those they name in turn, as far as the pages read reach. An id
    /// `listed` does not hold draws as a stub: not listed when every branch's page was read
    /// (`complete`), else perhaps on a branch whose page was not, and answered again when
    /// `listed` holds more.
    pub fn take_from(&mut self, listed: &HashMap<String, CommitOutput>, complete: bool) {
        self.forget_unlisted();
        let absent = if complete {
            Absent::NotListed
        } else {
            Absent::Unlisted
        };
        // Each pass answers every id wanted, so the passes end.
        loop {
            let wanted = self.draw().wanted;
            if wanted.is_empty() {
                return;
            }
            for id in wanted {
                match listed.get(&id) {
                    Some(commit) => self.merged_read(id, Some(commit.clone())),
                    None => {
                        self.missing.insert(id, absent);
                    }
                }
            }
        }
    }

    /// Why merged-in commit `id` is not drawn, when it was asked for and not had.
    pub fn absent(&self, id: &str) -> Option<Absent> {
        self.missing.get(id).copied()
    }

    /// Whether `commit`, merged in, sorts among the rows read: one below the oldest own commit
    /// read waits for the page that reaches its time.
    pub fn shows(&self, commit: &CommitOutput) -> bool {
        self.complete
            || self
                .own
                .last()
                .is_none_or(|bound| newest_first(commit, bound) == Ordering::Less)
    }

    /// The commits to draw and the merged-in commits to read next. Merged-in commits no commit
    /// read reaches any more (the first page read again, without the pages that named them)
    /// are let go, so what is kept is bounded by what is drawn.
    pub fn draw(&mut self) -> Drawn {
        let mut seen: HashSet<&str> = self
            .own
            .iter()
            .map(|c| c.graph_commit_id.as_str())
            .collect();
        let mut stack: Vec<&str> = self
            .own
            .iter()
            .rev()
            .filter_map(|c| c.merged_parent_commit_id.as_deref())
            .collect();
        let mut extra: Vec<&CommitOutput> = Vec::new();
        let mut reached: HashSet<String> = HashSet::new();
        let mut named_missing: HashMap<String, Absent> = HashMap::new();
        let mut unread: HashSet<&str> = HashSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            match self.merged.get(id) {
                Some(commit) => {
                    reached.insert(id.to_owned());
                    // Held back: its parents are older still, and wait with it.
                    if self.shows(commit) {
                        extra.push(commit);
                        stack.extend(commit.merged_parent_commit_id.as_deref());
                        stack.extend(commit.parent_commit_id.as_deref());
                    }
                }
                None => match self.missing.get(id) {
                    Some(absent) => {
                        named_missing.insert(id.to_owned(), *absent);
                    }
                    None => {
                        unread.insert(id);
                    }
                },
            }
        }
        let mut commits: Vec<CommitOutput> = self.own.clone();
        commits.extend(extra.into_iter().cloned());
        commits.sort_by(newest_first);
        let mut wanted = Vec::new();
        let mut asked: HashSet<&str> = HashSet::new();
        for commit in &commits {
            for parent in commit
                .parent_commit_id
                .iter()
                .chain(&commit.merged_parent_commit_id)
            {
                let parent = parent.as_str();
                if unread.contains(parent) && asked.insert(parent) {
                    wanted.push(parent.to_owned());
                }
            }
        }
        self.merged.retain(|id, _| reached.contains(id));
        self.missing = named_missing;
        Drawn { commits, wanted }
    }

    /// How many merged-in commits are held, drawn or held back.
    #[cfg(test)]
    pub fn merged_held(&self) -> usize {
        self.merged.len()
    }
}

/// Newest first by creation time, then manifest version, then id: commits of several chains
/// in one order, each chain's parents after their children.
fn newest_first(a: &CommitOutput, b: &CommitOutput) -> Ordering {
    b.created_at
        .cmp(&a.created_at)
        .then(b.graph_manifest_version.cmp(&a.graph_manifest_version))
        .then(b.graph_commit_id.cmp(&a.graph_commit_id))
}

/// Branch names by the commit each points at, from each branch's head.
pub fn branch_heads<'a>(
    heads: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> HashMap<String, Vec<String>> {
    let mut by_commit: HashMap<String, Vec<String>> = HashMap::new();
    for (branch, head) in heads {
        by_commit
            .entry(head.to_owned())
            .or_default()
            .push(branch.to_owned());
    }
    // The names of the branches at one commit: main first, then the rest by name.
    for names in by_commit.values_mut() {
        names.sort_by(|a, b| (a != "main").cmp(&(b != "main")).then(a.cmp(b)));
    }
    by_commit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, at: i64, parent: Option<&str>, merged: Option<&str>) -> CommitOutput {
        CommitOutput {
            graph_commit_id: id.into(),
            graph_manifest_version: 0,
            created_at: at,
            graph_branch: None,
            parent_commit_id: parent.map(Into::into),
            merged_parent_commit_id: merged.map(Into::into),
            actor_id: None,
            kind: graph_client::api::CommitKind::Published,
        }
    }

    fn ids(drawn: &Drawn) -> Vec<&str> {
        drawn
            .commits
            .iter()
            .map(|c| c.graph_commit_id.as_str())
            .collect()
    }

    /// Every commit the merged parents reach joins the history, read one at a time as each
    /// names the next; commits of the merged branch after the merge never do.
    #[test]
    fn merged_branches_join_the_history_as_their_commits_are_read() {
        let feature = [
            commit("b3", 35, Some("b2"), None),
            commit("b2", 25, Some("b1"), None),
            commit("b1", 15, Some("m1"), None),
        ];
        let mut read = HistoryRead::default();
        read.first_page(
            vec![
                commit("m3", 30, Some("m2"), Some("b2")),
                commit("m2", 20, Some("m1"), None),
                commit("m1", 10, None, None),
            ],
            true,
            false,
        );
        let drawn = read.draw();
        assert_eq!(ids(&drawn), ["m3", "m2", "m1"]);
        assert_eq!(drawn.wanted, ["b2"]);
        read.merged_read("b2".into(), Some(feature[1].clone()));
        assert_eq!(read.draw().wanted, ["b1"]);
        read.merged_read("b1".into(), Some(feature[2].clone()));
        let drawn = read.draw();
        // b3 came after the merge: nothing names it.
        assert_eq!(ids(&drawn), ["m3", "b2", "m2", "b1", "m1"]);
        assert!(drawn.wanted.is_empty());
    }

    #[test]
    fn a_merge_from_a_commit_the_server_does_not_have_stays_a_stub() {
        let mut read = HistoryRead::default();
        read.first_page(
            vec![
                commit("m2", 20, Some("m1"), Some("gone")),
                commit("m1", 10, None, None),
            ],
            true,
            false,
        );
        assert_eq!(read.draw().wanted, ["gone"]);
        read.merged_read("gone".into(), None);
        let drawn = read.draw();
        assert_eq!(drawn.commits.len(), 2);
        assert!(drawn.wanted.is_empty(), "asked for again");
        assert_eq!(read.absent("gone"), Some(Absent::NotHad));
    }

    /// A merged-in commit older than the oldest own commit read waits for the page that
    /// reaches its time, and its parents with it; the next page draws it.
    #[test]
    fn merged_commits_below_the_pages_read_wait_for_the_next() {
        let mut read = HistoryRead::default();
        read.first_page(
            vec![
                commit("m4", 40, Some("m3"), Some("x2")),
                commit("m3", 30, Some("m2"), None),
            ],
            false,
            false,
        );
        assert_eq!(read.draw().wanted, ["x2"]);
        read.merged_read("x2".into(), Some(commit("x2", 35, Some("x1"), None)));
        assert_eq!(read.draw().wanted, ["x1"]);
        read.merged_read("x1".into(), Some(commit("x1", 25, Some("x0"), None)));
        let drawn = read.draw();
        assert_eq!(ids(&drawn), ["m4", "x2", "m3"]);
        assert!(
            drawn.wanted.is_empty(),
            "x1's parent is read before x1 shows"
        );
        assert_eq!(read.merged_held(), 2, "x1 is kept, held back");
        read.next_page(
            vec![
                commit("m2", 20, Some("m1"), None),
                commit("m1", 10, None, None),
            ],
            true,
        );
        let drawn = read.draw();
        assert_eq!(ids(&drawn), ["m4", "x2", "m3", "x1", "m2", "m1"]);
        assert_eq!(drawn.wanted, ["x0"]);
    }

    /// The first page read again after commits landed goes on top of the pages held when it
    /// reaches their head; a Refresh, or a page that does not reach it, replaces them. Merged-in
    /// commits no commit kept reaches are let go.
    #[test]
    fn a_first_page_read_again_keeps_the_pages_below_its_head() {
        let mut read = HistoryRead::default();
        read.first_page(
            vec![
                commit("m3", 30, Some("m2"), None),
                commit("m2", 20, Some("m1"), Some("x1")),
            ],
            false,
            false,
        );
        read.next_page(vec![commit("m1", 10, None, None)], true);
        read.merged_read("x1".into(), Some(commit("x1", 15, None, None)));
        assert_eq!(read.draw().commits.len(), 4);

        let landed = vec![
            commit("m4", 40, Some("m3"), None),
            commit("m3", 30, Some("m2"), None),
        ];
        assert!(read.first_page(landed.clone(), false, true));
        assert!(read.complete, "the pages held were the whole chain");
        assert_eq!(ids(&read.draw()), ["m4", "m3", "m2", "x1", "m1"]);

        assert!(!read.first_page(landed, false, false), "a Refresh");
        assert!(!read.complete);
        let drawn = read.draw();
        assert_eq!(ids(&drawn), ["m4", "m3"]);
        assert_eq!(read.merged_held(), 0, "x1 let go");
    }

    /// From the other branches' listings, a merge's chain is answered whole in one go, as far as
    /// the pages read; an id no listing holds stays a stub.
    #[test]
    fn listed_commits_answer_a_merged_chain_at_once() {
        let mut read = HistoryRead::default();
        read.first_page(
            vec![
                commit("m4", 40, Some("m3"), Some("b3")),
                commit("m3", 30, Some("m2"), Some("gone")),
                commit("m2", 20, Some("m1"), None),
            ],
            false,
            false,
        );
        let listed: HashMap<String, CommitOutput> = [
            commit("b3", 35, Some("b2"), None),
            commit("b2", 25, Some("b1"), None),
            commit("b1", 15, Some("m1"), None),
        ]
        .into_iter()
        .map(|c| (c.graph_commit_id.clone(), c))
        .collect();
        read.take_from(&listed, true);
        let drawn = read.draw();
        assert_eq!(ids(&drawn), ["m4", "b3", "m3", "b2", "m2"]);
        assert!(drawn.wanted.is_empty());
        assert_eq!(read.merged_held(), 3, "b1 is kept, held back");
        assert_eq!(read.absent("gone"), Some(Absent::NotListed), "says why");
        assert_eq!(read.absent("b3"), None);
    }

    /// While some branch's page could not be read, an id the pages read do not hold may be on
    /// that branch: it says so, and is answered again once that page is read, there or not.
    #[test]
    fn a_commit_a_page_not_read_may_hold_is_answered_once_it_is_read() {
        let mut read = HistoryRead::default();
        read.first_page(
            vec![
                commit("m3", 30, Some("m2"), Some("b2")),
                commit("m2", 20, Some("m1"), Some("gone")),
                commit("m1", 10, None, None),
            ],
            true,
            false,
        );
        read.take_from(&HashMap::new(), false);
        assert_eq!(ids(&read.draw()), ["m3", "m2", "m1"]);
        assert_eq!(read.absent("b2"), Some(Absent::Unlisted));
        assert_eq!(read.absent("gone"), Some(Absent::Unlisted));
        // The page read now holds b2 and the commit it names.
        let listed: HashMap<String, CommitOutput> = [
            commit("b2", 25, Some("b1"), None),
            commit("b1", 15, Some("m1"), None),
        ]
        .into_iter()
        .map(|c| (c.graph_commit_id.clone(), c))
        .collect();
        read.take_from(&listed, true);
        let drawn = read.draw();
        assert_eq!(ids(&drawn), ["m3", "b2", "m2", "b1", "m1"]);
        assert!(drawn.wanted.is_empty());
        assert_eq!(read.absent("b2"), None);
        assert_eq!(read.absent("gone"), Some(Absent::NotListed));
        // A load forgets what a page not read left unanswered, and nothing else: that commit is
        // wanted again.
        let mut read = HistoryRead::default();
        read.first_page(
            vec![
                commit("m2", 20, Some("m1"), Some("x1")),
                commit("m1", 10, None, Some("gone")),
            ],
            true,
            false,
        );
        read.merged_read("gone".into(), None);
        read.take_from(&HashMap::new(), false);
        assert_eq!(read.absent("x1"), Some(Absent::Unlisted));
        read.forget_unlisted();
        assert_eq!(read.absent("x1"), None);
        assert_eq!(read.absent("gone"), Some(Absent::NotHad));
        assert_eq!(read.draw().wanted, ["x1"]);
    }

    #[test]
    fn heads_list_main_first_then_by_name() {
        let heads = branch_heads([
            ("zeta", "c2"),
            ("main", "c2"),
            ("alpha", "c2"),
            ("fix", "c1"),
        ]);
        assert_eq!(heads["c2"], ["main", "alpha", "zeta"]);
        assert_eq!(heads["c1"], ["fix"]);
    }
}
