//! GQ as Zed's editor knows a language (spec 007): the tree-sitter grammar in
//! `grammars/tree-sitter-gq` with its highlight, bracket, indent and outline queries, `//`
//! comments and auto-closed brackets. The editor's buffer parses with it, so highlighting
//! follows the syntax tree and folds stop at the end of what they enclose.
//!
//! The lexical colouring in `gq_language::tokenize` stays for the read-only views of a query,
//! which hold no buffer; both use the same theme names.

use std::sync::Arc;

use gpui::{App, Global, SharedString};
use language::{
    BlockCommentConfig, BracketPair, BracketPairConfig, Language, LanguageConfig, LanguageMatcher,
};
use ui::ActiveTheme as _;

struct GqSyntax {
    language: Arc<Language>,
    /// The theme the language's colours were taken from.
    theme: Option<SharedString>,
}

impl Global for GqSyntax {}

fn build() -> anyhow::Result<Language> {
    let bracket = |start: &str, end: &str| BracketPair {
        start: start.into(),
        end: end.into(),
        close: true,
        surround: true,
        newline: true,
    };
    let config = LanguageConfig {
        name: "GQ".into(),
        grammar: Some("gq".into()),
        matcher: Arc::new(LanguageMatcher {
            path_suffixes: vec!["gq".into()],
            ..Default::default()
        }),
        line_comments: vec!["// ".into()],
        block_comment: Some(BlockCommentConfig {
            start: "/*".into(),
            end: "*/".into(),
            prefix: "".into(),
            tab_size: 0,
        }),
        brackets: BracketPairConfig {
            pairs: vec![bracket("{", "}"), bracket("(", ")"), bracket("[", "]"), {
                BracketPair {
                    start: "\"".into(),
                    end: "\"".into(),
                    close: true,
                    surround: true,
                    newline: false,
                }
            }],
            disabled_scopes_by_bracket_ix: Vec::new(),
        },
        ..Default::default()
    };
    Language::new(
        config,
        Some(tree_sitter::Language::new(tree_sitter_gq::LANGUAGE)),
    )
    .with_highlights_query(tree_sitter_gq::HIGHLIGHTS_QUERY)?
    .with_brackets_query(tree_sitter_gq::BRACKETS_QUERY)?
    .with_indents_query(tree_sitter_gq::INDENTS_QUERY)?
    .with_outline_query(tree_sitter_gq::OUTLINE_QUERY)
}

/// The GQ language, with colours from the active theme. Built once; a theme change recolours it
/// the next time an editor asks. `None` if the grammar's queries do not compile (a test guards
/// against it), and the editor then colours words lexically as before.
pub(crate) fn language(cx: &mut App) -> Option<Arc<Language>> {
    if !cx.has_global::<GqSyntax>() {
        let language = build().ok()?;
        cx.set_global(GqSyntax {
            language: Arc::new(language),
            theme: None,
        });
    }
    let theme = cx.theme().clone();
    let syntax = cx.global_mut::<GqSyntax>();
    if syntax.theme.as_ref() != Some(&theme.name) {
        syntax.language.set_theme(theme.syntax());
        syntax.theme = Some(theme.name.clone());
    }
    Some(syntax.language.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_language_builds_with_every_query() {
        let language = build().expect("the queries compile");
        let grammar = language.grammar().expect("a grammar");
        assert!(grammar.highlights_config.is_some());
        assert!(grammar.outline_config.is_some());
        assert!(grammar.brackets_config.is_some());
        assert!(grammar.indents_config.is_some());
    }
}
