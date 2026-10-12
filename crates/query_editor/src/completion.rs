//! Completion for the query editor through Zed's `CompletionProvider`, without a language
//! server: candidates come from `gq_language::completions` on the session's schema.
//!
// Modelled on zed-industries/zed crates/keymap_editor/src/action_completion_provider.rs
// @ v1.23.2, a provider that also runs without a project.

use anyhow::Result;
use editor::{CompletionProvider, Editor};
use gpui::{Context, Entity, Task, WeakEntity, Window};
use graph_store::GraphSession;
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};

pub struct GqCompletionProvider {
    session: Option<WeakEntity<GraphSession>>,
}

impl GqCompletionProvider {
    pub fn new(session: Option<WeakEntity<GraphSession>>) -> Self {
        Self { session }
    }
}

impl CompletionProvider for GqCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let schema = self
            .session
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|session| session.read(cx).schema_model());
        let Some(schema) = schema else {
            return Task::ready(Ok(Vec::new()));
        };
        let buffer = buffer.read(cx);
        let text = buffer.text();
        let offset = buffer_position.to_offset(buffer);
        let (start, items) = gq_language::completions(&schema, &text, offset);
        let replace_range = buffer.anchor_before(start)..buffer_position;
        let completions = items
            .into_iter()
            .map(|item| {
                let label = match &item.detail {
                    // The detail is shown muted after the name and is not matched against.
                    Some(detail) => CodeLabel::filtered(
                        format!("{} {detail}", item.label),
                        item.label.len(),
                        None,
                        Vec::new(),
                    ),
                    None => CodeLabel::plain(item.label.clone(), None),
                };
                Completion {
                    replace_range: replace_range.clone(),
                    new_text: item.label,
                    label,
                    documentation: None,
                    source: CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    match_start: None,
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                    group: None,
                }
            })
            .collect();
        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            // What fits depends on where the cursor is (`$p.` offers properties), so ask
            // again as the word grows rather than filtering the first answer.
            is_incomplete: true,
        }]))
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<Buffer>,
        _position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        text.chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$'))
    }
}
