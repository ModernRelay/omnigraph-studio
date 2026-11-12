//! The first run (spec 029): the choices an empty Open dialog offers, and the recipe for a graph
//! on this computer. Studio runs none of the recipe's commands: it shows them to copy, then adds
//! the server they start as an anonymous CLI target and waits for it (constitution I; spec 012
//! E1 and E5). Its text lives here, beside the tests that its files are what the CLI takes.

use std::time::Duration;

/// Where the recipe's server listens, as its last step starts it.
pub(crate) const LOCAL_ADDRESS: &str = "http://127.0.0.1:8787";
/// The CLI target Studio adds for it.
pub(crate) const LOCAL_TARGET: &str = "local";
/// The graph the recipe's `cluster.yaml` declares, opened once the server answers: the server
/// does not list its graphs without a cluster policy (spec 012 E47).
pub(crate) const LOCAL_GRAPH: &str = "knowledge";
/// How often the server is checked while the recipe waits for it, and for how long.
pub(crate) const WAIT_EVERY: Duration = Duration::from_secs(2);
pub(crate) const WAIT_AT_MOST: Duration = Duration::from_secs(10 * 60);

/// The recipe's `cluster.yaml`.
pub(crate) const CLUSTER_YAML: &str = "version: 1
state: { backend: cluster, lock: true }
graphs:
  knowledge:
    schema: schema.pg
";

/// The recipe's schema: one node type, to be changed with `cluster apply` later.
pub(crate) const SCHEMA: &str = "node Person {
  slug: String @key
  name: String
}
";

/// One of the start screen's rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Cloud,
    Server,
    Local,
}

impl Choice {
    pub(crate) const ALL: [Choice; 3] = [Choice::Cloud, Choice::Server, Choice::Local];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Choice::Cloud => "Omnigraph Cloud",
            Choice::Server => "Connect to a server",
            Choice::Local => "Make a local graph",
        }
    }

    pub(crate) fn line(self) -> &'static str {
        match self {
            Choice::Cloud => "Sign in and open a cluster of your organization.",
            Choice::Server => "Add a server by its address.",
            Choice::Local => "Run a graph on this computer with the Omnigraph CLI.",
        }
    }

    /// Whether the choice runs the Omnigraph CLI: a server is added as one of its targets. Cloud
    /// is Studio's own sign-in (spec 021).
    pub(crate) fn needs_cli(self) -> bool {
        !matches!(self, Choice::Cloud)
    }
}

/// One step of the recipe: what it does, and the command to copy.
pub(crate) struct Step {
    pub(crate) says: &'static str,
    pub(crate) command: String,
}

/// The recipe's steps, for this system's shell: a POSIX shell on macOS and Linux, PowerShell on
/// Windows.
pub(crate) fn steps(windows: bool) -> Vec<Step> {
    let (source, data) = if windows {
        (r"$HOME\omnigraph\source", r"$HOME\omnigraph\data")
    } else {
        ("~/omnigraph/source", "~/omnigraph/data")
    };
    let files = if windows {
        format!(
            "New-Item -ItemType Directory -Force {source}, {data} | Out-Null\n\
             Set-Content {source}\\cluster.yaml @'\n{CLUSTER_YAML}'@\n\
             Set-Content {source}\\schema.pg @'\n{SCHEMA}'@"
        )
    } else {
        format!(
            "mkdir -p {source} {data}\n\
             cat > {source}/cluster.yaml <<'EOF'\n{CLUSTER_YAML}EOF\n\
             cat > {source}/schema.pg <<'EOF'\n{SCHEMA}EOF"
        )
    };
    // `cluster apply` keeps its admission lock when it ends, and the server will not start while
    // it is held (spec 012 E45): it is released by the id `cluster status` reports.
    let unlock = if windows {
        format!(
            "omnigraph cluster force-unlock (omnigraph cluster status --target {data} --json | \
             ConvertFrom-Json).lock_id --target {data}"
        )
    } else {
        format!(
            "omnigraph cluster force-unlock \"$(omnigraph cluster status --target {data} --json | \
             sed -n 's/.*\"lock_id\": \"\\(.*\\)\".*/\\1/p')\" --target {data}"
        )
    };
    let bind = LOCAL_ADDRESS.trim_start_matches("http://");
    vec![
        Step {
            says: "Make a folder with the graph's schema, and one for its data.",
            command: files,
        },
        Step {
            says: "Make the graph.",
            command: format!("omnigraph cluster apply --target {data} --config {source}"),
        },
        Step {
            says: "Release the lock the last step keeps.",
            command: unlock,
        },
        Step {
            says: "Start the server, and leave it running.",
            command: format!("omnigraph-server --cluster {data} --bind {bind} --unauthenticated"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recipe's files are what the CLI and the compiler take: the cluster declares the
    /// graph Studio opens, with the schema beside it, and the schema compiles.
    #[test]
    fn the_recipes_files_are_a_cluster_with_one_graph() {
        let cluster: serde_norway::Value = serde_norway::from_str(CLUSTER_YAML).unwrap();
        assert_eq!(cluster["version"], serde_norway::Value::from(1));
        assert_eq!(
            cluster["graphs"][LOCAL_GRAPH]["schema"],
            serde_norway::Value::from("schema.pg")
        );
        let schema = gq_language::SchemaModel::parse(SCHEMA).unwrap();
        assert_eq!(schema.nodes.len(), 1);
    }

    /// Each step's command names the folders and the address Studio waits on, for each shell.
    #[test]
    fn the_steps_start_the_server_studio_waits_for() {
        for windows in [false, true] {
            let steps = steps(windows);
            assert_eq!(steps.len(), 4);
            let last = &steps[3].command;
            assert!(last.contains("--bind 127.0.0.1:8787"), "{last}");
            assert!(last.contains("--unauthenticated"), "{last}");
            assert!(steps[2].command.contains("force-unlock"));
            assert!(steps[0].command.contains("knowledge:"));
        }
        assert!(steps(false)[0].command.contains("<<'EOF'"));
        assert!(steps(true)[0].command.contains("@'"));
    }
}
