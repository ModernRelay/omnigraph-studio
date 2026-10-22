//! Omnigraph's environment variables, which no process Studio starts for the agent inherits
//! (spec 013): neither the agent nor the CLI that `omnigraph_run` starts. Every variable whose
//! name starts with `OMNIGRAPH_` is one: the CLI's tokens and addressing, tokens kept for other
//! servers (`OMNIGRAPH_MR_BEARER_TOKEN`) and a local server's token list alike. Matching the
//! prefix rather than a list of names removes a variable Omnigraph adds later too.

/// What the name of every Omnigraph variable starts with.
pub(crate) const PREFIX: &str = "OMNIGRAPH_";

/// Whether `name` is one of Omnigraph's variables: it starts with [`PREFIX`], in any ASCII case
/// when `fold_case`, as Windows reads names (there `omnigraph_profile` is the profile the CLI
/// reads).
pub(crate) fn is_omnigraph_variable(name: &str, fold_case: bool) -> bool {
    name.get(..PREFIX.len()).is_some_and(|start| {
        if fold_case {
            start.eq_ignore_ascii_case(PREFIX)
        } else {
            start == PREFIX
        }
    })
}

/// The Omnigraph variables among `names` that a process Studio starts for the agent must not
/// inherit, sorted: every one but those `kept` names, which the person set for the agent
/// themselves (matched in any ASCII case when `fold_case`). A name that is not Unicode is left:
/// it cannot be named on a command line here, and no program reads it by an Omnigraph name,
/// which is.
pub(crate) fn removed(
    names: impl IntoIterator<Item = std::ffi::OsString>,
    kept: &[&str],
    fold_case: bool,
) -> Vec<String> {
    let same = |a: &str, b: &str| {
        if fold_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let mut found: Vec<String> = names
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| is_omnigraph_variable(name, fold_case))
        .filter(|name| !kept.iter().any(|kept| same(kept, name)))
        .collect();
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prefix alone decides, whole and at the start of the name; Windows matches it in any
    /// ASCII case, and only there.
    #[test]
    fn every_name_with_omnigraphs_prefix_is_omnigraphs() {
        for name in [
            "OMNIGRAPH_TOKEN_PERSONAL",
            "OMNIGRAPH_BEARER_TOKEN",
            "OMNIGRAPH_CONTROL_TOKEN",
            "OMNIGRAPH_A_VARIABLE_ADDED_LATER",
            "OMNIGRAPH_",
        ] {
            assert!(is_omnigraph_variable(name, false), "{name}");
            assert!(is_omnigraph_variable(name, true), "{name}");
        }
        for name in ["omnigraph_profile", "Omnigraph_Token_Work"] {
            assert!(is_omnigraph_variable(name, true), "{name}");
            assert!(!is_omnigraph_variable(name, false), "{name}");
        }
        for name in [
            "OMNIGRAPH",
            "OMNIGRAPHX",
            "XOMNIGRAPH_TOKEN",
            "MY_OMNIGRAPH_TOKEN",
            "OMNIGRAPHé_",
            "",
            "PATH",
        ] {
            assert!(!is_omnigraph_variable(name, true), "{name}");
        }
    }

    #[test]
    fn what_is_removed_is_every_omnigraph_variable_but_those_kept() {
        let names = [
            "PATH",
            "OMNIGRAPH_TOKEN_PERSONAL",
            "OMNIGRAPH_HOME",
            "AWS_PROFILE",
            "OMNIGRAPH_PROFILE",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            removed(names.clone(), &["OMNIGRAPH_HOME"], false),
            ["OMNIGRAPH_PROFILE", "OMNIGRAPH_TOKEN_PERSONAL"]
        );
        assert_eq!(
            removed(names, &[], false),
            [
                "OMNIGRAPH_HOME",
                "OMNIGRAPH_PROFILE",
                "OMNIGRAPH_TOKEN_PERSONAL"
            ]
        );
        let names = ["omnigraph_home", "Omnigraph_Token_Work"].map(std::ffi::OsString::from);
        assert_eq!(
            removed(names, &["OMNIGRAPH_HOME"], true),
            ["Omnigraph_Token_Work"]
        );
    }
}
