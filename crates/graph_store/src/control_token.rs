//! A managed cluster's data token from its control plane (spec 021, The data token), in place of
//! the CLI's `credentials export`: asked for while the profile's control plane has a sign-in
//! held, with the CLI's export as the source otherwise.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use control_client::{ControlClient, ControlError, DataCredential, Envelope};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use http_client::HttpClient;

use crate::cli_tokens::{Lease, NoToken, Token, TokenSource};
use crate::sign_in::OAuthSource;

/// How long before a data token ends the next is asked for (spec 021, Limits).
const RENEW_LEAD: Duration = Duration::from_secs(60);
/// After a cluster refused a data token, the next is asked for no sooner than this after the
/// last (spec 021, Limits).
const REFUSED_PAUSE: Duration = Duration::from_secs(60);

/// What a control plane shares with its clusters' token sources: where it is, and the sign-in it
/// holds now, which a sign-in, a sign-out or a launch's kept item changes for all of them.
#[derive(Clone)]
pub struct PlaneHandle {
    pub(crate) route: String,
    pub(crate) http: Arc<dyn HttpClient>,
    pub(crate) held: Arc<Mutex<Option<OAuthSource>>>,
}

impl PlaneHandle {
    pub(crate) fn new(route: String, http: Arc<dyn HttpClient>) -> Self {
        Self {
            route,
            http,
            held: Arc::default(),
        }
    }

    pub(crate) fn hold(&self, source: Option<OAuthSource>) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = source;
    }

    fn held(&self) -> Option<OAuthSource> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// A managed cluster's token source: the control plane's data token while it holds a sign-in,
/// else the CLI's. To the connection it is the managed target's login, as the CLI's is, so the
/// cluster's own sign-in is still preferred where its 401 offers one (spec 016).
pub(crate) struct ControlTokenSource {
    /// The CLI's export for the target, when a CLI target names the cluster.
    cli: Option<Arc<dyn TokenSource>>,
    /// The profile of a cluster no CLI target names: its control plane's, when one names it.
    profile: Option<String>,
    /// The organization the cluster is in, which each request names.
    organization: Option<String>,
    /// The control plane that gives its token.
    plane: PlaneHandle,
    cluster_id: String,
    /// The cluster's data endpoint: a token for any other is dropped.
    endpoint: String,
    /// When a data token was last asked for.
    asked: Mutex<Option<SystemTime>>,
    /// The cluster refused the last token given.
    refused: AtomicBool,
}

impl ControlTokenSource {
    /// The source of a CLI target in `organization` whose export is `cli`, while its control
    /// plane holds no sign-in.
    pub(crate) fn new(
        cli: Arc<dyn TokenSource>,
        plane: PlaneHandle,
        organization: Option<String>,
        cluster_id: String,
        endpoint: String,
    ) -> Self {
        Self::with(Some(cli), None, organization, plane, cluster_id, endpoint)
    }

    /// The source of a cluster no CLI target names, in `organization`: only the control plane
    /// gives its token.
    pub(crate) fn of_plane(
        profile: Option<String>,
        organization: String,
        plane: PlaneHandle,
        cluster_id: String,
        endpoint: String,
    ) -> Self {
        Self::with(
            None,
            profile,
            Some(organization),
            plane,
            cluster_id,
            endpoint,
        )
    }

    fn with(
        cli: Option<Arc<dyn TokenSource>>,
        profile: Option<String>,
        organization: Option<String>,
        plane: PlaneHandle,
        cluster_id: String,
        endpoint: String,
    ) -> Self {
        Self {
            cli,
            profile,
            organization,
            plane,
            cluster_id,
            endpoint,
            asked: Mutex::new(None),
            refused: AtomicBool::new(false),
        }
    }
}

impl TokenSource for ControlTokenSource {
    fn token(&self, hurry: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
        let Some(source) = self.plane.held() else {
            return match &self.cli {
                Some(cli) => cli.token(hurry),
                None => futures::future::ready(Err(no_sign_in())).boxed(),
            };
        };
        let now = SystemTime::now();
        {
            let mut asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
            let refused = self.refused.swap(false, Ordering::SeqCst);
            if refused
                && asked
                    .is_some_and(|at| now.duration_since(at).unwrap_or_default() < REFUSED_PAUSE)
            {
                return futures::future::ready(Err(NoToken {
                    why: "The cluster refused the control plane's token a moment ago; Studio asks \
                          again in a minute"
                        .into(),
                    passing: true,
                }))
                .boxed();
            }
            *asked = Some(now);
        }
        let plane = self.plane.clone();
        let organization = self.organization.clone();
        let cluster_id = self.cluster_id.clone();
        let endpoint = self.endpoint.clone();
        async move {
            let organization = organization.as_deref();
            let mut answer = issue(&plane, &source, organization, &cluster_id, &endpoint).await;
            // One silent refresh of the control plane's own token (spec 021, Errors).
            if matches!(answer, Err(ControlError::Unauthenticated(_))) {
                source.note_refused();
                answer = issue(&plane, &source, organization, &cluster_id, &endpoint).await;
            }
            let answer = answer.map_err(|error| NoToken {
                why: format!("The control plane gave no data token: {error}"),
                passing: error.is_passing(),
            })?;
            leased(answer.data)
        }
        .boxed()
    }

    /// Held back as a managed login is, whether or not a profile names it: a cluster whose 401
    /// offers its own sign-in is signed in to instead (spec 016).
    fn waits_for_server_sign_in(&self) -> bool {
        true
    }

    fn login_profile(&self) -> Option<String> {
        match &self.cli {
            Some(cli) => cli.login_profile(),
            None => self.profile.clone(),
        }
    }

    fn organization(&self) -> Option<String> {
        match &self.cli {
            Some(cli) => cli.organization(),
            None => self.organization.clone(),
        }
    }

    fn organization_unread(&self) -> bool {
        self.cli
            .as_ref()
            .is_some_and(|cli| cli.organization_unread())
    }

    fn note_refused(&self) {
        self.refused.store(true, Ordering::SeqCst);
        if let Some(cli) = &self.cli {
            cli.note_refused();
        }
    }
}

fn no_sign_in() -> NoToken {
    NoToken {
        why: "Sign in to the organization's control plane to reach this cluster".into(),
        passing: false,
    }
}

/// One `POST /v1/clusters/{id}/tokens`, with the control plane's access token. Never retried
/// here: a lost answer is asked for again only by the next read that needs a token.
async fn issue(
    plane: &PlaneHandle,
    source: &OAuthSource,
    organization: Option<&str>,
    cluster_id: &str,
    endpoint: &str,
) -> Result<Envelope<DataCredential>, ControlError> {
    let access = source
        .token(Arc::new(AtomicBool::new(false)))
        .await
        .map_err(|no| {
            if no.passing {
                ControlError::Unavailable {
                    status: None,
                    why: no.why,
                }
            } else {
                ControlError::Unauthenticated(None)
            }
        })?;
    let mut client = ControlClient::new(plane.http.clone(), &plane.route, access.into_string())?;
    if let Some(organization) = organization {
        client = client.in_organization(organization);
    }
    client.data_token(cluster_id, endpoint).await
}

/// The data token as one the connection renews a minute before it ends.
fn leased(credential: DataCredential) -> Result<Token, NoToken> {
    let expires_at = chrono::DateTime::parse_from_rfc3339(&credential.expires_at)
        .map(SystemTime::from)
        .map_err(|_| NoToken {
            why: "The control plane gave a data token that does not say when it ends".into(),
            passing: false,
        })?;
    let now = SystemTime::now();
    let lives = expires_at.duration_since(now).unwrap_or_default();
    let renew_at = (expires_at - RENEW_LEAD.min(lives / 2)).max(now + Duration::from_secs(1));
    Ok(Token::leased(
        credential.token,
        Lease {
            expires_at,
            renew_at,
            holder: credential.actor,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::AsyncReadExt as _;
    use http_client::http::Response;
    use http_client::{AsyncBody, FakeHttpClient};
    use serde_json::json;

    use super::*;

    /// The CLI's export: one token, counted.
    struct Cli(Arc<Mutex<u32>>);

    impl TokenSource for Cli {
        fn token(&self, _: Arc<AtomicBool>) -> BoxFuture<'static, Result<Token, NoToken>> {
            *self.0.lock().unwrap() += 1;
            futures::future::ready(Ok(Token::typed("from-the-cli".into()))).boxed()
        }

        fn login_profile(&self) -> Option<String> {
            Some("work".into())
        }
    }

    fn control(asked: Arc<Mutex<Vec<String>>>, endpoint: &'static str) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |request| {
            let asked = asked.clone();
            async move {
                let url = request.uri().to_string();
                let mut body = String::new();
                request.into_body().read_to_string(&mut body).await?;
                asked.lock().unwrap().push(format!("{url} {body}"));
                let expires = (chrono::Utc::now() + chrono::Duration::seconds(900)).to_rfc3339();
                Ok(Response::builder().status(200).body(AsyncBody::from(
                    json!({"data": {
                        "version": 2, "token": "data-token", "endpoint": endpoint,
                        "expires_at": expires, "kid": "k", "actor": "user_1",
                    }, "meta": {
                        "provenance": "service_db", "assurance": "observed", "stale": false,
                        "observed_at": "2026-11-05T00:00:00Z",
                    }})
                    .to_string(),
                ))?)
            }
        })
    }

    #[test]
    fn without_a_sign_in_the_clis_export_is_the_source() {
        let exported = Arc::new(Mutex::new(0));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let plane = PlaneHandle::new(
            "https://control.test".into(),
            control(asked.clone(), "https://acme.test"),
        );
        let source = ControlTokenSource::new(
            Arc::new(Cli(exported.clone())),
            plane,
            Some("org_1".into()),
            "01C".into(),
            "https://acme.test".into(),
        );
        let token = smol::block_on(source.token(Arc::default())).unwrap();
        assert_eq!(token.into_string(), "from-the-cli");
        assert_eq!(*exported.lock().unwrap(), 1);
        assert!(asked.lock().unwrap().is_empty());
        // Still the managed target's login to the connection.
        assert_eq!(source.login_profile().as_deref(), Some("work"));
    }

    #[test]
    fn a_data_token_is_leased_until_a_minute_before_it_ends() {
        let expires = (chrono::Utc::now() + chrono::Duration::seconds(900)).to_rfc3339();
        let token = leased(DataCredential {
            version: 2,
            token: "t".into(),
            endpoint: "https://acme.test".into(),
            expires_at: expires,
            kid: "k".into(),
            actor: "user_1".into(),
        })
        .unwrap();
        let lease = token.lease().unwrap().clone();
        assert_eq!(lease.holder, "user_1");
        let lead = lease.expires_at.duration_since(lease.renew_at).unwrap();
        assert_eq!(lead, RENEW_LEAD);
        assert!(
            leased(DataCredential {
                version: 2,
                token: "t".into(),
                endpoint: "https://acme.test".into(),
                expires_at: "tomorrow".into(),
                kid: "k".into(),
                actor: "user_1".into(),
            })
            .is_err()
        );
    }
}
