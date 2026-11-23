//! What a type's cards show (spec 048, "Format"): the default rule, a kept format read against
//! the schema as it is now, and the changes the Fields menu makes to one. Pure: no GPUI, no
//! reads. The format itself is `studio_settings::CardFormat`, which the settings file keeps by
//! graph and type and a Cards tab saves with itself.

use gq_language::{NodeType, Property, ScalarKind};
use studio_settings::{CardCover, CardFormat};

/// How many fields the default card shows.
const DEFAULT_FIELDS: usize = 3;

/// What a card of one type shows under a format: resolved against the schema.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardFace {
    /// The fields in card order: properties the type has, at most
    /// [`CardFormat::MOST_FIELDS`].
    pub fields: Vec<Property>,
    /// The file property shown as the cover; `None` when the format says none or the type has
    /// no file.
    pub cover: Option<String>,
}

/// Whether a property can be a card's field: every property but a file, which a card shows as
/// its cover, and a vector, which no row holds.
fn can_be_field(property: &Property) -> bool {
    !matches!(property.kind, ScalarKind::Blob | ScalarKind::Vector)
}

/// The properties a card's field can be, in schema order.
pub fn field_choices(node: &NodeType) -> Vec<&Property> {
    node.properties
        .iter()
        .filter(|property| can_be_field(property))
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

/// The names of the fields a format shows on `node`, in card order: its own, or the default
/// rule's when it names none. A field the schema no longer has, or one named twice, is left
/// out, and no more than [`CardFormat::MOST_FIELDS`] are kept.
pub fn shown_fields(format: &CardFormat, node: &NodeType) -> Vec<String> {
    let Some(named) = &format.fields else {
        return default_fields(node);
    };
    let mut shown: Vec<String> = Vec::new();
    for name in named {
        if shown.len() == CardFormat::MOST_FIELDS {
            break;
        }
        if node.property(name).is_some_and(can_be_field) && !shown.contains(name) {
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

/// What a card of `node` shows under `format`.
pub fn face(format: &CardFormat, node: &NodeType) -> CardFace {
    CardFace {
        fields: shown_fields(format, node)
            .iter()
            .filter_map(|name| node.property(name).cloned())
            .collect(),
        cover: shown_cover(format, node),
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

/// `format` with the field `name` shown, last. `None` when it is shown already, cannot be a
/// field, or the card is full.
pub fn with_field_shown(format: &CardFormat, node: &NodeType, name: &str) -> Option<CardFormat> {
    let mut fields = shown_fields(format, node);
    if fields.len() >= CardFormat::MOST_FIELDS
        || fields.iter().any(|field| field == name)
        || !node.property(name).is_some_and(can_be_field)
    {
        return None;
    }
    fields.push(name.to_owned());
    Some(with_fields(format, node, fields))
}

/// `format` without the field `name`. `None` when it is not shown.
pub fn with_field_hidden(format: &CardFormat, node: &NodeType, name: &str) -> Option<CardFormat> {
    let mut fields = shown_fields(format, node);
    let at = fields.iter().position(|field| field == name)?;
    fields.remove(at);
    Some(with_fields(format, node, fields))
}

/// `format` with the field `name` moved `by` places in card order (−1 up, 1 down). `None` when
/// it is not shown or is at that end already.
pub fn with_field_moved(
    format: &CardFormat,
    node: &NodeType,
    name: &str,
    by: isize,
) -> Option<CardFormat> {
    let mut fields = shown_fields(format, node);
    let at = fields.iter().position(|field| field == name)?;
    let to = at.checked_add_signed(by).filter(|&to| to < fields.len())?;
    fields.swap(at, to);
    Some(with_fields(format, node, fields))
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

/// Whether `format` shows another card than the default one on `node`: other fields, another
/// order, or another cover. Its size is not the card's: the size control says it.
pub fn is_changed(format: &CardFormat, node: &NodeType) -> bool {
    let default = CardFormat::default();
    shown_fields(format, node) != default_fields(node)
        || shown_cover(format, node) != shown_cover(&default, node)
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
}
