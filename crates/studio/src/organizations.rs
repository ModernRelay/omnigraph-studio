//! The control planes in the Open dialog (spec 021): Omnigraph Cloud, and any other a CLI
//! profile names. One sign-in each serves every organization the person is a member of. The
//! dialog's sidebar lists them by name under their plane, and its pane shows the one chosen
//! with its clusters as the control plane answered them. Studio keeps no list of its own: the
//! answers are let go when the dialog closes.

use control_client::{ClusterView, ControlError};
use gpui::{AnyElement, Context, Entity, EntityId};
use graph_store::cli_config::ManagedTarget;
use graph_store::sign_in::SignIn;
use graph_store::{ControlPlane, Listing, Organizations};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{self, MenuRow, size, space};
use ui::prelude::*;

use crate::open_graph::{OpenGraphModal, Place};

/// The places the keyboard reaches in the sidebar's control planes, in order: each plane's
/// organizations once listed, or its one sign-in row before.
pub(crate) fn nav_places(planes: &[Entity<ControlPlane>], cx: &App) -> Vec<Place> {
    let mut places = Vec::new();
    for plane in planes {
        let id = plane.entity_id();
        let p = plane.read(cx);
        match (p.sign_in_state(), p.organizations()) {
            (SignIn::SignedIn { .. }, Some(Organizations::Listed(organizations))) => {
                places.extend(organizations.iter().map(|organization| {
                    Place::Organization(id, organization.organization_id.clone())
                }))
            }
            _ => places.push(Place::Plane(id)),
        }
    }
    places
}

/// The clusters of `organization` on `plane` the keyboard reaches in its pane: those that open
/// for this run, and those a CLI target names, which choose that server.
pub(crate) fn pane_places(
    plane: &Entity<ControlPlane>,
    organization: &str,
    managed: &[ManagedTarget],
    cx: &App,
) -> Vec<Place> {
    let id = plane.entity_id();
    match plane.read(cx).listing(organization) {
        Some(Listing::Listed(listed)) => listed
            .data
            .clusters
            .iter()
            .filter(|cluster| !is_purged(cluster))
            .filter(|cluster| {
                let row = cluster_row(cluster, managed);
                row.opens || row.target.is_some()
            })
            .map(|cluster| Place::Cluster(id, organization.to_owned(), cluster.cluster_id.clone()))
            .collect(),
        _ => Vec::new(),
    }
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
    /// The CLI target that names it, which clicking it chooses.
    pub target: Option<String>,
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
            target: Some(target.name.clone()),
        },
        (None, Some(_)) => ClusterRow {
            name: cluster.name.clone(),
            meta: format!("Not a CLI target · {state}"),
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

/// The control plane, and the organization of it, that the dialog's pane shows in place of a
/// server's graphs: no organization while the plane has none listed (its sign-in, or why its
/// organizations could not be read).
pub(crate) type Shown = (EntityId, Option<String>);

/// The sidebar's rows for the control planes: each plane's label (Omnigraph Cloud, or its
/// host), then the person's organizations once signed in, or one row for its sign-in before.
pub(crate) fn nav(
    planes: &[Entity<ControlPlane>],
    shown: Option<&Shown>,
    cx: &mut Context<OpenGraphModal>,
) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    for (index, plane) in planes.iter().enumerate() {
        let id = plane.entity_id();
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
        rows.push(
            ds::section_label(plane_name(&route, cloud))
                .pt(px(space::XL))
                .into_any_element(),
        );
        let listed = match (&state, &organizations) {
            (SignIn::SignedIn { .. }, Some(Organizations::Listed(list))) => Some(list),
            _ => None,
        };
        let Some(list) = listed else {
            let selected = shown == Some(&(id, None));
            rows.push(
                div()
                    .debug_selector(|| "control-plane-sign-in".into())
                    .child(
                        ds::SidebarRow::new(SharedString::from(format!("plane-{index}")), {
                            plane_row_label(&state, organizations.as_ref())
                        })
                        .icon(IconName::Person)
                        .muted(!selected)
                        .selected(selected)
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.move_to(Place::Plane(id), window, cx),
                        )),
                    )
                    .into_any_element(),
            );
            continue;
        };
        for (row, organization) in list.iter().enumerate() {
            let org = organization.organization_id.clone();
            let name = organization.name.clone().unwrap_or_else(|| org.clone());
            let count = match plane.read(cx).listing(&org) {
                Some(Listing::Listed(listed)) => Some(
                    listed
                        .data
                        .clusters
                        .iter()
                        .filter(|cluster| !is_purged(cluster))
                        .count(),
                ),
                _ => None,
            };
            let selected = shown == Some(&(id, Some(org.clone())));
            rows.push(
                ds::SidebarRow::new(SharedString::from(format!("plane-{index}-org-{row}")), name)
                    .icon(IconName::UserGroup)
                    .selected(selected)
                    .when_some(count, |row, count| {
                        row.end(ds::sidebar_count(count.to_string()))
                    })
                    .tip(ds::tooltip(profile_note(&profiles, &org)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.move_to(Place::Organization(id, org.clone()), window, cx)
                    }))
                    .into_any_element(),
            );
        }
        // A profile in an organization the person is not a member of.
        for (at, (profile, of)) in profiles.iter().enumerate() {
            let Some(of) = of else { continue };
            if list
                .iter()
                .any(|organization| &organization.organization_id == of)
            {
                continue;
            }
            rows.push(
                ds::SidebarRow::new(
                    SharedString::from(format!("plane-{index}-elsewhere-{at}")),
                    profile.clone(),
                )
                .icon(IconName::UserGroup)
                .muted(true)
                .tip(ds::tooltip(format!(
                    "CLI profile {profile} names {of}, an organization you are not a member of."
                )))
                .into_any_element(),
            );
        }
    }
    rows
}

/// What a control plane is called in the sidebar: Omnigraph Cloud, or its host.
pub(crate) fn plane_name(route: &str, cloud: bool) -> String {
    if cloud {
        "Omnigraph Cloud".to_owned()
    } else {
        oauth_host(route)
    }
}

/// The one row a control plane has in the sidebar while it lists no organizations.
fn plane_row_label(state: &SignIn, organizations: Option<&Organizations>) -> &'static str {
    match (state, organizations) {
        (SignIn::SignedIn { .. }, Some(Organizations::Failed(_))) => "Organizations unavailable",
        (SignIn::SignedIn { .. }, _) => "Reading organizations…",
        (SignIn::Waiting { .. } | SignIn::Finishing { .. } | SignIn::Reading, _) => "Signing in…",
        (SignIn::Unavailable(_), _) => "Sign-in unavailable",
        (SignIn::Offered { .. } | SignIn::NotOffered, _) => "Sign in",
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

/// The pane for a control plane the sidebar chose: the organization's clusters, or, with no
/// organization, the plane's sign-in and what it is doing.
pub(crate) fn pane(
    plane: &Entity<ControlPlane>,
    organization: Option<&str>,
    managed: &[ManagedTarget],
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    match organization {
        Some(organization) => organization_pane(plane, organization, managed, cursor, cx),
        None => sign_in_pane(plane, cx),
    }
}

/// A control plane with no organizations listed: its sign-in, as an empty state with the one
/// action that moves it on.
fn sign_in_pane(plane: &Entity<ControlPlane>, cx: &mut Context<OpenGraphModal>) -> AnyElement {
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
    let action =
        |label: &'static str, id: &'static str| ds::button(id, label).style(ButtonStyle::Outlined);
    let p = plane.clone();
    let (detail, button): (String, Option<AnyElement>) = match (&state, &organizations) {
        (SignIn::SignedIn { .. }, Some(Organizations::Failed(error))) => (
            failure(error, "this control plane"),
            Some(
                action("Try again", "plane-retry")
                    .on_click(
                        cx.listener(move |_, _, _, cx| p.update(cx, |p, cx| p.read_clusters(cx))),
                    )
                    .into_any_element(),
            ),
        ),
        (SignIn::SignedIn { .. }, _) => ("Reading your organizations…".to_owned(), None),
        (SignIn::Finishing { .. }, _) => ("Keeping the sign-in in your keychain…".to_owned(), None),
        (SignIn::Waiting { service }, _) => (
            format!("Finish signing in at {service} in your browser."),
            Some(
                action("Cancel", "plane-cancel")
                    .on_click(
                        cx.listener(move |_, _, _, cx| p.update(cx, |p, cx| p.cancel_sign_in(cx))),
                    )
                    .into_any_element(),
            ),
        ),
        (SignIn::Reading, _) => ("Reading the control plane's sign-in…".to_owned(), None),
        (SignIn::Unavailable(refusal), _) => (refusal.why.to_string(), None),
        (SignIn::Offered { why, .. }, _) => (
            why.clone().map(|why| why.to_string()).unwrap_or_else(|| {
                "One sign-in lists every organization you are a member of, with its clusters."
                    .to_owned()
            }),
            Some(
                ds::action_button("plane-sign-in", "Sign in")
                    .tip(ds::tooltip(format!("Opens {host} in your browser")))
                    .on_click(cx.listener(move |_, _, _, cx| p.update(cx, |p, cx| p.sign_in(cx))))
                    .into_any_element(),
            ),
        ),
        (SignIn::NotOffered, _) => (format!("{host} offers no sign-in Studio can use."), None),
    };
    let named = (!profiles.is_empty()).then(|| {
        let named = profiles
            .iter()
            .map(|(profile, _)| profile.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        ds::state_detail(if profiles.len() == 1 {
            format!("Used by CLI profile {named}")
        } else {
            format!("Used by CLI profiles {named}")
        })
    });
    ds::empty_state(IconName::Public, plane_name(&route, cloud))
        .child(
            v_flex()
                .items_center()
                .gap(px(space::XS))
                .child(ds::state_detail(detail))
                .children(named),
        )
        .children(button)
        .into_any_element()
}

/// An organization's clusters, under its name and the plane's Refresh and Sign out.
fn organization_pane(
    plane: &Entity<ControlPlane>,
    organization: &str,
    managed: &[ManagedTarget],
    cursor: Option<&Place>,
    cx: &mut Context<OpenGraphModal>,
) -> AnyElement {
    let plane_id = plane.entity_id();
    let (name, profiles, listing) = {
        let p = plane.read(cx);
        let name = match p.organizations() {
            Some(Organizations::Listed(list)) => list
                .iter()
                .find(|o| o.organization_id == organization)
                .and_then(|o| o.name.clone()),
            _ => None,
        }
        .unwrap_or_else(|| organization.to_owned());
        (name, p.profiles.clone(), p.listing(organization).cloned())
    };
    let named: Vec<String> = profiles
        .iter()
        .filter(|(_, of)| of.as_deref() == Some(organization))
        .map(|(profile, _)| profile.clone())
        .collect();
    let refresh = plane.clone();
    let sign_out = plane.clone();
    let head = crate::open_graph::pane_head(
        name.clone(),
        Some(ds::text::meta(profile_note(&profiles, organization)).into_any_element()),
        vec![
            ds::button("plane-refresh", "Refresh")
                .style(ButtonStyle::Subtle)
                .on_click(cx.listener(move |_, _, _, cx| {
                    refresh.update(cx, |p, cx| p.read_clusters(cx))
                }))
                .into_any_element(),
            ds::button("plane-sign-out", "Sign out")
                .style(ButtonStyle::Subtle)
                .tip(ds::tooltip(
                    "Forgets this sign-in for every organization, and asks the sign-in service to end it where it can.",
                ))
                .on_click(cx.listener(move |_, _, _, cx| {
                    sign_out.update(cx, |p, cx| p.sign_out(cx))
                }))
                .into_any_element(),
        ],
    );
    let body: AnyElement = match listing {
        None | Some(Listing::Reading) => {
            ds::loading_state("Reading its clusters…").into_any_element()
        }
        Some(Listing::Failed(error)) => {
            ds::error_state("Clusters unavailable", failure(&error, &name)).into_any_element()
        }
        Some(Listing::Listed(listed)) => {
            let clusters = &listed.data.clusters;
            // A purged cluster is gone for good: counted, not listed.
            let purged = clusters.iter().filter(|c| is_purged(c)).count();
            let missing = missing_targets(clusters, managed, &named);
            if clusters.len() == purged && missing.is_empty() {
                ds::empty_state(IconName::Server, "No clusters")
                    .children((purged > 0).then(|| ds::state_detail(purged_note(purged))))
                    .into_any_element()
            } else {
                let mut rows: Vec<AnyElement> = Vec::new();
                for (at, cluster) in clusters.iter().enumerate() {
                    if is_purged(cluster) {
                        continue;
                    }
                    let shown = cluster_row(cluster, managed);
                    let place = Place::Cluster(
                        plane_id,
                        organization.to_owned(),
                        cluster.cluster_id.clone(),
                    );
                    let mut item = MenuRow::new(SharedString::from(format!("cluster-{at}")))
                        .height(size::ROW)
                        .start_slot(ds::dot(state_color(&cluster.state)))
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap(px(space::MD))
                                .child(ds::text::body(shown.name.clone()))
                                .child(ds::text::meta(shown.meta.clone()).truncate()),
                        );
                    if shown.opens {
                        let plane = plane.clone();
                        let cluster = cluster.clone();
                        let organization = organization.to_owned();
                        let command = add_command(&cluster, named.first().map(String::as_str));
                        item = item
                            .toggle_state(cursor == Some(&place))
                            .aria_label(format!(
                                "Open {} for this run. To keep it, add it as a CLI target: {command}",
                                cluster.name
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_cluster(&plane, &organization, &cluster, window, cx)
                            }));
                    } else if let Some(target) = shown.target.clone() {
                        item = item
                            .toggle_state(cursor == Some(&place))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_target(&target, window, cx)
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
                for target in missing {
                    rows.push(
                        MenuRow::new(SharedString::from(format!("missing-{}", target.name)))
                            .height(size::ROW)
                            .disabled(true)
                            .start_slot(ds::dot(Color::Warning))
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap(px(space::MD))
                                    .child(ds::text::body(target.name.clone()))
                                    .child(
                                        ds::text::meta(format!("CLI target, not in {name}"))
                                            .truncate(),
                                    ),
                            )
                            .into_any_element(),
                    );
                }
                let notes = [
                    (purged > 0).then(|| purged_note(purged)),
                    listed
                        .meta
                        .stale
                        .then(|| format!("As of {}", listed.meta.observed_at)),
                ];
                v_flex()
                    .min_h_0()
                    .gap(px(space::XXS))
                    .child(crate::open_graph::pane_label("Clusters"))
                    .child(
                        v_flex()
                            .id("cluster-list")
                            .debug_selector(|| "control-plane-clusters".into())
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .children(notes.into_iter().flatten().map(|note| {
                        div()
                            .px(px(space::MD))
                            .pt(px(space::XS))
                            .child(ds::text::meta(note))
                    }))
                    .into_any_element()
            }
        }
    };
    v_flex()
        .size_full()
        .min_h_0()
        .gap(px(space::LG))
        .child(head)
        .child(v_flex().flex_1().min_h_0().child(body))
        .into_any_element()
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
