//! A table grouped by one property (spec 004, group-by): one section per value,
//! with its count from one grouped read under the table's filters, and each open section reading
//! its own keyset pages narrowed to its value. A closed section reads nothing.
//!
//! A node table can also be grouped by a relationship (`out:WorksAt`): one section per neighbour,
//! named by its label, and one for the rows with none; a section's pages are narrowed with
//! "related to" that neighbour, or "has none". By a neighbour's property (`out:WorksAt.city`):
//! one section per value some neighbour holds, and one for the rows no neighbour gives a value;
//! a row with neighbours of two values is in both. An edge table can be grouped by a property of
//! its source or target (`from.title`, `to.city`): every edge has one of each, so its sections
//! divide the rows as a property's do.
//!
//! The table keeps every loaded row in one list, sections in order, so the cursor, editing and
//! selection keep addressing rows by their position in it; a section knows its slice of the
//! list. The grid draws [`Line`]s: a header per section, its rows while open, then a line that
//! reads more of it.

use std::ops::Range;

use gq_language::builder::{self, GroupKey};
use gq_language::{Filter, FilterOp};
use graph_client::api::ReadOutput;
use graph_store::Loadable;
use serde_json::{Map, Value};

use crate::model::{Paging, same_value};

/// How many values one grouped read returns.
pub const GROUP_LIMIT: u32 = 200;
/// Rows an open section reads first; "Show more" reads a full page.
pub const SECTION_FIRST_PAGE: u32 = 10;

/// One value of the grouping property and the rows that hold it.
#[derive(Debug, Clone)]
pub struct Section {
    /// The property's value, or the neighbour's `@id`; null for rows without one.
    pub value: Value,
    /// The neighbour's label, when grouped by a relationship whose other type has one.
    pub label: Option<String>,
    /// Rows with this value that match the filters, at the table's commit.
    pub count: u64,
    pub expanded: bool,
    pub paging: Paging,
    pub loading: bool,
    pub error: Option<String>,
    /// The section's rows: this slice of the table's row list.
    pub range: Range<usize>,
}

impl Section {
    fn new(value: Value, count: u64) -> Self {
        Self {
            value,
            label: None,
            count,
            expanded: true,
            paging: Paging::default(),
            loading: false,
            error: None,
            range: 0..0,
        }
    }

    /// Whether rows of this section are still to be read.
    pub fn has_more(&self) -> bool {
        !self.paging.exhausted && (self.range.len() as u64) < self.count
    }
}

/// A grouped grid's lines, as [`Grouping::lines`] lists them, held per section rather than per
/// row: the grid lays them out each frame, and an open section can hold thousands of rows.
#[derive(Debug, Clone, Default)]
pub struct Lines {
    sections: Vec<SectionLines>,
    len: usize,
    other_values: bool,
}

/// One section's lines: its header at `start`, then its rows while open, then its read-more
/// line.
#[derive(Debug, Clone)]
struct SectionLines {
    start: usize,
    index: usize,
    rows: std::ops::Range<usize>,
    more: bool,
}

impl Lines {
    pub fn len(&self) -> usize {
        self.len
    }

    /// The line at `ix`, found by its section.
    pub fn get(&self, ix: usize) -> Option<Line> {
        if ix >= self.len {
            return None;
        }
        let at = self.sections.partition_point(|section| section.start <= ix);
        if let Some(section) = at.checked_sub(1).map(|at| &self.sections[at]) {
            let offset = ix - section.start;
            if offset == 0 {
                return Some(Line::Header(section.index));
            }
            if offset <= section.rows.len() {
                return Some(Line::Row(section.rows.start + offset - 1));
            }
            if section.more && offset == section.rows.len() + 1 {
                return Some(Line::More(section.index));
            }
        }
        (self.other_values && ix + 1 == self.len).then_some(Line::OtherValues)
    }

    /// The lines in `range`, those past the end left out.
    pub fn slice(&self, range: std::ops::Range<usize>) -> impl Iterator<Item = Line> + '_ {
        range.filter_map(|ix| self.get(ix))
    }

    /// The rows the lines show, in line order.
    fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.sections
            .iter()
            .flat_map(|section| section.rows.clone())
    }
}

/// One line of a grouped grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Header(usize),
    Row(usize),
    /// Reads more of a section, or says it is being read or could not be.
    More(usize),
    /// The table has values beyond the ones read.
    OtherValues,
}

#[derive(Debug, Clone)]
pub struct Grouping {
    pub property: String,
    pub sections: Loadable<Vec<Section>>,
    /// Every value was read; otherwise only the first [`GROUP_LIMIT`].
    pub complete: bool,
    /// Bumped by every read of the values, so an answer to an older one is left out.
    pub generation: u64,
}

impl Grouping {
    pub fn new(property: impl Into<String>) -> Self {
        Self {
            property: property.into(),
            sections: Loadable::NotLoaded,
            complete: true,
            generation: 0,
        }
    }

    pub fn sections(&self) -> &[Section] {
        match &self.sections {
            Loadable::Loaded(sections) => sections,
            _ => &[],
        }
    }

    pub fn section_mut(&mut self, index: usize) -> Option<&mut Section> {
        match &mut self.sections {
            Loadable::Loaded(sections) => sections.get_mut(index),
            _ => None,
        }
    }

    /// Starts a new read of the values: sections and their rows are forgotten.
    pub fn restart(&mut self) -> u64 {
        self.generation += 1;
        self.sections = Loadable::Loading;
        self.complete = true;
        self.generation
    }

    /// Takes the grouped counts: a section per value in value order, rows without a value
    /// last. Every section starts open with nothing read.
    pub fn accept_counts(&mut self, output: &ReadOutput, limit: u32) {
        let rows = match &output.rows {
            Value::Array(rows) => rows.as_slice(),
            _ => &[],
        };
        let mut sections: Vec<Section> = rows
            .iter()
            .filter_map(Value::as_object)
            .map(|row| {
                let value = row.get("value").cloned().unwrap_or(Value::Null);
                let count = row.get("n").and_then(Value::as_u64).unwrap_or(0);
                Section {
                    label: row.get("label").and_then(Value::as_str).map(str::to_owned),
                    ..Section::new(value, count)
                }
            })
            .collect();
        sections.sort_by_key(|section| section.value.is_null());
        self.complete = (rows.len() as u32) < limit;
        self.sections = Loadable::Loaded(sections);
    }

    /// Whether the rows are grouped by a relationship rather than a property.
    pub fn by_relationship(&self) -> bool {
        builder::parse_relationship_key(&self.property).is_some()
    }

    /// Whether the rows are grouped by a property of their own, which their values hold.
    pub fn by_own_property(&self) -> bool {
        builder::parse_group_key(&self.property).is_own_property()
    }

    /// The value a new row is grouped under, when it is known: its own property's, and none
    /// for a new node grouped by its neighbours, which it has none of yet. An edge's ends are
    /// not among its values.
    pub fn value_of_new_row(&self, values: &Map<String, Value>) -> Option<Value> {
        match builder::parse_group_key(&self.property) {
            GroupKey::Property(property) => {
                Some(values.get(property).cloned().unwrap_or(Value::Null))
            }
            GroupKey::Relationship(..) | GroupKey::NeighbourProperty { .. } => Some(Value::Null),
            GroupKey::EndpointProperty { .. } => None,
        }
    }

    /// Adds the section of rows with no neighbour, or none with a value, last, when there are
    /// any.
    pub fn add_none_section(&mut self, count: u64) {
        if let Loadable::Loaded(sections) = &mut self.sections
            && count > 0
        {
            sections.push(Section::new(Value::Null, count));
        }
    }

    /// The filter that narrows the table's pages to one section.
    pub fn filter_for(&self, index: usize) -> Option<Filter> {
        let section = self.sections().get(index)?;
        if self.by_relationship() {
            return Some(Filter {
                property: self.property.clone(),
                op: if section.value.is_null() {
                    FilterOp::HasNone
                } else {
                    FilterOp::RelatedTo
                },
                value: section.value.clone(),
            });
        }
        Some(if section.value.is_null() {
            Filter {
                property: self.property.clone(),
                op: FilterOp::IsEmpty,
                value: Value::Null,
            }
        } else {
            Filter {
                property: self.property.clone(),
                op: FilterOp::Eq,
                value: section.value.clone(),
            }
        })
    }

    /// The section holding `value`.
    pub fn section_with(&self, value: &Value) -> Option<usize> {
        self.sections()
            .iter()
            .position(|section| same_value(&section.value, value))
    }

    /// The section a row of the table's list belongs to.
    pub fn section_of(&self, row: usize) -> Option<usize> {
        self.sections()
            .iter()
            .position(|section| section.range.contains(&row))
    }

    /// `count` rows were put into section `index`'s slice: it grows and the later ones move.
    pub fn inserted(&mut self, index: usize, count: usize) {
        if let Loadable::Loaded(sections) = &mut self.sections {
            for (ix, section) in sections.iter_mut().enumerate() {
                if ix == index {
                    section.range.end += count;
                } else if ix > index {
                    section.range.start += count;
                    section.range.end += count;
                }
            }
        }
    }

    /// The row at `row` left the list: its section shrinks and the later ones move back.
    pub fn removed(&mut self, row: usize) {
        if let Loadable::Loaded(sections) = &mut self.sections {
            for section in sections.iter_mut() {
                if section.range.contains(&row) {
                    section.range.end -= 1;
                    section.count = section.count.saturating_sub(1);
                } else if section.range.start > row {
                    section.range.start -= 1;
                    section.range.end -= 1;
                }
            }
        }
    }

    /// The grid's lines, by section: each header, then its rows and a read-more line while it
    /// is open.
    pub fn line_map(&self) -> Lines {
        let mut sections = Vec::new();
        let mut len = 0;
        for (index, section) in self.sections().iter().enumerate() {
            let rows = if section.expanded {
                section.range.clone()
            } else {
                0..0
            };
            let more = section.expanded
                && (section.loading || section.error.is_some() || section.has_more());
            sections.push(SectionLines {
                start: len,
                index,
                rows: rows.clone(),
                more,
            });
            len += 1 + rows.len() + usize::from(more);
        }
        let other_values = !self.complete && matches!(self.sections, Loadable::Loaded(_));
        Lines {
            sections,
            len: len + usize::from(other_values),
            other_values,
        }
    }

    /// The grid's lines, one by one: each header, then its rows and a read-more line while it
    /// is open.
    #[cfg(test)]
    pub fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        for (index, section) in self.sections().iter().enumerate() {
            lines.push(Line::Header(index));
            if !section.expanded {
                continue;
            }
            lines.extend(section.range.clone().map(Line::Row));
            if section.loading || section.error.is_some() || section.has_more() {
                lines.push(Line::More(index));
            }
        }
        if !self.complete && matches!(self.sections, Loadable::Loaded(_)) {
            lines.push(Line::OtherValues);
        }
        lines
    }

    /// The line showing a row, while its section is open: counted as [`Self::lines`] lays them
    /// out, without making them.
    pub fn line_of_row(&self, row: usize) -> Option<usize> {
        let mut line = 0;
        for section in self.sections() {
            line += 1;
            if !section.expanded {
                continue;
            }
            if section.range.contains(&row) {
                return Some(line + row - section.range.start);
            }
            line += section.range.len();
            if section.loading || section.error.is_some() || section.has_more() {
                line += 1;
            }
        }
        None
    }

    /// The row `step` rows away from `row` among the rows shown, staying at the ends; from no
    /// row, the first shown.
    pub fn step_row(&self, row: Option<usize>, step: isize) -> Option<usize> {
        let shown: Vec<usize> = self.line_map().rows().collect();
        let at = row.and_then(|row| shown.iter().position(|r| *r == row));
        let Some(at) = at else {
            return shown.first().copied();
        };
        let next = (at as isize + step).clamp(0, shown.len() as isize - 1) as usize;
        shown.get(next).copied()
    }

    /// The rows shown between two rows, both included, in line order.
    pub fn rows_between(&self, a: usize, b: usize) -> Vec<usize> {
        let shown: Vec<usize> = self.line_map().rows().collect();
        let (Some(x), Some(y)) = (
            shown.iter().position(|r| *r == a),
            shown.iter().position(|r| *r == b),
        ) else {
            return vec![b];
        };
        shown[x.min(y)..=x.max(y)].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use graph_client::api::ReadTargetOutput;
    use serde_json::json;

    use super::*;

    fn counts(rows: Value) -> ReadOutput {
        ReadOutput {
            query_name: "studio_value_counts".into(),
            target: ReadTargetOutput {
                branch: Some("main".into()),
                snapshot: None,
            },
            row_count: rows.as_array().map_or(0, |r| r.len() as i64),
            columns: vec!["value".into(), "n".into()],
            rows,
            graph_commit_id: Some("c1".into()),
        }
    }

    fn grouped() -> Grouping {
        let mut grouping = Grouping::new("title");
        grouping.restart();
        grouping.accept_counts(
            &counts(json!([
                {"n": 2},
                {"value": "Designer", "n": 3},
                {"value": "Engineer", "n": 12},
            ])),
            GROUP_LIMIT,
        );
        grouping
    }

    /// The line map finds each line where the list of lines has it, however its sections are
    /// open, read, being read or complete.
    #[test]
    fn the_line_map_matches_the_lines() {
        for case in 0..64u32 {
            let mut grouping = grouped();
            if let Loadable::Loaded(sections) = &mut grouping.sections {
                let mut next = 0;
                for (i, section) in sections.iter_mut().enumerate() {
                    let bits = case >> (i * 2);
                    section.expanded = bits & 1 == 1;
                    let n = ((bits >> 1) & 1) as usize * (i + 2);
                    section.range = next..next + n;
                    next += n;
                    section.loading = case % 5 == i as u32;
                }
            }
            grouping.complete = case % 3 == 0;
            let lines = grouping.lines();
            let map = grouping.line_map();
            assert_eq!(map.len(), lines.len(), "case {case}");
            for (ix, line) in lines.iter().enumerate() {
                assert_eq!(map.get(ix), Some(*line), "case {case}, line {ix}");
            }
            assert_eq!(map.get(lines.len()), None);
            assert_eq!(map.slice(0..lines.len() + 3).collect::<Vec<_>>(), lines);
        }
    }

    #[test]
    fn sections_come_in_value_order_with_no_value_last() {
        let grouping = grouped();
        let values: Vec<&Value> = grouping.sections().iter().map(|s| &s.value).collect();
        assert_eq!(
            values,
            [&json!("Designer"), &json!("Engineer"), &json!(null)]
        );
        assert!(grouping.complete);
        assert_eq!(
            grouping.lines(),
            [
                Line::Header(0),
                Line::More(0),
                Line::Header(1),
                Line::More(1),
                Line::Header(2),
                Line::More(2)
            ],
            "every section opens, with a line that reads its first rows"
        );
        let filter = grouping.filter_for(2).unwrap();
        assert_eq!(filter.op, FilterOp::IsEmpty);
        let filter = grouping.filter_for(1).unwrap();
        assert_eq!((filter.op, filter.value), (FilterOp::Eq, json!("Engineer")));
    }

    #[test]
    fn rows_read_into_a_section_move_the_later_ones_and_steps_skip_closed_sections() {
        let mut grouping = grouped();
        // Engineer reads 2 rows (list 0..2), then Designer 3 (list 0..3, Engineer 3..5).
        grouping.inserted(1, 2);
        grouping.inserted(0, 3);
        let ranges: Vec<Range<usize>> = grouping
            .sections()
            .iter()
            .map(|s| s.range.clone())
            .collect();
        assert_eq!(ranges, [0..3, 3..5, 5..5]);
        assert_eq!(grouping.section_of(4), Some(1));
        if let Some(section) = grouping.section_mut(0) {
            section.paging.exhausted = true;
        }
        assert_eq!(
            grouping.lines(),
            [
                Line::Header(0),
                Line::Row(0),
                Line::Row(1),
                Line::Row(2),
                Line::Header(1),
                Line::Row(3),
                Line::Row(4),
                Line::More(1),
                Line::Header(2),
                Line::More(2)
            ]
        );
        assert_eq!(grouping.step_row(Some(2), 1), Some(3), "over the header");
        grouping.section_mut(0).unwrap().expanded = false;
        assert_eq!(
            grouping.step_row(Some(3), -1),
            Some(3),
            "closed rows are skipped"
        );
        assert_eq!(grouping.step_row(None, 1), Some(3));
        assert_eq!(grouping.line_of_row(1), None);
        // Counted where the lines put each row.
        let lines = grouping.lines();
        for row in 0..6 {
            let at = lines.iter().position(|line| *line == Line::Row(row));
            assert_eq!(grouping.line_of_row(row), at, "row {row}");
        }
        assert_eq!(grouping.rows_between(4, 3), [3, 4]);

        grouping.removed(3);
        let ranges: Vec<Range<usize>> = grouping
            .sections()
            .iter()
            .map(|s| s.range.clone())
            .collect();
        assert_eq!(ranges, [0..3, 3..4, 4..4]);
        assert_eq!(grouping.sections()[1].count, 11);
    }

    #[test]
    fn a_relationship_groups_by_neighbour_with_the_unrelated_rows_last() {
        let mut grouping = Grouping::new("out:WorksAt");
        grouping.restart();
        grouping.accept_counts(
            &counts(json!([
                {"value": "company-1", "label": "Acme", "n": 4},
                {"value": "company-2", "n": 2},
            ])),
            GROUP_LIMIT,
        );
        grouping.add_none_section(3);
        let sections = grouping.sections();
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].label.as_deref(), Some("Acme"));
        assert_eq!(sections[1].label, None);
        assert!(sections[2].value.is_null());
        let filter = grouping.filter_for(0).unwrap();
        assert_eq!(
            (filter.property.as_str(), filter.op, filter.value),
            ("out:WorksAt", FilterOp::RelatedTo, json!("company-1"))
        );
        assert_eq!(grouping.filter_for(2).unwrap().op, FilterOp::HasNone);

        let mut none = Grouping::new("out:WorksAt");
        none.restart();
        none.accept_counts(&counts(json!([])), GROUP_LIMIT);
        none.add_none_section(0);
        assert!(none.sections().is_empty(), "no section for no rows");
    }

    #[test]
    fn a_neighbours_property_narrows_by_its_value_and_an_end_has_no_value_in_the_row() {
        let mut grouping = Grouping::new("out:WorksAt.city");
        assert!(!grouping.by_relationship() && !grouping.by_own_property());
        grouping.restart();
        grouping.accept_counts(&counts(json!([{"value": "Tokyo", "n": 58}])), GROUP_LIMIT);
        grouping.add_none_section(38);
        let filter = grouping.filter_for(0).unwrap();
        assert_eq!(
            (filter.property.as_str(), filter.op, filter.value),
            ("out:WorksAt.city", FilterOp::Eq, json!("Tokyo"))
        );
        assert_eq!(grouping.filter_for(1).unwrap().op, FilterOp::IsEmpty);
        let values = Map::from_iter([("name".to_owned(), json!("Ada"))]);
        assert_eq!(
            grouping.value_of_new_row(&values),
            Some(Value::Null),
            "a new node has no neighbours"
        );

        let ends = Grouping::new("to.city");
        assert!(!ends.by_own_property());
        assert_eq!(ends.value_of_new_row(&values), None);
        let own = Grouping::new("name");
        assert!(own.by_own_property());
        assert_eq!(own.value_of_new_row(&values), Some(json!("Ada")));
    }

    #[test]
    fn a_read_cut_at_the_limit_says_more_values_exist() {
        let mut grouping = Grouping::new("title");
        grouping.accept_counts(&counts(json!([{"value": "A", "n": 1}])), 1);
        assert!(!grouping.complete);
        assert_eq!(grouping.lines().last(), Some(&Line::OtherValues));
    }
}
