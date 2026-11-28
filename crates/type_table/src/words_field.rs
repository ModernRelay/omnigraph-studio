//! "Filter in words" (spec 019): the toolbar's sentence field. A person writes what they
//! want, a model answers with filters and, where the view takes them, an order, a grouping and
//! columns, and the answer's filters become the bar's pills. The table has it and so does a
//! Cards tab (spec 048, slice 7): each holds a [`WordsField`] and is a [`WordsHost`], which
//! builds on the filter bar's host, so the question, the call, the judging of the answer and
//! the field's drawing are one thing. The state and the functions were `TypeTable`'s.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable, Subscription, Task, Window};
use gq_language::{Filter, FilterOp, builder};
use serde_json::Value;
use studio_ui::ds::Tipped;
use studio_workspace::models::{self, Error as ModelError, Models, Request as ModelRequest};
use ui::prelude::*;

use crate::filter_bar::{FilterHost, filter_placeholder};
use crate::model::TableKind;
use crate::suggest;
use crate::words;

/// The sentence field's state (spec 019): idle, with what the last answer could not use;
/// calling; or failed, with why.
pub(crate) enum WordsState {
    Typing(Option<String>),
    Running { _call: Task<()> },
    Failed(String),
}

/// The least the toolbar's sentence field is squeezed to before the chips after it are cut.
pub(crate) const WORDS_FIELD_MIN: f32 = 140.;

/// What the sentence field shows while it is empty: its name, or, while no OpenRouter key is
/// stored, that it needs one, so nobody types a sentence to learn it (spec 019).
fn words_placeholder(needs_key: bool) -> &'static str {
    if needs_key {
        models::WORDS_NEED_KEY
    } else {
        "Filter in words…"
    }
}

/// What a view holds of its sentence field: the field, what its call is doing, the filters
/// its last answer made, which the next answer replaces, and whether a key is stored.
pub(crate) struct WordsField {
    pub(crate) editor: Entity<Editor>,
    pub(crate) state: WordsState,
    pub(crate) made: Vec<Filter>,
    /// No OpenRouter key is stored: the field says so before it is used, and follows the key
    /// when one is stored or removed.
    pub(crate) needs_key: bool,
    _key_changes: Option<Subscription>,
}

impl WordsField {
    /// The field of a view being made: empty, saying whether it needs a key, and following the
    /// key from then on ([`WordsHost::key_changed`]).
    pub(crate) fn new<H: WordsHost>(window: &mut Window, cx: &mut Context<H>) -> Self {
        let needs_key = Models::needs_key(cx);
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(words_placeholder(needs_key), window, cx);
            editor.set_text_style_refinement(studio_ui::ds::field_text(cx));
            editor
        });
        // The sentence field says whether a key is stored, and follows the key.
        let key_changes = Models::try_global(cx).map(|models| {
            cx.observe_in(&models, window, |this, _, window, cx| {
                this.key_changed(window, cx)
            })
        });
        Self {
            editor,
            state: WordsState::Typing(None),
            made: Vec::new(),
            needs_key,
            _key_changes: key_changes,
        }
    }
}

/// What the sentence field asks of the view that holds it, beside what the filter bar asks:
/// what a sentence may set beside its filters, and how the view takes it. The answer's filters
/// go into the host's own ([`FilterHost::filters_mut`]), and it reads again through
/// [`FilterHost::filters_changed`].
pub(crate) trait WordsHost: FilterHost {
    fn words(&self) -> &WordsField;
    fn words_mut(&mut self) -> &mut WordsField;

    /// What a sentence may set beside its filters, each by its key: the orders, the groupings
    /// and the columns the view offers. A part the view has none of is left empty, and the
    /// question leaves it out.
    fn view_choices(&self) -> words::ViewChoices;

    /// Takes an answer's order, grouping and columns ([`words::view`], checked against
    /// [`Self::view_choices`]). The filters are set and the view reads again after it.
    fn apply_words_view(&mut self, view: words::View, cx: &mut Context<Self>);

    /// Whether the sentence field has the keyboard: ↵ then asks, and the view's own keys wait.
    fn words_field_focused(&self, window: &Window, cx: &App) -> bool {
        self.words().editor.focus_handle(cx).is_focused(window)
    }

    /// What "Filter in words" could not use, or why its call failed, for the filter bar's
    /// note ([`FilterHost::bar_note`]).
    fn words_note(&self) -> Option<Label> {
        match &self.words().state {
            WordsState::Typing(Some(note)) => Some(studio_ui::ds::text::muted(note.clone())),
            WordsState::Failed(why) => Some(studio_ui::ds::text::error(why.clone())),
            _ => None,
        }
    }

    /// Esc for the sentence field, and whether the key was its: it leaves the field and clears
    /// what it said; a call under way is dropped, and the filters it made stay as pills.
    fn leave_words(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.words_field_focused(window, cx)
            || !matches!(self.words().state, WordsState::Typing(None))
        {
            self.words_mut().state = WordsState::Typing(None);
            window.focus(&self.host_focus(), cx);
            cx.notify();
            return true;
        }
        false
    }

    /// ⌘⇧F: the sentence field takes the keyboard.
    fn open_words(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.words().editor.focus_handle(cx), cx);
        cx.notify();
    }

    /// The keys the filter menu offers, with their operators and properties, for the model.
    fn words_choices(&self) -> Vec<words::Choice> {
        self.filter_choices()
            .iter()
            .cloned()
            .map(|(key, label, _)| words::Choice {
                ops: self.ops_for(&key),
                property: self.property_for_key(&key),
                other_type: self.related_type(&key),
                traversal: builder::relationship_traversal(&key, builder::NODE_VAR, "$r"),
                key,
                label,
            })
            .collect()
    }

    /// ↵ in the sentence field: the sentence goes to the model, once the task is allowed.
    fn run_words(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sentence = self.words().editor.read(cx).text(cx).trim().to_owned();
        if sentence.is_empty() || matches!(self.words().state, WordsState::Running { .. }) {
            return;
        }
        let Some(models) = Models::try_global(cx) else {
            self.words_mut().state = WordsState::Failed(ModelError::NoKey.to_string());
            cx.notify();
            return;
        };
        self.start_words(models, sentence, window, cx);
    }

    fn start_words(
        &mut self,
        models: Entity<Models>,
        sentence: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choices = self.words_choices();
        let offered = self.view_choices();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        // A node table's sentence may test one related record several ways, a relationship's
        // own properties and further hops, as groups (spec 025).
        let schema = self
            .schema()
            .cloned()
            .filter(|_| self.kind() == TableKind::Node);
        let type_name = self.type_name().to_owned();
        let reach = schema.as_deref().map(|schema| words::Reach {
            schema,
            type_name: &type_name,
        });
        let request = ModelRequest {
            task: &words::TASK,
            instructions: words::INSTRUCTIONS,
            input: words::input_with(&type_name, &choices, &offered, &today, &sentence, reach),
            schema: words::schema_with(&choices, &offered, reach),
        };
        let call = models.read(cx).call::<words::Answer>(request, cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            let answer = match call.await {
                Ok(answer) => answer,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.words_mut().state = WordsState::Failed(error.to_string());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let (view, mut refused) = words::view(&answer.value, &offered);
            let reach = schema.as_deref().map(|schema| words::Reach {
                schema,
                type_name: &type_name,
            });
            let (mut filters, filters_refused) = words::filters_with(answer.value, &choices, reach);
            refused.extend(filters_refused);
            // A `related_to` names a record; the name becomes its id through the node search
            // the filter's own value field uses, only when one node is named (spec 031): the
            // only match, or the only one the name is exactly. When several are, the person
            // picks among them in the filter's value field; when none, the sentence says so.
            let mut labels: Vec<(String, String)> = Vec::new();
            let mut to_pick: Option<(String, String, String, Vec<suggest::Suggestion>)> = None;
            for filter in &mut filters {
                if filter.op != FilterOp::RelatedTo {
                    continue;
                }
                let Some(name) = filter.value.as_str().map(str::to_owned) else {
                    continue;
                };
                let other = choices
                    .iter()
                    .find(|choice| choice.key == filter.property)
                    .and_then(|choice| choice.other_type.clone());
                let search = this
                    .read_with(cx, |this, cx| {
                        let other = other.as_deref()?;
                        Some(suggest::search_nodes(
                            this.session(),
                            other,
                            &name,
                            this.pinned(),
                            cx,
                        ))
                    })
                    .ok()
                    .flatten();
                let other = other.unwrap_or_default();
                let named = match search {
                    Some(search) => search.await.map(|hits| suggest::named(&hits)),
                    None => Ok(suggest::Named::None),
                };
                filter.value = Value::Null;
                match named {
                    Ok(suggest::Named::One(node)) => {
                        if let Some(label) = node.label {
                            labels.push((node.value.clone(), label));
                        }
                        filter.value = Value::String(node.value);
                    }
                    Ok(suggest::Named::None) => {
                        refused.push(format!("no {other} matching \"{name}\""));
                    }
                    Ok(suggest::Named::Several(offers)) if to_pick.is_none() => {
                        to_pick = Some((filter.property.clone(), name.clone(), other, offers));
                    }
                    Ok(suggest::Named::Several(offers)) => {
                        refused.push(suggest::pick_note(false, &other, &name, &offers));
                    }
                    Err(error) => {
                        refused.push(format!("{other} could not be searched ({error})"));
                    }
                }
            }
            filters.retain(|filter| filter.op != FilterOp::RelatedTo || !filter.value.is_null());
            // A condition the model wrote is judged as a typed one is: by the compiler, before
            // any read; one it refuses is dropped and the diagnostic named.
            let mut judged = Vec::with_capacity(filters.len());
            for filter in filters {
                if filter.op == FilterOp::Expr {
                    let text = filter.value.as_str().unwrap_or_default().to_owned();
                    match this
                        .read_with(cx, |this, _| this.check_condition(&text))
                        .unwrap_or_else(|_| Err("The table was closed.".to_owned()))
                    {
                        Ok(()) => judged.push(filter),
                        Err(why) => refused.push(format!("condition: {why}")),
                    }
                } else {
                    judged.push(filter);
                }
            }
            let filters = judged;
            this.update_in(cx, |this, window, cx| {
                this.bar_mut().node_labels.extend(labels);
                // The last sentence's filters go; ones added by hand stay.
                let made = std::mem::take(&mut this.words_mut().made);
                this.filters_mut().retain(|filter| !made.contains(filter));
                this.words_mut().made = filters.clone();
                this.filters_mut().extend(filters);
                // What the sentence says of the order, the grouping and the columns replaces
                // the table's; what it does not say stays as it is.
                this.apply_words_view(view, cx);
                let mut note =
                    (!refused.is_empty()).then(|| format!("Could not use: {}", refused.join("; ")));
                if let Some((key, name, other, offers)) = to_pick {
                    // The note says what the person sees: the choices in the filter when it
                    // opened, else the choices themselves and what to do instead.
                    let opened = this.ask_to_pick(key, name.clone(), offers.clone(), window, cx);
                    let ask = suggest::pick_note(opened, &other, &name, &offers);
                    note = Some(match note {
                        Some(note) => format!("{ask} {note}"),
                        None => ask,
                    });
                }
                this.words_mut().state = WordsState::Typing(note);
                this.filters_changed(cx);
                cx.notify();
            })
            .ok();
        });
        self.words_mut().state = WordsState::Running { _call: task };
        cx.notify();
    }

    /// A sentence's "related to" named several nodes (spec 031): its filter opens as a draft,
    /// the name in its value field and the nodes it names offered, best first, for the person
    /// to pick one. Nothing is filtered by it until they do. Whether the draft opened: a key
    /// the filter bar cannot test "related to" on (one reached through a relationship's
    /// group) opens none, and the caller says so.
    fn ask_to_pick(
        &mut self,
        key: String,
        name: String,
        offers: Vec<suggest::Suggestion>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.ops_for(&key).contains(&FilterOp::RelatedTo) {
            return false;
        }
        self.set_draft_property(key.clone(), window, cx);
        if self
            .bar()
            .draft
            .as_ref()
            .is_none_or(|draft| draft.property != key)
        {
            return false;
        }
        self.bar().editor.update(cx, |editor, cx| {
            editor.set_text(name.clone(), window, cx);
            editor.set_placeholder_text(filter_placeholder(FilterOp::RelatedTo), window, cx);
        });
        if let Some(draft) = &mut self.bar_mut().draft {
            draft.op = FilterOp::RelatedTo;
            draft.offers.set(offers, true);
            draft.answers = Some(name);
        }
        // The keyboard goes where the name is, to pick with: the relationship's own first
        // test takes no value, and a draft of that has the host's keyboard.
        self.focus_filter_draft(window, cx);
        self.redraw_offers(cx);
        cx.notify();
        true
    }

    /// A key was stored or removed, or the keychain answered: the sentence field says whether
    /// it can be used, and the table draws again only when that changed.
    fn key_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let needs_key = Models::needs_key(cx);
        if needs_key == self.words().needs_key {
            return;
        }
        self.words_mut().needs_key = needs_key;
        self.words().editor.update(cx, |editor, cx| {
            editor.set_placeholder_text(words_placeholder(needs_key), window, cx)
        });
        cx.notify();
    }

    /// What the sentence field's tooltip says: why the last call failed, else that the field
    /// needs a key while none is stored, else what a sentence sends and to which model.
    fn words_about(&self, cx: &App) -> String {
        match &self.words().state {
            WordsState::Failed(why) => why.clone(),
            _ if self.words().needs_key => models::needs_key_tip("Filter in words"),
            _ => format!(
                "Filter in words (↵): {} go to {}",
                words::TASK.sends,
                Models::model_for(words::TASK.tier, cx)
            ),
        }
    }

    /// The toolbar's sentence field (spec 019): a search field with the model's mark, its
    /// spinner while the call is out, and the field's red line when the call failed; the
    /// answer's filters are the bar's pills. ↵ asks, Esc leaves. Without a key it says so,
    /// empty, and in its tooltip, before a sentence is typed.
    fn render_words_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = matches!(self.words().state, WordsState::Running { .. });
        let failed = matches!(self.words().state, WordsState::Failed(_));
        let about = self.words_about(cx);
        studio_ui::ds::pill_field("filter-words", failed, cx)
            .debug_selector(|| "filter-words".into())
            .key_context("FilterWords")
            // As wide as a search field where there is room, and the first to give way where
            // there is not, so the controls after it are not cut off.
            .w(px(studio_ui::ds::size::FIELD_WIDE))
            .min_w(px(WORDS_FIELD_MIN))
            .flex_shrink(1.)
            .tip(studio_ui::ds::tooltip(about))
            .child(studio_ui::ds::icon::meta(IconName::ZedAssistant))
            .child(studio_ui::ds::field_input(self.words().editor.clone()))
            .when(running, |field| field.child(studio_ui::ds::spinner()))
            .into_any_element()
    }
}
