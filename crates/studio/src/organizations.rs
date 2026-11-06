//! The control planes under the Open dialog's servers (spec 021): Omnigraph Cloud, and any other
//! a CLI profile names. One sign-in each serves every organization the person is a member of;
//! while the dialog is open, it lists them by name, each with its clusters as the control plane
//! answered them. Studio keeps no list of its own: the answers are let go when the dialog closes.

use control_client::{ClusterView, ControlError};
use gpui::{AnyElement, Context, Entity};
use graph_store::cli_config::ManagedTarget;
use graph_store::sign_in::SignIn;
use graph_store::{ControlPlane, Listing, Organizations};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{self, MenuRow, size, space};
use ui::prelude::*;

use crate::open_graph::{OpenGraphModal, Place};

/// The places the keyboard reaches in the control planes, in order: each plane's sign-in line,
/// then the clusters that open for this run, organization by organization.
pub(crate) fn places(
    planes: &[Entity<ControlPlane>],
    managed: &[ManagedTarget],
    cx: &App,
) -> Vec<Place> {
    let mut places = Vec::new();
    for plane in planes {
        let id = plane.entity_id();
        places.push(Place::Plane(id));
        let p = plane.read(cx);
        let Some(Organizations::Listed(organizations)) = p.organizations() else {
            continue;
        };
        for organization in organizations {
            if let Some(Listing::Listed(listed)) = p.listing(&organization.organization_id) {
                for cluster in &listed.data.clusters {
                    if cluster_row(cluster, managed).opens {
                        places.push(Place::Cluster(
                            id,
                            organization.organization_id.clone(),
                            cluster.cluster_id.clone(),
                        ));
                    }
                }
            }
        }
    }
    places
}

/// ↵ on a control plane's line: signs in, cancels a sign-in under way, or reads again.
pub(crate) fn enter(plane: &mut ControlPlane, cx: &mut gpui::Context<ControlPlane>) {
    match plane.sign_in_state() {
        SignIn::SignedIn { .. } => plane.read_clusters(cx),
        SignIn::Waiting { .. } | SignIn::Reading => plane.cancel_sign_in(cx),
        SignIn::Finishing { .. } | SignIn::Unavailable(_) => {}
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
    /// Clicking it opens the cluster for this run: no CLI target names it, and it has a data
    /// endpoint.
    pub opens: bool,
}

/// The CLI command that keeps `cluster` as a target of `profile` (constitution VI).
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

/// The control planes' block, or nothing when there is none (no profile names one, and
/// `omnigraph_cloud` is empty).
pub(crate) fn render(
    planes: &[Entity<ControlPlane>],
    managed: &[ManagedTarget],
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> Option<AnyElement> {
    if planes.is_empty() {
        return None;
    }
    let sections: Vec<AnyElement> = planes
        .iter()
        .enumerate()
        .map(|(index, plane)| render_plane(index, plane, managed, cursor, cx))
        .collect();
    Some(
        v_flex()
            .gap(px(space::MD))
            .pt(px(space::MD))
            .children(sections)
            .into_any_element(),
    )
}

/// One control plane: its heading, its one sign-in, then the person's organizations, each with
/// its clusters.
fn render_plane(
    index: usize,
    plane: &Entity<ControlPlane>,
    managed: &[ManagedTarget],
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let at = cursor == Some(&Place::Plane(plane.entity_id()));
    let (route, cloud, state, profiles, organizations) = {
        let p = plane.read(cx);
        (
            p.route.clone(),
            p.is_default(),
            p.sign_in_state().clone(),
            p.profiles.clone(),
            p.organizations().cloned(),
        )
    };
    let host = oauth_host(&route);
    let heading = h_flex()
        .px(px(space::MD))
        .gap(px(space::XS))
        .justify_between()
        .child(ds::text::meta(if cloud {
            "Omnigraph Cloud".to_owned()
        } else {
            host.clone()
        }))
        .children(cloud.then(|| ds::text::meta(host.clone()).truncate()));
    let line = sign_in_line(index, plane, &host, &state, cx);
    let mut block = v_flex().gap(px(space::XXS)).child(heading).child(
        MenuRow::new(SharedString::from(format!("plane-{index}")))
            .height(size::FEED_ROW)
            .toggle_state(at)
            .child(line),
    );
    if !matches!(state, SignIn::SignedIn { .. }) {
        // Before a sign-in, the profiles say where they point.
        if !profiles.is_empty() {
            let named = profiles
                .iter()
                .map(|(profile, _)| profile.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            block = block.child(
                div()
                    .px(px(space::MD))
                    .child(ds::text::meta(format!("CLI profiles {named}")).truncate()),
            );
        }
        return block.into_any_element();
    }
    match organizations {
        None | Some(Organizations::Reading) => {
            block = block.child(
                div()
                    .px(px(space::MD))
                    .child(ds::text::meta("Reading your organizations…")),
            );
        }
        Some(Organizations::Failed(error)) => {
            block = block.child(
                div()
                    .px(px(space::MD))
                    .child(ds::text::error_note(failure(&error, "this control plane"))),
            );
        }
        Some(Organizations::Listed(list)) => {
            for (row, organization) in list.iter().enumerate() {
                let id = &organization.organization_id;
                let name = organization.name.clone().unwrap_or_else(|| id.clone());
                let named: Vec<String> = profiles
                    .iter()
                    .filter(|(_, of)| of.as_deref() == Some(id.as_str()))
                    .map(|(profile, _)| profile.clone())
                    .collect();
                let meta = match named.as_slice() {
                    [] => id.clone(),
                    [one] => format!("CLI profile {one}"),
                    many => format!("CLI profiles {}", many.join(", ")),
                };
                block = block.child(
                    v_flex()
                        .pt(px(space::XS))
                        .child(
                            v_flex()
                                .px(px(space::MD))
                                .min_w_0()
                                .child(ds::text::body(name.clone()).truncate())
                                .child(ds::text::meta(meta).truncate()),
                        )
                        .child(render_listing(
                            index, row, plane, id, &name, &named, managed, cursor, cx,
                        )),
                );
            }
            // A profile in an organization the person is not a member of.
            for (profile, of) in &profiles {
                let Some(of) = of else { continue };
                if list
                    .iter()
                    .any(|organization| &organization.organization_id == of)
                {
                    continue;
                }
                block = block.child(
                    v_flex()
                        .pt(px(space::XS))
                        .px(px(space::MD))
                        .child(ds::text::body(of.clone()).truncate())
                        .child(
                            ds::text::meta(format!(
                                "CLI profile {profile} · You are not a member of it"
                            ))
                            .truncate(),
                        ),
                );
            }
        }
    }
    block.into_any_element()
}

/// The plane's one sign-in, and what it is doing.
fn sign_in_line(
    index: usize,
    plane: &Entity<ControlPlane>,
    host: &str,
    state: &SignIn,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let id = |what: &str| SharedString::from(format!("plane-{index}-{what}"));
    let plane = plane.clone();
    let line = h_flex().w_full().gap(px(space::XS));
    match state.clone() {
        // The account is the token's subject, an id that means nothing to the person: the
        // control plane names no email for it yet.
        SignIn::SignedIn { .. } => {
            let refresh = plane.clone();
            line.child(fill(ds::text::meta("Signed in")))
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
                            plane.update(cx, |p, cx| p.sign_out(cx))
                        })),
                )
                .into_any_element()
        }
        // The service answered, and the keychain is keeping the sign-in; macOS may be asking
        // whether Studio may. It is not cancelled from here.
        SignIn::Finishing { .. } => line
            .child(fill(ds::text::meta("Keeping the sign-in in your keychain…")))
            .into_any_element(),
        SignIn::Waiting { service } => line
            .child(fill(ds::text::meta(format!(
                "Signing in at {service} in your browser…"
            ))))
            .child(
                ds::button(id("cancel"), "Cancel")
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        plane.update(cx, |p, cx| p.cancel_sign_in(cx))
                    })),
            )
            .into_any_element(),
        SignIn::Reading => line
            .child(fill(ds::text::meta("Reading the control plane's sign-in…")))
            .into_any_element(),
        SignIn::Unavailable(refusal) => line
            .child(fill(ds::text::error_note(refusal.why)))
            .into_any_element(),
        SignIn::Offered { why, .. } => v_flex()
            .w_full()
            .gap(px(space::XXS))
            .children(why.map(ds::text::error_note))
            .child(
                line.debug_selector(|| "control-plane-sign-in".into())
                    .child(fill(ds::text::meta("Sign in to list your organizations")))
                    .child(
                        ds::button(id("sign-in"), "Sign in")
                            .style(ButtonStyle::Subtle)
                            .tip(ds::tooltip(format!(
                                "Opens {host} in your browser to sign in. One sign-in lists every organization you are a member of."
                            )))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                plane.update(cx, |p, cx| p.sign_in(cx))
                            })),
                    ),
            )
            .into_any_element(),
        SignIn::NotOffered => line.into_any_element(),
    }
}

/// An organization's clusters, set in under it.
#[allow(clippy::too_many_arguments)]
fn render_listing(
    index: usize,
    row: usize,
    plane: &Entity<ControlPlane>,
    organization: &str,
    name: &str,
    profiles: &[String],
    managed: &[ManagedTarget],
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let plane_id = plane.entity_id();
    let listing = plane.read(cx).listing(organization).cloned();
    let body = v_flex()
        .debug_selector(|| "control-plane-clusters".into())
        .pl(px(space::LG));
    match listing {
        None | Some(Listing::Reading) => body
            .child(
                div()
                    .px(px(space::MD))
                    .child(ds::text::meta("Reading its clusters…")),
            )
            .into_any_element(),
        Some(Listing::Failed(error)) => body
            .child(
                div()
                    .px(px(space::MD))
                    .child(ds::text::error_note(failure(&error, name))),
            )
            .into_any_element(),
        Some(Listing::Listed(listed)) => {
            let clusters = &listed.data.clusters;
            // A purged cluster is gone for good: counted, not listed.
            let purged = clusters.iter().filter(|c| is_purged(c)).count();
            let mut rows: Vec<AnyElement> = Vec::new();
            for (at, cluster) in clusters.iter().enumerate() {
                if is_purged(cluster) {
                    continue;
                }
                let shown = cluster_row(cluster, managed);
                let mut item = MenuRow::new(SharedString::from(format!(
                    "plane-{index}-{row}-cluster-{at}"
                )))
                .height(size::FEED_ROW)
                .child(
                    v_flex()
                        .min_w_0()
                        .child(ds::text::body(shown.name))
                        .child(ds::text::meta(shown.meta).truncate()),
                );
                if shown.opens {
                    item = item.toggle_state(
                        cursor
                            == Some(&Place::Cluster(
                                plane_id,
                                organization.to_owned(),
                                cluster.cluster_id.clone(),
                            )),
                    );
                    let plane = plane.clone();
                    let cluster = cluster.clone();
                    let organization = organization.to_owned();
                    let command = add_command(&cluster, profiles.first().map(String::as_str));
                    item = item
                        .aria_label(format!(
                            "Open {} for this run. To keep it, add it as a CLI target: {command}",
                            cluster.name
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_cluster(&plane, &organization, &cluster, window, cx)
                        }));
                }
                rows.push(if shown.dimmed {
                    div()
                        .opacity(ds::opacity::DIMMED)
                        .child(item)
                        .into_any_element()
                } else {
                    item.into_any_element()
                });
            }
            for target in missing_targets(clusters, managed, profiles) {
                rows.push(
                    MenuRow::new(SharedString::from(format!(
                        "plane-{index}-{row}-missing-{}",
                        target.name
                    )))
                    .height(size::FEED_ROW)
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(ds::text::body(target.name.clone()))
                            .child(ds::text::meta(format!(
                                "Not in {name} on the control plane"
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
                        .child(ds::text::meta("No clusters"))
                        .into_any_element(),
                );
            }
            let stale = listed.meta.stale.then(|| {
                div()
                    .px(px(space::MD))
                    .child(ds::text::meta(format!("As of {}", listed.meta.observed_at)))
            });
            body.children(stale).children(rows).into_any_element()
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
