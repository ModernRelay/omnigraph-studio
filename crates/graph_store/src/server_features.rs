//! Which Omnigraph server features Studio may use on a connection (spec 003, "Server
//! features").
//!
//! Every server Studio connects to speaks HTTP contract 0.14, and a later release adds to it
//! without changing the header: a field an answer may carry, a parameter a route accepts, a
//! rule a write must meet. Answers are decoded tolerantly whatever the server (`graph_client`
//! reads every later field as optional); this table says what Studio may *ask for* and how it
//! must *save*, worked out once per health answer from the release `/healthz` reports.
//!
//! A feature a later server serves goes in [`FEATURES`] with the first release that serves it,
//! and in `docs/deferred-engine-changes.md` beside that release; code asks
//! [`ServerFeatures::has`], never compares versions itself.

use semver::Version;

/// A server feature Studio uses where the server serves it, and does without otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerFeature {
    /// A read ordered by `nearest`, `bm25` or `rrf()` says how it ranked its rows
    /// (`ReadOutput.ranking`, #1130). Read from the answer; the query editor shows it.
    RankedReadOutput,
    /// `GET /branches?detailed=true` answers each branch's head, start and times in one request
    /// (`BranchListOutput.details`, #1012). Without it each branch's head is its own read.
    DetailedBranchList,
    /// `GET /access` names the OAuth client a signed-in person acts through
    /// (`AccessOutput.client_id`, #1196). Read from the answer.
    AccessClientId,
    /// A `@card` minimum is checked on every write that inserts or updates a source node
    /// (#1212): a new node must be saved with the edges it needs in one mutation, and a node
    /// already below its minimum is refused even a property-only update.
    CardMinimumOnEveryWrite,
    /// A schema apply refuses `@unique` on a list or vector property (#1306), which no write
    /// could satisfy; a graph that already has one opens, but every apply keeping it is refused.
    /// The schema view says which the server does.
    UniqueOnScalarsOnly,
}

/// Every feature, with the first Omnigraph release that serves it. Keep it in step with the
/// "Server features" table in `docs/deferred-engine-changes.md` (a test checks it).
pub const FEATURES: &[(ServerFeature, &str)] = &[
    (ServerFeature::RankedReadOutput, "0.15.0"),
    (ServerFeature::DetailedBranchList, "0.15.0"),
    (ServerFeature::AccessClientId, "0.15.0"),
    (ServerFeature::CardMinimumOnEveryWrite, "0.15.0"),
    (ServerFeature::UniqueOnScalarsOnly, "0.15.0"),
];

/// The oldest release Studio supports, which every server is taken to be at least: one whose
/// version cannot be read is taken to be this.
pub const OLDEST_SUPPORTED: &str = "0.14.0";

/// What a server serves, by its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFeatures {
    /// The release the features were worked out for: the server's, or [`OLDEST_SUPPORTED`]
    /// when its version could not be read.
    release: Version,
    /// The version text `/healthz` reported could be read.
    read: bool,
}

impl Default for ServerFeatures {
    /// A server not yet heard from: the oldest supported release, which asks nothing new of it.
    fn default() -> Self {
        Self::oldest()
    }
}

impl ServerFeatures {
    /// The features of the oldest supported release: none of [`FEATURES`].
    pub fn oldest() -> Self {
        Self {
            release: oldest(),
            read: false,
        }
    }

    /// The features of a server reporting `version` (`HealthOutput.version`).
    ///
    /// The version is read as semver. An edge or dev build (`0.15.1-edge+54ced5d`,
    /// `0.15.0-dev`) has a prerelease, which sorts before its release: Omnigraph names an edge
    /// build after the release it leads to, so `0.15.1-edge` serves everything 0.15.0 does, and a
    /// `0.15.0-edge` build, made before 0.15.0, is not taken to serve 0.15.0's features. Build
    /// metadata is ignored. A version that does not parse, or one older than
    /// [`OLDEST_SUPPORTED`], is taken to be the oldest supported release.
    pub fn of(version: Option<&str>) -> Self {
        let parsed = version
            .map(str::trim)
            .map(|v| v.strip_prefix('v').unwrap_or(v))
            .and_then(|v| Version::parse(v).ok());
        match parsed {
            Some(release) if release >= oldest() => Self {
                release,
                read: true,
            },
            Some(_) | None => Self::oldest(),
        }
    }

    /// Whether the server serves `feature`.
    pub fn has(&self, feature: ServerFeature) -> bool {
        FEATURES
            .iter()
            .find(|(f, _)| *f == feature)
            .is_some_and(|(_, first)| self.release >= release(first))
    }

    /// Whether `other` serves the same features: another release can serve the same ones.
    pub fn serves_same(&self, other: &Self) -> bool {
        FEATURES
            .iter()
            .all(|(feature, _)| self.has(*feature) == other.has(*feature))
    }

    /// The release the features were worked out for.
    pub fn release(&self) -> &Version {
        &self.release
    }

    /// Whether the server's version could be read; when not, it is taken to be
    /// [`OLDEST_SUPPORTED`].
    pub fn version_read(&self) -> bool {
        self.read
    }
}

fn release(text: &str) -> Version {
    Version::parse(text).expect("FEATURES names releases as semver")
}

fn oldest() -> Version {
    release(OLDEST_SUPPORTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_all(features: &ServerFeatures) -> Vec<ServerFeature> {
        FEATURES
            .iter()
            .map(|(f, _)| *f)
            .filter(|f| features.has(*f))
            .collect()
    }

    /// Each release serves the features of its own and earlier releases, none later.
    #[test]
    fn each_release_serves_its_features() {
        let every: Vec<_> = FEATURES.iter().map(|(f, _)| *f).collect();
        assert_eq!(has_all(&ServerFeatures::of(Some("0.14.0"))), []);
        assert_eq!(has_all(&ServerFeatures::of(Some("0.14.3"))), []);
        assert_eq!(has_all(&ServerFeatures::of(Some("0.15.0"))), every);
        assert_eq!(has_all(&ServerFeatures::of(Some("0.15.2"))), every);
        assert_eq!(has_all(&ServerFeatures::of(Some("0.16.0"))), every);
        assert_eq!(has_all(&ServerFeatures::of(Some("1.0.0"))), every);
        assert!(ServerFeatures::of(Some("0.15.0")).version_read());
    }

    /// An edge build serves what the release before it does: `0.15.1-edge` came after 0.15.0,
    /// `0.14.1-edge` before it, and a `0.15.0` prerelease is not 0.15.0.
    #[test]
    fn an_edge_build_serves_the_release_before_it() {
        let every: Vec<_> = FEATURES.iter().map(|(f, _)| *f).collect();
        assert_eq!(
            has_all(&ServerFeatures::of(Some("0.15.1-edge+54ced5d"))),
            every
        );
        assert_eq!(
            has_all(&ServerFeatures::of(Some("0.14.1-edge+7711e4a"))),
            []
        );
        assert_eq!(
            has_all(&ServerFeatures::of(Some("0.15.0-edge+50283ce"))),
            []
        );
        assert_eq!(has_all(&ServerFeatures::of(Some("0.15.0-dev"))), []);
        assert_eq!(has_all(&ServerFeatures::of(Some("v0.15.0"))), every);
    }

    /// A version Studio cannot read, or none, is the oldest supported release: nothing new is
    /// asked of the server.
    #[test]
    fn an_unreadable_version_is_the_oldest_supported() {
        for version in [
            None,
            Some(""),
            Some("unknown"),
            Some("0.15"),
            Some("0.13.2"),
        ] {
            let features = ServerFeatures::of(version);
            assert_eq!(features, ServerFeatures::oldest(), "{version:?}");
            assert_eq!(has_all(&features), [], "{version:?}");
            assert_eq!(features.release().to_string(), OLDEST_SUPPORTED);
        }
        assert!(!ServerFeatures::default().version_read());
        assert!(ServerFeatures::default().serves_same(&ServerFeatures::of(Some("0.14.2"))));
        assert!(!ServerFeatures::default().serves_same(&ServerFeatures::of(Some("0.15.0"))));
    }

    /// Every feature is in the docs' table with its release, so the two cannot drift.
    #[test]
    fn every_feature_is_named_in_the_docs_with_its_release() {
        const DOC: &str = include_str!("../../../docs/deferred-engine-changes.md");
        for (feature, first) in FEATURES {
            let name = format!("`{feature:?}`");
            let row = DOC
                .lines()
                .find(|line| line.starts_with(&format!("| {name} |")))
                .unwrap_or_else(|| panic!("{name} is not in docs/deferred-engine-changes.md"));
            assert!(
                row.contains(&format!("`{first}`")),
                "{name}'s row does not name release {first}: {row}"
            );
        }
        let names: std::collections::HashSet<_> = FEATURES.iter().map(|(f, _)| *f).collect();
        assert_eq!(names.len(), FEATURES.len(), "a feature is listed twice");
    }
}
