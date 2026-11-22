//! Omnigraph Cloud's waitlist (spec 047): the one place that says where Studio offers it, in
//! which words, and the page it opens. Cloud is in private preview and invitations go out from
//! the waitlist. Every place that offers it uses this module's rule, text and link, so they stay
//! the same. The copy states the fact and the action, never a pitch. The browser does the asking:
//! Studio opens the page and sends nothing itself (constitution I).

use gpui::{AnyElement, App};
use graph_store::sign_in::SignIn;
use graph_store::{ConnectionRegistry, ControlPlane, Organizations};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{self, Variant, space};
use ui::prelude::*;

/// What Omnigraph Cloud is today, said in full: the sign-in pane and Settings.
pub(crate) const PREVIEW: &str =
    "Omnigraph Cloud is in private preview. Invitations go out from the waitlist.";
/// The same, short, before a link: the About window, the local recipe and the Copy Link toast.
pub(crate) const PREVIEW_SHORT: &str = "Omnigraph Cloud: private preview";
/// The same under Omnigraph Cloud's own name: its sidebar row.
pub(crate) const PREVIEW_UNDER_NAME: &str = "Private preview";
/// The link's label everywhere.
pub(crate) const JOIN: &str = "Join the waitlist";

/// The place in Studio a waitlist link is shown, which its page address names
/// (`utm_content`), and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// The Omnigraph Cloud row of the Open dialog's Get started.
    GetStarted,
    /// Omnigraph Cloud's sign-in pane in the Open dialog.
    SignIn,
    /// A sign-in in no organization, in the Open dialog or Settings → Clusters.
    NoOrganizations,
    /// Omnigraph Cloud's row in the Open dialog's sidebar.
    Sidebar,
    /// Settings → Omnigraph Cloud, signed out.
    Settings,
    /// The toast after copying a link to a server on this computer.
    CopyLink,
    /// The Help menu, the palette and the key (`studio::JoinCloudWaitlist`).
    Help,
    /// The About window.
    About,
    /// Under the steps for a graph on this computer.
    LocalRecipe,
}

impl Surface {
    pub(crate) const ALL: [Surface; 9] = [
        Surface::GetStarted,
        Surface::SignIn,
        Surface::NoOrganizations,
        Surface::Sidebar,
        Surface::Settings,
        Surface::CopyLink,
        Surface::Help,
        Surface::About,
        Surface::LocalRecipe,
    ];

    pub(crate) fn content(self) -> &'static str {
        match self {
            Surface::GetStarted => "get-started",
            Surface::SignIn => "sign-in",
            Surface::NoOrganizations => "no-organizations",
            Surface::Sidebar => "sidebar",
            Surface::Settings => "settings",
            Surface::CopyLink => "copy-link",
            Surface::Help => "help",
            Surface::About => "about",
            Surface::LocalRecipe => "local-recipe",
        }
    }

    /// The surface an action names (`JoinCloudWaitlist::from`): Help, unless it names
    /// another by its `utm_content`. Anything else is Help, so the address never carries text
    /// from outside this list.
    pub(crate) fn named(from: Option<&str>) -> Surface {
        from.and_then(|from| {
            Surface::ALL
                .into_iter()
                .find(|surface| surface.content() == from)
        })
        .unwrap_or(Surface::Help)
    }
}

/// The page a link on `surface` opens: Omnigraph Cloud's page at its form, with a constant
/// query. It holds no version, no ID and nothing about the person or their graphs.
pub(crate) fn url(surface: Surface) -> String {
    format!(
        "{}?utm_source=omnigraph-studio&utm_medium=app&utm_content={}#join",
        studio_settings::OMNIGRAPH_CLOUD_WAITLIST,
        surface.content()
    )
}

/// Whether `plane` offers the waitlist, the one rule every place follows: it is Omnigraph Cloud
/// (the plane at the `omnigraph_cloud` address) at Omnigraph Cloud's own address, not another
/// control plane the setting or a CLI profile names, and the person holds no sign-in there that
/// is a member of an organization. A plane signed in to whose organizations are not read yet
/// offers none, so the link never comes and goes while they are read.
pub(crate) fn offered(plane: &ControlPlane) -> bool {
    plane.is_default()
        && is_cloud_address(&plane.route)
        && match plane.sign_in_state() {
            SignIn::SignedIn { .. } => matches!(
                plane.organizations(),
                Some(Organizations::Listed(listed)) if listed.is_empty()
            ),
            _ => true,
        }
}

/// Whether `route` is Omnigraph Cloud's own address (`OMNIGRAPH_CLOUD`), whatever its last `/`.
pub(crate) fn is_cloud_address(route: &str) -> bool {
    route.trim().trim_end_matches('/') == studio_settings::OMNIGRAPH_CLOUD.trim_end_matches('/')
}

/// Whether Studio offers the waitlist at all now: [`offered`] for the registry's Omnigraph Cloud
/// plane; never while Cloud is off, when there is none. What places outside a plane's own views
/// follow: Help, About, the local recipe, the Copy Link toast.
pub(crate) fn offered_now(cx: &App) -> bool {
    ConnectionRegistry::try_global(cx).is_some_and(|registry| {
        registry
            .read(cx)
            .control_planes
            .iter()
            .any(|plane| offered(plane.read(cx)))
    })
}

/// "Join the waitlist ↗": opens the page for `surface` in the browser. A quiet link beside a
/// primary Sign in, or `primary` where it is the one way on.
pub(crate) fn link(id: impl Into<ElementId>, surface: Surface, primary: bool) -> ds::Button {
    opens(
        ds::button(id, JOIN).variant(if primary {
            Variant::Primary
        } else {
            Variant::Ghost
        }),
        surface,
    )
}

/// `button` as the waitlist's link: the external-link icon at its end, its tooltip, and the page
/// for `surface` on a click.
fn opens(button: ds::Button, surface: Surface) -> ds::Button {
    button
        .end_icon(Icon::new(IconName::ArrowUpRight))
        .tip(ds::tooltip("Opens omnigraph.dev in your browser"))
        .on_click(move |_, _, cx| cx.open_url(&url(surface)))
}

/// A muted line, `lead · Join the waitlist ↗`, under what it is about: the sidebar row, the
/// About window, the local recipe. Found in tests by `waitlist-<utm_content>`.
pub(crate) fn note(surface: Surface, lead: &'static str) -> AnyElement {
    let selector = format!("waitlist-{}", surface.content());
    h_flex()
        .debug_selector(move || selector.clone())
        .flex_wrap()
        .gap(px(space::XS))
        .items_center()
        .child(ds::text::meta(lead))
        .child(ds::text::meta("·"))
        .child({
            let id = format!("waitlist-link-{}", surface.content());
            div()
                .debug_selector({
                    let id = id.clone();
                    move || id.clone()
                })
                .child(opens(
                    ds::link_button(SharedString::from(id), JOIN),
                    surface,
                ))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    /// Each surface's link is the Cloud page at its form, with the one constant query and its
    /// own `utm_content`, and nothing else.
    #[test]
    fn each_surface_opens_the_page_with_only_its_name() {
        let contents: Vec<&str> = Surface::ALL.into_iter().map(Surface::content).collect();
        assert_eq!(
            contents,
            [
                "get-started",
                "sign-in",
                "no-organizations",
                "sidebar",
                "settings",
                "copy-link",
                "help",
                "about",
                "local-recipe",
            ]
        );
        assert_eq!(
            url(Surface::GetStarted),
            "https://www.omnigraph.dev/cloud?utm_source=omnigraph-studio&utm_medium=app\
             &utm_content=get-started#join"
        );
        for surface in Surface::ALL {
            let parsed = url::Url::parse(&url(surface)).unwrap();
            assert_eq!(parsed.host_str(), Some("www.omnigraph.dev"));
            assert_eq!(parsed.path(), "/cloud");
            assert_eq!(parsed.fragment(), Some("join"));
            let pairs: Vec<(String, String)> = parsed
                .query_pairs()
                .map(|(key, value)| (key.into(), value.into()))
                .collect();
            assert_eq!(
                pairs,
                [
                    ("utm_source".into(), "omnigraph-studio".into()),
                    ("utm_medium".into(), "app".into()),
                    ("utm_content".into(), surface.content().into()),
                ]
            );
        }
    }

    /// An action names its surface by `utm_content`; anything else is Help.
    #[test]
    fn an_action_names_only_a_known_surface() {
        assert_eq!(Surface::named(None), Surface::Help);
        assert_eq!(Surface::named(Some("copy-link")), Surface::CopyLink);
        assert_eq!(Surface::named(Some("you@example.com")), Surface::Help);
    }

    /// The copy states facts: no question, no exclamation, no pitch.
    #[test]
    fn the_copy_is_plain() {
        for text in [PREVIEW, PREVIEW_SHORT, PREVIEW_UNDER_NAME, JOIN] {
            assert!(!text.contains('?') && !text.contains('!'), "{text}");
            assert!(!text.to_lowercase().contains("for you"), "{text}");
        }
    }

    fn plane(route: &str, cloud: bool, cx: &mut TestAppContext) -> gpui::Entity<ControlPlane> {
        let route = route.to_owned();
        cx.new(|cx| {
            let mut plane = ControlPlane::new(route, None, cx);
            plane.set_default_for_test(cloud, cx);
            plane
        })
    }

    /// Omnigraph Cloud offers the waitlist while signed out, and while signed in to no
    /// organization; not once signed in to one, nor while its organizations are read; never on
    /// another address, or on a plane only a CLI profile names.
    #[gpui::test]
    fn the_waitlist_is_offered_only_where_it_answers(cx: &mut TestAppContext) {
        let cloud = plane(studio_settings::OMNIGRAPH_CLOUD, true, cx);
        assert!(cloud.read_with(cx, |p, _| offered(p)), "signed out");
        cloud.update(cx, |p, cx| p.signed_in_for_test(Vec::new(), Vec::new(), cx));
        assert!(cloud.read_with(cx, |p, _| offered(p)), "in no organization");
        cloud.update(cx, |p, cx| {
            p.signed_in_for_test(
                vec![control_client::OrganizationSummary {
                    organization_id: "org_1".into(),
                    name: Some("Acme".into()),
                }],
                Vec::new(),
                cx,
            )
        });
        assert!(
            !cloud.read_with(cx, |p, _| offered(p)),
            "in an organization"
        );
        cloud.update(cx, |p, cx| p.forget_clusters(cx));
        assert!(
            !cloud.read_with(cx, |p, _| offered(p)),
            "signed in, its organizations not read"
        );

        let slash = plane("https://platform.omnigraph.dev/", true, cx);
        assert!(
            slash.read_with(cx, |p, _| offered(p)),
            "a last / is the same"
        );
        let elsewhere = plane("http://127.0.0.1:8874", true, cx);
        assert!(
            !elsewhere.read_with(cx, |p, _| offered(p)),
            "the setting at another address"
        );
        let profile = plane(studio_settings::OMNIGRAPH_CLOUD, false, cx);
        assert!(
            !profile.read_with(cx, |p, _| offered(p)),
            "a plane a CLI profile names"
        );
    }
}
