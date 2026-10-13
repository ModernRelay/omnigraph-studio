//! OmniGraph's languages as Zed's editor knows a language (spec 007): GQ for the query editor and
//! the schema language for the schema view, each a tree-sitter grammar under `grammars/` with its
//! highlight, bracket, indent and outline queries, `//` comments and auto-closed brackets. A
//! buffer that parses with one is coloured from its syntax tree, and its folds stop at the end of
//! what they enclose.
//!
//! The lexical colouring in `gq_language::tokenize` stays for the read-only views of a query,
//! which hold no buffer; every view uses the same theme names.

use std::sync::Arc;

use gpui::{App, Global, SharedString};
use language::{
    BlockCommentConfig, BracketPair, BracketPairConfig, Language, LanguageConfig, LanguageMatcher,
};
use ui::ActiveTheme as _;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Syntax {
    /// Queries and mutations, `.gq`.
    Gq,
    /// Schemas, `.pg`.
    Pg,
}

struct Built {
    language: Arc<Language>,
    /// The theme the language's colours were taken from.
    theme: Option<SharedString>,
}

#[derive(Default)]
struct Languages(Vec<(Syntax, Built)>);

impl Global for Languages {}

/// The tree-sitter grammar and the editor's queries of one language.
struct Grammar {
    name: &'static str,
    suffix: &'static str,
    language: tree_sitter::Language,
    highlights: &'static str,
    brackets: &'static str,
    indents: &'static str,
    outline: &'static str,
}

impl Syntax {
    fn grammar(self) -> Grammar {
        match self {
            Syntax::Gq => Grammar {
                name: "GQ",
                suffix: "gq",
                language: tree_sitter::Language::new(tree_sitter_gq::LANGUAGE),
                highlights: tree_sitter_gq::HIGHLIGHTS_QUERY,
                brackets: tree_sitter_gq::BRACKETS_QUERY,
                indents: tree_sitter_gq::INDENTS_QUERY,
                outline: tree_sitter_gq::OUTLINE_QUERY,
            },
            Syntax::Pg => Grammar {
                name: "PG",
                suffix: "pg",
                language: tree_sitter::Language::new(tree_sitter_pg::LANGUAGE),
                highlights: tree_sitter_pg::HIGHLIGHTS_QUERY,
                brackets: tree_sitter_pg::BRACKETS_QUERY,
                indents: tree_sitter_pg::INDENTS_QUERY,
                outline: tree_sitter_pg::OUTLINE_QUERY,
            },
        }
    }
}

fn build(syntax: Syntax) -> anyhow::Result<Language> {
    let grammar = syntax.grammar();
    let bracket = |start: &str, end: &str| BracketPair {
        start: start.into(),
        end: end.into(),
        close: true,
        surround: true,
        newline: true,
    };
    let config = LanguageConfig {
        name: grammar.name.into(),
        grammar: Some(grammar.suffix.into()),
        matcher: Arc::new(LanguageMatcher {
            path_suffixes: vec![grammar.suffix.into()],
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
    Language::new(config, Some(grammar.language))
        .with_highlights_query(grammar.highlights)?
        .with_brackets_query(grammar.brackets)?
        .with_indents_query(grammar.indents)?
        .with_outline_query(grammar.outline)
}

/// A language, with colours from the active theme. Built once; a theme change recolours it the
/// next time a view asks. `None` if the grammar's queries do not compile (a test guards against
/// it), and the view then shows the text without the grammar's colours.
pub(crate) fn language(syntax: Syntax, cx: &mut App) -> Option<Arc<Language>> {
    let built = cx
        .default_global::<Languages>()
        .0
        .iter()
        .any(|(s, _)| *s == syntax);
    if !built {
        let language = build(syntax).ok()?;
        cx.global_mut::<Languages>().0.push((
            syntax,
            Built {
                language: Arc::new(language),
                theme: None,
            },
        ));
    }
    let theme = cx.theme().clone();
    let languages = cx.global_mut::<Languages>();
    let (_, built) = languages.0.iter_mut().find(|(s, _)| *s == syntax)?;
    if built.theme.as_ref() != Some(&theme.name) {
        built.language.set_theme(theme.syntax());
        built.theme = Some(theme.name.clone());
    }
    Some(built.language.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_languages_build_with_every_query() {
        for syntax in [Syntax::Gq, Syntax::Pg] {
            let language = build(syntax).expect("the queries compile");
            let grammar = language.grammar().expect("a grammar");
            assert!(grammar.highlights_config.is_some());
            assert!(grammar.outline_config.is_some());
            assert!(grammar.brackets_config.is_some());
            assert!(grammar.indents_config.is_some());
        }
    }
}
