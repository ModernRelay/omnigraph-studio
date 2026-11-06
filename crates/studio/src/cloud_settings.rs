//! The Omnigraph Cloud page of Settings (spec 021, The settings page): under the control plane's
//! address, the sign-in it holds and every organization it lists, each cluster with its state,
//! its MCP link, data endpoint, id and repository, each copied with a click. The lists are read
//! when the page shows and let go when it no longer does, as the Open dialog's are: Studio keeps
//! no list of its own. A cluster's Manage section reads its state and latest delivery while it is
//! open, and gives the CLI command for each change Studio does not make itself (constitution
//! VI): push, plan, apply and delete.

use control_client::ClusterView;
use gpui::{AnyElement, Context, Entity, EntityId, Subscription};
use graph_store::cli_config::ManagedTarget;
use graph_store::control_plane::ClusterCard;
use graph_store::sign_in::SignIn;
use graph_store::{ConnectionRegistry, ControlPlane, Listing, Organizations};
use studio_ui::CopyableValue;
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{self, Dotted as _, size, space};
use ui::prelude::*;

use crate::organizations;

/// The page: the control planes the registry holds, followed while the tab lives.
pub struct CloudPage {
    planes: Vec<Entity<ControlPlane>>,
    managed: Vec<ManagedTarget>,
    /// Whether Settings shows the page now: only then are the lists read and held.
    shown: bool,
    followed: Vec<(EntityId, Subscription)>,
    /// The cluster whose Manage section is open, at most one.
    managing: Option<Managing>,
    _subscriptions: Vec<Subscription>,
}

/// An open Manage section: its cluster, and the card reading the cluster's state while it is
/// open (none without a sign-in).
struct Managing {
    plane: EntityId,
    cluster_id: String,
    card: Option<Entity<ClusterCard>>,
    _watch: Option<Subscription>,
}

impl CloudPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            planes: Vec::new(),
            managed: Vec::new(),
            shown: false,
            followed: Vec::new(),
            managing: None,
            _subscriptions: Vec::new(),
        };
        if let Some(registry) = ConnectionRegistry::try_global(cx) {
            this._subscriptions
                .push(cx.observe(&registry, |this, registry, cx| this.follow(&registry, cx)));
            this.follow(&registry, cx);
        }
        // Closing Settings lets the lists go.
        this._subscriptions.push(cx.on_release(|this, cx| {
            if this.shown {
                for plane in &this.planes {
                    plane.update(cx, |plane, cx| plane.forget_clusters(cx));
                }
            }
        }));
        this
    }

    /// Follows the registry's control planes and managed targets, drawing again only when they
    /// change.
    fn follow(&mut self, registry: &Entity<ConnectionRegistry>, cx: &mut Context<Self>) {
        let (planes, managed) = {
            let registry = registry.read(cx);
            (
                registry.control_planes.clone(),
                registry.managed_targets.clone(),
            )
        };
        let same = planes.len() == self.planes.len()
            && planes
                .iter()
                .zip(&self.planes)
                .all(|(a, b)| a.entity_id() == b.entity_id());
        if same && managed == self.managed {
            return;
        }
        self.followed
            .retain(|(id, _)| planes.iter().any(|plane| plane.entity_id() == *id));
        for plane in &planes {
            let id = plane.entity_id();
            if self.followed.iter().any(|(followed, _)| *followed == id) {
                continue;
            }
            let watch = cx.observe(plane, |this, plane, cx| {
                if !this.shown {
                    return;
                }
                // A sign-in that landed while the page shows lists its organizations.
                read_if_signed_in(&plane, cx);
                cx.notify();
            });
            self.followed.push((id, watch));
            if self.shown {
                read_if_signed_in(plane, cx);
            }
        }
        self.planes = planes;
        self.managed = managed;
        cx.notify();
    }

    /// Settings shows the page, or stops showing it: the lists are read, or let go.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        if !shown {
            self.managing = None;
        }
        for plane in &self.planes {
            if shown {
                read_if_signed_in(plane, cx);
            } else {
                plane.update(cx, |plane, cx| plane.forget_clusters(cx));
            }
        }
        cx.notify();
    }
}

impl CloudPage {
    /// Opens the Manage section of `cluster_id`, closing any other, or closes it when it is
    /// open: its state is read only while the section shows (spec 021, Limits).
    fn toggle_manage(
        &mut self,
        plane: &Entity<ControlPlane>,
        organization: &str,
        cluster_id: &str,
        cx: &mut Context<Self>,
    ) {
        let open = self.managing.as_ref().is_some_and(|managing| {
            managing.plane == plane.entity_id() && managing.cluster_id == cluster_id
        });
        self.managing = if open {
            None
        } else {
            let card = ControlPlane::card(
                plane,
                Some(organization.to_owned()),
                cluster_id.to_owned(),
                cx,
            );
            let watch = card
                .as_ref()
                .map(|card| cx.observe(card, |_, _, cx| cx.notify()));
            Some(Managing {
                plane: plane.entity_id(),
                cluster_id: cluster_id.to_owned(),
                card,
                _watch: watch,
            })
        };
        cx.notify();
    }
}

fn read_if_signed_in(plane: &Entity<ControlPlane>, cx: &mut gpui::App) {
    plane.update(cx, |plane, cx| {
        if plane.holds_sign_in() && plane.organizations().is_none() {
            plane.read_clusters(cx);
        }
    });
}

impl Render for CloudPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let many = self.planes.len() > 1;
        let planes = self.planes.clone();
        v_flex()
            .w_full()
            .gap(px(space::BETWEEN))
            .when(planes.is_empty(), |page| {
                page.child(ds::text::muted(
                    "Omnigraph Cloud is off, and no CLI profile names a control plane. Set its \
                     address above to sign in.",
                ))
            })
            .children(
                planes
                    .iter()
                    .map(|plane| self.render_plane(plane, many, cx)),
            )
    }
}

impl CloudPage {
    fn render_plane(
        &self,
        plane: &Entity<ControlPlane>,
        many: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (route, cloud, state, profiles, listed) = {
            let p = plane.read(cx);
            (
                p.route.clone(),
                p.is_default(),
                p.sign_in_state().clone(),
                p.profiles.clone(),
                p.organizations().cloned(),
            )
        };
        let id = plane.entity_id().as_u64();
        let name = organizations::plane_name(&route, cloud);
        let account = ds::card(cx)
            .px(px(space::XL))
            .child(account_row(plane, &state, &profiles, id, cx));
        let body: AnyElement = match (&state, listed) {
            (SignIn::SignedIn { .. }, Some(Organizations::Listed(listed))) if listed.is_empty() => {
                ds::text::muted("You are a member of no organization here.").into_any_element()
            }
            (SignIn::SignedIn { .. }, Some(Organizations::Listed(listed))) => v_flex()
                .gap(px(space::BETWEEN))
                .children(listed.iter().map(|organization| {
                    let label = organization
                        .name
                        .clone()
                        .unwrap_or_else(|| organization.organization_id.clone());
                    self.render_organization(
                        plane,
                        &organization.organization_id,
                        label,
                        &profiles,
                        cx,
                    )
                }))
                .into_any_element(),
            (SignIn::SignedIn { .. }, Some(Organizations::Failed(error))) => {
                let p = plane.clone();
                h_flex()
                    .gap(px(space::CONTROLS))
                    .child(ds::text::error_note(organizations::failure(
                        &error,
                        "this control plane",
                    )))
                    .child(ds::row_button(("cloud-retry", id), "Try again").on_click(
                        cx.listener(move |_, _, _, cx| p.update(cx, |p, cx| p.read_clusters(cx))),
                    ))
                    .into_any_element()
            }
            (SignIn::SignedIn { .. }, _) => {
                ds::text::muted("Reading your organizations…").into_any_element()
            }
            _ => div().into_any_element(),
        };
        v_flex()
            .gap(px(space::SM))
            .when(many, |group| group.child(ds::text::meta(name)))
            .child(account)
            .child(body)
            .into_any_element()
    }

    fn render_organization(
        &self,
        plane: &Entity<ControlPlane>,
        organization: &str,
        label: String,
        profiles: &[(String, Option<String>)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let note = organizations::profile_note(profiles, organization);
        let listing = plane.read(cx).listing(organization).cloned();
        // The organization titles its card, in the card's header row, as a card's title is.
        let header = |count: Option<String>, cx: &App| {
            ds::card_header(cx)
                .px(px(space::XL))
                .child(ds::icon::lead(IconName::UserGroup))
                .child(ds::text::body(label.clone()))
                .children(count.map(ds::text::meta))
                .child(div().flex_1())
                .child(ds::text::meta(note.clone()))
        };
        let message = |text: AnyElement| div().px(px(space::XL)).py(px(space::MD)).child(text);
        let card = match listing {
            Some(Listing::Listed(listed)) => {
                let clusters: Vec<&ClusterView> = listed
                    .data
                    .clusters
                    .iter()
                    .filter(|cluster| !organizations::is_purged(cluster))
                    .collect();
                let purged = listed.data.clusters.len() - clusters.len();
                let count = clusters.len();
                let counted = match count {
                    1 => "1 cluster".to_owned(),
                    n => format!("{n} clusters"),
                };
                ds::card(cx)
                    .child(header(Some(counted), cx))
                    .when(count == 0, |card| {
                        card.child(message(
                            ds::text::muted("No clusters in this organization.").into_any_element(),
                        ))
                    })
                    .when(count > 0, |card| {
                        card.child(v_flex().px(px(space::XL)).children(
                            clusters.into_iter().enumerate().map(|(index, cluster)| {
                                let row =
                                    self.cluster_row(plane, organization, profiles, cluster, cx);
                                if index + 1 < count {
                                    row.border_b_1().dotted(cx).into_any_element()
                                } else {
                                    row.into_any_element()
                                }
                            }),
                        ))
                    })
                    .when(purged > 0, |card| {
                        card.child(
                            ds::card_footer(cx)
                                .px(px(space::XL))
                                .child(ds::text::meta(organizations::purged_note(purged))),
                        )
                    })
            }
            Some(Listing::Failed(error)) => ds::card(cx).child(header(None, cx)).child(message(
                ds::text::error_note(organizations::failure(&error, organization))
                    .into_any_element(),
            )),
            _ => ds::card(cx).child(header(None, cx)).child(message(
                ds::text::muted("Reading its clusters…").into_any_element(),
            )),
        };
        card.into_any_element()
    }
}

/// The sign-in's row: what it holds, and Sign in, Cancel or Sign out with Refresh.
fn account_row(
    plane: &Entity<ControlPlane>,
    state: &SignIn,
    profiles: &[(String, Option<String>)],
    id: u64,
    cx: &mut Context<CloudPage>,
) -> AnyElement {
    let host = organizations::oauth_host(&plane.read(cx).route);
    let named = (!profiles.is_empty()).then(|| {
        let named = profiles
            .iter()
            .map(|(profile, _)| profile.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if profiles.len() == 1 {
            format!(" Used by CLI profile {named}.")
        } else {
            format!(" Used by CLI profiles {named}.")
        }
    });
    let p = plane.clone();
    let (description, control): (String, AnyElement) = match state {
        SignIn::SignedIn { service, .. } => {
            let refresh = plane.clone();
            (
                format!(
                    "Signed in at {service}. One sign-in reaches every organization's clusters."
                ),
                h_flex()
                    .gap(px(space::XS))
                    .child(
                        ds::row_icon_button(("cloud-refresh", id), IconName::RotateCw)
                            .tip(ds::tooltip("Read the organizations and clusters again"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                refresh.update(cx, |p, cx| p.read_clusters(cx))
                            })),
                    )
                    .child(
                        ds::button(("cloud-sign-out", id), "Sign out")
                            .style(ButtonStyle::Outlined)
                            .tip(ds::tooltip(
                                "Forgets this sign-in for every organization, and its clusters' \
                                 tokens with it",
                            ))
                            .on_click(
                                cx.listener(move |_, _, _, cx| {
                                    p.update(cx, |p, cx| p.sign_out(cx))
                                }),
                            ),
                    )
                    .into_any_element(),
            )
        }
        SignIn::Waiting { service } => (
            format!("Finish signing in at {service} in your browser."),
            ds::button(("cloud-cancel", id), "Cancel")
                .style(ButtonStyle::Outlined)
                .on_click(
                    cx.listener(move |_, _, _, cx| p.update(cx, |p, cx| p.cancel_sign_in(cx))),
                )
                .into_any_element(),
        ),
        SignIn::Finishing { .. } => (
            "Keeping the sign-in in your keychain…".to_owned(),
            div().into_any_element(),
        ),
        SignIn::Reading => (
            "Reading the control plane's sign-in…".to_owned(),
            div().into_any_element(),
        ),
        SignIn::Unavailable(refusal) => (refusal.why.to_string(), div().into_any_element()),
        SignIn::Offered { why, .. } => (
            why.as_ref().map(|why| why.to_string()).unwrap_or_else(|| {
                "Not signed in. One sign-in lists every organization you are a member of, with \
                 its clusters."
                    .to_owned()
            }),
            ds::action_button(("cloud-sign-in", id), "Sign in")
                .tip(ds::tooltip(format!("Opens {host} in your browser")))
                .on_click(cx.listener(move |_, _, _, cx| p.update(cx, |p, cx| p.sign_in(cx))))
                .into_any_element(),
        ),
        SignIn::NotOffered => (
            format!("{host} offers no sign-in Studio can use."),
            div().into_any_element(),
        ),
    };
    ds::setting_row(
        ("cloud-account", id),
        "Account",
        format!("{description}{}", named.unwrap_or_default()),
        control,
        None,
    )
    .into_any_element()
}

impl CloudPage {
    /// A cluster: its name, state and target, with Copy MCP link and Manage; then its links and
    /// ids, each copied with a click; then, while open, its Manage section.
    fn cluster_row(
        &self,
        plane: &Entity<ControlPlane>,
        organization: &str,
        profiles: &[(String, Option<String>)],
        cluster: &ClusterView,
        cx: &mut Context<Self>,
    ) -> Div {
        let row = organizations::cluster_row(cluster, &self.managed);
        let id: SharedString = cluster.cluster_id.clone().into();
        let mcp = cluster.mcp_url.clone();
        let open = self.managing.as_ref().filter(|managing| {
            managing.plane == plane.entity_id() && managing.cluster_id == cluster.cluster_id
        });
        let toggle = {
            let plane = plane.clone();
            let organization = organization.to_owned();
            let cluster_id = cluster.cluster_id.clone();
            cx.listener(move |this, _, _, cx| {
                this.toggle_manage(&plane, &organization, &cluster_id, cx)
            })
        };
        let manage_selector = format!("manage-{id}");
        let head = h_flex()
            .gap(px(space::SM))
            .child(ds::dot(organizations::state_color(&cluster.state)))
            .child(ds::text::body(row.name.clone()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(ds::text::meta(row.meta.clone()).truncate()),
            )
            .when_some(mcp.clone(), |head, mcp| {
                let selector = format!("copy-mcp-{id}");
                head.child(
                    div().debug_selector(move || selector.clone()).child(
                        ds::row_button(
                            SharedString::from(format!("copy-mcp-{id}")),
                            "Copy MCP link",
                        )
                        .tip(ds::tooltip(mcp.clone()))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(mcp.clone()))
                        }),
                    ),
                )
            })
            .child(
                div().debug_selector(move || manage_selector.clone()).child(
                    ds::row_button(SharedString::from(format!("manage-{id}")), "Manage")
                        .toggle_state(open.is_some())
                        .tip(ds::tooltip("Its state, latest delivery and CLI commands"))
                        .on_click(toggle),
                ),
            );
        let repository = {
            let binding = &cluster.binding;
            let mut at = binding.repository.clone();
            if let Some(path) = &binding.path {
                at.push('/');
                at.push_str(path.trim_start_matches('/'));
            }
            if let Some(r#ref) = &binding.r#ref {
                at.push_str(&format!(" @ {ref}"));
            }
            at
        };
        let section = open.map(|managing| {
            let lines = match &managing.card {
                Some(card) => card.read(cx).lines(),
                None => vec!["Sign in to read the cluster's state.".to_owned()],
            };
            let profile = profiles
                .iter()
                .find(|(_, of)| of.as_deref() == Some(organization))
                .map(|(profile, _)| profile.as_str());
            v_flex()
                .gap(px(space::SM))
                .pt(px(space::SM))
                .child(
                    v_flex()
                        .gap(px(space::WITHIN))
                        .children(lines.into_iter().map(ds::text::muted)),
                )
                .children(
                    commands(cluster, row.target.as_deref(), profile)
                        .into_iter()
                        .map(|(label, key, command)| field(label, key, &id, Some(command))),
                )
        });
        v_flex()
            .w_full()
            .py(px(space::MD))
            .gap(px(space::SM))
            .when(row.dimmed, |row| row.opacity(ds::opacity::DIMMED))
            .child(head)
            .child(field("MCP link", "mcp", &id, mcp))
            .child(field(
                "Endpoint",
                "endpoint",
                &id,
                cluster.data_endpoint.clone(),
            ))
            .child(field(
                "Cluster ID",
                "cluster",
                &id,
                Some(cluster.cluster_id.clone()),
            ))
            .child(field("Repository", "repository", &id, Some(repository)))
            .children(section)
    }
}

/// A labelled value, copied with a click.
fn field(label: &'static str, key: &'static str, id: &str, value: Option<String>) -> Div {
    h_flex()
        .gap(px(space::MD))
        .min_w_0()
        .child(
            div()
                .w(px(size::FIELD_LABEL))
                .flex_none()
                .child(ds::text::meta(label)),
        )
        .child(div().min_w_0().flex_1().child(CopyableValue::new(
            SharedString::from(format!("{key}-{id}")),
            value,
        )))
}

/// The CLI commands that manage `cluster`, each with its label and key (constitution VI): through
/// the CLI target that names it, or, without one, the target `add_command` keeps it as, by its
/// name. Placeholders in angle brackets are the person's to fill.
pub(crate) fn commands(
    cluster: &ClusterView,
    target: Option<&str>,
    profile: Option<&str>,
) -> Vec<(&'static str, &'static str, String)> {
    let named = target.unwrap_or(&cluster.name);
    let mut commands = Vec::new();
    if target.is_none() {
        commands.push((
            "Keep as target",
            "add",
            organizations::add_command(cluster, profile),
        ));
    }
    let cli = |rest: String| format!("omnigraph cluster {rest} --target {named}");
    commands.extend([
        ("Status", "status", cli("status".to_owned())),
        ("History", "history", cli("history".to_owned())),
        (
            "Push",
            "push",
            cli(
                "push --config <DIR> --expected-revision <REVISION> --message \"<MESSAGE>\""
                    .to_owned(),
            ),
        ),
        ("Plan", "plan", cli("plan --rev <REVISION>".to_owned())),
        ("Apply", "apply", cli("apply --plan <PLAN>".to_owned())),
        (
            "Delete",
            "delete",
            cli(format!("delete --incarnation {}", cluster.incarnation)),
        ),
    ]);
    commands
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    /// A control plane signed in to, listing `org_1` (Acme) with the cluster `acme` and its MCP
    /// link.
    fn signed_in_plane(cx: &mut VisualTestContext) -> Entity<ControlPlane> {
        let listed: control_client::Envelope<control_client::ClusterList> =
            serde_json::from_value(serde_json::json!({
                "data": {"clusters": [{
                    "cluster_id": "01C", "name": "acme", "incarnation": "01I",
                    "canonical_root": "s3://b/c",
                    "binding": {"mode": "github", "repository": "a/g", "ref": "main"},
                    "state": "ready", "cell": {"namespace": "ns", "cluster": "cell"},
                    "created_by": "u", "created_at": "2026-11-01T00:00:00Z",
                    "data_endpoint": "https://acme.test",
                    "mcp_url": "https://acme.test/mcp",
                }]},
                "meta": {"provenance": "service_db", "assurance": "observed", "stale": false,
                         "observed_at": "2026-11-05T00:00:00Z"},
            }))
            .unwrap();
        cx.new(|cx| {
            let mut plane = ControlPlane::new("https://control.test".into(), None, cx);
            plane.signed_in_for_test(
                vec![control_client::OrganizationSummary {
                    organization_id: "org_1".into(),
                    name: Some("Acme".into()),
                }],
                vec![("org_1".into(), Listing::Listed(listed))],
                cx,
            );
            plane
        })
    }

    /// The page shows each organization's clusters with a Copy MCP link that copies the
    /// cluster's link, and lets the lists go when Settings stops showing it.
    #[gpui::test]
    fn the_page_copies_a_clusters_mcp_link_and_lets_the_lists_go(cx: &mut TestAppContext) {
        cx.update(studio_theme::init_test);
        let (page, cx): (_, &mut VisualTestContext) =
            cx.add_window_view(|_, cx| CloudPage::new(cx));
        let plane = signed_in_plane(cx);
        page.update(cx, |page, cx| {
            page.planes = vec![plane.clone()];
            page.set_shown(true, cx);
        });
        cx.run_until_parked();
        let copy = cx
            .debug_bounds("copy-mcp-01C")
            .expect("the cluster's Copy MCP link");
        cx.simulate_click(copy.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("https://acme.test/mcp".to_owned())
        );

        // Manage opens the cluster's section, and closes it again.
        let manage = cx.debug_bounds("manage-01C").expect("the cluster's Manage");
        cx.simulate_click(manage.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(page.read_with(cx, |page, _| page.managing.is_some()));
        let manage = cx.debug_bounds("manage-01C").expect("the cluster's Manage");
        cx.simulate_click(manage.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(page.read_with(cx, |page, _| page.managing.is_none()));

        page.update(cx, |page, cx| {
            page.managing = None;
            page.set_shown(false, cx)
        });
        cx.run_until_parked();
        assert!(plane.read_with(cx, |plane, _| plane.organizations().is_none()));
    }

    /// A cluster a target names is managed through that target; one no target names first gets
    /// the command that keeps it as one, in the profile's organization.
    #[test]
    fn each_command_names_the_clusters_target() {
        let cluster: ClusterView = serde_json::from_value(serde_json::json!({
            "cluster_id": "01C", "name": "acme", "incarnation": "01I",
            "canonical_root": "s3://b/c", "binding": {"mode": "github", "repository": "a/g"},
            "state": "ready", "cell": {"namespace": "ns", "cluster": "cell"},
            "created_by": "u", "created_at": "2026-11-01T00:00:00Z",
        }))
        .unwrap();
        let targeted = commands(&cluster, Some("prod"), Some("work"));
        assert_eq!(
            targeted.iter().map(|(_, key, _)| *key).collect::<Vec<_>>(),
            ["status", "history", "push", "plan", "apply", "delete"]
        );
        assert_eq!(
            targeted[5].2,
            "omnigraph cluster delete --incarnation 01I --target prod"
        );
        let untargeted = commands(&cluster, None, Some("work"));
        assert_eq!(
            untargeted[0].2,
            "omnigraph target add acme --cluster 01C --profile work"
        );
        assert_eq!(untargeted[1].2, "omnigraph cluster status --target acme");
    }
}
