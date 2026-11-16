//! Model calls for every view (spec 019): one [`Models`] per app holds the OpenRouter key, read
//! from the system keychain, the model each tier runs on from the settings, which tasks the
//! person allowed to send their input, and the list of models the picker offers. A view asks
//! it for a call and shows the answer in its own controls; it never calls OpenRouter itself.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use graph_store::sign_in::{Keychain, PlatformKeychain};
use model::Reasoning;
use serde::de::DeserializeOwned;
use studio_settings::StudioSettings;

pub use model::{Answer, Error, Limits, ModelChoice, Request, Task as ModelTask, Tier};

/// The connection test's answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Probe {
    pub ok: bool,
}

/// What a sentence field shows, empty, while no key is stored: it cannot work, and says so
/// before a sentence is typed (constitution VI). What is needed comes first, since a table's
/// field is cut short in a narrow tab: it shows about 37 characters in a window of the default
/// size and about 16 at its narrowest.
pub const WORDS_NEED_KEY: &str = "OpenRouter key needed · Filter in words";

/// What a control that asks a model says in its tooltip while no key is stored, in place of
/// what its task sends: `what` names the control, as "Filter in words".
pub fn needs_key_tip(what: &str) -> String {
    format!("{what} needs an OpenRouter key. Add one in Settings, under Models.")
}

/// The keychain item the key is kept under (spec 016's mechanism).
const KEYCHAIN_KEY: &str = "https://openrouter.ai";
const KEYCHAIN_USER: &str = "studio";
/// How old the model list may be before it is read again.
const LIST_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether a key is held: not read yet, none, or one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    Reading,
    None,
    Stored,
}

struct GlobalModels(Entity<Models>);

impl Global for GlobalModels {}

pub struct Models {
    client: model::Client,
    keychain: Arc<dyn Keychain>,
    key: Option<String>,
    key_state: KeyState,
    /// The models the picker offers, newest read first.
    choices: Vec<ModelChoice>,
    /// When the list was read (seconds since the epoch), 0 for never: kept rather than read
    /// from disk again on the main thread at every key change.
    read_at: u64,
    /// The list's refresh under way: a key changed again meanwhile starts no second one.
    _refreshing: Option<Task<()>>,
}

/// The cached model list: when it was read, and what.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedList {
    /// What a list of this build holds about each model ([`LIST_VERSION`]); 0 in a file from
    /// before the field.
    #[serde(default)]
    version: u32,
    read_at: u64,
    models: Vec<ModelChoice>,
}

/// 2: each model says how little it can reason. A list kept by an earlier build says nothing
/// of it, which would read as "can be turned off" for every model, so it is not used and the
/// list is read again.
const LIST_VERSION: u32 = 2;

impl Models {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalModels>().0.clone()
    }

    /// The models, once Studio has set them up (not in every test).
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalModels>()
            .map(|global| global.0.clone())
    }

    /// Sets up the client over Studio's HTTP client and reads the key from the keychain. The
    /// model list is read once a key is held, when the one kept is a day old: without a key
    /// nothing goes to OpenRouter (spec 019).
    pub fn init(cx: &mut App) {
        let http = cx.http_client();
        let keychain = PlatformKeychain::start(cx);
        let models = cx.new(|cx| Self::new(model::Client::new(http), keychain, cx));
        cx.set_global(GlobalModels(models));
    }

    /// For tests: a client over `http`, with `key` as if the keychain held it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn init_for_tests(
        http: Arc<dyn http_client::HttpClient>,
        key: Option<&str>,
        cx: &mut App,
    ) -> Entity<Self> {
        let keychain: Arc<dyn Keychain> = Arc::new(graph_store::sign_in::MemoryKeychain::default());
        let models = cx.new(|_| Self {
            client: model::Client::new(http),
            keychain,
            key: key.map(str::to_owned),
            key_state: if key.is_some() {
                KeyState::Stored
            } else {
                KeyState::None
            },
            choices: Vec::new(),
            read_at: 0,
            _refreshing: None,
        });
        cx.set_global(GlobalModels(models.clone()));
        models
    }

    fn new(client: model::Client, keychain: Arc<dyn Keychain>, cx: &mut Context<Self>) -> Self {
        let (read_at, choices) = read_cached_list()
            .map(|list| (list.read_at, list.models))
            .unwrap_or_default();
        Self::with_list(client, keychain, read_at, choices, cx)
    }

    /// [`Self::new`], with the model list as it was last kept: `choices`, read at `read_at`.
    fn with_list(
        client: model::Client,
        keychain: Arc<dyn Keychain>,
        read_at: u64,
        choices: Vec<ModelChoice>,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = keychain.read(KEYCHAIN_KEY.to_owned());
        cx.spawn(async move |this, cx| {
            let key = match read.await {
                Ok(Some((_, secret))) => String::from_utf8(secret).ok().filter(|k| !k.is_empty()),
                Ok(None) => None,
                Err(error) => {
                    log::warn!("the keychain did not answer for the OpenRouter key: {error}");
                    None
                }
            };
            this.update(cx, |this, cx| {
                this.key_state = if key.is_some() {
                    KeyState::Stored
                } else {
                    KeyState::None
                };
                this.key = key;
                this.sync_key_stored(cx);
                // Only now, and only with a key: the list is not read for a person who
                // stored none.
                this.refresh_list_if_stale(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self {
            client,
            keychain,
            key: None,
            key_state: KeyState::Reading,
            choices,
            read_at,
            _refreshing: None,
        }
    }

    pub fn key_state(&self) -> KeyState {
        self.key_state.clone()
    }

    /// Whether a model call would be refused for want of a key: none is stored, or the models
    /// were never set up (a view's test). False while the keychain is still being read, so a
    /// control does not say it needs a key in the first frames of a run that holds one.
    pub fn needs_key(cx: &App) -> bool {
        Self::try_global(cx).is_none_or(|models| models.read(cx).key_state == KeyState::None)
    }

    /// The settings mirror whether a key is held, so the row shows it; the key itself never
    /// leaves the keychain.
    fn sync_key_stored(&self, cx: &mut App) {
        let stored = self.key.is_some();
        if StudioSettings::get(cx).models.key_stored != stored {
            studio_settings::update(cx, move |settings| settings.models.key_stored = stored);
        }
    }

    /// Stores `key` in the keychain and holds it; an empty key removes the one held.
    pub fn set_key(&mut self, key: String, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let key = key.trim().to_owned();
        let write = if key.is_empty() {
            self.keychain.delete(KEYCHAIN_KEY.to_owned())
        } else {
            self.keychain.write(
                KEYCHAIN_KEY.to_owned(),
                KEYCHAIN_USER.to_owned(),
                key.clone().into_bytes(),
            )
        };
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                // The keychain refused the write: the key is held for this run and said so.
                let held = (!key.is_empty()).then_some(key);
                this.key_state = if held.is_some() {
                    KeyState::Stored
                } else {
                    KeyState::None
                };
                this.key = held;
                this.sync_key_stored(cx);
                this.refresh_list_if_stale(cx);
                cx.notify();
            })
            .ok();
            result
        })
    }

    /// Tries the key and the fast model with the smallest call there is: the answer is the
    /// model's name and the time it took, or why it failed.
    pub fn test_connection(&self, cx: &App) -> Task<Result<Answer<Probe>, Error>> {
        static PROBE: ModelTask = ModelTask {
            name: "connection-test",
            sends: "the word ping",
            tier: Tier::Fast,
            limits: Limits {
                input_bytes: 64,
                output_tokens: 20,
                time: Duration::from_secs(15),
            },
        };
        self.call::<Probe>(
            Request {
                task: &PROBE,
                instructions: "Answer with the JSON object only.",
                input: "ping".into(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}},
                    "required": ["ok"],
                    "additionalProperties": false,
                }),
            },
            cx,
        )
    }

    /// The model the settings name for a tier, or the tier's default when they name none.
    pub fn model_for(tier: Tier, cx: &App) -> String {
        // Settings not set up (a view's test): the tier's default.
        let named = cx
            .try_global::<StudioSettings>()
            .map(|settings| match tier {
                Tier::Fast => settings.models.fast.clone(),
                Tier::Careful => settings.models.careful.clone(),
            })
            .filter(|named| !named.trim().is_empty());
        named.unwrap_or_else(|| tier.default_model().to_owned())
    }

    /// Asks the task's model for `request`'s answer, off the main thread. Dropping the task
    /// ends the call. [`Error::NoKey`] until a key is stored.
    pub fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        request: Request<'static>,
        cx: &App,
    ) -> Task<Result<Answer<T>, Error>> {
        let Some(key) = self.key.clone() else {
            return Task::ready(Err(Error::NoKey));
        };
        let model = Self::model_for(request.task.tier, cx);
        let reasoning = self.reasoning_of(&model);
        let client = self.client.clone();
        cx.background_spawn(async move { client.call(&model, &reasoning, &key, request).await })
    }

    /// How little `model` can be made to reason: what the list says of it, or of a tier's
    /// default before the list is read. A model the list does not hold is taken to turn it off.
    fn reasoning_of(&self, model: &str) -> Reasoning {
        if let Some(listed) = self.choices.iter().find(|choice| choice.id == model) {
            return listed.reasoning.clone();
        }
        [Tier::Fast, Tier::Careful]
            .into_iter()
            .find(|tier| tier.default_model() == model)
            .map(Tier::default_reasoning)
            .unwrap_or_default()
    }

    /// The models the picker offers, by name; the two defaults when none were read.
    pub fn choices(&self) -> Vec<ModelChoice> {
        if self.choices.is_empty() {
            return [Tier::Fast, Tier::Careful]
                .into_iter()
                .map(|tier| ModelChoice {
                    id: tier.default_model().to_owned(),
                    name: tier.default_model().to_owned(),
                    prompt_per_million: 0.0,
                    completion_per_million: 0.0,
                    reasoning: tier.default_reasoning(),
                })
                .collect();
        }
        self.choices.clone()
    }

    /// Reads the model list again when the one held is a day old, for a person who holds a
    /// key. Without one nothing is sent to OpenRouter, this request included (spec 019): the
    /// picker then offers the list last read, or the two defaults. The request itself carries
    /// no key.
    fn refresh_list_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.key.is_none() {
            return;
        }
        let fresh = now_secs().saturating_sub(self.read_at) < LIST_AGE.as_secs();
        if (fresh && !self.choices.is_empty()) || self._refreshing.is_some() {
            return;
        }
        let client = self.client.clone();
        self._refreshing = Some(cx.spawn(async move |this, cx| {
            let models = cx
                .background_spawn(async move { client.list_models().await })
                .await;
            match models {
                Ok(models) => {
                    let cached = CachedList {
                        version: LIST_VERSION,
                        read_at: now_secs(),
                        models: models.clone(),
                    };
                    if let Ok(text) = serde_json::to_vec_pretty(&cached) {
                        studio_settings::write_atomic_in_turn(
                            list_path(),
                            text,
                            studio_settings::Access::Private,
                        );
                    }
                    this.update(cx, |this, cx| {
                        this.choices = models;
                        this.read_at = cached.read_at;
                        this._refreshing = None;
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    log::info!("the model list was not read: {error}");
                    this.update(cx, |this, _| this._refreshing = None).ok();
                }
            }
        }));
    }
}

fn list_path() -> std::path::PathBuf {
    studio_settings::data_dir().join("models.json")
}

fn read_cached_list() -> Option<CachedList> {
    let text = std::fs::read(list_path()).ok()?;
    kept_list(&text)
}

/// A kept list, when this build wrote it or one that holds the same about each model.
fn kept_list(text: &[u8]) -> Option<CachedList> {
    serde_json::from_slice::<CachedList>(text)
        .ok()
        .filter(|list| list.version == LIST_VERSION)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::AsyncReadExt as _;
    use gpui::TestAppContext;
    use graph_store::sign_in::MemoryKeychain;
    use http_client::{AsyncBody, FakeHttpClient, Response};

    use super::*;

    const LIST: &str = r#"{"data":[
      {"id":"anthropic/claude-haiku-5.5","name":"Anthropic: Claude Haiku 5.5","pricing":{"prompt":"0.0000001","completion":"0.0000005"},"supported_parameters":["structured_outputs"]}
    ]}"#;

    /// A model service that lists one model and counts how often its list is asked for. It is
    /// asked for nothing else here, and never with the key.
    fn service(reads: Arc<AtomicUsize>) -> model::Client {
        let http = FakeHttpClient::create(move |request| {
            let reads = reads.clone();
            async move {
                assert_eq!(request.uri().to_string(), model::MODELS_ENDPOINT);
                assert!(
                    request.headers().get("Authorization").is_none(),
                    "the list is read without the key"
                );
                reads.fetch_add(1, Ordering::SeqCst);
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(LIST.to_owned()))
                    .unwrap())
            }
        });
        model::Client::new(http)
    }

    /// The models of a person whose keychain holds `key`, with no list kept from before.
    fn models(
        key: Option<&str>,
        reads: &Arc<AtomicUsize>,
        cx: &mut TestAppContext,
    ) -> Entity<Models> {
        models_over(service(reads.clone()), key, cx)
    }

    fn models_over(
        client: model::Client,
        key: Option<&str>,
        cx: &mut TestAppContext,
    ) -> Entity<Models> {
        let keychain = MemoryKeychain::default();
        if let Some(key) = key {
            keychain.items.lock().unwrap().insert(
                KEYCHAIN_KEY.to_owned(),
                (KEYCHAIN_USER.to_owned(), key.as_bytes().to_vec()),
            );
        }
        let keychain: Arc<dyn Keychain> = Arc::new(keychain);
        cx.update(|cx| {
            studio_settings::init_test(cx);
            studio_settings::keep_changes_in_memory(cx);
            cx.new(|cx| Models::with_list(client, keychain, 0, Vec::new(), cx))
        })
    }

    /// A service that lists one model that can turn reasoning off and one that cannot, answers
    /// every call, and keeps each call's body.
    fn answering(bodies: Arc<Mutex<Vec<serde_json::Value>>>) -> model::Client {
        const LIST: &str = r#"{"data":[
          {"id":"anthropic/claude-haiku-5.5","name":"Anthropic: Claude Haiku 5.5","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":false,"supported_efforts":["high","medium","low","none"]}},
          {"id":"google/gemini-3.8-flash","name":"Google: Gemini 3.8 Flash","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":true,"supported_efforts":["high","medium","low"]}}
        ]}"#;
        let http = FakeHttpClient::create(move |request| {
            let bodies = bodies.clone();
            async move {
                if *request.uri() == model::MODELS_ENDPOINT {
                    return Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(LIST.to_owned()))
                        .unwrap());
                }
                let mut sent = String::new();
                request.into_body().read_to_string(&mut sent).await.unwrap();
                bodies
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&sent).unwrap());
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(
                        r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#.to_owned(),
                    ))
                    .unwrap())
            }
        });
        model::Client::new(http)
    }

    /// A short call to a model that cannot turn reasoning off asks for the least effort the
    /// list names for it; one that can is told to. Before, both were told to turn it off, and
    /// OpenRouter refused the first (spec 019, 2026-11-16).
    #[gpui::test]
    async fn a_call_asks_for_the_least_reasoning_its_model_takes(cx: &mut TestAppContext) {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let models = models_over(answering(bodies.clone()), Some("sk-or-test"), cx);
        cx.run_until_parked();
        for (model, reasoning) in [
            (
                "google/gemini-3.8-flash",
                serde_json::json!({"effort": "low"}),
            ),
            (
                "anthropic/claude-haiku-5.5",
                serde_json::json!({"enabled": false}),
            ),
            // One the list does not hold is taken to turn it off, as before.
            ("some/other", serde_json::json!({"enabled": false})),
        ] {
            cx.update(|cx| {
                studio_settings::update(cx, move |settings| settings.models.fast = model.into())
            });
            cx.run_until_parked();
            let answer = cx
                .update(|cx| models.read(cx).test_connection(cx))
                .await
                .unwrap();
            assert!(answer.value.ok);
            let body = bodies.lock().unwrap().pop().unwrap();
            assert_eq!(body["model"], model);
            assert_eq!(body["reasoning"], reasoning, "{model}");
        }
    }

    /// A list kept by a build that did not read how little each model reasons is not used: it
    /// would say every model can turn reasoning off.
    #[test]
    fn a_list_kept_by_an_earlier_build_is_read_again() {
        let models = r#""models":[{"id":"google/gemini-3.8-flash","name":"G","prompt_per_million":0.8,"completion_per_million":4.1}]"#;
        assert!(kept_list(format!(r#"{{"read_at":5,{models}}}"#).as_bytes()).is_none());
        assert!(kept_list(format!(r#"{{"version":1,"read_at":5,{models}}}"#).as_bytes()).is_none());
        let kept = kept_list(format!(r#"{{"version":2,"read_at":5,{models}}}"#).as_bytes());
        assert_eq!(kept.map(|list| list.read_at), Some(5));
    }

    /// Nothing goes to OpenRouter for a person who stored no key, the model list included.
    /// Before, Studio asked for the list at every start whose kept copy was a day old, key or
    /// not, while its settings said "Nothing is sent without a key."
    #[gpui::test]
    async fn without_a_key_the_model_list_is_not_read(cx: &mut TestAppContext) {
        let reads = Arc::new(AtomicUsize::new(0));
        let models = models(None, &reads, cx);
        cx.run_until_parked();
        models.read_with(cx, |models, _| {
            assert_eq!(models.key_state(), KeyState::None);
            assert_eq!(
                models.choices().len(),
                2,
                "the picker offers the two defaults"
            );
        });
        assert_eq!(reads.load(Ordering::SeqCst), 0, "no key, no request");
    }

    /// A key held at start reads the list once, and a list read today is not read again.
    #[gpui::test]
    async fn a_key_held_at_start_reads_the_list_once(cx: &mut TestAppContext) {
        let reads = Arc::new(AtomicUsize::new(0));
        let models = models(Some("sk-or-test"), &reads, cx);
        cx.run_until_parked();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        models.update(cx, |models, cx| {
            assert_eq!(models.key_state(), KeyState::Stored);
            let offered = models.choices();
            assert_eq!(offered.len(), 1);
            assert_eq!(offered[0].id, "anthropic/claude-haiku-5.5");
            models.refresh_list_if_stale(cx);
        });
        cx.run_until_parked();
        assert_eq!(reads.load(Ordering::SeqCst), 1, "a list read today is kept");
    }

    /// A control learns that a key is wanting from one place: none set up, none stored, and
    /// not while the keychain is still being read.
    #[gpui::test]
    async fn a_key_is_wanting_when_none_is_stored(cx: &mut TestAppContext) {
        assert!(cx.update(|cx| Models::needs_key(cx)), "no models set up");
        let reads = Arc::new(AtomicUsize::new(0));
        let models = models(None, &reads, cx);
        cx.update(|cx| {
            cx.set_global(GlobalModels(models.clone()));
            assert!(!Models::needs_key(cx), "the keychain has not answered yet");
        });
        cx.run_until_parked();
        assert!(cx.update(|cx| Models::needs_key(cx)), "it holds no key");

        models
            .update(cx, |models, cx| models.set_key("sk-or-test".into(), cx))
            .await
            .unwrap();
        assert!(!cx.update(|cx| Models::needs_key(cx)), "a key is stored");
        assert_eq!(
            needs_key_tip("Filter in words"),
            "Filter in words needs an OpenRouter key. Add one in Settings, under Models."
        );
    }

    /// Storing a key is what reads the list, and once the key is removed it is not read again,
    /// however old the one kept grows.
    #[gpui::test]
    async fn storing_a_key_reads_the_list_and_removing_it_reads_no_more(cx: &mut TestAppContext) {
        let reads = Arc::new(AtomicUsize::new(0));
        let models = models(None, &reads, cx);
        cx.run_until_parked();
        assert_eq!(reads.load(Ordering::SeqCst), 0);

        models
            .update(cx, |models, cx| models.set_key("sk-or-test".into(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(reads.load(Ordering::SeqCst), 1, "storing the key reads it");

        models
            .update(cx, |models, cx| models.set_key(String::new(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        models.update(cx, |models, cx| {
            assert_eq!(models.key_state(), KeyState::None);
            models.read_at = 0;
            models.refresh_list_if_stale(cx);
            assert_eq!(
                models.choices().len(),
                1,
                "the list last read is still offered"
            );
        });
        cx.run_until_parked();
        assert_eq!(reads.load(Ordering::SeqCst), 1, "no key, no request");
    }
}
