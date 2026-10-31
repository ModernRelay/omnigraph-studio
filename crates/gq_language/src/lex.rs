//! A lexer for `.gq` source (spec 007): tokens for highlighting, and the outline of a file (its
//! settings prefix and the byte range of each declaration), so the editor can run the
//! declaration under the cursor.
//!
//! Lexical only. It follows the terminals of `query.pest` at Omnigraph `v0.14.0` (line and
//! block comments, strings with escapes, `$variables`, `@meta` fields, `@Catalogue`
//! types, identifiers, numbers) and never type-checks; `check` is the authority on what a source
//! means.

use std::ops::Range;

/// What a token is, for highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Keyword,
    /// `$p`.
    Variable,
    /// A capitalised identifier: a node or edge type, or a scalar type such as `String`.
    Type,
    /// `name` in `$p.name`, and metadata such as `@id`.
    Property,
    /// `@description` and `@instruction`.
    Annotation,
    /// The name after `query`.
    QueryName,
    /// Any other identifier: an edge in a traversal, an assignment key, a setting name.
    Identifier,
    String,
    Number,
    Comment,
    Punctuation,
}

impl TokenKind {
    /// The theme's syntax style a token takes, by the names Zed's themes define; `None` for
    /// plain text. A read-only view of a query, which holds no buffer to parse (the agent
    /// panel's query cards), is coloured by it; the query editor colours from its syntax tree.
    pub fn syntax_name(self) -> Option<&'static str> {
        Some(match self {
            TokenKind::Keyword => "keyword",
            TokenKind::Variable => "variable",
            TokenKind::Type => "type",
            TokenKind::Property => "property",
            TokenKind::Annotation => "attribute",
            TokenKind::QueryName => "function",
            TokenKind::String => "string",
            TokenKind::Number => "number",
            TokenKind::Comment => "comment",
            TokenKind::Identifier | TokenKind::Punctuation => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

/// Words that are keywords wherever they stand: drawn as keywords, and offered by completion.
pub(crate) const KEYWORDS: &[&str] = &[
    "query",
    "match",
    "return",
    "distinct",
    "order",
    "limit",
    "insert",
    "update",
    "set",
    "where",
    "delete",
    "not",
    "exists",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "and",
    "or",
    "is",
    "null",
    "in",
    "as",
    "asc",
    "desc",
    "nearest",
    "search",
    "fuzzy",
    "match_text",
    "bm25",
    "rrf",
    "now",
    "explain",
    "branch",
    "show",
    "reset",
    "true",
    "false",
    "date",
    "datetime",
    "contains",
    "starts_with",
];

/// Words that are keywords only inside a statement: one whose first word is `branch`, `commit`,
/// `load` or `show`, or `explain load`.
const STATEMENT_KEYWORDS: &[&str] = &[
    "create",
    "from",
    "into",
    "merge",
    "merge-base",
    "diff",
    "to",
    "after",
    "page",
    "dry",
    "run",
    "list",
    "all",
    "commit",
    "publish",
    "drop",
    "load",
    "csv",
    "node",
    "edge",
    "mode",
    "overwrite",
    "append",
    "delimiter",
    "header",
    "columns",
];

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Where the tokenizer is against a file's settings prefix: the `set …;` and `reset …;` lines
/// and comments [`outline`] reads before the first statement. A word that starts a statement
/// (`branch`, `show`, `commit`, `load`) does so only as the first word after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    /// At the first word after the prefix.
    Start,
    /// Inside a `set` or `reset`, to its `;`.
    Setting,
    /// After an `explain` that is the first word: a `load` still starts a statement.
    Explain,
    Past,
}

/// Splits `.gq` source into tokens. Whitespace is skipped; anything the lexer does not know is
/// one punctuation token per character, so every non-space byte belongs to a token.
pub fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut in_statement = false;
    let mut prefix = Prefix::Start;
    let mut i = 0;
    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let kind = if source[i..].starts_with("//") {
            i = source[i..].find('\n').map_or(len, |n| i + n);
            TokenKind::Comment
        } else if source[i..].starts_with("/*") {
            i = source[i + 2..].find("*/").map_or(len, |n| i + 2 + n + 2);
            TokenKind::Comment
        } else if b == b'"' {
            i += 1;
            while i < len {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            i = i.min(len);
            TokenKind::String
        } else if b == b'$' {
            i += 1;
            while i < len && is_ident_byte(bytes[i]) {
                i += 1;
            }
            TokenKind::Variable
        } else if b == b'@' {
            i += 1;
            while i < len && is_ident_byte(bytes[i]) {
                i += 1;
            }
            match &source[start + 1..i] {
                "description" | "instruction" => TokenKind::Annotation,
                // `@Operator`: an engine catalogue type, where a meta field is lowercase.
                name if name.starts_with(|c: char| c.is_ascii_uppercase()) => TokenKind::Type,
                _ => TokenKind::Property,
            }
        } else if b.is_ascii_digit() {
            while i < len && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < len && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                i += 1;
                while i < len && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            TokenKind::Number
        } else if b.is_ascii_alphabetic() || b == b'_' {
            while i < len && is_ident_byte(bytes[i]) {
                i += 1;
            }
            // `merge-base` is one word to the server.
            if in_statement
                && &source[start..i] == "merge"
                && source[i..].starts_with("-base")
                && !bytes.get(i + 5).copied().is_some_and(is_ident_byte)
            {
                i += 5;
            }
            let word = &source[start..i];
            // The word before, a comment between them aside (`query /* … */ q(`).
            let previous = tokens.iter().rev().find(|t| t.kind != TokenKind::Comment);
            let after_dot = previous.is_some_and(|t| {
                t.kind == TokenKind::Punctuation && &source[t.range.clone()] == "."
            });
            let after_keyword = |token: Option<&Token>, keyword: &str| {
                token.is_some_and(|t| {
                    t.kind == TokenKind::Keyword && &source[t.range.clone()] == keyword
                })
            };
            let after_query = after_keyword(previous, "query");
            // How a declaration publishes: `as commit`, `as unattached branch on …`, after its
            // parameters' `)`; an alias named `commit` follows the value it names.
            let after_signature = || {
                let before = tokens
                    .iter()
                    .rev()
                    .filter(|t| t.kind != TokenKind::Comment)
                    .nth(1);
                before.is_some_and(|t| {
                    t.kind == TokenKind::Punctuation && &source[t.range.clone()] == ")"
                })
            };
            let publishes = match word {
                "commit" | "unattached" => after_keyword(previous, "as") && after_signature(),
                "on" => {
                    let before = tokens
                        .iter()
                        .rev()
                        .filter(|t| t.kind != TokenKind::Comment)
                        .nth(1);
                    after_keyword(previous, "branch") && after_keyword(before, "unattached")
                }
                _ => false,
            };
            if after_dot {
                TokenKind::Property
            } else if after_query {
                TokenKind::QueryName
            } else if publishes {
                TokenKind::Keyword
            } else if b.is_ascii_uppercase() {
                TokenKind::Type
            } else if KEYWORDS.contains(&word) {
                if matches!(word, "branch" | "show") && prefix == Prefix::Start {
                    in_statement = true;
                }
                TokenKind::Keyword
            } else if (word == "commit" && prefix == Prefix::Start)
                || (word == "load" && matches!(prefix, Prefix::Start | Prefix::Explain))
            {
                in_statement = true;
                TokenKind::Keyword
            } else if in_statement && STATEMENT_KEYWORDS.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Identifier
            }
        } else {
            i += source[i..].chars().next().map_or(1, char::len_utf8);
            TokenKind::Punctuation
        };
        let text = &source[start..i];
        prefix = match (prefix, kind) {
            (_, TokenKind::Comment) => prefix,
            (Prefix::Start, TokenKind::Keyword) if matches!(text, "set" | "reset") => {
                Prefix::Setting
            }
            (Prefix::Start, TokenKind::Keyword) if text == "explain" => Prefix::Explain,
            (Prefix::Setting, TokenKind::Punctuation) if text == ";" => Prefix::Start,
            (Prefix::Setting, _) => Prefix::Setting,
            _ => Prefix::Past,
        };
        tokens.push(Token {
            kind,
            range: start..i,
        });
    }
    tokens
}

/// One `query` declaration in a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The declared name; empty while it is still being typed.
    pub name: String,
    /// From the `query` keyword to the closing brace, or to the end of the source when the
    /// body is not closed yet.
    pub range: Range<usize>,
    /// The declaration is wrapped in an `explain` statement.
    pub explained: bool,
}

/// What a whole source holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outline {
    /// Settings lines and comments before the first statement or declaration.
    pub prefix: Range<usize>,
    pub declarations: Vec<Declaration>,
    /// The source is a `branch`, `commit`, `load` or `show` statement rather than declarations.
    pub statement: Option<Statement>,
}

/// A statement that is not a query declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statement {
    /// `show …`, `branch list`, `branch diff`, `branch merge-base` and `branch merge … dry
    /// run`: answered as rows, each naming its own branches.
    Read,
    /// `branch create`, `branch delete`, `branch merge`, `commit publish`, `commit drop`:
    /// changes branches or commits, each naming its own target.
    Write,
    /// `load csv … into …`: loads into the request's branch, as a mutation does.
    Load,
    /// `explain load …`: the plan of a load into the request's branch head, answered as rows.
    ExplainLoad,
}

impl Statement {
    /// Whether running it changes the graph.
    pub fn writes(self) -> bool {
        matches!(self, Statement::Write | Statement::Load)
    }

    /// Whether it is sent to the request's branch rather than naming its own.
    pub fn targets_branch(self) -> bool {
        matches!(self, Statement::Load | Statement::ExplainLoad)
    }
}

/// The declarations of a source and its settings prefix.
pub fn outline(source: &str) -> Outline {
    outline_of(source, &tokenize(source))
}

/// [`outline`] from `source`'s tokens, [`tokenize`]d once by a caller that reads them too.
pub fn outline_of(source: &str, tokens: &[Token]) -> Outline {
    let text = |t: &Token| &source[t.range.clone()];
    let mut outline = Outline::default();

    // The prefix is every `set …;`/`reset …;` line and comment before the first statement.
    let mut first = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token.kind == TokenKind::Comment {
            index += 1;
            continue;
        }
        if token.kind == TokenKind::Keyword && matches!(text(token), "set" | "reset") {
            while index < tokens.len() && text(&tokens[index]) != ";" {
                index += 1;
            }
            index += 1;
            continue;
        }
        first = Some(index);
        break;
    }
    let Some(first) = first else {
        outline.prefix = 0..source.len();
        return outline;
    };
    outline.prefix = 0..tokens[first].range.start;

    // A statement is told by its words, whatever comments sit between them.
    let words: Vec<&Token> = tokens[first..]
        .iter()
        .filter(|token| token.kind != TokenKind::Comment)
        .collect();
    match text(words[0]) {
        "show" => {
            outline.statement = Some(Statement::Read);
            return outline;
        }
        "branch" => {
            let verb = words.get(1).copied();
            let rest = verb.map_or("", |verb| &source[verb.range.start..]);
            // `merge-base` is one word to the server; `dry run` ends a merge that writes nothing.
            let dry_run = words
                .windows(2)
                .any(|pair| text(pair[0]) == "dry" && text(pair[1]) == "run");
            let reads = rest.starts_with("merge-base")
                || matches!(verb.map(text), Some("list" | "diff"))
                || (verb.map(text) == Some("merge") && dry_run);
            outline.statement = Some(if reads {
                Statement::Read
            } else {
                Statement::Write
            });
            return outline;
        }
        "commit" => {
            outline.statement = Some(Statement::Write);
            return outline;
        }
        "load" => {
            outline.statement = Some(Statement::Load);
            return outline;
        }
        "explain" if words.get(1).map(|word| text(word)) == Some("load") => {
            outline.statement = Some(Statement::ExplainLoad);
            return outline;
        }
        _ => {}
    }

    // `query`, a name and `(` begin a declaration even inside one left unclosed: a missing `}`
    // ends that one there rather than swallowing every declaration after it. Inside a body,
    // `query` can be a property (`order { query asc }`), which no `(` follows.
    let declares = |at: usize| {
        let mut words = tokens
            .get(at..)
            .unwrap_or_default()
            .iter()
            .filter(|t| t.kind != TokenKind::Comment);
        words
            .next()
            .is_some_and(|t| t.kind == TokenKind::Keyword && text(t) == "query")
            && words.next().is_some_and(|t| t.kind == TokenKind::QueryName)
            && words.next().is_some_and(|t| text(t) == "(")
    };
    let mut depth = 0usize;
    let mut current: Option<(usize, bool, String)> = None;
    let mut opened = false;
    let mut explain_pending = false;
    for (index, token) in tokens.iter().enumerate().skip(first) {
        let word = text(token);
        match token.kind {
            TokenKind::Keyword if word == "explain" && (depth == 0 || declares(index + 1)) => {
                close(&mut current, token.range.start, &mut outline.declarations);
                depth = 0;
                explain_pending = true;
            }
            TokenKind::Keyword if word == "query" && (depth == 0 || declares(index)) => {
                close(&mut current, token.range.start, &mut outline.declarations);
                depth = 0;
                let name = tokens[index + 1..]
                    .iter()
                    .find(|t| t.kind != TokenKind::Comment)
                    .filter(|t| t.kind == TokenKind::QueryName)
                    .map(|t| text(t).to_owned())
                    .unwrap_or_default();
                current = Some((token.range.start, explain_pending, name));
                explain_pending = false;
                opened = false;
            }
            TokenKind::Punctuation if word == "{" => {
                depth += 1;
                opened = true;
            }
            TokenKind::Punctuation if word == "}" => {
                depth = depth.saturating_sub(1);
                if depth == 0 && opened {
                    close(&mut current, token.range.end, &mut outline.declarations);
                    opened = false;
                }
            }
            _ => {}
        }
    }
    close(&mut current, source.len(), &mut outline.declarations);
    outline
}

/// Ends the declaration open in `current` (where it starts, whether it is explained, its name),
/// if one is, at `end`.
fn close(
    current: &mut Option<(usize, bool, String)>,
    end: usize,
    declarations: &mut Vec<Declaration>,
) {
    if let Some((start, explained, name)) = current.take() {
        declarations.push(Declaration {
            name,
            range: start..end,
            explained,
        });
    }
}

impl Outline {
    /// The declaration to run for a cursor at `offset`: the one whose range holds it, its end
    /// excluded, else the last one ending at or before it, else the first. The end is excluded
    /// because an unclosed declaration ends where the next one starts: that byte is the next
    /// one's, so the Run line above it runs it.
    pub fn declaration_at(&self, offset: usize) -> Option<&Declaration> {
        self.declarations
            .iter()
            .find(|d| d.range.contains(&offset))
            .or_else(|| {
                self.declarations
                    .iter()
                    .rev()
                    .find(|d| d.range.end <= offset)
            })
            .or_else(|| self.declarations.first())
    }

    /// The text that runs one declaration on its own: the settings prefix, then the
    /// declaration. A sibling with a syntax error does not stop it.
    pub fn runnable_text(&self, source: &str, declaration: &Declaration) -> String {
        let mut text = source[self.prefix.clone()].to_owned();
        text.push_str(&source[declaration.range.clone()]);
        text
    }

    /// The text that asks for one read declaration's plan: `explain query …`.
    pub fn explain_text(&self, source: &str, declaration: &Declaration) -> String {
        let mut text = source[self.prefix.clone()].to_owned();
        text.push_str("explain ");
        text.push_str(&source[declaration.range.clone()]);
        text
    }
}

/// Whether text typed at `offset` of `source`, whose `tokens` these are, goes into a comment or
/// a string: inside one, or at the end of one still open. A line comment runs to the end of its
/// line, so its end is inside it.
pub fn in_comment_or_string(source: &str, tokens: &[Token], offset: usize) -> bool {
    tokens.iter().any(|token| {
        let text = &source[token.range.clone()];
        let open = match token.kind {
            TokenKind::Comment => {
                text.starts_with("//") || !(text.len() >= 4 && text.ends_with("*/"))
            }
            TokenKind::String => !string_is_closed(text),
            _ => return false,
        };
        token.range.start < offset
            && (offset < token.range.end || (open && offset == token.range.end))
    })
}

/// Whether a string token ends with its closing quote, read as [`tokenize`] reads it: a
/// backslash takes the byte after it, and the first other `"` ends the string.
fn string_is_closed(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1 == bytes.len(),
            _ => i += 1,
        }
    }
    false
}

/// Whether a declaration's text writes, judged from its words alone: an `insert`, `update` or
/// `delete` inside its body. Used only when the compiler cannot say (the text does not parse);
/// a wrong guess is safe, because the server refuses a write sent as a read.
pub fn looks_like_mutation(source: &str) -> bool {
    tokenize(source).iter().any(|t| {
        t.kind == TokenKind::Keyword
            && matches!(&source[t.range.clone()], "insert" | "update" | "delete")
    })
}

/// The byte range to underline for a problem reported at `byte`: the token there among
/// `tokens` (`source`'s, [`tokenize`]d once for all its problems), or one character when no
/// token starts or covers it.
pub fn problem_range(source: &str, tokens: &[Token], byte: usize) -> Range<usize> {
    let byte = byte.min(source.len());
    // `source`'s tokens, made once for all its problems, in order and apart: the one holding
    // `byte` is found by halving.
    let at = tokens.partition_point(|t| t.range.end <= byte);
    if let Some(token) = tokens.get(at).filter(|t| t.range.start <= byte) {
        return token.range.clone();
    }
    let start = source.floor_char_boundary(byte);
    let end = source[start..]
        .chars()
        .next()
        .map_or(start, |c| start + c.len_utf8());
    if end > start {
        start..end
    } else {
        // At the end of the source: underline the last character instead.
        let previous = source[..start]
            .chars()
            .next_back()
            .map_or(start, |c| start - c.len_utf8());
        previous..start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<(TokenKind, &str)> {
        tokenize(source)
            .into_iter()
            .map(|t| (t.kind, &source[t.range]))
            .collect()
    }

    #[test]
    fn tokens_cover_every_kind() {
        let source = "// hi\nquery q($n: String) @description(\"a \\\"b\\\"\") {\n  match { $p: Person { name: $n } $p worksAt $c }\n  return { $p.name, $p.@id, count($c) }\n  limit 10 /* end */\n}";
        let tokens = kinds(source);
        assert_eq!(tokens[0], (TokenKind::Comment, "// hi"));
        assert_eq!(tokens[1], (TokenKind::Keyword, "query"));
        assert_eq!(tokens[2], (TokenKind::QueryName, "q"));
        assert!(tokens.contains(&(TokenKind::Variable, "$n")));
        assert!(tokens.contains(&(TokenKind::Type, "String")));
        assert!(tokens.contains(&(TokenKind::Annotation, "@description")));
        assert!(tokens.contains(&(TokenKind::String, "\"a \\\"b\\\"\"")));
        assert!(tokens.contains(&(TokenKind::Type, "Person")));
        assert!(tokens.contains(&(TokenKind::Identifier, "worksAt")));
        assert!(tokens.contains(&(TokenKind::Property, "name")));
        assert!(tokens.contains(&(TokenKind::Property, "@id")));
        assert!(tokens.contains(&(TokenKind::Keyword, "count")));
        assert!(tokens.contains(&(TokenKind::Number, "10")));
        assert_eq!(tokens.last(), Some(&(TokenKind::Punctuation, "}")));
        assert!(tokens.contains(&(TokenKind::Comment, "/* end */")));
    }

    #[test]
    fn statement_words_are_keywords_only_in_statements() {
        let tokens = kinds("branch create feature from main");
        assert!(tokens.contains(&(TokenKind::Keyword, "create")));
        assert!(tokens.contains(&(TokenKind::Keyword, "from")));
        let tokens = kinds("query q() { match { $p: Person } return { $p.list } }");
        assert!(tokens.contains(&(TokenKind::Property, "list")));
        for (source, words) in [
            (
                "branch diff \"01J\" to main after page \"p\" limit 10",
                &["branch", "diff", "to", "after", "page", "limit"][..],
            ),
            (
                "branch merge-base feature into main",
                &["branch", "merge-base", "into"],
            ),
            (
                "branch merge feature dry run",
                &["branch", "merge", "dry", "run"],
            ),
            (
                "branch merge feature as commit",
                &["branch", "merge", "as", "commit"],
            ),
            ("commit publish \"01J\"", &["commit", "publish"]),
            ("commit drop \"01J\"", &["commit", "drop"]),
            (
                "load csv \"p.csv\" into node:Person mode overwrite delimiter \"|\" header columns (a)",
                &[
                    "load",
                    "csv",
                    "into",
                    "node",
                    "mode",
                    "overwrite",
                    "delimiter",
                    "header",
                    "columns",
                ],
            ),
            (
                "explain load csv \"k.csv\" into edge:Knows mode append",
                &["explain", "load", "csv", "into", "edge", "mode", "append"],
            ),
        ] {
            let keywords: Vec<&str> = kinds(source)
                .into_iter()
                .filter(|(kind, _)| *kind == TokenKind::Keyword)
                .map(|(_, word)| word)
                .collect();
            assert_eq!(keywords, words, "{source}");
        }
        // A declaration's publication is keywords.
        let keywords = |source: &str| -> Vec<String> {
            kinds(source)
                .into_iter()
                .filter(|(kind, _)| *kind == TokenKind::Keyword)
                .map(|(_, word)| word.to_owned())
                .collect()
        };
        assert_eq!(
            keywords("query q() as commit { insert Person { slug: \"a\" } }"),
            ["query", "as", "commit", "insert"]
        );
        assert_eq!(
            keywords(
                "query q() as unattached branch on \"01J\" { delete Person where slug = \"a\" }"
            ),
            [
                "query",
                "as",
                "unattached",
                "branch",
                "on",
                "delete",
                "where"
            ]
        );
        // An alias named `commit` is a name.
        let tokens = kinds("query q() { match { $p: Person } return { $p.name as commit } }");
        assert!(tokens.contains(&(TokenKind::Identifier, "commit")));
        // Outside a statement they are names: a property `load`, an edge `commit`, a variable's
        // edge `on`.
        let tokens =
            kinds("query q() { match { $p commit $c $p on $d } return { $p.load, $p.mode } }");
        assert!(tokens.contains(&(TokenKind::Identifier, "commit")));
        assert!(tokens.contains(&(TokenKind::Identifier, "on")));
        assert!(tokens.contains(&(TokenKind::Property, "load")));
        assert!(tokens.contains(&(TokenKind::Property, "mode")));
        // An engine catalogue type is a type; a meta field stays a property.
        let tokens = kinds("query q() { match { $o: @Operator } return { $o.@id } }");
        assert!(tokens.contains(&(TokenKind::Type, "@Operator")));
        assert!(tokens.contains(&(TokenKind::Property, "@id")));
    }

    #[test]
    fn unterminated_strings_and_comments_end_at_the_source_end() {
        assert_eq!(kinds("\"abc"), vec![(TokenKind::String, "\"abc")]);
        assert_eq!(kinds("/* abc"), vec![(TokenKind::Comment, "/* abc")]);
        assert_eq!(kinds("é"), vec![(TokenKind::Punctuation, "é")]);
    }

    const TWO: &str = "set engine = v2;\n// first\nquery a() {\n  match { $p: Person }\n  return { $p.name }\n}\n\nquery b($x: I32) {\n  match { $c: Company { founded: $x } }\n  return { $c.name }\n}\n";

    #[test]
    fn outline_finds_declarations_and_the_prefix() {
        let outline = outline(TWO);
        assert_eq!(&TWO[outline.prefix.clone()], "set engine = v2;\n// first\n");
        assert_eq!(outline.declarations.len(), 2);
        assert_eq!(outline.declarations[0].name, "a");
        assert!(TWO[outline.declarations[0].range.clone()].starts_with("query a()"));
        assert!(TWO[outline.declarations[0].range.clone()].ends_with('}'));
        assert_eq!(outline.declarations[1].name, "b");
        assert!(outline.statement.is_none());
    }

    #[test]
    fn the_declaration_under_the_cursor_is_chosen() {
        let outline = outline(TWO);
        let inside_b = TWO.find("Company").unwrap();
        assert_eq!(outline.declaration_at(inside_b).unwrap().name, "b");
        let between = TWO.find("\n\nquery b").unwrap() + 1;
        assert_eq!(outline.declaration_at(between).unwrap().name, "a");
        assert_eq!(outline.declaration_at(0).unwrap().name, "a");
        let text = outline.runnable_text(TWO, outline.declaration_at(inside_b).unwrap());
        assert!(text.starts_with("set engine = v2;\n// first\nquery b($x: I32)"));
        assert!(!text.contains("query a"));
        let explain = outline.explain_text(TWO, outline.declaration_at(0).unwrap());
        assert!(explain.starts_with("set engine = v2;\n// first\nexplain query a()"));
    }

    #[test]
    fn an_unclosed_declaration_runs_to_the_end() {
        let source = "query a() { match { $p: Person } return { $p.name ";
        let outline = outline(source);
        assert_eq!(outline.declarations[0].range, 0..source.len());
        assert_eq!(outline.declaration_at(source.len()).unwrap().name, "a");
        // ...or to the next declaration, which it does not swallow.
        let source = "query a() { match { $p: Person } return { $p.name\n\n\
            query b() { match { $p: Person } return { $p } }\n\
            explain query c() { match { $p: Person } return { $p } }";
        let declarations = super::outline(source).declarations;
        let names: Vec<&str> = declarations.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(declarations[0].range.end, source.find("query b").unwrap());
        assert!(declarations[2].explained);
        let unclosed =
            "query a() { match {\nexplain query b() { match { $p: Person } return { $p } }";
        let declarations = super::outline(unclosed).declarations;
        assert_eq!(declarations[0].range.end, unclosed.find("explain").unwrap());
        assert!(declarations[1].explained);
        // A property named `query` is no declaration; a comment does not hide one.
        let ordered = "query a() { match { $s: Search } return { $s.query } order { query asc }";
        assert_eq!(super::outline(ordered).declarations.len(), 1);
        let commented = "query a() { match {\nquery /* the next */ b() { match { $p: Person } }";
        let names: Vec<String> = super::outline(commented)
            .declarations
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    /// An unclosed declaration ends where the next one starts, and that byte is the next one's:
    /// the Run line above the second sets the cursor there and runs the second.
    #[test]
    fn a_declaration_starts_where_an_unclosed_one_before_it_ends() {
        let source = "query a() {\n  match { $p: Person }\n  return { $p.name }\n\nquery b() {\n  match { $c: Company }\n  return { $c.name }\n}\n";
        let outline = outline(source);
        let (a, b) = (&outline.declarations[0], &outline.declarations[1]);
        assert_eq!(a.range.end, b.range.start, "a runs on to b");
        assert_eq!(outline.declaration_at(b.range.start).unwrap().name, "b");
        assert_eq!(outline.declaration_at(b.range.start - 1).unwrap().name, "a");
        // Past the last one's end, the last one is still the one to run.
        assert_eq!(outline.declaration_at(source.len()).unwrap().name, "b");
    }

    /// Text typed inside a comment or a string, or at the end of one still open, goes into it;
    /// typed after a closed one, it does not.
    #[test]
    fn comments_and_strings_hold_what_is_typed_in_them() {
        let at_end = |source: &str| in_comment_or_string(source, &tokenize(source), source.len());
        assert!(at_end("// see the docs."));
        assert!(at_end("query q() {} // group by company"));
        assert!(at_end("/* open"));
        assert!(at_end("query q() { match { $p: Person { name: \"Ad"));
        assert!(at_end("query q() @description(\"for comp"));
        assert!(at_end("\"a \\\""), "an escaped quote does not close it");
        assert!(!at_end("/* closed */"));
        assert!(!at_end("query q() { match { $p: Person { name: \"Ada\""));
        assert!(!at_end("query q() { match { $p: Pe"));
        let source = "// note\nquery";
        assert!(in_comment_or_string(source, &tokenize(source), 3));
        assert!(
            !in_comment_or_string(source, &tokenize(source), 0),
            "before the comment starts"
        );
        assert!(!in_comment_or_string(
            source,
            &tokenize(source),
            source.len()
        ));
    }

    #[test]
    fn explain_and_statements_are_recognised() {
        let outline_explain = outline("explain query a() { match { $p: Person } return { $p } }");
        assert!(outline_explain.declarations[0].explained);
        assert_eq!(outline("branch list").statement, Some(Statement::Read));
        assert_eq!(outline("show all;").statement, Some(Statement::Read));
        assert_eq!(
            outline("commit publish \"c1\"").statement,
            Some(Statement::Write)
        );
        assert_eq!(
            outline("load csv \"s3://b/p.csv\" into node:Person mode append").statement,
            Some(Statement::Load)
        );
        assert_eq!(
            outline("explain load csv \"s3://b/p.csv\" into node:Person mode append").statement,
            Some(Statement::ExplainLoad)
        );
        // The branch reads a server answers as rows, and the merge that only decides.
        for read in [
            "branch diff feature into main",
            "branch merge-base feature into main",
            "branch merge feature into main dry run",
        ] {
            assert_eq!(outline(read).statement, Some(Statement::Read), "{read}");
        }
        assert_eq!(
            outline("branch merge \"dry\" into main").statement,
            Some(Statement::Write),
            "a branch named dry"
        );
        assert_eq!(
            outline("branch create x from main").statement,
            Some(Statement::Write)
        );
        assert!(outline("").declarations.is_empty());
        // Comments between a statement's words do not change what it is.
        assert_eq!(
            outline("branch /* all of them */ list").statement,
            Some(Statement::Read)
        );
        assert_eq!(
            outline("explain // the plan\nload csv \"s3://b/p.csv\" into node:Person mode append")
                .statement,
            Some(Statement::ExplainLoad)
        );
    }

    #[test]
    fn mutations_are_recognised_lexically() {
        assert!(looks_like_mutation(
            "query a($s: String) { insert Company { slug: $s, name: $s } }"
        ));
        assert!(!looks_like_mutation(
            "query a() { match { $p: Person } return { $p.name } }"
        ));
        // A property called `delete` is not a mutation.
        assert!(!looks_like_mutation(
            "query a() { match { $p: Person } return { $p.delete } }"
        ));
    }

    #[test]
    fn problems_underline_a_token() {
        let source = "query q() { match { $p: Person } return { $p.nope } }";
        let at = source.find("nope").unwrap();
        assert_eq!(
            &source[problem_range(source, &tokenize(source), at)],
            "nope"
        );
        assert_eq!(
            &source[problem_range(source, &tokenize(source), source.len())],
            "}"
        );
        assert_eq!(
            problem_range(source, &tokenize(source), at + 4),
            at + 4..at + 5
        );
    }

    /// A statement starts at the first word after the settings prefix, as `outline` reads it:
    /// a comment or a `set` before it leaves `branch merge-base` a statement, and a leading
    /// `reset` (a setting) makes no statement of the queries after it.
    #[test]
    fn statements_start_after_the_settings_prefix() {
        let words = |source: &str, kind: TokenKind| -> Vec<String> {
            tokenize(source)
                .into_iter()
                .filter(|token| token.kind == kind)
                .map(|token| source[token.range].to_owned())
                .collect()
        };
        let merge = "// first\nset engine = v2;\nbranch merge-base x into main";
        assert!(words(merge, TokenKind::Keyword).contains(&"merge-base".to_owned()));
        let reset = "reset engine;\nquery q() { match { $p: Person } return { $p.mode } }";
        assert!(!words(reset, TokenKind::Keyword).contains(&"mode".to_owned()));
    }
}
