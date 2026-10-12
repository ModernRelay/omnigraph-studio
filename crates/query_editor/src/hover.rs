//! Hover for the query editor (spec 007): a binding's or parameter's type, a property's
//! declared type, a type's kind, a relationship's ends; through Zed's `SemanticsProvider`,
//! with `gq_language::hover` on the session's schema. Zed's hover popover needs no project.
//! Every other part of the provider answers that it has nothing.

use std::ops::Range;
use std::sync::Arc;

use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::{GotoDefinitionKind, RenameTarget, SemanticsProvider};
use futures::future::Shared;
use gpui::{App, Entity, Task, WeakEntity};
use graph_store::GraphSession;
use language::{Anchor, Buffer, BufferId, BufferRow, LanguageServerId, ToOffset as _};
use project::lsp_store::{BufferSemanticTokens, CacheInlayHints};
use project::{
    DocumentHighlight, Hover, HoverBlock, HoverBlockKind, InlayHint, InvalidationStrategy,
    LocationLink, ProjectTransaction,
};

pub struct GqSemantics {
    session: Option<WeakEntity<GraphSession>>,
}

impl GqSemantics {
    pub fn new(session: Option<WeakEntity<GraphSession>>) -> Self {
        Self { session }
    }
}

impl SemanticsProvider for GqSemantics {
    fn hover(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        cx: &mut App,
    ) -> Option<Task<Option<Vec<Hover>>>> {
        let schema = self
            .session
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|session| session.read(cx).schema_model())?;
        let buffer = buffer.read(cx);
        let text = buffer.text();
        let offset = position.to_offset(buffer);
        let found = gq_language::hover(&schema, &text, offset).map(|(range, shown)| {
            vec![Hover {
                contents: vec![HoverBlock {
                    text: shown,
                    kind: HoverBlockKind::PlainText,
                }],
                range: Some(buffer.anchor_before(range.start)..buffer.anchor_after(range.end)),
                language: None,
            }]
        });
        Some(Task::ready(found))
    }

    fn inline_values(
        &self,
        _buffer: Entity<Buffer>,
        _range: Range<Anchor>,
        _cx: &mut App,
    ) -> Option<Task<Result<Vec<InlayHint>>>> {
        None
    }

    fn applicable_inlay_chunks(
        &self,
        _buffer: &Entity<Buffer>,
        _ranges: &[Range<Anchor>],
        _cx: &mut App,
    ) -> Vec<Range<BufferRow>> {
        Vec::new()
    }

    fn invalidate_inlay_hints(&self, _for_buffers: &HashSet<BufferId>, _cx: &mut App) {}

    fn inlay_hints(
        &self,
        _invalidate: InvalidationStrategy,
        _buffer: Entity<Buffer>,
        _ranges: Vec<Range<Anchor>>,
        _known_chunks: Option<(clock::Global, HashSet<Range<BufferRow>>)>,
        _cx: &mut App,
    ) -> Option<HashMap<Range<BufferRow>, Task<Result<CacheInlayHints>>>> {
        None
    }

    fn semantic_tokens(
        &self,
        _buffer: Entity<Buffer>,
        _cx: &mut App,
    ) -> Option<Shared<Task<std::result::Result<BufferSemanticTokens, Arc<anyhow::Error>>>>> {
        None
    }

    fn supports_inlay_hints(&self, _buffer: &Entity<Buffer>, _cx: &mut App) -> bool {
        false
    }

    fn supports_semantic_tokens(&self, _buffer: &Entity<Buffer>, _cx: &mut App) -> bool {
        false
    }

    fn document_highlights(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _cx: &mut App,
    ) -> Option<Task<Result<Vec<DocumentHighlight>>>> {
        None
    }

    fn definitions(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _kind: GotoDefinitionKind,
        _cx: &mut App,
    ) -> Option<Task<Result<Option<Vec<LocationLink>>>>> {
        None
    }

    fn range_for_rename(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _cx: &mut App,
    ) -> Task<Result<Option<RenameTarget>>> {
        Task::ready(Ok(None))
    }

    fn perform_rename(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        _new_name: String,
        _language_server_id: Option<LanguageServerId>,
        _cx: &mut App,
    ) -> Option<Task<Result<ProjectTransaction>>> {
        None
    }
}
