//! What a type's cards show (spec 048, "Format"): the default rule, a kept format read against
//! the schema as it is now, and the changes the Fields menu makes to one. A field is one of the
//! type's properties or one of its relationships, which shows a node's neighbours. Pure: no
//! GPUI, no reads. The format itself is `studio_settings::CardFormat`, which the settings file
//! keeps by graph and type and a Cards tab saves with itself.

use gq_language::{NodeType, Property, RelationshipGroup, ScalarKind, builder};
use studio_settings::{CardCover, CardFormat};

/// How many fields the default card shows.
const DEFAULT_FIELDS: usize = 3;

/// A node type as a card sees it: its properties, and the relationships its nodes have, as a
/// table of the type has a column for each ([`crate::model::node_columns`]). A type alone
/// (`&NodeType`) is one with no relationship.
#[derive(Debug, Clone, Copy)]
pub struct CardType<'a> {
    pub node: &'a NodeType,
    pub relationships: &'a [RelationshipGroup],
}

impl<'a> From<&'a NodeType> for CardType<'a> {
    fn from(node: &'a NodeType) -> Self {
        Self {
            node,
            relationships: &[],
        }
    }
}

impl CardType<'_> {
    /// The relationship a format names by `key` (`out:FiledUnder`), when the type has it.
    fn relationship(&self, key: &str) -> Option<&RelationshipGroup> {
        self.relationships
            .iter()
            .find(|group| builder::relationship_key(group) == key)
    }

    /// Whether `name` can be a card's field: a property that can, or a relationship.
    fn has_field(&self, name: &str) -> bool {
        self.node.property(name).is_some_and(can_be_field) || self.relationship(name).is_some()
    }
}

/// What a card's field is of.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldOf {
    /// A property: the row's value.
    Property(Property),
    /// A relationship: the row's neighbours through it, read apart from the row.
    Relationship(RelationshipGroup),
}

/// One field of a card.
#[derive(Debug, Clone, PartialEq)]
pub struct CardField {
    /// How a format names it: the property's name, or the relationship's key
    /// (`out:FiledUnder`), which no property's name can be.
    pub name: String,
    /// What a card says before its value: the property's name, or the relationship as a table
    /// titles its column (`filedUnder →`).
    pub title: String,
    pub of: FieldOf,
}

impl CardField {
    pub fn relationship(&self) -> Option<&RelationshipGroup> {
        match &self.of {
            FieldOf::Relationship(group) => Some(group),
            FieldOf::Property(_) => None,
        }
    }
}

/// How a card titles the relationship `key`: as the table's column of it is titled.
pub fn relationship_title(key: &str) -> String {
    builder::relationship_label(key).unwrap_or_else(|| key.to_owned())
}

/// What a card of one type shows under a format: resolved against the schema.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardFace {
    /// The fields in card order: properties and relationships the type has, at most
    /// [`CardFormat::MOST_FIELDS`].
    pub fields: Vec<CardField>,
    /// The file property shown as the cover; `None` when the format says none or the type has
    /// no file.
    pub cover: Option<String>,
}

/// Whether a property can be a card's field: every property but a file, which a card shows as
/// its cover, and a vector, which no row holds.
fn can_be_field(property: &Property) -> bool {
    !matches!(property.kind, ScalarKind::Blob | ScalarKind::Vector)
}

/// What a card's field can be, by the name a format gives it: the properties, in schema order,
/// then the relationships, in the order a table has their columns. No relationship is a field
/// until it is chosen: each costs reads.
pub fn field_choices<'a>(of: impl Into<CardType<'a>>) -> Vec<String> {
    let of = of.into();
    of.node
        .properties
        .iter()
        .filter(|property| can_be_field(property))
        .map(|property| property.name.clone())
        .chain(of.relationships.iter().map(builder::relationship_key))
        .collect()
}

/// The type's file properties, in schema order: what its cover can be.
pub fn files(node: &NodeType) -> Vec<String> {
    node.properties
        .iter()
        .filter(|property| property.kind == ScalarKind::Blob)
        .map(|property| property.name.clone())
        .collect()
}

/// The default rule's fields: the first three properties in schema order, leaving out the
/// property that names the node (a card's title), the key, files and vectors.
pub fn default_fields(node: &NodeType) -> Vec<String> {
    let name = node.label_property();
    node.properties
        .iter()
        .filter(|property| can_be_field(property))
        .filter(|property| Some(property.name.as_str()) != name)
        .filter(|property| !property.is_key)
        .take(DEFAULT_FIELDS)
        .map(|property| property.name.clone())
        .collect()
}

/// The names of the fields a format shows on a type, in card order: its own, or the default
/// rule's when it names none. A field the schema no longer has (a property, or a
/// relationship), or one named twice, is left out, and no more than
/// [`CardFormat::MOST_FIELDS`] are kept.
pub fn shown_fields<'a>(format: &CardFormat, of: impl Into<CardType<'a>>) -> Vec<String> {
    let of = of.into();
    let Some(named) = &format.fields else {
        return default_fields(of.node);
    };
    let mut shown: Vec<String> = Vec::new();
    for name in named {
        if shown.len() == CardFormat::MOST_FIELDS {
            break;
        }
        if of.has_field(name) && !shown.contains(name) {
            shown.push(name.clone());
        }
    }
    shown
}

/// The file a format shows as the cover on `node`: the type's first file, the one it names
/// while the type still has it as a file, or none.
pub fn shown_cover(format: &CardFormat, node: &NodeType) -> Option<String> {
    let files = files(node);
    match &format.cover {
        CardCover::First => files.into_iter().next(),
        CardCover::None => None,
        // A cover the schema no longer has is left out; the format is kept as it was written.
        CardCover::Property(name) => files.into_iter().find(|file| file == name),
    }
}

/// What a card of a type shows under `format`.
pub fn face<'a>(format: &CardFormat, of: impl Into<CardType<'a>>) -> CardFace {
    let of = of.into();
    CardFace {
        fields: shown_fields(format, of)
            .into_iter()
            .filter_map(|name| {
                let (title, field) = match of.node.property(&name) {
                    Some(property) => (name.clone(), FieldOf::Property(property.clone())),
                    None => (
                        relationship_title(&name),
                        FieldOf::Relationship(of.relationship(&name)?.clone()),
                    ),
                };
                Some(CardField {
                    name,
                    title,
                    of: field,
                })
            })
            .collect(),
        cover: shown_cover(format, of.node),
    }
}

/// `format` showing `fields` on `node`: kept as the list, or as no list when it is the one the
/// default rule gives, so a card put back as it was is the default card again.
fn with_fields(format: &CardFormat, node: &NodeType, fields: Vec<String>) -> CardFormat {
    CardFormat {
        fields: (fields != default_fields(node)).then_some(fields),
        ..format.clone()
    }
}

/// `format` with the field `name` shown, last: a property, or a relationship by its key.
/// `None` when it is shown already, cannot be a field, or the card is full.
pub fn with_field_shown<'a>(
    format: &CardFormat,
    of: impl Into<CardType<'a>>,
    name: &str,
) -> Option<CardFormat> {
    let of = of.into();
    let mut fields = shown_fields(format, of);
    if fields.len() >= CardFormat::MOST_FIELDS
        || fields.iter().any(|field| field == name)
        || !of.has_field(name)
    {
        return None;
    }
    fields.push(name.to_owned());
    Some(with_fields(format, of.node, fields))
}

/// `format` without the field `name`. `None` when it is not shown.
pub fn with_field_hidden<'a>(
    format: &CardFormat,
    of: impl Into<CardType<'a>>,
    name: &str,
) -> Option<CardFormat> {
    let of = of.into();
    let mut fields = shown_fields(format, of);
    let at = fields.iter().position(|field| field == name)?;
    fields.remove(at);
    Some(with_fields(format, of.node, fields))
}

/// `format` with the field `name` moved `by` places in card order (−1 up, 1 down). `None` when
/// it is not shown or is at that end already.
pub fn with_field_moved<'a>(
    format: &CardFormat,
    of: impl Into<CardType<'a>>,
    name: &str,
    by: isize,
) -> Option<CardFormat> {
    let of = of.into();
    let mut fields = shown_fields(format, of);
    let at = fields.iter().position(|field| field == name)?;
    let to = at.checked_add_signed(by).filter(|&to| to < fields.len())?;
    fields.swap(at, to);
    Some(with_fields(format, of.node, fields))
}

/// `format` with `cover` as its cover: one of the type's files, or none. The type's first file
/// is kept as [`CardCover::First`], which is the default. `None` when the type has no such file.
pub fn with_cover(format: &CardFormat, node: &NodeType, cover: Option<&str>) -> Option<CardFormat> {
    let files = files(node);
    let cover = match cover {
        None => CardCover::None,
        Some(name) if files.first().is_some_and(|first| first == name) => CardCover::First,
        Some(name) if files.iter().any(|file| file == name) => CardCover::Property(name.to_owned()),
        Some(_) => return None,
    };
    Some(CardFormat {
        cover,
        ..format.clone()
    })
}

/// `format` with the next cover: the type's files in schema order, then none, then the first
/// file again. `None` when the type has no file.
pub fn with_next_cover(format: &CardFormat, node: &NodeType) -> Option<CardFormat> {
    let files = files(node);
    let next = match shown_cover(format, node) {
        Some(shown) => files
            .iter()
            .position(|file| *file == shown)
            .and_then(|at| files.get(at + 1)),
        None => Some(files.first()?),
    };
    with_cover(format, node, next.map(String::as_str))
}

/// Whether `format` shows another card than the default one on a type: other fields, another
/// order, or another cover. Its size is not the card's: the size control says it.
pub fn is_changed<'a>(format: &CardFormat, of: impl Into<CardType<'a>>) -> bool {
    let of = of.into();
    let default = CardFormat::default();
    shown_fields(format, of) != default_fields(of.node)
        || shown_cover(format, of.node) != shown_cover(&default, of.node)
}

/// `format` showing the default card, at the size it has.
pub fn reset(format: &CardFormat) -> CardFormat {
    CardFormat {
        size: format.size,
        ..CardFormat::default()
    }
}

#[cfg(test)]
mod tests {
    use gq_language::SchemaModel;
    use studio_settings::CardSize;

    use super::*;

    fn node(schema: &str, name: &str) -> NodeType {
        SchemaModel::parse(schema)
            .expect("the schema parses")
            .node(name)
            .expect("the type")
            .clone()
    }

    const NOTE: &str = "node Note {\n  slug: String @key\n  title: String\n  body: String?\n  \
        written: Date?\n  attachment: Blob?\n  rating: I32?\n  scan: Blob?\n  tags: [String]?\n}\n";

    fn names(face: &CardFace) -> Vec<&str> {
        face.fields
            .iter()
            .map(|property| property.name.as_str())
            .collect()
    }

    /// The default card: the first three properties in schema order that are not the name, the
    /// key, a file or a vector; the type's first file as the cover, or none when it has none.
    #[test]
    fn the_default_card_is_three_properties_and_the_first_file() {
        let note = node(NOTE, "Note");
        let default = face(&CardFormat::default(), &note);
        assert_eq!(names(&default), ["body", "written", "rating"]);
        assert_eq!(default.cover.as_deref(), Some("attachment"));

        // No file: no cover. Fewer than three properties left: those there are.
        let topic = node(
            "node Topic {\n  slug: String @key\n  name: String\n  rank: I32?\n}\n",
            "Topic",
        );
        let default = face(&CardFormat::default(), &topic);
        assert_eq!(names(&default), ["rank"]);
        assert_eq!(default.cover, None);

        // The key and the name lead the schema: the three after them. A vector is passed over.
        let person = node(
            "node Person {\n  id: String @key\n  name: String\n  embedding: Vector(3)?\n  \
             role: enum(Founder, Engineer)?\n  joined: Date?\n  bio: String?\n  email: String?\n}\n",
            "Person",
        );
        assert_eq!(
            names(&face(&CardFormat::default(), &person)),
            ["role", "joined", "bio"]
        );
        // A type named by another property than `name` leaves that one out instead.
        let mut by_bio = person.clone();
        by_bio.label = Some("bio".into());
        assert_eq!(
            names(&face(&CardFormat::default(), &by_bio)),
            ["name", "role", "joined"]
        );
        // Only a key: a card is its title.
        let bare = node("node Tag {\n  slug: String @key\n}\n", "Tag");
        assert_eq!(face(&CardFormat::default(), &bare), CardFace::default());
    }

    /// A kept format is read against the schema as it is: a field or a cover the type no
    /// longer has is left out, as are a file or a vector named as a field, a field named twice
    /// and the fields past eight. The format itself is not changed by being read.
    #[test]
    fn a_format_shows_what_the_schema_still_has() {
        let note = node(NOTE, "Note");
        let kept = CardFormat {
            fields: Some(vec![
                "written".into(),
                "gone".into(),
                "attachment".into(),
                "written".into(),
                "slug".into(),
            ]),
            cover: CardCover::Property("thumbnail".into()),
            size: CardSize::Large,
        };
        let before = kept.clone();
        let shown = face(&kept, &note);
        assert_eq!(names(&shown), ["written", "slug"], "the key may be a field");
        assert_eq!(
            shown.cover, None,
            "the cover it names is gone: none is shown"
        );
        assert_eq!(kept, before);
        // A file named as the cover that is one of the type's.
        let scan = CardFormat {
            cover: CardCover::Property("scan".into()),
            ..CardFormat::default()
        };
        assert_eq!(face(&scan, &note).cover.as_deref(), Some("scan"));
        // No field at all is a format too.
        let none = CardFormat {
            fields: Some(Vec::new()),
            cover: CardCover::None,
            ..CardFormat::default()
        };
        assert_eq!(face(&none, &note), CardFace::default());
        // At most eight.
        let wide: String = (0..12).map(|i| format!("  p{i}: String?\n")).collect();
        let wide = node(
            &format!("node Wide {{\n  id: String @key\n{wide}}}\n"),
            "Wide",
        );
        let all = CardFormat {
            fields: Some((0..12).map(|i| format!("p{i}")).collect()),
            ..CardFormat::default()
        };
        assert_eq!(face(&all, &wide).fields.len(), CardFormat::MOST_FIELDS);
    }

    /// The Fields menu's changes: a field shown goes last, one hidden leaves, one moved swaps
    /// with its neighbour and stops at the ends; the ninth is refused; a card put back as the
    /// default rule has it is the default card again, and reset keeps the size.
    #[test]
    fn fields_are_shown_hidden_and_moved_within_eight() {
        let note = node(NOTE, "Note");
        let default = CardFormat::default();
        assert!(!is_changed(&default, &note));
        let shown = with_field_shown(&default, &note, "tags").unwrap();
        assert_eq!(
            shown.fields.as_deref(),
            Some(&["body", "written", "rating", "tags"].map(String::from)[..])
        );
        assert!(is_changed(&shown, &note));
        assert_eq!(
            with_field_shown(&shown, &note, "tags"),
            None,
            "shown already"
        );
        assert_eq!(with_field_shown(&shown, &note, "scan"), None, "a file");
        assert_eq!(with_field_shown(&shown, &note, "gone"), None);

        let moved = with_field_moved(&shown, &note, "tags", -1).unwrap();
        assert_eq!(
            shown_fields(&moved, &note),
            ["body", "written", "tags", "rating"]
        );
        assert_eq!(with_field_moved(&moved, &note, "rating", 1), None, "last");
        assert_eq!(with_field_moved(&moved, &note, "body", -1), None, "first");
        assert_eq!(
            with_field_moved(&moved, &note, "slug", 1),
            None,
            "not shown"
        );
        let back = with_field_moved(&moved, &note, "tags", 1).unwrap();
        assert_eq!(back, shown);

        let hidden = with_field_hidden(&back, &note, "tags").unwrap();
        assert_eq!(
            hidden, default,
            "as the default rule has it: no list is kept"
        );
        assert_eq!(with_field_hidden(&hidden, &note, "tags"), None);
        let fewer = with_field_hidden(&hidden, &note, "written").unwrap();
        assert_eq!(shown_fields(&fewer, &note), ["body", "rating"]);

        // Eight shown: the ninth is refused.
        let wide: String = (0..12).map(|i| format!("  p{i}: String?\n")).collect();
        let wide = node(
            &format!("node Wide {{\n  id: String @key\n{wide}}}\n"),
            "Wide",
        );
        let mut format = CardFormat::default();
        for i in 3..8 {
            format = with_field_shown(&format, &wide, &format!("p{i}")).unwrap();
        }
        assert_eq!(shown_fields(&format, &wide).len(), 8);
        assert_eq!(
            with_field_shown(&format, &wide, "p8"),
            None,
            "a card is full"
        );
        let room = with_field_hidden(&format, &wide, "p0").unwrap();
        assert!(with_field_shown(&room, &wide, "p8").is_some());

        let large = CardFormat {
            size: CardSize::Large,
            ..format
        };
        assert_eq!(
            reset(&large),
            CardFormat {
                size: CardSize::Large,
                ..CardFormat::default()
            }
        );
        assert!(
            !is_changed(&reset(&large), &wide),
            "a size is not the card's"
        );
    }

    /// The cover goes round the type's files and then none; the first file is the default and
    /// is kept as such. A type with no file has no cover to change.
    #[test]
    fn the_cover_goes_round_the_files_and_none() {
        let note = node(NOTE, "Note");
        let first = CardFormat::default();
        let second = with_next_cover(&first, &note).unwrap();
        assert_eq!(second.cover, CardCover::Property("scan".into()));
        let none = with_next_cover(&second, &note).unwrap();
        assert_eq!(none.cover, CardCover::None);
        assert_eq!(shown_cover(&none, &note), None);
        assert!(is_changed(&none, &note));
        let again = with_next_cover(&none, &note).unwrap();
        assert_eq!(again, first, "the first file is the default cover");
        assert_eq!(
            with_cover(&none, &note, Some("attachment")).unwrap().cover,
            CardCover::First
        );
        assert_eq!(with_cover(&first, &note, Some("body")), None, "not a file");
        // A cover the type lost: F starts again from the first file.
        let lost = CardFormat {
            cover: CardCover::Property("thumbnail".into()),
            ..CardFormat::default()
        };
        assert_eq!(
            with_next_cover(&lost, &note).unwrap().cover,
            CardCover::First
        );

        let topic = node(
            "node Topic {\n  slug: String @key\n  name: String\n}\n",
            "Topic",
        );
        assert_eq!(with_next_cover(&first, &topic), None);
        assert_eq!(
            with_cover(&first, &topic, None).unwrap().cover,
            CardCover::None
        );
        assert!(
            !is_changed(&with_cover(&first, &topic, None).unwrap(), &topic),
            "no file either way"
        );
    }

    /// A relationship is a field a format names by its key. The choices list the type's
    /// relationships after its properties; none is a field by default; a card titles one as
    /// the table titles its column; they count toward the eight; and the rules that show,
    /// hide, move and reset fields cover them. One the schema lost is left out, and the format
    /// is kept as it was written.
    #[test]
    fn a_relationship_is_a_field_by_its_key() {
        const FILED: &str = "node Note {\n  slug: String @key\n  title: String\n  body: String?\n  \
            written: Date?\n  attachment: Blob?\n}\nnode Topic {\n  slug: String @key\n  \
            name: String\n}\nnode Person {\n  slug: String @key\n  name: String\n}\n\
            edge FiledUnder: Note -> Topic @card(0..)\nedge Wrote: Person -> Note @card(0..)\n";
        let schema = SchemaModel::parse(FILED).expect("the schema parses");
        let note = CardType {
            node: schema.node("Note").unwrap(),
            relationships: schema.relationships_of("Note"),
        };
        let keys: Vec<String> = note
            .relationships
            .iter()
            .map(builder::relationship_key)
            .collect();
        assert_eq!(keys, ["out:FiledUnder", "in:Wrote"]);
        assert_eq!(
            field_choices(note),
            [
                "slug",
                "title",
                "body",
                "written",
                "out:FiledUnder",
                "in:Wrote"
            ],
            "the properties, then the relationships"
        );
        // As the table titles their columns.
        let columns = crate::model::node_columns(note.node, note.relationships);
        for key in &keys {
            let column = columns.iter().find(|column| column.key == *key).unwrap();
            assert_eq!(relationship_title(key), column.title);
        }
        assert_eq!(
            (
                relationship_title("out:FiledUnder").as_str(),
                relationship_title("in:Wrote").as_str()
            ),
            ("filedUnder →", "← wrote")
        );

        // No relationship is a field until it is chosen.
        let default = CardFormat::default();
        assert_eq!(names(&face(&default, note)), ["body", "written"]);
        assert!(!is_changed(&default, note));

        // Shown last, by its key; moved, hidden and reset as any field.
        let shown = with_field_shown(&default, note, "out:FiledUnder").expect("it can be shown");
        assert_eq!(
            shown.fields.as_deref(),
            Some(
                &[
                    "body".to_owned(),
                    "written".to_owned(),
                    "out:FiledUnder".to_owned()
                ][..]
            )
        );
        assert!(is_changed(&shown, note));
        assert!(
            with_field_shown(&shown, note, "out:FiledUnder").is_none(),
            "shown already"
        );
        assert!(
            with_field_shown(&shown, note, "out:Wrote").is_none(),
            "no such relationship"
        );
        let card = face(&shown, note);
        assert_eq!(names(&card), ["body", "written", "out:FiledUnder"]);
        let field = &card.fields[2];
        assert_eq!(field.title, "filedUnder →");
        assert_eq!(field.relationship(), Some(&note.relationships[0]));
        assert_eq!(card.fields[0].relationship(), None);
        assert_eq!(card.fields[0].title, "body");
        let moved = with_field_moved(&shown, note, "out:FiledUnder", -1).unwrap();
        assert_eq!(
            names(&face(&moved, note)),
            ["body", "out:FiledUnder", "written"]
        );
        let hidden = with_field_hidden(&moved, note, "out:FiledUnder").unwrap();
        assert_eq!(hidden.fields, None, "the default card again");
        assert_eq!(reset(&moved).fields, None);

        // They count toward the eight.
        let full = CardFormat {
            fields: Some(
                [
                    "slug",
                    "title",
                    "body",
                    "written",
                    "out:FiledUnder",
                    "in:Wrote",
                ]
                .map(String::from)
                .to_vec(),
            ),
            ..CardFormat::default()
        };
        assert_eq!(face(&full, note).fields.len(), 6);
        let wide = node(
            "node Wide {\n  id: String @key\n  a: String?\n  b: String?\n  c: String?\n  \
             d: String?\n  e: String?\n  f: String?\n  g: String?\n}\n",
            "Wide",
        );
        let eight = CardFormat {
            fields: Some(
                ["id", "a", "b", "c", "d", "e", "f", "g"]
                    .map(String::from)
                    .to_vec(),
            ),
            ..CardFormat::default()
        };
        let wide = CardType {
            node: &wide,
            relationships: note.relationships,
        };
        assert!(
            with_field_shown(&eight, wide, "out:FiledUnder").is_none(),
            "the card is full"
        );

        // A relationship the schema lost is left out, and one named twice shown once; the
        // format is not changed by being read. A type seen alone has no relationship.
        let kept = CardFormat {
            fields: Some(
                ["out:Gone", "title", "out:FiledUnder", "out:FiledUnder"]
                    .map(String::from)
                    .to_vec(),
            ),
            ..CardFormat::default()
        };
        assert_eq!(names(&face(&kept, note)), ["title", "out:FiledUnder"]);
        assert_eq!(names(&face(&kept, note.node)), ["title"]);
        assert_eq!(kept.fields.as_ref().map(Vec::len), Some(4));
    }
}
