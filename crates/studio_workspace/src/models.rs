//! Model calls for every view (spec 019): one [`Models`] per app holds the OpenRouter key, read
//! from the system keychain, the model each tier runs on from the settings, which tasks the
//! person allowed to send their input, and the list of models the picker offers. A view asks
//! it for a call and shows the answer in its own controls; it never calls OpenRouter itself.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use graph_store::sign_in::{Keychain, PlatformKeychain};
use serde::de::DeserializeOwned;
use studio_settings::StudioSettings;

pub use model::{Answer, Error, Limits, ModelChoice, Request, Task as ModelTask, Tier};

/// The connection test's answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Probe {
    pub ok: bool,
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
}

/// The cached model list: when it was read, and what.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedList {
    read_at: u64,
    models: Vec<ModelChoice>,
}

impl Models {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalModels>().0.clone()
    }

    /// The models, once Studio has set them up (not in every test).
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalModels>()
            .map(|global| global.0.clone())
    }

    /// Sets up the client over Studio's HTTP client, reads the key from the keychain, and
    /// reads the model list when the cached one is a day old.
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
        });
        cx.set_global(GlobalModels(models.clone()));
        models
    }

    fn new(client: model::Client, keychain: Arc<dyn Keychain>, cx: &mut Context<Self>) -> Self {
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
                cx.notify();
            })
            .ok();
        })
        .detach();
        let choices = read_cached_list()
            .map(|list| list.models)
            .unwrap_or_default();
        let mut this = Self {
            client,
            keychain,
            key: None,
            key_state: KeyState::Reading,
            choices,
        };
        this.refresh_list_if_stale(cx);
        this
    }

    pub fn key_state(&self) -> KeyState {
        self.key_state.clone()
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
        let settings = &StudioSettings::get(cx).models;
        let named = match tier {
            Tier::Fast => &settings.fast,
            Tier::Careful => &settings.careful,
        };
        if named.trim().is_empty() {
            tier.default_model().to_owned()
        } else {
            named.clone()
        }
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
        let client = self.client.clone();
        cx.background_spawn(async move { client.call(&model, &key, request).await })
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
                })
                .collect();
        }
        self.choices.clone()
    }

    fn refresh_list_if_stale(&mut self, cx: &mut Context<Self>) {
        let fresh = read_cached_list()
            .is_some_and(|list| now_secs().saturating_sub(list.read_at) < LIST_AGE.as_secs());
        if fresh && !self.choices.is_empty() {
            return;
        }
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_spawn(async move { client.list_models().await })
                .await;
            match models {
                Ok(models) => {
                    let cached = CachedList {
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
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => log::info!("the model list was not read: {error}"),
            }
        })
        .detach();
    }
}

fn list_path() -> std::path::PathBuf {
    studio_settings::data_dir().join("models.json")
}

fn read_cached_list() -> Option<CachedList> {
    let text = std::fs::read(list_path()).ok()?;
    serde_json::from_slice(&text).ok()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
