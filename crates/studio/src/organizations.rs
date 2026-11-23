//! The control planes' organizations and clusters in Studio's words (spec 021): Omnigraph
//! Cloud, and any other a CLI profile names. One sign-in each serves every organization the
//! person is a member of. What a cluster's row shows, what a plane's sign-in and a failed read
//! say, and the CLI commands offered beside them, which the Open dialog's control planes
//! (`open_graph::planes`) and the Cloud settings pages (`cloud_settings`) both show. Nothing here
//! draws either surface.

use control_client::{ClusterView, ControlError};
use gpui::Entity;
use graph_store::cli_config::ManagedTarget;
use graph_store::sign_in::SignIn;
use graph_store::{ControlPlane, Listing};
use ui::prelude::*;

/// ↵ on a control plane's line: signs in, cancels a sign-in under way, or reads again.
pub(crate) fn enter(plane: &mut ControlPlane, cx: &mut gpui::Context<ControlPlane>) {
    match plane.sign_in_state() {
        SignIn::SignedIn { .. } => plane.read_clusters(cx),
        SignIn::Waiting { .. } | SignIn::Reading => plane.cancel_sign_in(cx),
        SignIn::Finishing { .. } => {}
        // Reads the sign-in details again: what failed may have passed (spec 029).
        SignIn::Unavailable(_) => plane.sign_in(cx),
        SignIn::Offered { .. } | SignIn::NotOffered => plane.sign_in(cx),
    }
}

/// The cluster `cluster_id` of `organization`'s list on the plane, while it is shown.
pub(crate) fn cluster(
    plane: &Entity<ControlPlane>,
    organization: &str,
    cluster_id: &str,
    cx: &App,
) -> Option<ClusterView> {
    match plane.read(cx).listing(organization) {
        Some(Listing::Listed(listed)) => listed
            .data
            .clusters
            .iter()
            .find(|cluster| cluster.cluster_id == cluster_id)
            .cloned(),
        _ => None,
    }
}

/// How a cluster of the list is shown (spec 021, Listing clusters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClusterRow {
    pub name: String,
    pub meta: String,
    /// Shown dimmed: it has no data endpoint, so there is nothing to open.
    pub dimmed: bool,
    /// Clicking it opens the cluster from its control plane: no CLI target names it, and it has
    /// a data endpoint.
    pub opens: bool,
    /// The CLI target that names it, which clicking it chooses.
    pub target: Option<String>,
}

/// The CLI command that makes `cluster` a target of `profile`, for using it from the CLI too
/// (constitution VI). Studio needs none to open it or to reopen it.
pub(crate) fn add_command(cluster: &ClusterView, profile: Option<&str>) -> String {
    let command = format!(
        "omnigraph target add {} --cluster {}",
        cluster.name, cluster.cluster_id
    );
    match profile {
        Some(profile) => format!("{command} --profile {profile}"),
        // The CLI's default profile.
        None => command,
    }
}

/// The row for `cluster`, given the CLI's managed targets.
pub(crate) fn cluster_row(cluster: &ClusterView, managed: &[ManagedTarget]) -> ClusterRow {
    let target = managed.iter().find(|target| {
        target.cluster_id == cluster.cluster_id && target.cluster_incarnation == cluster.incarnation
    });
    let state = state_label(&cluster.state);
    match (target, &cluster.data_endpoint) {
        (Some(target), _) => ClusterRow {
            name: cluster.name.clone(),
            meta: format!("CLI target {} · {state}", target.name),
            dimmed: cluster.data_endpoint.is_none(),
            opens: false,
            target: Some(target.name.clone()),
        },
        (None, Some(_)) => ClusterRow {
            name: cluster.name.clone(),
            // A CLI target is a label a cluster may have; one without it needs no remark.
            meta: state.to_owned(),
            dimmed: false,
            opens: true,
            target: None,
        },
        (None, None) => ClusterRow {
            name: cluster.name.clone(),
            meta: if cluster.state == "provisioning" {
                "Provisioning".to_owned()
            } else {
                format!("No data endpoint · {state}")
            },
            dimmed: true,
            opens: false,
            target: None,
        },
    }
}

/// The CLI command that creates a cluster in the organization `profile` names, for an
/// organization with none (constitution VI): Studio creates none itself (spec 022). The CLI picks
/// the organization by its profile, so with no profile in this one the command names a
/// `<PROFILE>` to fill rather than leaving it to the default profile, which may be another
/// organization's. `<NAME>` is the person's to fill too.
pub(crate) fn create_command(profile: Option<&str>) -> String {
    format!(
        "omnigraph cluster create <NAME> --profile {}",
        profile.unwrap_or("<PROFILE>")
    )
}

/// What a sign-in that could not start says, with the control plane called by its name: the
/// reason is worded for any server (`oauth_client`), and here the server is Omnigraph Cloud, or
/// another control plane.
pub(crate) fn plane_words(why: &str, cloud: bool) -> String {
    let (capital, lower) = if cloud {
        ("Omnigraph Cloud", "Omnigraph Cloud")
    } else {
        ("The control plane", "the control plane")
    };
    why.replace("The server", capital)
        .replace("the server", lower)
}

/// What "No clusters" says: how a cluster is made, beside the command that makes it. Cloud's copy
/// states the fact and the action, never a pitch (spec 047, Copy).
pub(crate) const NO_CLUSTERS: &str = "This organization has no clusters. A member whose role \
     allows it creates one with the Omnigraph CLI, through a profile in this organization. Then \
     Refresh.";

/// What a sign-in in no organization says: where the waitlist is offered (`waitlist::offered`,
/// the same rule as its links), that Omnigraph Cloud is in private preview and how invitations
/// are given (spec 047); on any other plane or address, to ask an owner.
pub(crate) fn no_organizations(plane: &ControlPlane, account: &str) -> String {
    if crate::waitlist::offered(plane) {
        format!(
            "Signed in as {account}. No organization has you as a member yet. Omnigraph Cloud is \
             in private preview. An owner of an organization can invite you, and invitations go \
             out from the waitlist."
        )
    } else {
        format!(
            "You are signed in as {account}, but no organization has you as a member. Ask an \
             owner of one to invite you, then Refresh."
        )
    }
}

/// What Omnigraph Cloud's sign-in pane says before a sign-in (spec 047): it does not imply that
/// anyone can sign in and get a cluster.
pub(crate) const CLOUD_SIGN_IN: &str = "Omnigraph Cloud is in private preview. Invitations go out \
     from the waitlist. Once invited, sign in to open your organization's clusters.";

/// What a plane's sign-in pane says before a sign-in, with no reason to give: [`CLOUD_SIGN_IN`]
/// where the waitlist is offered, by the same rule as its link; otherwise what one sign-in does.
pub(crate) fn signed_out_words(plane: &ControlPlane) -> &'static str {
    if crate::waitlist::offered(plane) {
        CLOUD_SIGN_IN
    } else {
        "One sign-in lists every organization you are a member of, with its clusters."
    }
}

/// The CLI's targets on the control plane's profiles that name a cluster its list does not
/// have: "Not in org_… on the control plane".
pub(crate) fn missing_targets<'a>(
    clusters: &[ClusterView],
    managed: &'a [ManagedTarget],
    profiles: &[String],
) -> Vec<&'a ManagedTarget> {
    managed
        .iter()
        .filter(|target| profiles.contains(&target.profile))
        .filter(|target| {
            !clusters.iter().any(|cluster| {
                cluster.cluster_id == target.cluster_id
                    && cluster.incarnation == target.cluster_incarnation
            })
        })
        .collect()
}

/// A cluster's `state` as the person reads it: the first letter up, `_` as a space.
pub(crate) fn state_label(state: &str) -> String {
    let words = state.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Unknown state".to_owned(),
    }
}

/// What a failed read says, in the organization's words (spec 021, Errors).
pub(crate) fn failure(error: &ControlError, organization: &str) -> String {
    match error {
        ControlError::Unauthenticated(_) => "Sign in needed".to_owned(),
        ControlError::Forbidden(problem) => {
            let detail = problem
                .as_ref()
                .and_then(|problem| problem.detail.as_deref())
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default();
            format!("Your role in {organization} does not allow this{detail}")
        }
        ControlError::Unavailable { .. } => "Control plane unreachable".to_owned(),
        other => other.to_string(),
    }
}

/// What a control plane is called in the sidebar: Omnigraph Cloud, or its host.
pub(crate) fn plane_name(route: &str, cloud: bool) -> String {
    if cloud {
        "Omnigraph Cloud".to_owned()
    } else {
        oauth_host(route)
    }
}

/// Which CLI profiles use `organization`: "CLI profile work", or "No CLI profile".
pub(crate) fn profile_note(profiles: &[(String, Option<String>)], organization: &str) -> String {
    let named: Vec<&str> = profiles
        .iter()
        .filter(|(_, of)| of.as_deref() == Some(organization))
        .map(|(profile, _)| profile.as_str())
        .collect();
    match named.as_slice() {
        [] => "No CLI profile".to_owned(),
        [one] => format!("CLI profile {one}"),
        many => format!("CLI profiles {}", many.join(", ")),
    }
}

/// "1 purged cluster not shown", "5 purged clusters not shown".
pub(crate) fn purged_note(purged: usize) -> String {
    match purged {
        1 => "1 purged cluster not shown".to_owned(),
        n => format!("{n} purged clusters not shown"),
    }
}

/// A cluster's dot: green when ready, amber while it changes, grey otherwise.
pub(crate) fn state_color(state: &str) -> Color {
    match state {
        "ready" => Color::Success,
        "provisioning" | "updating" | "restoring" | "pausing" | "resuming" => Color::Warning,
        _ => Color::Muted,
    }
}

/// Whether `cluster` was purged: deleted for good, with nothing to open or undo.
pub(crate) fn is_purged(cluster: &ClusterView) -> bool {
    cluster.state == "purged"
}

pub(crate) fn oauth_host(route: &str) -> String {
    url::Url::parse(route)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| route.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cluster(state: &str, endpoint: Option<&str>) -> ClusterView {
        let mut value = json!({
            "cluster_id": "01C", "name": "acme", "incarnation": "01I",
            "canonical_root": "s3://b/c", "binding": {"mode": "github", "repository": "a/g"},
            "state": state, "cell": {"namespace": "ns", "cluster": "cell"},
            "created_by": "u", "created_at": "2026-11-01T00:00:00Z",
        });
        if let Some(endpoint) = endpoint {
            value["data_endpoint"] = json!(endpoint);
        }
        serde_json::from_value(value).unwrap()
    }

    fn target(name: &str, profile: &str, cluster: &str, incarnation: &str) -> ManagedTarget {
        ManagedTarget {
            name: name.into(),
            profile: profile.into(),
            cluster_id: cluster.into(),
            cluster_incarnation: incarnation.into(),
            endpoint: None,
            default_graph: None,
        }
    }

    #[test]
    fn each_cluster_is_shown_as_the_spec_says() {
        let ready = cluster("ready", Some("https://acme.test"));
        let targets = [target("prod", "work", "01C", "01I")];
        assert_eq!(
            cluster_row(&ready, &targets),
            ClusterRow {
                name: "acme".into(),
                meta: "CLI target prod · Ready".into(),
                dimmed: false,
                opens: false,
                target: Some("prod".into()),
            }
        );
        // Another incarnation of the same cluster is not the target's.
        let other = [target("prod", "work", "01C", "01OLD")];
        assert_eq!(cluster_row(&ready, &other).meta, "Ready");
        assert!(cluster_row(&ready, &other).opens);
        assert_eq!(
            add_command(&ready, Some("work")),
            "omnigraph target add acme --cluster 01C --profile work"
        );
        // Listed by Omnigraph Cloud with no profile: the CLI's default profile.
        assert_eq!(
            add_command(&ready, None),
            "omnigraph target add acme --cluster 01C"
        );
        assert_eq!(
            cluster_row(&cluster("provisioning", None), &[]),
            ClusterRow {
                name: "acme".into(),
                meta: "Provisioning".into(),
                dimmed: true,
                opens: false,
                target: None,
            }
        );
        let off = cluster_row(&cluster("data_access_off", None), &[]);
        assert_eq!(off.meta, "No data endpoint · Data access off");
        assert!(off.dimmed);
    }

    #[test]
    fn a_purged_cluster_is_counted_not_listed() {
        assert!(is_purged(&cluster("purged", None)));
        assert!(!is_purged(&cluster("ready", Some("https://acme.test"))));
    }

    #[test]
    fn a_target_the_list_does_not_have_is_said() {
        let clusters = [cluster("ready", Some("https://acme.test"))];
        let targets = [
            target("prod", "work", "01C", "01I"),
            target("gone", "work", "01X", "01I"),
            target("elsewhere", "other", "01Y", "01I"),
        ];
        let missing: Vec<&str> = missing_targets(&clusters, &targets, &["work".into()])
            .into_iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(missing, ["gone"]);
    }

    #[test]
    fn a_failure_is_said_in_the_organizations_words() {
        let forbidden = ControlError::Forbidden(Some(control_client::Problem {
            r#type: "scope_missing".into(),
            title: "Forbidden".into(),
            status: 403,
            detail: Some("needs cluster_read".into()),
            details_withheld: None,
        }));
        assert_eq!(
            failure(&forbidden, "org_1"),
            "Your role in org_1 does not allow this: needs cluster_read"
        );
        assert_eq!(
            failure(&ControlError::Unauthenticated(None), "org_1"),
            "Sign in needed"
        );
        assert_eq!(
            failure(
                &ControlError::Unavailable {
                    status: Some(503),
                    why: "x".into()
                },
                "org_1"
            ),
            "Control plane unreachable"
        );
    }
}
