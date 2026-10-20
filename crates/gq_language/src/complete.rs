//! Completion for `.gq` source from the schema (spec 007).
//!
//! A lexical heuristic, not a parser: after `$` it offers the parameters and bindings of the
//! declaration under the cursor; after `$var.` the bound type's properties; after `$param:` in
//! a signature the parameter types; otherwise types, relationships (traversal spelling) and
//! keywords that start with the word being typed.

use std::ops::Range;

use crate::lex::{KEYWORDS, TokenKind, outline, tokenize};
use crate::schema::{SchemaModel, traversal_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub detail: Option<String>,
}

/// The types a parameter can be declared with; `Vector` takes its dimension, `Vector(3)`.
const PARAMETER_TYPES: &[&str] = &[
    "String", "Bool", "I32", "I64", "U32", "U64", "F32", "F64", "Date", "DateTime", "Blob",
    "Vector",
];

/// Completions for the word ending at `offset` (a byte offset into `source`). Returns the byte
/// offset where the word starts, which the editor replaces, and the candidates.
pub fn completions(schema: &SchemaModel, source: &str, offset: usize) -> (usize, Vec<Completion>) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let word = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    // After `$var.`, a field's name begins with `@` (`@src`): it is part of the word.
    let start = if before[..word].ends_with(".@") {
        word - 1
    } else {
        word
    };
    let prefix = &source[start..offset];
    let lower = prefix.to_lowercase();
    let matches = |label: &str| label.to_lowercase().starts_with(&lower);

    let declaration = outline(source)
        .declaration_at(offset)
        .map(|d| d.range.clone())
        .unwrap_or(0..source.len());
    let signature = signature(source, declaration.clone());
    let in_signature = signature
        .as_ref()
        .is_some_and(|s| s.start <= start && offset <= s.end);

    // `$` → the declaration's parameters and bindings; none while a parameter is being named.
    if before[..start].ends_with('$') {
        if in_signature {
            return (start, Vec::new());
        }
        let mut items: Vec<Completion> = Vec::new();
        let names = parameters(source, signature.as_ref())
            .into_iter()
            .chain(bindings(schema, source, declaration, signature.as_ref()));
        for (name, detail) in names {
            if matches(&name) && name != prefix && !items.iter().any(|i| i.label == name) {
                items.push(Completion {
                    label: name,
                    detail: Some(detail),
                });
            }
        }
        return (start, items);
    }

    // `$param: ` in a signature → parameter types.
    if in_signature && ends_with_after_space(&before[..start], &[':', '[']) {
        let items = PARAMETER_TYPES
            .iter()
            .filter(|t| matches(t))
            .map(|t| Completion {
                label: (*t).into(),
                detail: Some("parameter type".into()),
            })
            .collect();
        return (start, items);
    }

    // `$var.` → properties of the type `$var` is bound to.
    if before[..start].ends_with('.') {
        let var_end = start - 1;
        let var_start = before[..var_end]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map(|(i, _)| i)
            .unwrap_or(var_end);
        if var_start > 0 && before.as_bytes()[var_start - 1] == b'$' {
            let var = &before[var_start..var_end];
            let mut items = Vec::new();
            let bound = bindings(schema, source, declaration, signature.as_ref())
                .into_iter()
                .find(|(name, _)| name == var)
                .map(|(_, type_name)| type_name);
            let is_edge = bound
                .as_ref()
                .is_some_and(|type_name| schema.edge(type_name).is_some());
            if let Some(type_name) = bound {
                let properties = schema
                    .node(&type_name)
                    .map(|n| n.properties.clone())
                    .or_else(|| schema.edge(&type_name).map(|e| e.properties.clone()))
                    .unwrap_or_default();
                for property in properties {
                    if matches(&property.name) {
                        items.push(Completion {
                            label: property.name.clone(),
                            detail: Some(property.display_type()),
                        });
                    }
                }
            }
            // A relationship also names its ends and its type.
            let metas: &[&str] = if is_edge {
                &["@id", "@src", "@dst", "@type"]
            } else {
                &["@id"]
            };
            for meta in metas {
                if matches(meta) {
                    items.push(Completion {
                        label: (*meta).into(),
                        detail: Some("String".into()),
                    });
                }
            }
            return (start, items);
        }
    }

    let mut items = Vec::new();
    for node in &schema.nodes {
        if matches(&node.name) {
            items.push(Completion {
                label: node.name.clone(),
                detail: Some("node".into()),
            });
        }
    }
    for edge in &schema.edges {
        let name = traversal_name(&edge.name);
        if matches(&name) {
            // What is inserted: a reserved word is quoted, as a traversal spells it.
            items.push(Completion {
                label: edge.traversal(),
                detail: Some(format!("{} → {}", edge.from, edge.to)),
            });
        }
    }
    for keyword in KEYWORDS {
        if !prefix.is_empty() && matches(keyword) {
            items.push(Completion {
                label: (*keyword).into(),
                detail: None,
            });
        }
    }
    (start, items)
}

/// Whether `text` ends with one of `chars`, ignoring trailing spaces.
fn ends_with_after_space(text: &str, chars: &[char]) -> bool {
    text.trim_end()
        .chars()
        .last()
        .is_some_and(|c| chars.contains(&c))
}

/// The text between a declaration's signature parentheses, or to its end while unclosed.
/// `None` when the declaration has no signature before its body.
fn signature(source: &str, declaration: Range<usize>) -> Option<Range<usize>> {
    let text = &source[declaration.clone()];
    let mut open = None;
    let mut depth = 0usize;
    for token in tokenize(text) {
        match &text[token.range.clone()] {
            "{" if open.is_none() => return None,
            "(" => {
                open.get_or_insert(token.range.end);
                depth += 1;
            }
            ")" if open.is_some() => {
                depth -= 1;
                if depth == 0 {
                    let open = open?;
                    return Some(declaration.start + open..declaration.start + token.range.start);
                }
            }
            _ => {}
        }
    }
    open.map(|open| declaration.start + open..declaration.end)
}

/// `$name: Type` parameters of a signature, as (name, declared type).
fn parameters(source: &str, signature: Option<&Range<usize>>) -> Vec<(String, String)> {
    let Some(signature) = signature else {
        return Vec::new();
    };
    let text = &source[signature.clone()];
    let mut parts = Vec::new();
    let (mut depth, mut from) = (0usize, 0);
    for (i, c) in text.char_indices() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[from..]);
    parts
        .into_iter()
        .filter_map(|part| {
            let (name, type_name) = part.trim().strip_prefix('$')?.split_once(':')?;
            let (name, type_name) = (name.trim(), type_name.trim());
            (!name.is_empty() && !type_name.is_empty())
                .then(|| (name.to_owned(), type_name.to_owned()))
        })
        .collect()
}

/// `$var: Type` and `$var:edge` bindings in a declaration's body, as (name, node or edge type).
fn bindings(
    schema: &SchemaModel,
    source: &str,
    declaration: Range<usize>,
    signature: Option<&Range<usize>>,
) -> Vec<(String, String)> {
    let body = signature.map_or(declaration.start, |s| s.end)..declaration.end;
    let text = &source[body];
    let tokens = tokenize(text);
    let word = |i: usize| tokens.get(i).map(|t| &text[t.range.clone()]);
    let mut found = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Variable || word(i + 1) != Some(":") {
            continue;
        }
        let Some(target) = tokens.get(i + 2) else {
            continue;
        };
        let name = &text[token.range.start + 1..token.range.end];
        // A reserved word is quoted as a binding's edge (`$e:"in"`).
        let target = text[target.range.clone()].trim_matches('"');
        let type_name = if schema.node(target).is_some() || schema.edge(target).is_some() {
            target.to_owned()
        } else if let Some(edge) = schema
            .edges
            .iter()
            .find(|e| traversal_name(&e.name) == target)
        {
            edge.name.clone()
        } else {
            // `$e:worksAt` for an edge the schema does not know: edges are capitalised.
            let mut chars = target.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => continue,
            }
        };
        if !name.is_empty() && !found.iter().any(|(n, _)| n == name) {
            found.push((name.to_owned(), type_name));
        }
    }
    // `$a worksAt $b`, `$a $e:worksAt {1,2} $b`, `$a "in" $b`: a traversal binds its ends to the
    // edge's, where nothing else bound them.
    let variable = |i: usize| {
        tokens
            .get(i)
            .filter(|t| t.kind == TokenKind::Variable)
            .map(|t| &text[t.range.start + 1..t.range.end])
    };
    for i in 0..tokens.len() {
        let Some(from) = variable(i) else {
            continue;
        };
        let mut at = i + 1;
        if variable(at).is_some() && word(at + 1) == Some(":") {
            at += 2;
        }
        let Some(edge) = word(at).map(|w| w.trim_matches('"')).and_then(|name| {
            schema
                .edges
                .iter()
                .find(|e| traversal_name(&e.name) == name)
        }) else {
            continue;
        };
        at += 1;
        if word(at) == Some("{") {
            while word(at).is_some_and(|w| w != "}") {
                at += 1;
            }
            at += 1;
        }
        let Some(to) = variable(at) else {
            continue;
        };
        for (name, type_name) in [(from, &edge.from), (to, &edge.to)] {
            if !name.is_empty() && !found.iter().any(|(n, _)| n == name) {
                found.push((name.to_owned(), type_name.clone()));
            }
        }
    }
    found
}

/// What hovering at `offset` shows (spec 007): a parameter's or binding's type, a property's
/// declared type, a type's kind, or a relationship's ends; with the byte range it covers.
pub fn hover(schema: &SchemaModel, source: &str, offset: usize) -> Option<(Range<usize>, String)> {
    let tokens = tokenize(source);
    let index = tokens
        .iter()
        .position(|t| t.range.start <= offset && offset < t.range.end)?;
    let token = &tokens[index];
    let word = &source[token.range.clone()];
    let declaration = outline(source)
        .declaration_at(offset)
        .map(|d| d.range.clone())
        .unwrap_or(0..source.len());
    let signature = signature(source, declaration.clone());
    let text = match token.kind {
        TokenKind::Variable => {
            let name = word.trim_start_matches('$');
            if let Some((_, ty)) = parameters(source, signature.as_ref())
                .into_iter()
                .find(|(n, _)| n == name)
            {
                format!("${name}: {ty} · parameter")
            } else {
                let (_, ty) = bindings(schema, source, declaration, signature.as_ref())
                    .into_iter()
                    .find(|(n, _)| n == name)?;
                let kind = if schema.edge(&ty).is_some() {
                    "edge"
                } else {
                    "node"
                };
                format!("${name}: {ty} · {kind}")
            }
        }
        TokenKind::Property => {
            // `$var.property`: the token before the dot is the variable.
            let var = index
                .checked_sub(2)
                .and_then(|i| tokens.get(i))
                .filter(|t| t.kind == TokenKind::Variable)
                .map(|t| source[t.range.clone()].trim_start_matches('$'))?;
            let (_, ty) = bindings(schema, source, declaration, signature.as_ref())
                .into_iter()
                .find(|(n, _)| n == var)?;
            let properties = schema
                .node(&ty)
                .map(|n| &n.properties)
                .or_else(|| schema.edge(&ty).map(|e| &e.properties))?;
            match properties.iter().find(|p| p.name == word) {
                Some(property) => format!("{ty}.{word}: {}", property.display_type()),
                None if word == "@id" => format!("{ty}.@id: String · the row's id"),
                None => match (schema.edge(&ty), word) {
                    (Some(edge), "@src") => {
                        format!("{ty}.@src: String · the id of the {} it leaves", edge.from)
                    }
                    (Some(edge), "@dst") => {
                        format!("{ty}.@dst: String · the id of the {} it reaches", edge.to)
                    }
                    (Some(_), "@type") => format!("{ty}.@type: String · the edge's type"),
                    _ => return None,
                },
            }
        }
        TokenKind::Type => {
            if let Some(node) = schema.node(word) {
                let n = node.properties.len();
                format!(
                    "node {word} · {n} propert{}",
                    if n == 1 { "y" } else { "ies" }
                )
            } else {
                let edge = schema.edge(word)?;
                format!("edge {word}: {} → {}", edge.from, edge.to)
            }
        }
        TokenKind::Identifier => {
            let edge = schema
                .edges
                .iter()
                .find(|e| traversal_name(&e.name) == word)?;
            format!("{word}: {} → {} · edge {}", edge.from, edge.to, edge.name)
        }
        _ => return None,
    };
    Some((token.range.clone(), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::dev;

    #[test]
    fn completes_properties_of_a_bound_variable() {
        let source = "query q() { match { $p: Person } return { $p.na";
        let (start, items) = completions(&dev(), source, source.len());
        assert_eq!(&source[start..], "na");
        assert_eq!(items[0].label, "name");
    }

    #[test]
    fn completes_edge_properties() {
        let source = "query q() { match { $a $e:worksAt $b } return { $e.ro";
        let (_, items) = completions(&dev(), source, source.len());
        assert_eq!(items[0].label, "role");
        // A relationship names its ends and its type too; a node does not.
        let labels = |source: &str| -> Vec<String> {
            let (_, items) = completions(&dev(), source, source.len());
            items.into_iter().map(|item| item.label).collect()
        };
        let edge = labels("query q() { match { $a $e:worksAt $b } return { $e.");
        for meta in ["@id", "@src", "@dst", "@type"] {
            assert!(edge.iter().any(|label| label == meta), "{meta}: {edge:?}");
        }
        let node = labels("query q() { match { $p: Person } return { $p.");
        assert!(node.iter().any(|label| label == "@id"));
        assert!(!node.iter().any(|label| label == "@src"), "{node:?}");
        // An edge named by a reserved word is bound in quotes, and is still an edge.
        let schema = SchemaModel::parse(
            "node Team {\n  slug: String @key\n}\nnode Person {\n  slug: String @key\n}\n\
             edge In: Person -> Team {\n  since: Date?\n}\n",
        )
        .unwrap();
        let source = "query q() { match { $p: Person  $p $e:\"in\" $t } return { $e.";
        let (_, items) = completions(&schema, source, source.len());
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert!(
            labels.contains(&"since") && labels.contains(&"@src"),
            "{labels:?}"
        );
        // Begun with `@`, the field is completed from its name, the `@` replaced with it.
        let source = "query q() { match { $a $e:worksAt $b } return { $e.@s";
        let (start, items) = completions(&dev(), source, source.len());
        assert_eq!(&source[start..], "@s");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["@src"]);
    }

    #[test]
    fn completes_types_relationships_and_keywords() {
        let schema = dev();
        let (_, items) = completions(&schema, "query q() { match { $p: Pe", 26);
        assert!(items.iter().any(|i| i.label == "Person"));
        let (_, items) = completions(&schema, "$p wo", 5);
        assert!(items.iter().any(|i| i.label == "worksAt"));
        let (_, items) = completions(&schema, "ret", 3);
        assert!(items.iter().any(|i| i.label == "return"));
    }

    #[test]
    fn completes_parameters_and_bindings_after_a_dollar() {
        let schema = dev();
        let source = "query q($name: String, $ids: [String]?) {\n  match { $p: Person  $p worksAt $c  $e:worksAt }\n  return { $";
        let (start, items) = completions(&schema, source, source.len());
        assert_eq!(start, source.len());
        let found: Vec<_> = items
            .iter()
            .map(|i| (i.label.as_str(), i.detail.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            found,
            [
                ("name", "String"),
                ("ids", "[String]?"),
                ("p", "Person"),
                ("e", "WorksAt"),
                ("c", "Company"),
            ]
        );
        // A traversal's end has the edge's type, so its properties complete.
        let ends = "query q() { match { $p: Person  $p worksAt $c } return { $c.na";
        let (_, properties) = completions(&schema, ends, ends.len());
        assert_eq!(properties[0].label, "name");
        let typed = format!("{source}i");
        let (_, items) = completions(&schema, &typed, typed.len());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "ids");
    }

    #[test]
    fn a_dollar_in_a_signature_names_a_parameter() {
        let schema = dev();
        let source = "query q($";
        let (_, items) = completions(&schema, source, source.len());
        assert!(items.is_empty());
        let source = "query q($n: I";
        let (_, items) = completions(&schema, source, source.len());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["I32", "I64"]);
    }

    #[test]
    fn variables_belong_to_the_declaration_under_the_cursor() {
        let schema = dev();
        let source = "query a() { match { $x: Company } return { $x.name } }\nquery b($x: String) { match { $p: Person } return { $p.na";
        let (_, items) = completions(&schema, source, source.len());
        assert_eq!(items[0].label, "name");
        let source = "query a() { match { $x: Company } return { $x.name } }\nquery b() { match { $x: Person } return { $x.bo";
        let (_, items) = completions(&schema, source, source.len());
        assert_eq!(
            items[0].label, "born",
            "`$x` is a Person here, not the first query's Company"
        );
    }

    #[test]
    fn hovers_name_types() {
        let schema = dev();
        let source = "query q($name: String) {\n  match { $p: Person  $p worksAt $c  $p.name = $name }\n  return { $p.born }\n}";
        let at = |needle: &str, skip: usize| source.find(needle).unwrap() + skip;
        let text = |offset| hover(&schema, source, offset).map(|(_, t)| t);
        assert_eq!(
            text(at("= $name", 3)),
            Some("$name: String · parameter".into())
        );
        assert_eq!(text(at("$p:", 1)), Some("$p: Person · node".into()));
        assert_eq!(text(at("$p.born", 3)), Some("Person.born: Date?".into()));
        assert_eq!(
            text(at("worksAt", 2)),
            Some("worksAt: Person → Company · edge WorksAt".into())
        );
        assert_eq!(
            text(at(": Person", 2)),
            Some("node Person · 6 properties".into())
        );
        assert_eq!(text(at("match", 1)), None, "keywords have no hover");
    }
}
