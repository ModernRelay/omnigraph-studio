//! `omnigraph-studio://` links (spec 002): a link opens a graph, a node, or a query tab with its
//! text filled in. A link only shows; it never runs a query, writes, adds a server, or carries a
//! token. The server it names must already be one Studio knows.
//!
//! - `omnigraph-studio://graph?server=<url>&graph=<id>[&branch=<name>]`
//! - `omnigraph-studio://node?server=<url>&graph=<id>&type=<Type>&id=<key>[&branch=<name>]`
//! - `omnigraph-studio://query?server=<url>&graph=<id>&text=<gq>[&branch=<name>]`
//! - `omnigraph-studio://open?target=<name>&binding=sha256:<hex>&home=sha256:<hex>[&graph=<id>]`
//!   is what `omnigraph studio` sends (Omnigraph 0.14.1, `protocols.studio` 1): a CLI target
//!   by name, with digests of what it resolves to and of the operator file it was read from,
//!   which Studio checks against its own `target inspect` before it opens the target's server.
//! - `omnigraph-studio://connect?code=<code>` was reserved for that launcher before it existed,
//!   and is still refused with a note.

use url::Url;

pub const SCHEME: &str = "omnigraph-studio";

/// The longest query text a link may fill in; a longer one is refused rather than cut.
pub const QUERY_TEXT_MAX: usize = 64 * 1024;

/// The longest link read at all: the longest query text, each of its bytes percent-encoded as
/// three, and room for the rest. A longer one is refused before it is parsed, whose cost grows
/// with it.
const LINK_MAX: usize = QUERY_TEXT_MAX * 3 + 4 * 1024;

/// Where a link points: a server Studio knows, a graph on it, and optionally a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub server: String,
    pub graph: String,
    pub branch: Option<String>,
}

/// What `omnigraph studio` sends: a CLI target by name, the digests the CLI reports for it
/// (`target inspect NAME --json`: `binding`, of what the target resolves to; `home`, of the
/// operator file), and the graph asked for, when one was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliLink {
    pub target: String,
    pub binding: String,
    pub home: String,
    pub graph: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    Graph(Target),
    Node {
        target: Target,
        type_name: String,
        id: String,
    },
    Query {
        target: Target,
        text: String,
    },
    /// A link from `omnigraph studio`, opened once its digests match this Mac's.
    Open(CliLink),
    /// The launcher's first shape, from before it existed; refused with a note.
    Connect,
}

impl DeepLink {
    pub fn target(&self) -> Option<&Target> {
        match self {
            DeepLink::Graph(target)
            | DeepLink::Node { target, .. }
            | DeepLink::Query { target, .. } => Some(target),
            DeepLink::Open(_) | DeepLink::Connect => None,
        }
    }
}

/// Whether an `open` link's digests are what the CLI here reports for its target (`binding`
/// and `home`, from `target inspect`); when not, which differs, and what that means.
pub fn verify(link: &CliLink, binding: &str, home: &str) -> Result<(), String> {
    if link.binding != binding {
        return Err(format!(
            "The link's target {} is not this Mac's: its binding digest differs, so the name \
             resolves to another server or credential here. Make the link again with \
             `omnigraph studio --target {0}` on this Mac.",
            link.target
        ));
    }
    if link.home != home {
        return Err(format!(
            "The link was made from another operator file than this Mac's CLI reads: its home \
             digest differs. Make the link again with `omnigraph studio --target {}` here.",
            link.target
        ));
    }
    Ok(())
}

/// Reads a link, or says what is wrong with it.
pub fn parse(link: &str) -> Result<DeepLink, String> {
    if link.len() > LINK_MAX {
        return Err(format!(
            "The link is longer than {} KB; open its query from a file instead.",
            LINK_MAX / 1024
        ));
    }
    let url = Url::parse(link.trim()).map_err(|_| "This is not a Studio link.".to_owned())?;
    if url.scheme() != SCHEME {
        return Err(format!("Studio opens {SCHEME}:// links only."));
    }
    let route = url.host_str().unwrap_or_default().to_owned();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.trim().is_empty())
    };
    let need = |key: &str| get(key).ok_or_else(|| format!("The link has no {key}."));
    if route == "connect" {
        return Ok(DeepLink::Connect);
    }
    if route == "open" {
        let digest = |key: &str| -> Result<String, String> {
            let value = need(key)?;
            let hex = value.strip_prefix("sha256:").unwrap_or_default();
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("The link's {key} is not a sha256 digest."));
            }
            Ok(value)
        };
        return Ok(DeepLink::Open(CliLink {
            target: need("target")?,
            binding: digest("binding")?,
            home: digest("home")?,
            graph: get("graph"),
        }));
    }
    let target = || -> Result<Target, String> {
        Ok(Target {
            server: need("server")?,
            graph: need("graph")?,
            branch: get("branch"),
        })
    };
    match route.as_str() {
        "graph" => Ok(DeepLink::Graph(target()?)),
        "node" => Ok(DeepLink::Node {
            target: target()?,
            type_name: need("type")?,
            id: need("id")?,
        }),
        "query" => {
            let text = need("text")?;
            if text.len() > QUERY_TEXT_MAX {
                return Err(format!(
                    "The link's query is longer than {} KB; open it from a file instead.",
                    QUERY_TEXT_MAX / 1024
                ));
            }
            Ok(DeepLink::Query {
                target: target()?,
                text,
            })
        }
        "" => Err("The link names nothing to open.".into()),
        other => Err(format!("Studio cannot open “{other}” links.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            server: "http://127.0.0.1:8787".into(),
            graph: "knowledge".into(),
            branch: None,
        }
    }

    #[test]
    fn links_open_a_graph_a_node_or_a_query() {
        assert_eq!(
            parse("omnigraph-studio://graph?server=http%3A%2F%2F127.0.0.1%3A8787&graph=knowledge"),
            Ok(DeepLink::Graph(target()))
        );
        assert_eq!(
            parse(
                "omnigraph-studio://node?server=http://127.0.0.1:8787&graph=knowledge\
                 &type=Person&id=person-000001&branch=review"
            ),
            Ok(DeepLink::Node {
                target: Target {
                    branch: Some("review".into()),
                    ..target()
                },
                type_name: "Person".into(),
                id: "person-000001".into(),
            })
        );
        assert_eq!(
            parse(
                "omnigraph-studio://query?server=http://127.0.0.1:8787&graph=knowledge\
                 &text=query%20q()%20%7B%7D"
            ),
            Ok(DeepLink::Query {
                target: target(),
                text: "query q() {}".into(),
            })
        );
        assert_eq!(
            parse("omnigraph-studio://connect?code=abc"),
            Ok(DeepLink::Connect)
        );
    }

    /// `omnigraph studio`'s link names a target and its two digests, and a graph when asked;
    /// a digest that is not one, or a part missing, is refused with the reason.
    #[test]
    fn an_open_link_names_a_target_and_its_digests() {
        let hex = "a".repeat(64);
        assert_eq!(
            parse(&format!(
                "omnigraph-studio://open?target=law&binding=sha256:{hex}&home=sha256:{hex}&graph=law"
            )),
            Ok(DeepLink::Open(CliLink {
                target: "law".into(),
                binding: format!("sha256:{hex}"),
                home: format!("sha256:{hex}"),
                graph: Some("law".into()),
            }))
        );
        assert!(matches!(
            parse(&format!(
                "omnigraph-studio://open?target=law&binding=sha256:{hex}&home=sha256:{hex}"
            )),
            Ok(DeepLink::Open(CliLink { graph: None, .. }))
        ));
        assert_eq!(
            parse(&format!(
                "omnigraph-studio://open?target=law&binding=sha256:{hex}"
            )),
            Err("The link has no home.".into())
        );
        assert_eq!(
            parse(&format!(
                "omnigraph-studio://open?binding=sha256:{hex}&home=sha256:{hex}"
            )),
            Err("The link has no target.".into())
        );
        assert_eq!(
            parse(&format!(
                "omnigraph-studio://open?target=law&binding=md5:abc&home=sha256:{hex}"
            )),
            Err("The link's binding is not a sha256 digest.".into())
        );
    }

    /// A link's digests must both be the CLI's; the refusal names the one that differs.
    #[test]
    fn an_open_link_is_verified_against_the_cli_digests() {
        let link = CliLink {
            target: "law".into(),
            binding: "sha256:b".into(),
            home: "sha256:h".into(),
            graph: None,
        };
        assert_eq!(verify(&link, "sha256:b", "sha256:h"), Ok(()));
        assert!(
            verify(&link, "sha256:x", "sha256:h")
                .unwrap_err()
                .contains("binding digest differs")
        );
        assert!(
            verify(&link, "sha256:b", "sha256:x")
                .unwrap_err()
                .contains("home digest differs")
        );
    }

    #[test]
    fn a_link_missing_a_part_or_of_another_kind_is_refused_with_the_reason() {
        assert_eq!(
            parse("omnigraph-studio://graph?server=http://x"),
            Err("The link has no graph.".into())
        );
        assert_eq!(
            parse("omnigraph-studio://node?server=http://x&graph=g&type=Person"),
            Err("The link has no id.".into())
        );
        assert_eq!(
            parse("https://example.com/graph"),
            Err("Studio opens omnigraph-studio:// links only.".into())
        );
        assert_eq!(
            parse("omnigraph-studio://run?server=http://x&graph=g"),
            Err("Studio cannot open “run” links.".into())
        );
        let long = format!(
            "omnigraph-studio://query?server=http://x&graph=g&text={}",
            "a".repeat(QUERY_TEXT_MAX + 1)
        );
        assert!(parse(&long).unwrap_err().contains("longer than"));
        // The longest text, every byte encoded, still reads; a link past that is refused unread.
        let encoded = format!(
            "omnigraph-studio://query?server=http://x&graph=g&text={}",
            "%41".repeat(QUERY_TEXT_MAX)
        );
        assert!(matches!(parse(&encoded), Ok(DeepLink::Query { .. })));
        let huge = format!("{encoded}{}", "a".repeat(LINK_MAX));
        assert!(
            parse(&huge)
                .unwrap_err()
                .starts_with("The link is longer than")
        );
    }
}
