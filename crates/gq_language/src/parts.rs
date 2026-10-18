//! The parts of a read declaration that a plan operator stands for (spec 007): each clause of
//! its `match` block (a binding, a traversal, a filter, a `not`, `exists` or counted block) and
//! its `return`, `order` and `limit` clauses, with their byte ranges, so hovering an operator of
//! an `explain` tree lights the text it came from.
//!
//! Lexical, like the outline: the compiler's AST carries no spans. Clauses follow `query.pest`
//! at OmniGraph v0.13.0, where a `match` block's clauses have no separator, so each one is read
//! to its end. An operator names what it reads (`binding`, `src` and `dst`, a filter's text,
//! an anti-join's outer variable), and those names are matched back to clauses.

use std::ops::Range;

use serde_json::Value;

use crate::lex::{Token, TokenKind, tokenize};

/// What a part of a declaration is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartKind {
    /// `$p: Person`, with its property matches.
    Binding {
        var: String,
    },
    /// `$p worksAt $c`, `$a <knows>{1,3} $b`.
    Traversal {
        src: String,
        dst: String,
    },
    /// A Boolean clause: `$p.age > 30`.
    Filter,
    /// `not { … }`.
    Negation,
    /// `exists { … }`, `count { … } > 2`.
    Subquery,
    Return,
    Order,
    Limit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPart {
    pub kind: PartKind,
    pub range: Range<usize>,
    /// The clause's tokens with no space between them: the form an operator's text is
    /// compared in, since the planner renders it with its own spacing.
    pub compact: String,
    /// The variables the clause names, without `$`, each once, in order.
    pub vars: Vec<String>,
}

/// The parts of the declaration at `declaration` in `source`. A declaration that does not
/// parse yields the parts read before the problem.
pub fn query_parts(source: &str, declaration: Range<usize>) -> Vec<QueryPart> {
    let tokens: Vec<Token> = tokenize(source)
        .into_iter()
        .filter(|t| {
            t.kind != TokenKind::Comment
                && t.range.start >= declaration.start
                && t.range.end <= declaration.end
        })
        .collect();
    let mut reader = Reader {
        source,
        tokens: &tokens,
        parts: Vec::new(),
    };
    // The body opens at the first brace: parameters and annotations hold none.
    let Some(open) = (0..tokens.len()).find(|&i| reader.is(i, "{")) else {
        return Vec::new();
    };
    let close = reader.closing(open, tokens.len());
    let mut i = open + 1;
    while i < close {
        let keyword = |word| reader.keyword(i, word);
        if keyword("match") && reader.is(i + 1, "{") {
            let end = reader.closing(i + 1, close);
            reader.clauses(i + 2, end);
            i = end + 1;
        } else if let Some(open) = (keyword("return") || keyword("order"))
            .then(|| {
                // `return distinct { … }` keeps one row per distinct tuple.
                let at = i + usize::from(keyword("return") && reader.keyword(i + 1, "distinct"));
                reader.is(at + 1, "{").then_some(at + 1)
            })
            .flatten()
        {
            let kind = if keyword("return") {
                PartKind::Return
            } else {
                PartKind::Order
            };
            let end = reader.closing(open, close);
            reader.push(kind, i, end.min(close - 1));
            i = end + 1;
        } else if keyword("limit") {
            let last = if reader.kind(i + 1) == Some(TokenKind::Number) {
                i + 1
            } else {
                i
            };
            reader.push(PartKind::Limit, i, last.min(close - 1));
            i = last + 1;
        } else {
            i += 1;
        }
    }
    reader.parts
}

/// The ranges of the parts that the operator `node` with `detail` stands for: JSON details for
/// the logical and physical trees, text for DataFusion's. Empty when nothing in the declaration
/// matches, as for an operator the planner adds on its own.
pub fn operator_parts(parts: &[QueryPart], node: &str, detail: &str) -> Vec<Range<usize>> {
    let name = node.strip_suffix("Exec").unwrap_or(node);
    let json = serde_json::from_str::<Value>(detail)
        .ok()
        .filter(Value::is_object);
    let vars = operator_vars(json.as_ref(), detail);
    let filters = operator_filters(name, json.as_ref(), detail);
    let of_kind = |kind: PartKind| -> Vec<Range<usize>> {
        parts
            .iter()
            .filter(|part| part.kind == kind)
            .map(|part| part.range.clone())
            .collect()
    };
    let mut found = match name {
        "Limit" | "Page" | "GlobalLimit" | "LocalLimit" | "Offset" => of_kind(PartKind::Limit),
        "Sort" | "TopK" | "PartialSort" | "SortPreservingMerge" => of_kind(PartKind::Order),
        "Projection" | "Aggregate" => of_kind(PartKind::Return),
        n if n.starts_with("AntiJoin") => blocks(parts, PartKind::Negation, &vars),
        n if n.contains("SemiJoin") || n.contains("Subquery") || n.contains("Exists") => {
            blocks(parts, PartKind::Subquery, &vars)
        }
        n if n.contains("Expand") || n.contains("Travers") => traversals(parts, &vars),
        n if n.contains("Filter") => matching_filters(parts, &filters),
        n if is_search(n) => {
            let mut found = of_kind(PartKind::Order);
            found.extend(
                parts
                    .iter()
                    .filter(|part| part.kind == PartKind::Filter && has_search_call(part))
                    .map(|part| part.range.clone()),
            );
            found
        }
        n if n.contains("Scan") || n.contains("Lookup") || n.contains("Join") => {
            let mut found = binding_of(parts, vars.first().map(String::as_str));
            found.extend(matching_filters(parts, &filters));
            found
        }
        _ => Vec::new(),
    };
    found.sort_by_key(|range| (range.start, range.end));
    found.dedup();
    found
}

/// A text's tokens with no space between them.
fn compact(text: &str) -> String {
    tokenize(text)
        .into_iter()
        .filter(|t| t.kind != TokenKind::Comment)
        .map(|t| &text[t.range])
        .collect()
}

/// The variables an operator reads: its `binding`, `src`, `dst` or `outer_var` details, or the
/// `$variables` of a text detail, or the `f` of DataFusion's `on=f.__id`.
fn operator_vars(json: Option<&Value>, detail: &str) -> Vec<String> {
    fn add(vars: &mut Vec<String>, var: &str) {
        let var = var.trim_start_matches('$');
        if !var.is_empty() && !vars.iter().any(|v| v == var) {
            vars.push(var.to_owned());
        }
    }
    let mut vars: Vec<String> = Vec::new();
    if let Some(json) = json {
        for key in ["binding", "src", "dst", "outer_var"] {
            if let Some(var) = json.get(key).and_then(Value::as_str) {
                add(&mut vars, var);
            }
        }
        return vars;
    }
    let tokens = tokenize(detail);
    for token in tokens.iter().filter(|t| t.kind == TokenKind::Variable) {
        add(&mut vars, &detail[token.range.clone()]);
    }
    if vars.is_empty() {
        for pair in tokens.windows(2) {
            if pair[0].kind == TokenKind::Identifier && &detail[pair[1].range.clone()] == "." {
                add(&mut vars, &detail[pair[0].range.clone()]);
            }
        }
    }
    vars
}

/// The compacted texts of the filters an operator applies.
fn operator_filters(name: &str, json: Option<&Value>, detail: &str) -> Vec<String> {
    let mut texts = Vec::new();
    match json {
        Some(json) => collect_filter_texts(json, &mut texts),
        None if name.contains("Filter") => texts.push(detail.to_owned()),
        None => {
            if let Some(start) = detail.find("filters=[") {
                let inside = &detail[start + "filters=[".len()..];
                texts.extend(split_top_level(inside));
            }
        }
    }
    texts
        .iter()
        .map(|text| compact(text))
        .filter(|text| !text.is_empty())
        .collect()
}

/// Every `text` string, and every string of a `filters` list, below `value`; the statistics a
/// physical operator carries under `properties` are not filters.
fn collect_filter_texts(value: &Value, texts: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                match (key.as_str(), value) {
                    ("properties", _) => {}
                    ("text", Value::String(text)) => texts.push(text.clone()),
                    ("filters", Value::Array(items)) => {
                        for item in items {
                            match item {
                                Value::String(text) => texts.push(text.clone()),
                                other => collect_filter_texts(other, texts),
                            }
                        }
                    }
                    (_, value) => collect_filter_texts(value, texts),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_filter_texts(item, texts);
            }
        }
        _ => {}
    }
}

/// The items of a bracketed list, up to the bracket that closes it: split at commas outside
/// strings and nested brackets.
fn split_top_level(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut current = String::new();
    for ch in text.chars() {
        if quoted {
            current.push(ch);
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => {
                quoted = true;
                current.push(ch);
            }
            '[' | '(' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ']' if depth == 0 => break,
            ']' | ')' | '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    items.push(current);
    items
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

fn is_search(name: &str) -> bool {
    [
        "Nearest", "Knn", "Vector", "Search", "Rrf", "Bm25", "FullText", "Fts",
    ]
    .iter()
    .any(|word| name.contains(word))
}

fn has_search_call(part: &QueryPart) -> bool {
    [
        "search(",
        "fuzzy(",
        "match_text(",
        "bm25(",
        "nearest(",
        "rrf(",
    ]
    .iter()
    .any(|call| part.compact.contains(call))
}

/// The blocks of `kind` that name one of `vars`, else every block of that kind.
fn blocks(parts: &[QueryPart], kind: PartKind, vars: &[String]) -> Vec<Range<usize>> {
    let of_kind = || parts.iter().filter(|part| part.kind == kind);
    let naming: Vec<_> = of_kind()
        .filter(|part| part.vars.iter().any(|var| vars.contains(var)))
        .map(|part| part.range.clone())
        .collect();
    if naming.is_empty() {
        of_kind().map(|part| part.range.clone()).collect()
    } else {
        naming
    }
}

/// The traversals between the operator's two variables, in either direction: a planner may
/// expand from either end.
fn traversals(parts: &[QueryPart], vars: &[String]) -> Vec<Range<usize>> {
    let [a, b, ..] = vars else {
        return Vec::new();
    };
    parts
        .iter()
        .filter(|part| match &part.kind {
            PartKind::Traversal { src, dst } => (src == a && dst == b) || (src == b && dst == a),
            _ => false,
        })
        .map(|part| part.range.clone())
        .collect()
}

/// Where `var` comes from: its binding, else the traversal that reaches it, else the one that
/// leaves it (`$p worksAt $c` binds `$c` when no `$c: Company` is written).
fn binding_of(parts: &[QueryPart], var: Option<&str>) -> Vec<Range<usize>> {
    let Some(var) = var else {
        return Vec::new();
    };
    let find = |matches: &dyn Fn(&PartKind) -> bool| -> Vec<Range<usize>> {
        parts
            .iter()
            .filter(|part| matches(&part.kind))
            .map(|part| part.range.clone())
            .collect()
    };
    let bound = find(&|kind| matches!(kind, PartKind::Binding { var: v } if v == var));
    if !bound.is_empty() {
        return bound;
    }
    let reached = find(&|kind| matches!(kind, PartKind::Traversal { dst, .. } if dst == var));
    if !reached.is_empty() {
        return reached;
    }
    find(&|kind| matches!(kind, PartKind::Traversal { src, .. } if src == var))
}

/// The filter clauses behind an operator's filter texts: the same text, else one holding the
/// other (the planner splits `and` and merges conjuncts), else one reading the same properties
/// of the same variables.
fn matching_filters(parts: &[QueryPart], filters: &[String]) -> Vec<Range<usize>> {
    let candidates: Vec<&QueryPart> = parts
        .iter()
        .filter(|part| part.kind == PartKind::Filter)
        .collect();
    let mut found = Vec::new();
    for filter in filters {
        let same: Vec<_> = candidates
            .iter()
            .filter(|part| &part.compact == filter)
            .collect();
        let holding: Vec<_> = candidates
            .iter()
            .filter(|part| part.compact.contains(filter.as_str()) || filter.contains(&part.compact))
            .collect();
        let reads = property_reads(filter);
        let reading: Vec<_> = candidates
            .iter()
            .filter(|part| !reads.is_empty() && property_reads(&part.compact) == reads)
            .collect();
        let chosen = [same, holding, reading]
            .into_iter()
            .find(|list| !list.is_empty())
            .unwrap_or_default();
        found.extend(chosen.into_iter().map(|part| part.range.clone()));
    }
    found
}

/// The `$var.property` reads of a compacted text, sorted, each once.
fn property_reads(text: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut reads: Vec<String> = tokens
        .windows(3)
        .filter(|w| {
            w[0].kind == TokenKind::Variable
                && &text[w[1].range.clone()] == "."
                && w[2].kind == TokenKind::Property
        })
        .map(|w| text[w[0].range.start..w[2].range.end].to_owned())
        .collect();
    reads.sort();
    reads.dedup();
    reads
}

struct Reader<'a> {
    source: &'a str,
    tokens: &'a [Token],
    parts: Vec<QueryPart>,
}

const AGGREGATES: &[&str] = &["count", "sum", "avg", "min", "max"];
const CALLS: &[&str] = &[
    "now",
    "search",
    "fuzzy",
    "match_text",
    "bm25",
    "rrf",
    "nearest",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "date",
    "datetime",
];
/// Words that join two operands.
const JOINERS: &[&str] = &["and", "or", "in", "contains", "starts_with"];
/// Words an edge may not be named.
const RESERVED: &[&str] = &["and", "or", "not", "is", "null", "in"];

impl Reader<'_> {
    fn text(&self, i: usize) -> &str {
        self.tokens
            .get(i)
            .map_or("", |t| &self.source[t.range.clone()])
    }

    fn kind(&self, i: usize) -> Option<TokenKind> {
        self.tokens.get(i).map(|t| t.kind)
    }

    fn is(&self, i: usize, punctuation: &str) -> bool {
        self.kind(i) == Some(TokenKind::Punctuation) && self.text(i) == punctuation
    }

    fn keyword(&self, i: usize, word: &str) -> bool {
        self.kind(i) == Some(TokenKind::Keyword) && self.text(i) == word
    }

    /// The index of the bracket closing the one at `open`, or `limit` when it is not closed
    /// before it.
    fn closing(&self, open: usize, limit: usize) -> usize {
        let close = match self.text(open) {
            "{" => "}",
            "(" => ")",
            "[" => "]",
            _ => return open,
        };
        let opener = self.text(open);
        let mut depth = 0usize;
        for i in open..limit.min(self.tokens.len()) {
            if self.kind(i) != Some(TokenKind::Punctuation) {
                continue;
            }
            match self.text(i) {
                t if t == opener => depth += 1,
                t if t == close => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
        }
        limit
    }

    /// Records the part spanning tokens `first..=last`.
    fn push(&mut self, kind: PartKind, first: usize, last: usize) {
        let (Some(start), Some(end)) = (self.tokens.get(first), self.tokens.get(last)) else {
            return;
        };
        let mut vars: Vec<String> = Vec::new();
        let mut compact = String::new();
        for token in &self.tokens[first..=last] {
            let text = &self.source[token.range.clone()];
            compact.push_str(text);
            if token.kind == TokenKind::Variable {
                let var = text.trim_start_matches('$').to_owned();
                if !vars.contains(&var) {
                    vars.push(var);
                }
            }
        }
        self.parts.push(QueryPart {
            kind,
            range: start.range.start..end.range.end,
            compact,
            vars,
        });
    }

    /// Reads the clauses from `i` up to the block's closing brace at `end`.
    fn clauses(&mut self, mut i: usize, end: usize) {
        while i < end {
            let next = self.clause(i, end);
            i = next.max(i + 1);
        }
    }

    /// Reads one clause starting at `i`; returns the index after it.
    fn clause(&mut self, i: usize, end: usize) -> usize {
        if (self.keyword(i, "not") || self.keyword(i, "exists")) && self.is(i + 1, "{") {
            let close = self.closing(i + 1, end);
            self.clauses(i + 2, close);
            let kind = if self.keyword(i, "not") {
                PartKind::Negation
            } else {
                PartKind::Subquery
            };
            self.push(kind, i, close.min(end - 1));
            return close + 1;
        }
        if self.kind(i) == Some(TokenKind::Keyword) && AGGREGATES.contains(&self.text(i)) {
            let mut j = i + 1;
            if self.is(j, "(") {
                j = self.closing(j, end) + 1;
            }
            if self.is(j, "{") {
                let close = self.closing(j, end);
                self.clauses(j + 1, close);
                let after = self.tail(close + 1, end);
                self.push(PartKind::Subquery, i, after.min(end) - 1);
                return after;
            }
        }
        if self.kind(i) == Some(TokenKind::Variable) {
            let var = self.text(i).trim_start_matches('$').to_owned();
            if self.is(i + 1, ":") && self.kind(i + 2) == Some(TokenKind::Type) {
                let mut j = i + 3;
                if self.is(j, "{") {
                    j = self.closing(j, end) + 1;
                }
                self.push(PartKind::Binding { var }, i, j.min(end) - 1);
                return j;
            }
            if let Some((after, dst)) = self.traversal(i, end) {
                self.push(PartKind::Traversal { src: var, dst }, i, after - 1);
                return after;
            }
        }
        let after = self.expr(i, end);
        self.push(PartKind::Filter, i, after.min(end) - 1);
        after
    }

    /// A traversal starting at the variable at `i`: the index after it and its far variable.
    fn traversal(&self, i: usize, end: usize) -> Option<(usize, String)> {
        let mut j = i + 1;
        // An edge binding: `$p $w:worksAt $c`.
        if self.kind(j) == Some(TokenKind::Variable) && self.is(j + 1, ":") {
            j += 2;
        }
        let edge_word = match self.kind(j) {
            Some(TokenKind::Identifier | TokenKind::String) => true,
            Some(TokenKind::Keyword) => !RESERVED.contains(&self.text(j)),
            _ => false,
        };
        if edge_word || self.is(j, "*") {
            j += 1;
        } else if self.is(j, "<") {
            j = (j + 1..end).find(|&k| self.is(k, ">"))? + 1;
        } else if self.is(j, "(") {
            j = self.closing(j, end) + 1;
        } else {
            return None;
        }
        if self.is(j, "{") && self.kind(j + 1) == Some(TokenKind::Number) {
            j = self.closing(j, end) + 1;
        }
        (j < end && self.kind(j) == Some(TokenKind::Variable))
            .then(|| (j + 1, self.text(j).trim_start_matches('$').to_owned()))
    }

    /// An expression starting at `i`; returns the index after it.
    fn expr(&self, i: usize, end: usize) -> usize {
        let j = self.prefixed_operand(i, end);
        self.tail(j, end)
    }

    fn prefixed_operand(&self, mut j: usize, end: usize) -> usize {
        while j < end && self.keyword(j, "not") && !self.is(j + 1, "{") {
            j += 1;
        }
        self.operand(j, end)
    }

    /// What may follow an operand: `is [not] null`, then a joining word or a comparison and the
    /// next operand, again and again.
    fn tail(&self, mut j: usize, end: usize) -> usize {
        loop {
            if self.keyword(j, "is") {
                j += 1;
                if self.keyword(j, "not") {
                    j += 1;
                }
                if self.keyword(j, "null") {
                    j += 1;
                }
            }
            if j >= end {
                return j.min(end);
            }
            if self.kind(j) == Some(TokenKind::Keyword) && JOINERS.contains(&self.text(j)) {
                j = self.prefixed_operand(j + 1, end);
                continue;
            }
            if let Some(width) = self.comparison(j) {
                j = self.prefixed_operand(j + width, end);
                continue;
            }
            return j;
        }
    }

    /// How many tokens the comparison operator at `j` spans: `!=`, `>=` and `<=` are two.
    fn comparison(&self, j: usize) -> Option<usize> {
        let then_equals = self.is(j + 1, "=");
        match self.text(j) {
            _ if self.kind(j) != Some(TokenKind::Punctuation) => None,
            "=" => Some(1),
            "!" if then_equals => Some(2),
            "<" | ">" => Some(if then_equals { 2 } else { 1 }),
            _ => None,
        }
    }

    fn operand(&self, j: usize, end: usize) -> usize {
        if j >= end {
            return j;
        }
        if self.is(j, "(") || self.is(j, "[") {
            return self.closing(j, end) + 1;
        }
        if self.is(j, "-") && self.kind(j + 1) == Some(TokenKind::Number) {
            return j + 2;
        }
        let call = match self.kind(j) {
            Some(TokenKind::Keyword) => CALLS.contains(&self.text(j)),
            Some(TokenKind::Identifier) => true,
            _ => false,
        };
        if call && self.is(j + 1, "(") {
            return self.closing(j + 1, end) + 1;
        }
        if self.kind(j) == Some(TokenKind::Variable) && self.is(j + 1, ".") {
            return j + 3;
        }
        j + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: &str = r#"query q() {
  match {
    $p: Person { name: "Ada" }
    $p knows $f
    not { $f worksAt $c }
    $p.name != $f.name and $p.born is not null
    count { $p contributes $j } > 2
  }
  return { $p.name, count($f) as friends }
  order { $p.name asc }
  limit 5
}"#;

    fn parts() -> Vec<QueryPart> {
        query_parts(QUERY, 0..QUERY.len())
    }

    fn shown(ranges: &[Range<usize>]) -> Vec<&'static str> {
        ranges.iter().map(|range| &QUERY[range.clone()]).collect()
    }

    #[test]
    fn the_fixture_is_a_query_the_compiler_accepts() {
        let check = crate::check(&crate::schema::tests::dev(), QUERY);
        assert_eq!(check.diagnostics, []);
    }

    #[test]
    fn clauses_are_read_to_their_ends() {
        let parts = parts();
        let texts: Vec<(&PartKind, &str)> = parts
            .iter()
            .map(|part| (&part.kind, &QUERY[part.range.clone()]))
            .collect();
        assert_eq!(
            texts,
            [
                (
                    &PartKind::Binding { var: "p".into() },
                    r#"$p: Person { name: "Ada" }"#
                ),
                (
                    &PartKind::Traversal {
                        src: "p".into(),
                        dst: "f".into()
                    },
                    "$p knows $f"
                ),
                (
                    &PartKind::Traversal {
                        src: "f".into(),
                        dst: "c".into()
                    },
                    "$f worksAt $c"
                ),
                (&PartKind::Negation, "not { $f worksAt $c }"),
                (
                    &PartKind::Filter,
                    "$p.name != $f.name and $p.born is not null"
                ),
                (
                    &PartKind::Traversal {
                        src: "p".into(),
                        dst: "j".into()
                    },
                    "$p contributes $j"
                ),
                (&PartKind::Subquery, "count { $p contributes $j } > 2"),
                (
                    &PartKind::Return,
                    "return { $p.name, count($f) as friends }"
                ),
                (&PartKind::Order, "order { $p.name asc }"),
                (&PartKind::Limit, "limit 5"),
            ]
        );
        // A return of distinct rows is the return clause too.
        let distinct = "query q() { match { $p: Person } return distinct { $p.name } }";
        assert_eq!(
            crate::check(&crate::schema::tests::dev(), distinct).diagnostics,
            []
        );
        let returns: Vec<&str> = query_parts(distinct, 0..distinct.len())
            .iter()
            .filter(|part| part.kind == PartKind::Return)
            .map(|part| &distinct[part.range.clone()])
            .collect();
        assert_eq!(returns, ["return distinct { $p.name }"]);
    }

    #[test]
    fn logical_and_physical_operators_find_their_clauses() {
        let parts = parts();
        let at = |node: &str, detail: &str| shown(&operator_parts(&parts, node, detail));
        assert_eq!(
            at(
                "TableScan",
                r#"{"binding":"p","table":"node:Person","filter":null}"#
            ),
            [r#"$p: Person { name: "Ada" }"#]
        );
        // `$c` has no binding of its own: the traversal that reaches it stands for it.
        assert_eq!(
            at("Scan", r#"{"binding":"c","table":"node:Company"}"#),
            ["$f worksAt $c"]
        );
        assert_eq!(
            at(
                "Expand",
                r#"{"src":"f","dst":"p","edges":{"kind":"named"}}"#
            ),
            ["$p knows $f"]
        );
        assert_eq!(
            at("AntiJoin", r#"{"outer_var":"f","predicate":"count = 0"}"#),
            ["not { $f worksAt $c }"]
        );
        // The planner splits `and`; each conjunct finds the clause holding it.
        assert_eq!(
            at(
                "Filter",
                r#"{"conjuncts":[{"kind":"gq","text":"$p.name != $f.name"}]}"#
            ),
            ["$p.name != $f.name and $p.born is not null"]
        );
        assert_eq!(
            at(
                "Filter",
                r#"{"filters":["$p.born is not null"],"properties":{}}"#
            ),
            ["$p.name != $f.name and $p.born is not null"]
        );
        assert_eq!(at("Page", r#"{"rows":5}"#), ["limit 5"]);
        assert_eq!(
            at("Sort", r#"{"keys":["$p.name asc"]}"#),
            ["order { $p.name asc }"]
        );
        assert_eq!(
            at("Aggregate", r#"{"exprs":["$p.name","count($f)"]}"#),
            ["return { $p.name, count($f) as friends }"]
        );
        assert!(at("OuterReference", r#"{"outer_var":"f"}"#).is_empty());
    }

    #[test]
    fn datafusion_operators_find_their_clauses() {
        let parts = parts();
        let at = |node: &str, detail: &str| shown(&operator_parts(&parts, node, detail));
        assert_eq!(
            at(
                "ExpandExec",
                "$p named [Knows out] $f: Person, hops=1..1, mode=indexed_scan"
            ),
            ["$p knows $f"]
        );
        assert_eq!(
            at(
                "ScanExec",
                r#"$p: Person, filters=[$p.name != "x", $p.born is not null], projection=[__id]"#
            ),
            [
                r#"$p: Person { name: "Ada" }"#,
                "$p.name != $f.name and $p.born is not null"
            ]
        );
        assert_eq!(
            at("HashJoinExec", "on=c.__id, fallback=id_lookup"),
            ["$f worksAt $c"]
        );
        assert_eq!(
            at("FilterExec", "$p.name != $f.name"),
            ["$p.name != $f.name and $p.born is not null"]
        );
        assert_eq!(
            at("AntiJoinMaskExec", "$f count = 0, bulk=WorksAt Out"),
            ["not { $f worksAt $c }"]
        );
        assert_eq!(at("LimitExec", "rows=5"), ["limit 5"]);
    }

    #[test]
    fn traversal_shapes_and_unclosed_text() {
        let source = "query q() { match { $a $w:knows{1,3} $b $b <knows> $c $c (owns | knows) $d $d.x = \"}\" }";
        let parts = query_parts(source, 0..source.len());
        let texts: Vec<&str> = parts.iter().map(|p| &source[p.range.clone()]).collect();
        assert_eq!(
            texts,
            [
                "$a $w:knows{1,3} $b",
                "$b <knows> $c",
                "$c (owns | knows) $d",
                "$d.x = \"}\"",
            ]
        );
        assert!(query_parts("query q(", 0..8).is_empty());
    }
}
