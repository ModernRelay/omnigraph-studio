//! The control planes in the Open dialog (spec 021): Omnigraph Cloud, and any other a CLI
//! profile names. The dialog's sidebar lists their organizations by name under their plane, and
//! its pane shows the one chosen with its clusters as the control plane answered them. Studio
//! keeps no list of its own: the answers are let go when the dialog closes. What a plane and a
//! cluster say is in `organizations`, which the Cloud settings pages show too.

use gpui::{AnyElement, Context, Entity, EntityId};
use graph_store::cli_config::ManagedTarget;
use graph_store::sign_in::SignIn;
use graph_store::{ControlPlane, Listing, Organizations};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{self, MenuRow, size, space};
use ui::prelude::*;

use super::{OpenGraphModal, Place, pane_head, pane_label};
use crate::organizations::{
    NO_CLUSTERS, add_command, cluster_row, create_command, failure, is_purged, missing_targets,
    no_organizations, oauth_host, plane_name, plane_words, profile_note, purged_note,
    signed_out_words, state_color,
};

/// The places the keyboard reaches in the sidebar's control planes, in order: each plane's
/// organizations once listed, or its one sign-in row before.
pub(super) fn nav_places(planes: &[Entity<ControlPlane>], cx: &App) -> Vec<Place> {
    let mut places = Vec::new();
    for plane in planes {
        let id = plane.entity_id();
        let p = plane.read(cx);
        match (p.sign_in_state(), p.organizations()) {
            (SignIn::SignedIn { .. }, Some(Organizations::Listed(organizations)))
                if !organizations.is_empty() =>
            {
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
/// from the control plane, and those a CLI target names, which choose that server.
pub(super) fn pane_places(
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

/// The control plane, and the organization of it, that the dialog's pane shows in place of a
/// server's graphs: no organization while the plane has none listed (its sign-in, or why its
/// organizations could not be read).
pub(super) type Shown = (EntityId, Option<String>);

impl OpenGraphModal {
    /// The sidebar's rows for the control planes: each plane's label (Omnigraph Cloud, or its
    /// host), then the person's organizations once signed in, or one row for its sign-in before.
    pub(super) fn render_planes(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let shown = self.plane_shown.as_ref();
        let mut rows = Vec::new();
        for (index, plane) in self.planes.iter().enumerate() {
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
            // A sign-in in no organization keeps the plane's one row, which says so.
            let listed = match (&state, &organizations) {
                (SignIn::SignedIn { .. }, Some(Organizations::Listed(list)))
                    if !list.is_empty() =>
                {
                    Some(list)
                }
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
                                move |this, _, window, cx| {
                                    this.move_to(Place::Plane(id), window, cx)
                                },
                            )),
                        )
                        .into_any_element(),
                );
                // Signed out of Omnigraph Cloud: a muted second line under its row (spec 047).
                let signed_out = !matches!(state, SignIn::SignedIn { .. });
                if signed_out && crate::waitlist::offered(plane.read(cx)) {
                    rows.push(
                        div()
                            // Under the row's icon: its margin and padding.
                            .pl(px(space::SM + space::SM))
                            .child(crate::waitlist::note(
                                crate::waitlist::Surface::Sidebar,
                                crate::waitlist::PREVIEW_UNDER_NAME,
                            ))
                            .into_any_element(),
                    );
                }
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
                    ds::SidebarRow::new(
                        SharedString::from(format!("plane-{index}-org-{row}")),
                        name,
                    )
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

    /// The pane for a control plane the sidebar chose: the organization's clusters, or, with no
    /// organization, the plane's sign-in and what it is doing.
    pub(super) fn render_plane(
        &self,
        plane: &Entity<ControlPlane>,
        organization: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match organization {
            Some(organization) => self.render_organization(plane, organization, cx),
            None => sign_in_pane(plane, cx),
        }
    }

    /// An organization's clusters, under its name and the plane's Refresh and Sign out.
    fn render_organization(
        &self,
        plane: &Entity<ControlPlane>,
        organization: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let managed: &[ManagedTarget] = &self.managed;
        let cursor = self.cursor.as_ref();
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
        let head = pane_head(
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
                        .debug_selector(|| "no-clusters".into())
                        .child(
                            v_flex()
                                .items_center()
                                .gap(px(space::XS))
                                .child(ds::state_detail(NO_CLUSTERS))
                                .children(
                                    (purged > 0).then(|| ds::state_detail(purged_note(purged))),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "create-cluster".into())
                                .w(px(size::STATE_DETAIL_WIDTH))
                                .child(ds::command_block(
                                    "create-cluster",
                                    create_command(named.first().map(String::as_str)),
                                    cx,
                                )),
                        )
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
                                    "Open {}. To use it from the CLI too, add it as a target: {command}",
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
                        .child(pane_label("Clusters"))
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
}

/// The one row a control plane has in the sidebar while it lists no organizations.
fn plane_row_label(state: &SignIn, organizations: Option<&Organizations>) -> &'static str {
    match (state, organizations) {
        (SignIn::SignedIn { .. }, Some(Organizations::Failed(_))) => "Organizations unavailable",
        (SignIn::SignedIn { .. }, Some(Organizations::Listed(_))) => "No organizations",
        (SignIn::SignedIn { .. }, _) => "Reading organizations…",
        (SignIn::Waiting { .. } | SignIn::Finishing { .. } | SignIn::Reading, _) => "Signing in…",
        (SignIn::Unavailable(_), _) => "Sign-in unavailable",
        (SignIn::Offered { .. } | SignIn::NotOffered, _) => "Sign in",
    }
}

/// A control plane with no organizations listed: its sign-in, as an empty state with the one
/// action that moves it on.
fn sign_in_pane(plane: &Entity<ControlPlane>, cx: &App) -> AnyElement {
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
    let waitlist = crate::waitlist::offered(plane.read(cx));
    let (detail, button): (String, Option<AnyElement>) = match (&state, &organizations) {
        (SignIn::SignedIn { .. }, Some(Organizations::Failed(error))) => (
            failure(error, "this control plane"),
            Some(
                action("Try again", "plane-retry")
                    .on_click(move |_, _, cx| p.update(cx, |p, cx| p.read_clusters(cx)))
                    .into_any_element(),
            ),
        ),
        (SignIn::SignedIn { account, .. }, Some(Organizations::Listed(_))) => {
            let refresh = p.clone();
            (
                no_organizations(plane.read(cx), account),
                Some(
                    h_flex()
                        .gap(px(space::SM))
                        // The one way on for most people, so the primary button (spec 047).
                        .when(waitlist, |row| {
                            row.child(
                                div()
                                    .debug_selector(|| "waitlist-no-organizations".into())
                                    .child(crate::waitlist::link(
                                        "plane-waitlist",
                                        crate::waitlist::Surface::NoOrganizations,
                                        true,
                                    )),
                            )
                        })
                        .child(
                            action("Refresh", "plane-refresh").on_click(move |_, _, cx| {
                                refresh.update(cx, |p, cx| p.read_clusters(cx))
                            }),
                        )
                        .child(
                            action("Sign out", "plane-sign-out")
                                .on_click(move |_, _, cx| p.update(cx, |p, cx| p.sign_out(cx))),
                        )
                        .into_any_element(),
                ),
            )
        }
        (SignIn::SignedIn { .. }, _) => ("Reading your organizations…".to_owned(), None),
        (SignIn::Finishing { .. }, _) => ("Keeping the sign-in in your keychain…".to_owned(), None),
        (SignIn::Waiting { service }, _) => (
            format!("Finish signing in at {service} in your browser."),
            Some(
                action("Cancel", "plane-cancel")
                    .on_click(move |_, _, cx| p.update(cx, |p, cx| p.cancel_sign_in(cx)))
                    .into_any_element(),
            ),
        ),
        (SignIn::Reading, _) => ("Reading the control plane's sign-in…".to_owned(), None),
        (SignIn::Unavailable(refusal), _) => (
            plane_words(&refusal.why, cloud),
            Some(
                action("Try again", "plane-sign-in-again")
                    .on_click(move |_, _, cx| p.update(cx, |p, cx| p.sign_in(cx)))
                    .into_any_element(),
            ),
        ),
        (SignIn::Offered { why, .. }, _) => (
            why.clone()
                .map(|why| why.to_string())
                .unwrap_or_else(|| signed_out_words(plane.read(cx)).to_owned()),
            Some(
                h_flex()
                    .gap(px(space::SM))
                    // Sign in stays the primary button: invited people are not slowed.
                    .child(
                        ds::action_button("plane-sign-in", "Sign in")
                            .tip(ds::tooltip(format!("Opens {host} in your browser")))
                            .on_click(move |_, _, cx| p.update(cx, |p, cx| p.sign_in(cx))),
                    )
                    .when(waitlist, |row| {
                        row.child(div().debug_selector(|| "waitlist-sign-in".into()).child(
                            crate::waitlist::link(
                                "plane-waitlist",
                                crate::waitlist::Surface::SignIn,
                                false,
                            ),
                        ))
                    })
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
