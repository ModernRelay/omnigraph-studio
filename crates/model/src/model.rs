//! Model calls through OpenRouter (spec 019): one way for any feature to ask a model for a typed
//! answer. A feature declares a [`Task`] (its name, what it sends, its tier and caps), builds
//! the answer's schema with `schemars`, and gets an [`Answer`] parsed as its type. This is the
//! only crate that makes a request to OpenRouter (constitution I). It holds no key and reads no
//! setting: the caller gives both with each call. Nothing is logged but a task's name, the
//! status code and the time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request as HttpRequest};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const MODELS_ENDPOINT: &str = "https://openrouter.ai/api/v1/models";
/// Where OpenRouter's usage page attributes Studio's calls.
const REFERER: &str = "https://omnigraph.dev/studio";
const TITLE: &str = "Omnigraph Studio";
/// The most of an answer body that is read: a schema-bound answer is small, and a body past
/// this is an error whatever it holds.
const MAX_ANSWER_BYTES: usize = 256 * 1024;
/// The model list's cap, larger than a call's answer (a few megabytes of descriptions), and
/// its time: a request that hangs gives up, as a call does.
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
const LIST_TIME: Duration = Duration::from_secs(30);

/// Which model setting a task uses: fast answers of a line or two, or careful ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Fast,
    Careful,
}

impl Tier {
    /// The default model of the tier: the ones measured on 2026-10-29 (spec 019).
    pub fn default_model(self) -> &'static str {
        match self {
            Tier::Fast => "anthropic/claude-haiku-5.5",
            Tier::Careful => "anthropic/claude-sonnet-5.5",
        }
    }

    /// How little the tier's default model can reason, for a call made before the model list
    /// was read: what OpenRouter's list said of each on 2026-11-16.
    pub fn default_reasoning(self) -> Reasoning {
        match self {
            Tier::Fast => Reasoning::Optional,
            Tier::Careful => Reasoning::AtLeast("low".into()),
        }
    }

    /// The caps a task of this tier has unless it names its own.
    pub const fn limits(self) -> Limits {
        match self {
            Tier::Fast => Limits {
                input_bytes: 8 * 1024,
                output_tokens: 300,
                time: Duration::from_secs(10),
            },
            Tier::Careful => Limits {
                input_bytes: 64 * 1024,
                output_tokens: 1200,
                time: Duration::from_secs(60),
            },
        }
    }
}

/// A task's caps: its input in bytes, its answer in tokens, and the call's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub input_bytes: usize,
    pub output_tokens: u32,
    pub time: Duration,
}

/// A kind of work a feature asks a model for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Task {
    /// Stable, kebab-case: the consent list's key.
    pub name: &'static str,
    /// What the person is told leaves the machine for it, as a sentence fragment: "the type's
    /// property names and your request".
    pub sends: &'static str,
    pub tier: Tier,
    pub limits: Limits,
}

/// One call: the task, its fixed instructions, the data, and the answer's shape.
pub struct Request<'a> {
    pub task: &'a Task,
    /// Fixed per task; never holds data.
    pub instructions: &'a str,
    /// The data: the request text, the schema excerpt, the condensed changes.
    pub input: String,
    /// The answer's shape; sent under `strict: true`.
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer<T> {
    pub value: T,
    /// The model that answered, as OpenRouter names it.
    pub model: String,
    pub cost_usd: Option<f64>,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No key is stored.
    NoKey,
    /// The server refused: the status and its message, for the view to show once.
    Refused(u16, String),
    /// The body was not an answer in the schema: the first 200 characters.
    NotInSchema(String),
    /// The input is over the task's byte cap; nothing was sent.
    TooLarge,
    /// The call passed the task's time cap.
    Timeout,
    Network(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoKey => write!(
                f,
                "No OpenRouter key is stored. Add one in Settings, under Models."
            ),
            Error::Refused(status, message) => {
                write!(f, "OpenRouter answered {status}: {message}")
            }
            Error::NotInSchema(text) => {
                write!(
                    f,
                    "The model's answer was not in the shape asked for: {text}"
                )
            }
            Error::TooLarge => write!(f, "Too much to send: the input is over this task's cap."),
            Error::Timeout => write!(f, "The model did not answer in time."),
            Error::Network(message) => write!(f, "OpenRouter could not be reached: {message}"),
        }
    }
}

/// How little a model can be made to reason, as OpenRouter's model list says
/// (`reasoning.mandatory` and `reasoning.supported_efforts`). A short answer wants none, and
/// some models cannot turn it off: asked to, OpenRouter answers 400, "Reasoning is mandatory
/// for this endpoint and cannot be disabled". Of the 304 models the picker listed on
/// 2026-11-16, 90 could not, Claude Sonnet 5.5 and Gemini 3.8 Flash among them.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    /// It can be turned off.
    #[default]
    Optional,
    /// It cannot: this is the least effort the model takes (`low`, `minimal`).
    AtLeast(String),
    /// It cannot, and the list names no effort to ask for.
    Always,
}

/// OpenRouter's efforts from the least, to pick a model's least by: a list is said to come
/// highest first, which is not relied on.
const EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

impl Reasoning {
    /// What the list says of a model: whether reasoning is mandatory, and the efforts it takes.
    fn listed(mandatory: bool, efforts: &[String]) -> Self {
        if !mandatory {
            return Reasoning::Optional;
        }
        // `none` is turning it off, which a mandatory model refuses. An effort this does not
        // know ranks last, so a known one is preferred.
        let least = efforts
            .iter()
            .filter(|effort| *effort != "none")
            .min_by_key(|effort| {
                EFFORTS
                    .iter()
                    .position(|known| known == effort)
                    .unwrap_or(EFFORTS.len())
            });
        match least {
            Some(effort) => Reasoning::AtLeast(effort.clone()),
            None => Reasoning::Always,
        }
    }

    /// The request's `reasoning` for the least the model allows.
    fn least(&self) -> Value {
        match self {
            Reasoning::Optional => json!({"enabled": false}),
            Reasoning::AtLeast(effort) => json!({"effort": effort}),
            Reasoning::Always => json!({"enabled": true}),
        }
    }
}

/// A model the picker offers: its id, name, prices per million tokens, and how little it can
/// be made to reason.
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
    pub prompt_per_million: f64,
    pub completion_per_million: f64,
    #[serde(default)]
    pub reasoning: Reasoning,
}

impl ModelChoice {
    /// "Claude Haiku 5.5 · $0.10/$0.50 per million".
    pub fn label(&self) -> String {
        format!(
            "{} · ${:.2}/${:.2} per million",
            self.name, self.prompt_per_million, self.completion_per_million
        )
    }
}

/// The client: one per app, over Studio's HTTP client.
#[derive(Clone)]
pub struct Client {
    http: Arc<dyn HttpClient>,
    endpoint: Arc<str>,
    models_endpoint: Arc<str>,
}

impl Client {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            endpoint: ENDPOINT.into(),
            models_endpoint: MODELS_ENDPOINT.into(),
        }
    }

    /// Asks `model` for `request`'s answer with `key`. `reasoning` is how little the model
    /// can be made to reason, from the model list. Dropping the future ends the call. The
    /// answer is parsed as `T`; a body that is not JSON in the schema is [`Error::NotInSchema`],
    /// since OpenRouter says schema enforcement varies by provider.
    pub async fn call<T: DeserializeOwned>(
        &self,
        model: &str,
        reasoning: &Reasoning,
        key: &str,
        request: Request<'_>,
    ) -> Result<Answer<T>, Error> {
        let task = request.task;
        if key.is_empty() {
            return Err(Error::NoKey);
        }
        if request.input.len() > task.limits.input_bytes {
            return Err(Error::TooLarge);
        }
        let body = json!({
            "model": model,
            "messages": [
                {"role": "system", "content": request.instructions},
                {"role": "user", "content": request.input},
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": task.name.replace('-', "_"), "strict": true, "schema": request.schema},
            },
            "max_tokens": task.limits.output_tokens,
            "provider": {"require_parameters": true, "data_collection": "deny"},
            // A fast task's answer is a few lines: reasoning would spend its token cap before
            // the answer (Haiku reasons by default, and the thinking counts). So it is turned
            // off, or down to the least a model takes when it cannot be turned off.
            "reasoning": match task.tier {
                Tier::Fast => reasoning.least(),
                Tier::Careful => json!({"enabled": true}),
            },
        });
        let http_request = HttpRequest::post(self.endpoint.as_ref())
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", REFERER)
            .header("X-Title", TITLE)
            .body(AsyncBody::from(body.to_string()))
            .map_err(|error| Error::Network(error.to_string()))?;
        let started = Instant::now();
        let http = self.http.clone();
        let work = async move {
            let mut response = http
                .send(http_request)
                .await
                .map_err(|error| Error::Network(error.to_string()))?;
            let status = response.status();
            let mut bytes = Vec::new();
            response
                .body_mut()
                .take(MAX_ANSWER_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| Error::Network(error.to_string()))?;
            Ok::<_, Error>((status, bytes))
        };
        let timer = smol::Timer::after(task.limits.time);
        let (status, bytes) = match futures::future::select(Box::pin(work), timer).await {
            futures::future::Either::Left((answer, _)) => answer?,
            futures::future::Either::Right(_) => {
                log::info!(
                    "model call {} timed out after {:?}",
                    task.name,
                    task.limits.time
                );
                return Err(Error::Timeout);
            }
        };
        let elapsed = started.elapsed();
        log::info!(
            "model call {} answered {} in {} ms",
            task.name,
            status.as_u16(),
            elapsed.as_millis()
        );
        let text = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            return Err(Error::Refused(status.as_u16(), refusal(&text)));
        }
        let completion: Completion = serde_json::from_str(&text)
            .map_err(|error| Error::NotInSchema(format!("{error} in {}", cut(&text, 200))))?;
        if let Some(error) = completion.error {
            return Err(Error::Refused(
                error.code.unwrap_or(status.as_u16()),
                error.message,
            ));
        }
        let choice = completion.choices.into_iter().next();
        let cut_short = choice
            .as_ref()
            .is_some_and(|choice| choice.finish_reason.as_deref() == Some("length"));
        let content = choice
            .and_then(|choice| choice.message.content)
            .unwrap_or_default();
        let value = serde_json::from_str::<T>(&content).map_err(|_| {
            if cut_short {
                Error::NotInSchema(format!(
                    "the answer ran past {} tokens and was cut off",
                    task.limits.output_tokens
                ))
            } else {
                Error::NotInSchema(cut(&content, 200).to_owned())
            }
        })?;
        Ok(Answer {
            value,
            model: completion.model.unwrap_or_else(|| model.to_owned()),
            cost_usd: completion.usage.and_then(|usage| usage.cost),
            elapsed,
        })
    }

    /// The models OpenRouter lists that take a schema (`structured_outputs`), by name. Needs no
    /// key.
    pub async fn list_models(&self) -> Result<Vec<ModelChoice>, Error> {
        let request = HttpRequest::get(self.models_endpoint.as_ref())
            .body(AsyncBody::default())
            .map_err(|error| Error::Network(error.to_string()))?;
        let http = self.http.clone();
        let work = async move {
            let mut response = http
                .send(request)
                .await
                .map_err(|error| Error::Network(error.to_string()))?;
            let mut bytes = Vec::new();
            response
                .body_mut()
                .take(MAX_LIST_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| Error::Network(error.to_string()))?;
            Ok::<_, Error>((response.status(), bytes))
        };
        let timer = smol::Timer::after(LIST_TIME);
        let (status, bytes) = match futures::future::select(Box::pin(work), timer).await {
            futures::future::Either::Left((answer, _)) => answer?,
            futures::future::Either::Right(_) => return Err(Error::Timeout),
        };
        if bytes.len() > MAX_LIST_BYTES {
            return Err(Error::NotInSchema(
                "the model list is too long to read".into(),
            ));
        }
        if !status.is_success() {
            return Err(Error::Refused(
                status.as_u16(),
                refusal(&String::from_utf8_lossy(&bytes)),
            ));
        }
        let list: ModelList = serde_json::from_slice(&bytes)
            .map_err(|_| Error::NotInSchema("not a model list".into()))?;
        let mut models: Vec<ModelChoice> = list
            .data
            .into_iter()
            .filter(|model| {
                model
                    .supported_parameters
                    .iter()
                    .any(|parameter| parameter == "structured_outputs")
            })
            .filter(|model| !model.id.ends_with(":batch") && !model.id.ends_with(":free"))
            .map(|model| ModelChoice {
                reasoning: model
                    .reasoning
                    .map(|listed| {
                        Reasoning::listed(
                            listed.mandatory,
                            &listed.supported_efforts.unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default(),
                id: model.id,
                name: model.name,
                prompt_per_million: per_million(&model.pricing.prompt),
                completion_per_million: per_million(&model.pricing.completion),
            })
            .collect();
        models.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(models)
    }
}

/// The server's message out of an error body, else the body's start.
fn refusal(text: &str) -> String {
    serde_json::from_str::<Completion>(text)
        .ok()
        .and_then(|completion| completion.error)
        .map(|error| error.message)
        .unwrap_or_else(|| cut(text, 200).to_owned())
}

fn cut(text: &str, chars: usize) -> &str {
    match text.char_indices().nth(chars) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

fn per_million(price: &str) -> f64 {
    price.parse::<f64>().unwrap_or(0.0) * 1_000_000.0
}

#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<Choice>,
    model: Option<String>,
    usage: Option<Usage>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    /// `null` when the answer holds no text: cut off before it, or refused.
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
    code: Option<u16>,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
    name: String,
    pricing: Pricing,
    #[serde(default)]
    supported_parameters: Vec<String>,
    /// Absent for a model that does not reason.
    #[serde(default)]
    reasoning: Option<ListedReasoning>,
}

#[derive(Deserialize)]
struct ListedReasoning {
    #[serde(default)]
    mandatory: bool,
    /// Absent or null when the model takes no effort by name.
    #[serde(default)]
    supported_efforts: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Pricing {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    completion: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};
    use std::sync::Mutex;

    const TASK: Task = Task {
        name: "table-filter",
        sends: "the type's property names and your request",
        tier: Tier::Fast,
        limits: Tier::Fast.limits(),
    };

    #[derive(Debug, Deserialize, PartialEq)]
    struct Filters {
        filters: Vec<Filter>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Filter {
        property: String,
        op: String,
    }

    fn client_with(status: u16, body: &'static str) -> (Client, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        let http = FakeHttpClient::create(move |request| {
            let kept = kept.clone();
            async move {
                let auth = request
                    .headers()
                    .get("Authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let mut sent = String::new();
                request.into_body().read_to_string(&mut sent).await.unwrap();
                kept.lock().unwrap().push(auth);
                kept.lock().unwrap().push(sent);
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body.to_owned()))
                    .unwrap())
            }
        });
        (Client::new(http), seen)
    }

    fn request() -> Request<'static> {
        Request {
            task: &TASK,
            instructions: "Turn the request into filters.",
            input: "dinners last month".into(),
            schema: json!({"type": "object", "properties": {"filters": {"type": "array"}}, "required": ["filters"], "additionalProperties": false}),
        }
    }

    /// A call sends the key, the model, the schema under strict and the provider policy, and
    /// the answer comes back parsed with the model and cost.
    #[test]
    fn a_call_sends_the_schema_and_parses_the_answer() {
        let (client, seen) = client_with(
            200,
            r#"{"model":"anthropic/claude-haiku-5.5","choices":[{"message":{"content":"{\"filters\":[{\"property\":\"kind\",\"op\":\"eq\"}]}"}}],"usage":{"cost":0.0004}}"#,
        );
        let answer: Answer<Filters> = smol::block_on(client.call(
            "anthropic/claude-haiku-5.5",
            &Reasoning::Optional,
            "sk-or-test",
            request(),
        ))
        .unwrap();
        assert_eq!(
            answer.value,
            Filters {
                filters: vec![Filter {
                    property: "kind".into(),
                    op: "eq".into()
                }]
            }
        );
        assert_eq!(answer.model, "anthropic/claude-haiku-5.5");
        assert_eq!(answer.cost_usd, Some(0.0004));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], "Bearer sk-or-test");
        let body: Value = serde_json::from_str(&seen[1]).unwrap();
        assert_eq!(body["model"], "anthropic/claude-haiku-5.5");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "table_filter"
        );
        assert_eq!(body["provider"]["require_parameters"], true);
        assert_eq!(body["provider"]["data_collection"], "deny");
        assert_eq!(body["max_tokens"], 300);
        assert_eq!(body["reasoning"]["enabled"], false);
        assert_eq!(body["messages"][1]["content"], "dinners last month");
    }

    /// A short answer is asked for with reasoning off, or at the least effort of a model that
    /// cannot turn it off; a careful one with it on. Before, every fast call said "off", and
    /// OpenRouter refused it for such a model: "400: Reasoning is mandatory for this endpoint
    /// and cannot be disabled" (Claude Sonnet 5.5 and Gemini 3.8 Flash, 2026-11-16).
    #[test]
    fn a_model_that_cannot_stop_reasoning_is_asked_for_its_least() {
        const ANSWER: &str = r#"{"choices":[{"message":{"content":"{\"filters\":[]}"}}]}"#;
        let sent = |task: &'static Task, reasoning: Reasoning| -> Value {
            let (client, seen) = client_with(200, ANSWER);
            let mut asked = request();
            asked.task = task;
            let _: Answer<Filters> =
                smol::block_on(client.call("m", &reasoning, "k", asked)).unwrap();
            let body: Value = serde_json::from_str(&seen.lock().unwrap()[1]).unwrap();
            body["reasoning"].clone()
        };
        const CAREFUL: Task = Task {
            name: "branch-summary",
            sends: "the changed records' titles",
            tier: Tier::Careful,
            limits: Tier::Careful.limits(),
        };
        assert_eq!(sent(&TASK, Reasoning::Optional), json!({"enabled": false}));
        assert_eq!(
            sent(&TASK, Reasoning::AtLeast("low".into())),
            json!({"effort": "low"})
        );
        assert_eq!(sent(&TASK, Reasoning::Always), json!({"enabled": true}));
        for reasoning in [Reasoning::Optional, Reasoning::AtLeast("low".into())] {
            assert_eq!(sent(&CAREFUL, reasoning), json!({"enabled": true}));
        }
    }

    /// An answer that is not in the schema, a refusal, a missing key and an input over the cap
    /// are each their own error, and nothing is sent for the last two.
    #[test]
    fn errors_are_told_apart() {
        let (client, _) = client_with(
            200,
            r#"{"choices":[{"message":{"content":"Here are your filters: kind = dinner"}}]}"#,
        );
        let answer: Result<Answer<Filters>, Error> =
            smol::block_on(client.call("m", &Reasoning::Optional, "k", request()));
        assert!(matches!(answer, Err(Error::NotInSchema(text)) if text.starts_with("Here are")));

        let (client, _) = client_with(
            401,
            r#"{"error":{"message":"No auth credentials found","code":401}}"#,
        );
        let answer: Result<Answer<Filters>, Error> =
            smol::block_on(client.call("m", &Reasoning::Optional, "k", request()));
        assert_eq!(
            answer.unwrap_err(),
            Error::Refused(401, "No auth credentials found".into())
        );

        let (client, seen) = client_with(200, "{}");
        let answer: Result<Answer<Filters>, Error> =
            smol::block_on(client.call("m", &Reasoning::Optional, "", request()));
        assert_eq!(answer.unwrap_err(), Error::NoKey);
        let mut large = request();
        large.input = "x".repeat(TASK.limits.input_bytes + 1);
        let answer: Result<Answer<Filters>, Error> =
            smol::block_on(client.call("m", &Reasoning::Optional, "k", large));
        assert_eq!(answer.unwrap_err(), Error::TooLarge);
        assert!(seen.lock().unwrap().is_empty(), "nothing sent");
    }

    /// A real OpenRouter answer (Haiku through Anthropic, BYOK, with reasoning details) parses.
    #[test]
    fn a_real_answer_parses() {
        let body = include_str!("fixtures/openrouter-haiku.json");
        let completion: Completion = serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            completion.model.as_deref(),
            Some("anthropic/claude-haiku-5.5")
        );
        assert_eq!(completion.usage.and_then(|u| u.cost), Some(0.0));
        assert!(
            completion.choices[0]
                .message
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with("{\"filters\""))
        );
        // A choice cut off before its answer parses too, with no content.
        let cut: Completion = serde_json::from_str(
            r#"{"choices":[{"finish_reason":"length","message":{"role":"assistant","content":null}}]}"#,
        )
        .unwrap();
        assert_eq!(cut.choices[0].finish_reason.as_deref(), Some("length"));
        assert!(cut.choices[0].message.content.is_none());
    }

    /// A model list longer than its cap is refused without reading past it. Before, it was read
    /// whole, however long, with no time limit.
    #[test]
    fn a_model_list_past_its_cap_is_refused() {
        let long: &'static str = Box::leak(" ".repeat(MAX_LIST_BYTES + 1).into_boxed_str());
        let (client, _) = client_with(200, long);
        assert!(matches!(
            smol::block_on(client.list_models()),
            Err(Error::NotInSchema(reason)) if reason.contains("too long")
        ));
    }

    /// The model list keeps the models that take a schema, without batch or free variants,
    /// with prices per million.
    #[test]
    fn the_model_list_keeps_schema_models_with_prices() {
        let (client, _) = client_with(
            200,
            r#"{"data":[
              {"id":"anthropic/claude-haiku-5.5","name":"Anthropic: Claude Haiku 5.5","pricing":{"prompt":"0.0000001","completion":"0.0000005"},"supported_parameters":["structured_outputs","tools"]},
              {"id":"anthropic/claude-haiku-5.5:batch","name":"Anthropic: Claude Haiku 5.5 (batch)","pricing":{"prompt":"0.00000005","completion":"0.00000025"},"supported_parameters":["structured_outputs"]},
              {"id":"some/chat-only","name":"Chat only","pricing":{"prompt":"0","completion":"0"},"supported_parameters":["tools"]}
            ]}"#,
        );
        let models = smol::block_on(client.list_models()).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "anthropic/claude-haiku-5.5");
        assert!((models[0].prompt_per_million - 0.1).abs() < 1e-9);
        assert_eq!(
            models[0].label(),
            "Anthropic: Claude Haiku 5.5 · $0.10/$0.50 per million"
        );
        assert_eq!(models[0].reasoning, Reasoning::Optional, "none is said");
    }

    /// The list says which models cannot turn reasoning off and the efforts each takes; the
    /// least of them is kept, wherever it stands in the list. The lines are OpenRouter's of
    /// 2026-11-16, cut to what is read.
    #[test]
    fn the_model_list_says_how_little_each_model_reasons() {
        let (client, _) = client_with(
            200,
            r#"{"data":[
              {"id":"a/haiku","name":"A","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":false,"supported_efforts":["max","xhigh","high","medium","low","none"],"default_effort":"medium"}},
              {"id":"b/gemini-flash","name":"B","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":true,"default_enabled":true,"supported_efforts":["high","medium","low"],"default_effort":"medium"}},
              {"id":"c/flash-lite","name":"C","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":true,"supported_efforts":["minimal","low","medium","high"]}},
              {"id":"d/thinking","name":"D","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":true}},
              {"id":"e/null","name":"E","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":true,"supported_efforts":null}},
              {"id":"f/odd","name":"F","pricing":{},"supported_parameters":["structured_outputs"],"reasoning":{"mandatory":true,"supported_efforts":["turbo","high","none"]}}
            ]}"#,
        );
        let models = smol::block_on(client.list_models()).unwrap();
        let said: Vec<(&str, &Reasoning)> = models
            .iter()
            .map(|model| (model.id.as_str(), &model.reasoning))
            .collect();
        assert_eq!(
            said,
            [
                ("a/haiku", &Reasoning::Optional),
                ("b/gemini-flash", &Reasoning::AtLeast("low".into())),
                ("c/flash-lite", &Reasoning::AtLeast("minimal".into())),
                ("d/thinking", &Reasoning::Always),
                ("e/null", &Reasoning::Always),
                ("f/odd", &Reasoning::AtLeast("high".into())),
            ]
        );
        // A list kept from before this was read still parses: its models say nothing.
        let kept: ModelChoice = serde_json::from_str(
            r#"{"id":"a/haiku","name":"A","prompt_per_million":0.1,"completion_per_million":0.5}"#,
        )
        .unwrap();
        assert_eq!(kept.reasoning, Reasoning::Optional);
        let round: ModelChoice =
            serde_json::from_str(&serde_json::to_string(&models[1]).unwrap()).unwrap();
        assert_eq!(round, models[1]);
    }
}
