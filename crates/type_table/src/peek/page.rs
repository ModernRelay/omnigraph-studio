//! A node's page as a document (spec 014, "The page as a document"): one title, a quiet meta
//! line, the properties as plain text in a label and value grid, Blobs as compact cards, and
//! each relationship titled by its type with both of its directions together. The page is the
//! same view as the inspector's peek, which keeps its compact look; only what the page draws is
//! here, and every field, edit, draft and write is the peek's own.

use super::*;

/// The mark [`NodePeek::copied`] holds while the page's key was just copied.
const KEY_COPIED: &str = "@id";

/// The mark [`NodePeek::copied`] holds while the page's title was just copied.
const TITLE_COPIED: &str = "@title";

/// The lines of a text Blob a page's card shows; the rest open with Open.
const TEXT_LINES: usize = 6;

/// The bytes a page's card spells out of a value that is neither text nor an image.
const BYTES_SHOWN: usize = 8;

/// A value as a page shows it: plain text, wrapping within its column.
pub(super) fn plain_value(text: impl Into<SharedString>) -> Div {
    div().min_w_0().child(studio_ui::ds::text::body(text))
}

/// A short value as a page shows it: plain text; a list's items with commas between them;
/// none a quiet dash.
pub(super) fn render_value(property: Option<&gq_language::Property>, value: &Value) -> AnyElement {
    let none = || {
        studio_ui::ds::text::body("—")
            .color(Color::Disabled)
            .into_any_element()
    };
    match value {
        Value::Null => none(),
        Value::Array(items) if items.is_empty() => none(),
        Value::Array(items) => plain_value(
            items
                .iter()
                .map(studio_ui::format_value)
                .collect::<Vec<_>>()
                .join(", "),
        )
        .into_any_element(),
        _ => plain_value(display_value(property, value)).into_any_element(),
    }
}

/// The first `lines` lines of `text`, and whether any follow.
fn first_lines(text: &str, lines: usize) -> (String, bool) {
    let mut shown: Vec<&str> = text.lines().take(lines + 1).collect();
    let more = shown.len() > lines;
    shown.truncate(lines);
    (shown.join("\n"), more)
}

/// A value's first bytes, spelled out in hex, for a card's one line.
fn bytes_line(head: &[u8]) -> String {
    let shown: Vec<String> = head
        .iter()
        .take(BYTES_SHOWN)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut line = shown.join(" ");
    if head.len() > BYTES_SHOWN {
        line.push_str(" …");
    }
    line
}

impl NodePeek {
    /// The property the page's title shows: the node's label, when it has one.
    pub(super) fn title_property(&self) -> Option<String> {
        self.label()?;
        names_by(self.schema.as_deref(), self.subject.type_name())
    }

    /// The key property, when its value is the node's id, which the meta line shows.
    pub(super) fn key_property(&self) -> Option<String> {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return None;
        };
        let key = self.schema.as_deref()?.node(type_name)?.key.clone()?;
        (self.values.loaded()?.get(&key)?.as_str() == Some(id.as_str())).then_some(key)
    }

    /// Whether a page leaves property `name` out of its list: the title or the meta line gives
    /// everything its row gave, and nothing about it needs its row: it is not being edited or
    /// held as a draft, and the commit shown holds it under its name. The name's case is
    /// [`Self::title_covers_name`]'s alone, so what the title does and what the list leaves out
    /// cannot drift apart. Editing the name (a click on the title, F2) opens its row.
    pub(super) fn page_hides(&self, name: &str, cx: &App) -> bool {
        let shown_above = if self.title_property().as_deref() == Some(name) {
            self.title_covers_name(cx)
        } else if self.key_property().as_deref() == Some(name) {
            // The key is never edited; a menu or a switch keeps its row.
            !self.property_named(name).is_some_and(picked_by_control)
                && !self.saving.on(&self.branch, name)
                && !self.saving_elsewhere.contains(name)
        } else {
            false
        };
        self.page
            && shown_above
            && !self.edits.contains_key(name)
            && !self.absent.contains_key(name)
            && self.draft_of(name, cx).is_none()
    }

    /// What a click on the page's title does with the name it shows: edits it
    /// ([`Self::title_edit`]) or copies it, when the credential may not change it. `None` when
    /// the title does neither for the name, and the name keeps its row: an enum's menu or a
    /// boolean's switch, or a name being saved.
    fn title_role(&self, cx: &App) -> Option<TitleRole> {
        if !self.page {
            return None;
        }
        let name = names_by(self.schema.as_deref(), self.subject.type_name())?;
        let property = self.property_named(&name)?;
        if picked_by_control(property)
            || self.saving.on(&self.branch, &name)
            || self.saving_elsewhere.contains(&name)
        {
            return None;
        }
        Some(if self.edits_property(property, cx) {
            TitleRole::Edits(property.clone())
        } else {
            TitleRole::Copies
        })
    }

    /// Whether the title gives everything the name's row gave: it edits the name, or it copies
    /// a name the credential may not change.
    pub(super) fn title_covers_name(&self, cx: &App) -> bool {
        self.title_role(cx).is_some()
    }

    /// The property a click on the page's title, or F2, edits: the one that names the node,
    /// where the credential may change it and it is edited as text (an enum's menu and a
    /// boolean's switch stay in their rows).
    pub(super) fn title_edit(&self, cx: &App) -> Option<gq_language::Property> {
        match self.title_role(cx)? {
            TitleRole::Edits(property) => Some(property),
            TitleRole::Copies => None,
        }
    }

    /// F2: edits the name the title shows, in its row, which shows while it is open.
    pub(super) fn on_edit_name(
        &mut self,
        _: &EditNodeName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(property) = self.title_edit(cx) {
            self.start_edit(property, window, cx);
        }
    }

    /// Whether a group's "+" shows at rest: while its search is open. Otherwise it shows under
    /// the pointer, on a page as in the inspector (spec 014).
    pub(super) fn add_shows_at_rest(&self, index: usize) -> bool {
        self.adding
            .as_ref()
            .is_some_and(|adding| adding.index == index)
    }

    /// Orders `items` so the groups of one relationship type sit together, each pair where its
    /// first group was: a page lists `Knows → Person` and `Knows ← Person` as one.
    pub(super) fn pair_directions(&self, items: &mut Vec<Item>) {
        let firsts: Vec<usize> = items
            .iter()
            .enumerate()
            .map(|(at, item)| match edge_of(self, item) {
                Some(edge) => items
                    .iter()
                    .position(|other| edge_of(self, other) == Some(edge))
                    .unwrap_or(at),
                None => at,
            })
            .collect();
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by_key(|&at| firsts[at]);
        *items = order.into_iter().map(|at| items[at]).collect();
    }

    /// Whether group `index` follows a group of its relationship type in the list, so its
    /// header leaves the type's name to the one above it.
    pub(super) fn continues_pair(&self, index: usize) -> bool {
        let Some(at) = self
            .list_items
            .iter()
            .position(|item| *item == Item::Group(index))
        else {
            return false;
        };
        at.checked_sub(1)
            .and_then(|before| self.list_items.get(before))
            .and_then(|before| edge_of(self, before))
            .is_some_and(|edge| Some(edge) == self.groups.get(index).map(|g| &g.group.edge))
    }

    /// A relationship group's header on a page: its type as the sidebar names it, the way it
    /// points and the other type, its count, its chevron; the "+" and the write's dot follow.
    /// The second direction of a type it already listed leaves out the type's name.
    pub(super) fn page_group_header(
        &self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let group = &self.groups[index];
        let continues = self.continues_pair(index);
        let arrow = match group.group.direction {
            Direction::Outgoing => IconName::ArrowRight,
            Direction::Incoming => IconName::ArrowLeft,
        };
        let count = match &group.count {
            Loadable::Loaded(n) => studio_ui::group_count(*n),
            Loadable::Failed(_) => "?".into(),
            Loadable::NotLoaded | Loadable::Loading => "…".into(),
        };
        let hover = studio_ui::ds::surface::hover(cx);
        let selector = format!("page-group-title-{index}");
        h_flex()
            .id(("peek-group", index))
            .w_full()
            .h(px(studio_ui::ds::size::SIDEBAR_ROW))
            .px(px(space::XS))
            .gap(px(space::SM))
            .items_center()
            .rounded(px(studio_ui::ds::radii(cx).sm))
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .when(!continues, |header| {
                header.child(
                    div()
                        .debug_selector(move || selector.clone())
                        .child(studio_ui::ds::text::body(group.group.edge.clone())),
                )
            })
            .child(
                h_flex()
                    .gap(px(space::XS))
                    .items_center()
                    .child(studio_ui::ds::icon::meta(arrow))
                    .child(studio_ui::ds::text::muted(group.group.other_type.clone())),
            )
            .child(studio_ui::ds::text::meta(count))
            .child(studio_ui::ds::icon::meta(if group.expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(div().flex_1())
    }

    /// The page's head: its title, its meta line, its drafts, then the node's values and
    /// Blobs; while the node is read, the same places as quiet bars.
    pub(super) fn render_page_head(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let reading = matches!(self.values, Loadable::NotLoaded | Loadable::Loading);
        let label = self.label();
        let meta = self.render_page_meta(label.is_some(), cx);
        if reading {
            return v_flex()
                .w_full()
                .debug_selector(|| "page-reading".into())
                .child(
                    v_flex()
                        .gap(px(space::SM))
                        .child(studio_ui::ds::skeleton(
                            studio_ui::ds::size::SKELETON_LONG,
                            studio_ui::ds::size::SKELETON_TITLE,
                            cx,
                        ))
                        .child(meta),
                )
                .child(v_flex().pt(px(space::LG)).children((0..3).map(|_| {
                    h_flex()
                        .h(px(studio_ui::ds::size::PROPERTY_ROW))
                        .gap(px(space::LG))
                        .items_center()
                        .child(
                            div()
                                .w(px(studio_ui::ds::size::DOCUMENT_LABEL))
                                .flex_none()
                                .child(studio_ui::ds::skeleton(
                                    studio_ui::ds::size::SKELETON_SHORT,
                                    studio_ui::ds::size::SKELETON_LINE,
                                    cx,
                                )),
                        )
                        .child(studio_ui::ds::skeleton(
                            studio_ui::ds::size::SKELETON_LONG,
                            studio_ui::ds::size::SKELETON_LINE,
                            cx,
                        ))
                })));
        }
        let id = self.subject.id().to_owned();
        let title = self.render_page_title(label.unwrap_or(id), cx);
        let has_properties = self
            .schema
            .as_ref()
            .is_some_and(|schema| !listed_properties(schema, &self.subject).is_empty());
        let properties = has_properties.then(|| self.render_properties(window, cx));
        let endpoints = self.render_endpoints(cx);
        let drafts_bar = self.render_drafts_bar(cx);
        let blobs = (!self.blobs.is_empty()).then(|| self.render_page_blobs(cx));
        v_flex()
            .w_full()
            .child(
                v_flex()
                    .gap(px(space::SM))
                    .pb(px(space::MD))
                    .child(title)
                    .child(meta),
            )
            .children(drafts_bar)
            .children(endpoints.map(|endpoints| div().mx(px(-space::XS)).child(endpoints)))
            .children(properties)
            .children(blobs)
    }

    /// The page's one title, the node's name or its id; a click on it edits the name.
    fn render_page_title(&self, title: String, cx: &mut Context<Self>) -> AnyElement {
        let text = title.clone();
        let title = studio_ui::ds::text::page_title(title);
        let Some(property) = self.title_edit(cx) else {
            // A name that cannot be edited here, or an id, copies on a click: its row is not
            // listed, so the title is where it is copied from, and says so beside itself.
            let copied = self.copied.as_deref() == Some(TITLE_COPIED);
            return h_flex()
                .items_center()
                .gap(px(space::SM))
                .child(
                    div()
                        .id("page-title")
                        .debug_selector(|| "page-title".into())
                        .cursor_pointer()
                        .tip(studio_ui::ds::tooltip("Click to copy"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_text(TITLE_COPIED.to_owned(), text.clone(), cx)
                        }))
                        .child(title),
                )
                .when(copied, |row| {
                    row.child(
                        div()
                            .debug_selector(|| "title-copied".into())
                            .child(studio_ui::ds::text::meta("Copied").color(Color::Success)),
                    )
                })
                .into_any_element();
        };
        let hover = studio_ui::ds::surface::hover(cx);
        let name = property.name.clone();
        // Pulled out by its hover inset, the title's text stays on the column's edge.
        div()
            .id("page-title")
            .debug_selector(|| "page-title".into())
            .mx(px(-space::XS))
            .px(px(space::XS))
            .rounded(px(studio_ui::ds::radii(cx).sm))
            .cursor_text()
            .hover(move |style| style.bg(hover))
            .tip_fn(move |_, cx| {
                studio_ui::ds::tooltip_for_action_view(format!("Edit {name}"), &EditNodeName, cx)
            })
            .on_click(
                cx.listener(move |this, _, window, cx| {
                    this.start_edit(property.clone(), window, cx)
                }),
            )
            .child(title)
            .into_any_element()
    }

    /// The meta line under the title: the type; the key, which a click copies, unless the title
    /// shows it and copies it itself (an unnamed node whose name the title cannot edit); "read at" a commit only when the page holds one or the branch
    /// has moved past it; and "Newer data · Refresh".
    fn render_page_meta(&self, named: bool, cx: &mut Context<Self>) -> Div {
        let type_name = self.subject.type_name().to_owned();
        let id = self.subject.id().to_owned();
        let is_node = matches!(self.subject, PeekSubject::Node { .. });
        let separator = || studio_ui::ds::text::muted("·");
        let key_copied = self.copied.as_deref() == Some(KEY_COPIED);
        let commit = self
            .snapshot
            .clone()
            .filter(|_| self.holds_commit || self.newer);
        let holds = self.holds_commit;
        // The title copies what it shows unless it edits the name: then the key is copied here.
        let key_shown = named || self.title_edit(cx).is_some();
        h_flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(space::SM))
            .gap_y(px(space::XS))
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .items_center()
                    .child(studio_ui::type_swatch(&type_name, self.session.read(cx).type_tints(), cx))
                    .child(studio_ui::ds::text::muted(type_name)),
            )
            .when(key_shown, |meta| {
                let copy = id.clone();
                meta.child(separator()).child(
                    div()
                        .id("page-key")
                        .debug_selector(|| "page-key".into())
                        .cursor_pointer()
                        .tip(studio_ui::ds::tooltip("Key · click to copy"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_text(KEY_COPIED.to_owned(), copy.clone(), cx)
                        }))
                        .child(studio_ui::ds::text::code(id, cx).color(Color::Muted)),
                )
            })
            .when(key_copied, |meta| {
                meta.child(
                    div()
                        .debug_selector(|| "key-copied".into())
                        .child(studio_ui::ds::text::meta("Copied").color(Color::Success)),
                )
            })
            .children(commit.map(|commit| {
                let short = studio_ui::short_commit_id(&commit);
                div()
                    .id("page-commit")
                    .tip(studio_ui::ds::tooltip(if holds {
                        format!("This page shows commit {commit}, not the branch's head")
                    } else {
                        format!("Read at commit {commit}; the branch has moved since")
                    }))
                    .child(
                        h_flex()
                            .gap(px(space::SM))
                            .items_center()
                            .child(separator())
                            .child(studio_ui::ds::text::muted("read at"))
                            .child(studio_ui::ds::text::code(short, cx).color(Color::Muted)),
                    )
            }))
            .children(self.render_last_change(cx))
            // A node opened by id reads again by id; an edge is read with its table.
            .when(self.newer && is_node, |meta| {
                meta.child(
                    studio_ui::ds::row_button("peek-newer", "Newer data · Refresh")
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .tip_fn(|_, cx| {
                            studio_ui::ds::tooltip_for_action_view(
                                "The branch moved since this was read. Refresh reads it at the head.",
                                &Refresh,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
            })
    }

    /// "Last changed by <actor> · <time>", once the node's history was read at the commit the
    /// page shows (spec 046): the page reads no history itself, and says nothing until then. A
    /// click opens the History.
    fn render_last_change(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let PeekSubject::Node { type_name, id } = &self.subject else {
            return None;
        };
        if self.holds_commit {
            return None;
        }
        let last = self.session.read(cx).last_change(type_name, id)?.clone();
        if self.snapshot.as_deref() != Some(last.head.as_str()) {
            return None;
        }
        let who = last.actor.clone().unwrap_or_else(|| "no actor".to_owned());
        let when = studio_ui::relative_time(last.at, studio_ui::now_micros());
        let tip = format!(
            "In commit {} · {}",
            studio_ui::short_commit_id(&last.commit),
            studio_ui::utc_date_time(last.at)
        );
        let open = studio_actions::graph::ShowNodeHistory {
            type_name: type_name.clone(),
            id: id.clone(),
            preview: true,
        };
        Some(
            h_flex()
                .gap(px(space::SM))
                .items_center()
                .child(studio_ui::ds::text::muted("·"))
                .child(
                    div()
                        .id("page-last-change")
                        .debug_selector(|| "page-last-change".into())
                        .cursor_pointer()
                        .tip_fn(move |_, cx| {
                            studio_ui::ds::tooltip_for_action_view(
                                format!("{tip}. Open its History"),
                                &studio_actions::workspace::OpenViewAt {
                                    index: 3,
                                    beside: false,
                                },
                                cx,
                            )
                        })
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(Box::new(open.clone()), cx)
                        })
                        .child(studio_ui::ds::text::muted(format!(
                            "Last changed by {who} · {when}"
                        ))),
                )
                .into_any_element(),
        )
    }

    /// The node's Blobs on a page, each under its name as a compact card: its kind and size,
    /// Open, Save…, Choose File… and Clear on its first line, then an image's preview, a
    /// text's first lines, or a line of other bytes, whose dump "Show bytes" reveals.
    fn render_page_blobs(&self, cx: &mut Context<Self>) -> AnyElement {
        let drop_bg = studio_ui::ds::surface::drop_target(cx);
        v_flex()
            .pt(px(space::MD))
            .gap(px(space::LG))
            .children(self.blobs.iter().enumerate().map(|(index, slot)| {
                let saving = self.saving.on(&self.branch, &slot.property);
                let facts = self.blob_facts(slot);
                let writes = self.render_blob_writes(index, slot, cx);
                let notice = self.blob_notice(index, slot, cx);
                let save = self.blob_save(index, slot, cx);
                let droppable = self.blob_write_refusal(&slot.property, cx).is_none() && !saving;
                let kind = match &slot.preview {
                    Loadable::Loaded(Preview::Empty) | Loadable::NotLoaded | Loadable::Loading => {
                        None
                    }
                    Loadable::Loaded(_) if !slot.head.is_empty() => {
                        Some(crate::tile::kind_word(crate::blob::value_kind(&slot.head)))
                    }
                    Loadable::Loaded(Preview::Image(_)) => Some("Image"),
                    Loadable::Loaded(Preview::Text { .. }) => Some("Text"),
                    Loadable::Loaded(Preview::Bytes { .. }) => Some("Bytes"),
                    Loadable::Failed(_) => None,
                };
                let bytes_shown = self.bytes_shown.contains(&slot.property);
                let toggle_bytes = matches!(slot.preview, Loadable::Loaded(Preview::Bytes { .. }))
                    .then(|| {
                        let property = slot.property.clone();
                        studio_ui::ds::button(
                            ("page-blob-bytes", index),
                            if bytes_shown {
                                "Hide bytes"
                            } else {
                                "Show bytes"
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .color(Color::Muted)
                        .tip(studio_ui::ds::tooltip(if bytes_shown {
                            "Hide the first bytes"
                        } else {
                            "Show the first bytes as a hex dump"
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.bytes_shown.remove(&property) {
                                this.bytes_shown.insert(property.clone());
                            }
                            cx.notify();
                        }))
                    });
                let body: Option<AnyElement> = match (&slot.meta, &slot.preview) {
                    (Loadable::Failed(error), _) | (_, Loadable::Failed(error)) => {
                        Some(studio_ui::ds::text::error(error.clone()).into_any_element())
                    }
                    (Loadable::Loaded(BlobMeta::External { uri }), _) => Some(
                        v_flex()
                            .gap(px(space::XS))
                            .child(studio_ui::CopyableValue::new(
                                ("blob-uri", index),
                                Some(uri.clone()),
                            ))
                            .child(studio_ui::ds::text::meta(
                                "Studio does not fetch objects outside the graph.",
                            ))
                            .into_any_element(),
                    ),
                    (_, Loadable::Loaded(Preview::Image(image))) => Some(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .child(
                                gpui::img(image.source())
                                    .max_w_full()
                                    .max_h(px(studio_ui::ds::size::PREVIEW_MAX_HEIGHT))
                                    .object_fit(gpui::ObjectFit::Contain),
                            )
                            .into_any_element(),
                    ),
                    (_, Loadable::Loaded(Preview::Text { text, .. })) => {
                        let (lines, _) = first_lines(text, TEXT_LINES);
                        Some(studio_ui::ds::code_block(lines, cx).into_any_element())
                    }
                    (_, Loadable::Loaded(Preview::Bytes { hex, .. })) if bytes_shown => {
                        Some(studio_ui::ds::code_block(hex.clone(), cx).into_any_element())
                    }
                    (_, Loadable::Loaded(Preview::Bytes { .. })) => Some(
                        div()
                            .debug_selector(|| "page-blob-bytes-line".into())
                            .child(
                                studio_ui::ds::text::code(bytes_line(&slot.head), cx)
                                    .color(Color::Muted),
                            )
                            .into_any_element(),
                    ),
                    _ => None,
                };
                let selector = format!("page-blob-{}", slot.property);
                let card = studio_ui::ds::card(cx)
                    .debug_selector(move || selector.clone())
                    .child(
                        h_flex()
                            .w_full()
                            .min_h(px(studio_ui::ds::size::SIDEBAR_ROW + space::MD))
                            .px(px(space::LG))
                            .gap(px(space::MD))
                            .items_center()
                            .child(studio_ui::ds::icon::meta(IconName::File))
                            .children(kind.map(studio_ui::ds::text::body))
                            .when(saving, |line| {
                                line.child(studio_ui::ds::dot(Color::Warning))
                            })
                            .child(studio_ui::ds::text::meta(facts))
                            .child(div().flex_1())
                            .children(toggle_bytes)
                            .children(save)
                            .child(writes),
                    )
                    .children(body.map(|body| {
                        div()
                            .w_full()
                            .px(px(space::LG))
                            .pb(px(space::LG))
                            .child(body)
                    }));
                let slot_view = v_flex()
                    .id(("peek-blob", index))
                    .gap(px(space::XS))
                    .child(studio_ui::ds::text::muted(slot.property.clone()))
                    .child(card)
                    .children(notice);
                // A file dropped from the file manager on the slot is stored in it.
                if droppable {
                    slot_view
                        .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                            this.drop_blob_files(index, paths.paths(), cx)
                        }))
                        .into_any_element()
                } else {
                    slot_view.into_any_element()
                }
            }))
            .into_any_element()
    }
}

/// What the page's title does with the name it shows ([`NodePeek::title_role`]).
enum TitleRole {
    /// A click or F2 edits it, in its row.
    Edits(gq_language::Property),
    /// A click copies it: the credential may not change it.
    Copies,
}

/// Whether a property is set with a control rather than typed: an enum's menu or a boolean's
/// switch, which stays in its row.
fn picked_by_control(property: &gq_language::Property) -> bool {
    !property.list
        && (property.enum_values.is_some() || property.kind == gq_language::ScalarKind::Bool)
}

/// The relationship type of a list item that is a group.
fn edge_of<'a>(view: &'a NodePeek, item: &Item) -> Option<&'a String> {
    match item {
        Item::Group(index) => view.groups.get(*index).map(|group| &group.group.edge),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;

    use crate::test_support::{PEOPLE, session, session_routing};

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            studio_theme::init_test(cx);
            editor::init(cx);
        });
    }

    /// p1's page (or its peek, `page` false) in a window, read.
    fn open(
        session: Entity<GraphSession>,
        page: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<NodePeek>, &mut VisualTestContext) {
        init(cx);
        let (view, cx) = cx.add_window_view(move |_, cx| {
            if page {
                NodePeek::page(session, "Person".into(), "p1".into(), None, cx)
            } else {
                let subject = PeekSubject::Node {
                    type_name: "Person".into(),
                    id: "p1".into(),
                };
                NodePeek::new(session, subject, None, None, HashMap::new(), false, cx)
            }
        });
        cx.run_until_parked();
        (view, cx)
    }

    /// The page reads no history; once the node's history was read at the commit it shows, its
    /// meta line says who changed it last, and not for a history read at another commit.
    #[gpui::test]
    async fn the_meta_line_says_who_changed_the_node_once_its_history_is_read(
        cx: &mut TestAppContext,
    ) {
        let session = session(PEOPLE, cx);
        let (view, cx) = open(session.clone(), true, cx);
        assert!(
            cx.debug_bounds("page-last-change").is_none(),
            "nothing read"
        );
        let shown = view
            .read_with(cx, |view, _| view.snapshot.clone())
            .expect("read");
        let last = |head: &str| graph_store::node_history::LastChange {
            head: head.to_owned(),
            commit: "c9".into(),
            actor: Some("act-dev".into()),
            at: 0,
        };
        session.update(cx, |session, cx| {
            session.hold_last_change("Person", "p1", last("another-head"), cx)
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("page-last-change").is_none(),
            "read at another commit"
        );
        session.update(cx, |session, cx| {
            session.hold_last_change("Person", "p1", last(&shown), cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("page-last-change").is_some());
    }

    /// A page shows the name as its title and the key in its meta line, and lists neither
    /// again; the inspector's peek keeps both rows. Editing the name opens its row.
    #[gpui::test]
    async fn a_page_lists_nothing_its_title_and_meta_show(cx: &mut TestAppContext) {
        let (view, cx) = open(session(PEOPLE, cx), true, cx);
        assert!(cx.debug_bounds("page-title").is_some());
        assert!(
            cx.debug_bounds("peek-row-name").is_none(),
            "the title shows it"
        );
        assert!(
            cx.debug_bounds("peek-row-slug").is_none(),
            "the meta line shows it"
        );
        view.read_with(cx, |view, cx| {
            assert_eq!(view.title_property().as_deref(), Some("name"));
            assert_eq!(view.key_property().as_deref(), Some("slug"));
            assert!(view.page_hides("name", cx) && view.page_hides("slug", cx));
        });

        let (_, cx) = open(session(PEOPLE, cx), false, cx);
        assert!(
            cx.debug_bounds("peek-row-name").is_some(),
            "the peek keeps it"
        );
        assert!(cx.debug_bounds("peek-row-slug").is_some());
    }

    /// A name picked from an enum keeps its row on the page, where its menu is: the title does
    /// not edit it, and a click on the title copies the name (review of #161).
    #[gpui::test]
    async fn an_enum_name_keeps_its_row_and_the_title_copies_it(cx: &mut TestAppContext) {
        const PICKED: &str = "node Person {\n  slug: String @key\n  name: enum(Ada, Grace)\n}\n";
        let (view, cx) = open(session(PICKED, cx), true, cx);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.title_property().as_deref(), Some("name"));
            assert!(
                view.title_edit(cx).is_none(),
                "an enum is picked in its row"
            );
            assert!(!view.page_hides("name", cx));
        });
        assert!(
            cx.debug_bounds("peek-row-name").is_some(),
            "its row, with its menu, stays"
        );
        let title = cx.debug_bounds("page-title").expect("the title");
        cx.simulate_click(title.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("Ada".to_owned())
        );
        assert!(
            cx.debug_bounds("title-copied").is_some(),
            "the title says it was copied"
        );
        assert!(
            cx.debug_bounds("key-copied").is_none(),
            "the key was not copied"
        );
        let key = cx.debug_bounds("page-key").expect("the key copies too");
        cx.simulate_click(key.center(), gpui::Modifiers::none());
        assert!(cx.debug_bounds("key-copied").is_some());
        assert!(cx.debug_bounds("title-copied").is_none());
    }

    /// What the list leaves out follows what the title does, from one owner: a page held at a
    /// commit cannot change the name, so the title copies it and its row goes; an enum's title
    /// neither edits nor covers it, so its row stays (review of #161).
    #[gpui::test]
    async fn the_title_and_the_list_agree_on_the_name(cx: &mut TestAppContext) {
        init(cx);
        let session = session(PEOPLE, cx);
        let (view, cx) = cx.add_window_view(move |_, cx| {
            NodePeek::page(session, "Person".into(), "p1".into(), Some("c1".into()), cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.title_edit(cx).is_none(), "held at a commit");
            assert!(view.title_covers_name(cx), "the title copies it");
            assert!(view.page_hides("name", cx));
        });
        assert!(cx.debug_bounds("peek-row-name").is_none());
        let title = cx.debug_bounds("page-title").expect("the title");
        cx.simulate_click(title.center(), gpui::Modifiers::none());
        assert!(
            cx.read_from_clipboard()
                .is_some_and(|item| item.text().is_some())
        );
    }

    /// An empty name names nothing: the title shows the id, which a click still answers, and
    /// the name keeps its row (review of #161).
    #[gpui::test]
    async fn an_empty_name_titles_the_page_by_its_id(cx: &mut TestAppContext) {
        let rows = json!([{"n": {"@id": "p1", "slug": "p1", "name": ""}}]);
        let (view, cx) = open(
            crate::test_support::session_reading(PEOPLE, rows, cx),
            true,
            cx,
        );
        view.read_with(cx, |view, cx| {
            assert!(view.label().is_none(), "no blank title");
            assert!(!view.page_hides("name", cx));
        });
        assert!(cx.debug_bounds("peek-row-name").is_some(), "its row stays");
        assert!(cx.debug_bounds("page-title").is_some(), "clickable");
    }

    /// A graph's schema, read through its session, draws Company and Person in different tints,
    /// where their names hash to one (review of #161: no test failed with the session's call
    /// taken out).
    #[gpui::test]
    async fn a_graphs_schema_draws_its_types_apart(cx: &mut TestAppContext) {
        init(cx);
        const FIRMS: &str =
            "node Person {\n  slug: String @key\n}\nnode Company {\n  slug: String @key\n}\n";
        let session = session(FIRMS, cx);
        cx.run_until_parked();
        session.read_with(cx, |session, _| {
            let tints = session.type_tints();
            assert_ne!(
                tints.color("Company"),
                tints.color("Person"),
                "Company and Person draw in one tint"
            );
        });
    }

    /// Two graphs open together that share a type name each draw it in their own tint: reading
    /// the second never moves the first's types onto one tint (review of #161).
    #[gpui::test]
    async fn two_graphs_sharing_a_type_keep_their_own_tints(cx: &mut TestAppContext) {
        init(cx);
        const A: &str =
            "node Person {\n  slug: String @key\n}\nnode Company {\n  slug: String @key\n}\n";
        const B: &str =
            "node Person {\n  slug: String @key\n}\nnode Topic {\n  slug: String @key\n}\n";
        let a = session(A, cx);
        cx.run_until_parked();
        let drawn = |session: &Entity<GraphSession>, name: &str, cx: &mut TestAppContext| {
            session.read_with(cx, |session, _| session.type_tints().color(name))
        };
        let (company, person) = (drawn(&a, "Company", cx), drawn(&a, "Person", cx));
        let b = session(B, cx);
        cx.run_until_parked();
        assert_eq!(drawn(&a, "Company", cx), company);
        assert_eq!(
            drawn(&a, "Person", cx),
            person,
            "the second graph moves nothing"
        );
        assert_ne!(drawn(&a, "Company", cx), drawn(&a, "Person", cx));
        assert_ne!(drawn(&b, "Person", cx), drawn(&b, "Topic", cx));
    }

    /// A node with no name is titled by its id, which the title cannot edit as a name: the key
    /// stays copyable in the meta line while the title edits the name (review of #161).
    #[gpui::test]
    async fn an_unnamed_nodes_key_stays_copyable(cx: &mut TestAppContext) {
        const OPTIONAL: &str = "node Person {\n  slug: String @key\n  name: String?\n}\n";
        let rows = json!([{"n": {"@id": "p1", "slug": "p1", "name": null}}]);
        let (view, cx) = open(
            crate::test_support::session_reading(OPTIONAL, rows, cx),
            true,
            cx,
        );
        view.read_with(cx, |view, cx| {
            assert!(view.label().is_none());
            assert!(view.title_edit(cx).is_some(), "the title sets a name");
        });
        let key = cx
            .debug_bounds("page-key")
            .expect("the key in the meta line");
        cx.simulate_click(key.center(), gpui::Modifiers::none());
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("p1".to_owned())
        );
    }

    /// A click on the title, or F2, opens the name for editing in its row; Esc closes it and
    /// the row goes again.
    #[gpui::test]
    async fn the_title_edits_the_name_on_a_click_or_f2(cx: &mut TestAppContext) {
        let (view, cx) = open(session(PEOPLE, cx), true, cx);
        let title = cx.debug_bounds("page-title").expect("an editable title");
        cx.simulate_click(title.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.edits.contains_key("name")));
        assert!(cx.debug_bounds("peek-row-name").is_some(), "its row, open");

        view.update_in(cx, |view, window, cx| {
            view.on_cancel_field(&CancelField, window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.edits.is_empty()));
        assert!(cx.debug_bounds("peek-row-name").is_none());

        cx.update(|_, cx| {
            cx.bind_keys([gpui::KeyBinding::new("f2", EditNodeName, Some("NodeView"))])
        });
        cx.update(|window, cx| {
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.edits.contains_key("name"), "F2"));
    }

    /// Both directions of a relationship type are listed together, the second without the
    /// type's name; before, every outgoing group came first and `Knows ←` came last.
    #[gpui::test]
    async fn both_directions_of_a_relationship_sit_together(cx: &mut TestAppContext) {
        let schema =
            format!("{PEOPLE}edge Knows: Person -> Person\nedge Mentors: Person -> Person\n");
        let (session, _) = session_routing(
            &schema,
            |request| match request["name"].as_str() {
                Some("studio_node") => {
                    json!([{ "n": { "@id": "p1", "slug": "p1", "name": "Ada" } }])
                }
                Some("studio_neighbours") => json!([{ "id": "p2", "edge_id": "e1" }]),
                _ => json!([]),
            },
            cx,
        );
        let (view, cx) = open(session, true, cx);
        let order = view.read_with(cx, |view, _| {
            view.list_items
                .iter()
                .filter_map(|item| match item {
                    Item::Group(index) => {
                        let group = &view.groups[*index].group;
                        Some((*index, group.edge.clone(), group.direction))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        let edges: Vec<&str> = order.iter().map(|(_, edge, _)| edge.as_str()).collect();
        assert_eq!(edges, ["Knows", "Knows", "Mentors", "Mentors"], "{order:?}");
        view.read_with(cx, |view, _| {
            assert!(!view.continues_pair(order[0].0) && view.continues_pair(order[1].0));
        });
        assert!(
            cx.debug_bounds(Box::leak(
                format!("page-group-title-{}", order[0].0).into_boxed_str()
            ))
            .is_some()
        );
        assert!(
            cx.debug_bounds(Box::leak(
                format!("page-group-title-{}", order[1].0).into_boxed_str()
            ))
            .is_none(),
            "the second direction leaves the type's name to the first"
        );
    }

    /// A group's "+" shows only under the pointer, and while its search is open.
    #[gpui::test]
    async fn a_groups_add_shows_only_under_the_pointer_or_while_adding(cx: &mut TestAppContext) {
        let schema = format!("{PEOPLE}edge Knows: Person -> Person\n");
        let (view, cx) = open(session(&schema, cx), true, cx);
        view.read_with(cx, |view, _| {
            assert!((0..view.groups.len()).all(|index| !view.add_shows_at_rest(index)));
        });
        view.update_in(cx, |view, window, cx| view.start_add(0, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.add_shows_at_rest(0), "its search is open");
            assert!(!view.add_shows_at_rest(1));
        });
    }

    /// While the node is read, the page draws quiet bars where its title and values will be,
    /// and its meta line names only the type: never the raw id as a title, nor a lone "#".
    #[gpui::test]
    async fn a_page_being_read_shows_bars_not_its_id(cx: &mut TestAppContext) {
        let (view, cx) = open(session(PEOPLE, cx), true, cx);
        view.update(cx, |view, cx| {
            view.values = Loadable::Loading;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("page-reading").is_some());
        assert!(cx.debug_bounds("page-title").is_none());
        view.read_with(cx, |view, _| assert_eq!(view.label(), None));
    }

    /// A Blob of bytes shows as a card with a line of its first bytes; Show bytes reveals the
    /// dump, and the inspector's peek keeps its dump.
    #[gpui::test]
    async fn a_blob_is_a_card_whose_bytes_show_on_asking(cx: &mut TestAppContext) {
        const DOCS: &str = "node Person {\n  slug: String @key\n  name: String\n  doc: Blob?\n}\n";
        let (session, blobs) = crate::test_support::session_with_blobs(DOCS, cx);
        let bytes: Vec<u8> = (0..=255).collect();
        blobs.set("Person/p1/doc", &bytes);
        let (view, cx) = open(session, true, cx);
        assert!(cx.debug_bounds("page-blob-doc").is_some(), "a card");
        assert!(
            cx.debug_bounds("page-blob-bytes-line").is_some(),
            "a line of bytes"
        );
        view.read_with(cx, |view, _| {
            assert!(matches!(
                view.blobs[0].preview,
                Loadable::Loaded(Preview::Bytes { .. })
            ));
            assert!(view.bytes_shown.is_empty(), "no dump by default");
        });
        let show = cx.debug_bounds("page-blob-doc").unwrap();
        view.update(cx, |view, cx| {
            view.bytes_shown.insert("doc".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("page-blob-bytes-line").is_none(),
            "the dump instead"
        );
        let shown = cx.debug_bounds("page-blob-doc").unwrap();
        assert!(
            shown.size.height > show.size.height,
            "the dump makes it taller"
        );
    }

    #[test]
    fn a_card_shows_a_texts_first_lines_and_a_line_of_bytes() {
        let (shown, more) = first_lines("a\nb\nc", 2);
        assert_eq!((shown.as_str(), more), ("a\nb", true));
        assert_eq!(first_lines("a\nb", 2), ("a\nb".to_owned(), false));
        assert_eq!(bytes_line(&[0, 1, 0xab]), "00 01 ab");
        assert_eq!(bytes_line(&[0; 9]), "00 00 00 00 00 00 00 00 …");
    }
}
