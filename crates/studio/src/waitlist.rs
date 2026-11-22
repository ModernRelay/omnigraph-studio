//! Omnigraph Cloud's waitlist (spec 047): where Studio offers it, and the page it opens. Cloud is
//! in private preview and invitations go out from the waitlist, so Studio offers it only where
//! the person is already looking at Cloud's sign-in, or would otherwise be left with no way in.
//! The browser does the asking: Studio opens the page and sends nothing itself (constitution I).

use graph_store::sign_in::SignIn;
use graph_store::{ControlPlane, Organizations};
use studio_ui::ds::Tipped as _;
use studio_ui::ds::{self, Variant};
use ui::prelude::*;

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
}

impl Surface {
    #[cfg(test)]
    pub(crate) const ALL: [Surface; 3] = [
        Surface::GetStarted,
        Surface::SignIn,
        Surface::NoOrganizations,
    ];

    fn content(self) -> &'static str {
        match self {
            Surface::GetStarted => "get-started",
            Surface::SignIn => "sign-in",
            Surface::NoOrganizations => "no-organizations",
        }
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

/// Whether `plane` offers the waitlist: it is Omnigraph Cloud (the plane at the
/// `omnigraph_cloud` address, not one only a CLI profile names), and the person holds no
/// sign-in there that is a member of an organization. A plane signed in to whose organizations
/// are not read yet offers none, so the link never comes and goes while they are read.
pub(crate) fn offered(plane: &ControlPlane) -> bool {
    plane.is_default()
        && match plane.sign_in_state() {
            SignIn::SignedIn { .. } => matches!(
                plane.organizations(),
                Some(Organizations::Listed(listed)) if listed.is_empty()
            ),
            _ => true,
        }
}

/// "Join the waitlist ↗": opens the page for `surface` in the browser. A quiet link beside a
/// primary Sign in, or `primary` where it is the one way on.
pub(crate) fn link(id: impl Into<ElementId>, surface: Surface, primary: bool) -> ds::Button {
    ds::button(id, "Join the waitlist")
        .variant(if primary {
            Variant::Primary
        } else {
            Variant::Ghost
        })
        .end_icon(Icon::new(IconName::ArrowUpRight))
        .tip(ds::tooltip("Opens omnigraph.dev in your browser"))
        .on_click(move |_, _, cx| cx.open_url(&url(surface)))
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    /// Each surface's link is the Cloud page at its form, with the one constant query and its
    /// own `utm_content`, and nothing else.
    #[test]
    fn each_surface_opens_the_page_with_only_its_name() {
        let urls: Vec<String> = Surface::ALL.into_iter().map(url).collect();
        assert_eq!(
            urls,
            [
                "https://www.omnigraph.dev/cloud?utm_source=omnigraph-studio&utm_medium=app\
                 &utm_content=get-started#join",
                "https://www.omnigraph.dev/cloud?utm_source=omnigraph-studio&utm_medium=app\
                 &utm_content=sign-in#join",
                "https://www.omnigraph.dev/cloud?utm_source=omnigraph-studio&utm_medium=app\
                 &utm_content=no-organizations#join",
            ]
        );
        for url in &urls {
            let parsed = url::Url::parse(url).unwrap();
            assert_eq!(parsed.host_str(), Some("www.omnigraph.dev"));
            assert_eq!(parsed.fragment(), Some("join"));
            let keys: Vec<String> = parsed.query_pairs().map(|(key, _)| key.into()).collect();
            assert_eq!(keys, ["utm_source", "utm_medium", "utm_content"]);
        }
    }

    /// Omnigraph Cloud offers the waitlist while signed out, and while signed in to no
    /// organization; not once signed in to one, nor while its organizations are read, and never
    /// on a plane only a CLI profile names.
    #[gpui::test]
    fn the_waitlist_is_offered_only_where_it_answers(cx: &mut TestAppContext) {
        let plane = |cloud: bool, cx: &mut TestAppContext| {
            cx.new(|cx| {
                let mut plane = ControlPlane::new("https://control.test".into(), None, cx);
                plane.set_default_for_test(cloud, cx);
                plane
            })
        };
        let cloud = plane(true, cx);
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

        let elsewhere = plane(false, cx);
        assert!(!elsewhere.read_with(cx, |p, _| offered(p)), "another plane");
    }
}
