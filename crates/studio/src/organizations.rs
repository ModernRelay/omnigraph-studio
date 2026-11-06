//! The organizations under the Open dialog's servers (spec 021): each control plane a CLI
//! profile names, its sign-in, and, while the dialog is open, the organization's clusters as the
//! control plane answered them. Studio keeps no list of its own: the answer is let go when the
//! dialog closes.

use control_client::{ClusterView, ControlError};
use gpui::{AnyElement, Context, Entity};
use graph_store::cli_config::ManagedTarget;
use graph_store::sign_in::SignIn;
use graph_store::{ControlPlane, Listing};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{self, MenuRow, size, space};
use ui::prelude::*;

use crate::open_graph::{OpenGraphModal, Place};

/// The places the keyboard reaches in the organizations, in order: each control plane's line,
/// then its clusters that open for this run.
pub(crate) fn places(
    planes: &[Entity<ControlPlane>],
    managed: &[ManagedTarget],
    cx: &App,
) -> Vec<Place> {
    let mut places = Vec::new();
    for plane in planes {
        let id = plane.entity_id();
        places.push(Place::Plane(id));
        if let Some(Listing::Listed(listed)) = plane.read(cx).listing() {
            for cluster in &listed.data.clusters {
                if cluster_row(cluster, managed).opens {
                    places.push(Place::Cluster(id, cluster.cluster_id.clone()));
                }
            }
        }
    }
    places
}

/// ↵ on a control plane's line: signs in, cancels a sign-in under way, or reads the clusters
/// again.
pub(crate) fn enter(plane: &mut ControlPlane, cx: &mut gpui::Context<ControlPlane>) {
    match plane.sign_in_state() {
        SignIn::SignedIn { .. } => plane.read_clusters(cx),
        SignIn::Waiting { .. } | SignIn::Reading => plane.cancel_sign_in(cx),
        SignIn::Finishing { .. } | SignIn::Unavailable(_) => {}
        SignIn::Offered { .. } | SignIn::NotOffered => plane.sign_in(cx),
    }
}

/// The cluster `cluster_id` of the plane's list, while it is shown.
pub(crate) fn cluster(
    plane: &Entity<ControlPlane>,
    cluster_id: &str,
    cx: &App,
) -> Option<ClusterView> {
    match plane.read(cx).listing() {
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
    /// Clicking it opens the cluster for this run: no CLI target names it, and it has a data
    /// endpoint.
    pub opens: bool,
}

/// The CLI command that keeps `cluster` as a target of `profile` (constitution VI).
pub(crate) fn add_command(cluster: &ClusterView, profile: Option<&str>) -> String {
    format!(
        "omnigraph target add {} --cluster {} --profile {}",
        cluster.name,
        cluster.cluster_id,
        profile.unwrap_or("<profile>")
    )
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
        },
        (None, Some(_)) => ClusterRow {
            name: cluster.name.clone(),
            meta: format!("Not a CLI target · {state}"),
            dimmed: false,
            opens: true,
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
        },
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
fn state_label(state: &str) -> String {
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

/// The organizations block, or nothing when no profile names a control plane.
pub(crate) fn render(
    planes: &[Entity<ControlPlane>],
    managed: &[ManagedTarget],
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> Option<AnyElement> {
    if planes.is_empty() {
        return None;
    }
    let blocks: Vec<AnyElement> = planes
        .iter()
        .enumerate()
        .map(|(index, plane)| render_plane(index, plane, managed, cursor, cx))
        .collect();
    Some(
        v_flex()
            .gap(px(space::XS))
            .pt(px(space::MD))
            .child(
                div()
                    .px(px(space::MD))
                    .pb(px(space::XS))
                    .child(ds::text::meta("Organizations")),
            )
            .children(blocks)
            .into_any_element(),
    )
}

fn render_plane(
    index: usize,
    plane: &Entity<ControlPlane>,
    managed: &[ManagedTarget],
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let at = cursor == Some(&Place::Plane(plane.entity_id()));
    let (route, profiles, organization, state) = {
        let p = plane.read(cx);
        (
            p.route.clone(),
            p.profiles.clone(),
            p.organization.clone(),
            p.sign_in_state().clone(),
        )
    };
    let host = oauth_host(&route);
    let profiles = match profiles.as_slice() {
        [one] => format!("CLI profile {one}"),
        many => format!("CLI profiles {}", many.join(", ")),
    };
    let heading = MenuRow::new(SharedString::from(format!("plane-{index}")))
        .height(size::FEED_ROW)
        .toggle_state(at)
        .child(
            v_flex()
                .min_w_0()
                .child(ds::text::body(organization.clone()))
                .child(ds::text::meta(format!("{host} · {profiles}")).truncate()),
        );
    let mut block = v_flex().gap(px(space::XXS)).child(heading);
    let plane = plane.clone();
    let id = |what: &str| SharedString::from(format!("plane-{index}-{what}"));
    match state {
        // The account is the token's subject, an id that means nothing to the person: the
        // control plane names no email or name for it yet (spec 021, CP3).
        SignIn::SignedIn { .. } => {
            let refresh = plane.clone();
            let out = plane.clone();
            block = block.child(
                h_flex()
                    .px(px(space::MD))
                    .gap(px(space::XS))
                    .child(fill(ds::text::meta("Signed in")))
                    .child(
                        ds::button(id("refresh"), "Refresh")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                refresh.update(cx, |p, cx| p.read_clusters(cx))
                            })),
                    )
                    .child(
                        ds::button(id("sign-out"), "Sign out")
                            .style(ButtonStyle::Subtle)
                            .tip(ds::tooltip(
                                "Forgets this sign-in, and asks the sign-in service to end it where it can.",
                            ))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                out.update(cx, |p, cx| p.sign_out(cx))
                            })),
                    ),
            );
            block = block.child(render_listing(
                index,
                &plane,
                managed,
                &organization,
                cursor,
                cx,
            ));
        }
        // Finishing: the service answered, and the keychain is keeping the sign-in; macOS may be
        // asking whether Studio may. It is not cancelled from here.
        SignIn::Finishing { .. } => {
            block = block.child(div().px(px(space::MD)).child(fill(ds::text::meta(
                "Keeping the sign-in in your keychain…",
            ))));
        }
        SignIn::Waiting { service } => {
            let cancel = plane.clone();
            block = block.child(
                h_flex()
                    .px(px(space::MD))
                    .gap(px(space::XS))
                    .child(fill(ds::text::meta(format!(
                        "Signing in at {service} in your browser…"
                    ))))
                    .child(
                        ds::button(id("cancel"), "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cancel.update(cx, |p, cx| p.cancel_sign_in(cx))
                            })),
                    ),
            );
        }
        SignIn::Reading => {
            block = block.child(
                div()
                    .px(px(space::MD))
                    .child(ds::text::meta("Reading the control plane's sign-in…")),
            );
        }
        SignIn::Unavailable(refusal) => {
            block = block.child(
                div()
                    .px(px(space::MD))
                    .child(ds::text::error_note(refusal.why)),
            );
        }
        SignIn::Offered { why, .. } => {
            block = block.child(sign_in_line(index, &plane, &host, why, cx));
        }
        SignIn::NotOffered => {
            block = block.child(sign_in_line(index, &plane, &host, None, cx));
        }
    }
    block.into_any_element()
}

/// "Sign in to list this organization's clusters", and why a sign-in did not hold.
fn sign_in_line(
    index: usize,
    plane: &Entity<ControlPlane>,
    host: &str,
    why: Option<String>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let plane = plane.clone();
    v_flex()
        .px(px(space::MD))
        .gap(px(space::XXS))
        .children(why.map(ds::text::error_note))
        .child(
            h_flex()
                .debug_selector(|| "control-plane-sign-in".into())
                .gap(px(space::XS))
                .child(fill(ds::text::meta(
                    "Sign in to list this organization's clusters",
                )))
                .child(
                    ds::button(SharedString::from(format!("plane-{index}-sign-in")), "Sign in")
                        .style(ButtonStyle::Subtle)
                        .tip(ds::tooltip(format!(
                            "Opens {host} in your browser to sign in to this organization's control plane."
                        )))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            plane.update(cx, |p, cx| p.sign_in(cx))
                        })),
                ),
        )
        .into_any_element()
}

fn render_listing(
    index: usize,
    plane: &Entity<ControlPlane>,
    managed: &[ManagedTarget],
    organization: &str,
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let plane_id = plane.entity_id();
    let (profiles, host, listing) = {
        let p = plane.read(cx);
        (
            p.profiles.clone(),
            oauth_host(&p.route),
            p.listing().cloned(),
        )
    };
    match listing.as_ref() {
        None | Some(Listing::Reading) => div()
            .px(px(space::MD))
            .child(ds::text::meta("Reading the organization's clusters…"))
            .into_any_element(),
        Some(Listing::Failed(error)) => {
            let said = failure(error, organization);
            match error {
                ControlError::Unauthenticated(_) => {
                    sign_in_line(index, plane, &host, Some(said), cx)
                }
                _ => div()
                    .px(px(space::MD))
                    .child(ds::text::error_note(said))
                    .into_any_element(),
            }
        }
        Some(Listing::Listed(listed)) => {
            let clusters = &listed.data.clusters;
            // A purged cluster is gone for good: counted, not listed.
            let purged = clusters.iter().filter(|c| is_purged(c)).count();
            let mut rows: Vec<AnyElement> = clusters
                .iter()
                .enumerate()
                .filter(|(_, cluster)| !is_purged(cluster))
                .map(|(row, cluster)| {
                    let shown = cluster_row(cluster, managed);
                    let mut item =
                        MenuRow::new(SharedString::from(format!("plane-{index}-cluster-{row}")))
                            .height(size::FEED_ROW)
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(ds::text::body(shown.name))
                                    .child(ds::text::meta(shown.meta).truncate()),
                            );
                    if shown.opens {
                        item = item.toggle_state(
                            cursor == Some(&Place::Cluster(plane_id, cluster.cluster_id.clone())),
                        );
                        let plane = plane.clone();
                        let cluster = cluster.clone();
                        let command = add_command(&cluster, profiles.first().map(String::as_str));
                        item = item
                            .aria_label(format!(
                                "Open {} for this run. To keep it, add it as a CLI target: {command}",
                                cluster.name
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_cluster(&plane, &cluster, window, cx)
                            }));
                    }
                    if shown.dimmed {
                        div().opacity(0.5).child(item).into_any_element()
                    } else {
                        item.into_any_element()
                    }
                })
                .collect();
            for target in missing_targets(clusters, managed, &profiles) {
                rows.push(
                    MenuRow::new(SharedString::from(format!(
                        "plane-{index}-missing-{}",
                        target.name
                    )))
                    .height(size::FEED_ROW)
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(ds::text::body(target.name.clone()))
                            .child(ds::text::meta(format!(
                                "Not in {organization} on the control plane"
                            ))),
                    )
                    .into_any_element(),
                );
            }
            if purged > 0 {
                rows.push(
                    div()
                        .px(px(space::MD))
                        .child(ds::text::meta(match purged {
                            1 => "1 purged cluster not shown".to_owned(),
                            n => format!("{n} purged clusters not shown"),
                        }))
                        .into_any_element(),
                );
            }
            if clusters.is_empty() {
                rows.push(
                    div()
                        .px(px(space::MD))
                        .child(ds::text::meta(format!("No clusters in {organization}")))
                        .into_any_element(),
                );
            }
            let stale = listed.meta.stale.then(|| {
                div()
                    .px(px(space::MD))
                    .child(ds::text::meta(format!("As of {}", listed.meta.observed_at)))
            });
            v_flex()
                .debug_selector(|| "control-plane-clusters".into())
                .children(stale)
                .children(rows)
                .into_any_element()
        }
    }
}

/// Whether `cluster` was purged: deleted for good, with nothing to open or undo.
pub(crate) fn is_purged(cluster: &ClusterView) -> bool {
    cluster.state == "purged"
}

/// A line's text beside its buttons: it takes the width they leave and truncates, so the
/// buttons always show.
fn fill(text: Label) -> Div {
    div().flex_1().min_w_0().child(text.truncate())
}

fn oauth_host(route: &str) -> String {
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
            }
        );
        // Another incarnation of the same cluster is not the target's.
        let other = [target("prod", "work", "01C", "01OLD")];
        assert_eq!(cluster_row(&ready, &other).meta, "Not a CLI target · Ready");
        assert!(cluster_row(&ready, &other).opens);
        assert_eq!(
            add_command(&ready, Some("work")),
            "omnigraph target add acme --cluster 01C --profile work"
        );
        assert_eq!(
            cluster_row(&cluster("provisioning", None), &[]),
            ClusterRow {
                name: "acme".into(),
                meta: "Provisioning".into(),
                dimmed: true,
                opens: false,
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
