//! The design system gallery (spec 002, "Visual language"): every token and component of
//! `studio_ui::ds` in the current window style. Design changes are rendered here first, flat
//! and glass, before they reach a surface.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, Hsla, Render, SharedString, Window,
};
use studio_actions::agent::{Allow, AllowAlways, OpenAgent, Reject};
use studio_ui::ds::{
    self, ActivityRow, Dotted as _, SidebarRow, radii, size, space, surface, text,
};
use studio_workspace::{Item, ItemEvent, SavedItem};
use ui::Severity;
use ui::prelude::*;

pub struct DesignGallery {
    focus_handle: FocusHandle,
}

impl DesignGallery {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl EventEmitter<ItemEvent> for DesignGallery {}

impl Focusable for DesignGallery {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for DesignGallery {
    fn tab_title(&self, _cx: &App) -> SharedString {
        "Design system".into()
    }

    fn tab_icon(&self, _cx: &App) -> Option<IconName> {
        Some(IconName::Sparkle)
    }

    fn dedupe_key(&self, _cx: &App) -> Option<SharedString> {
        Some("design-system".into())
    }

    fn serialize(&self, _cx: &App) -> Option<SavedItem> {
        Some(SavedItem {
            kind: "design-system".into(),
            data: serde_json::Value::Null,
        })
    }
}

/// A gallery section: a quiet heading, then its specimens.
fn section(title: &'static str, body: impl IntoElement) -> Div {
    v_flex()
        .gap(px(space::LG))
        .child(text::meta(title))
        .child(body)
}

impl Render for DesignGallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let glass = ds::is_glass(cx);
        let focus = self.focus_handle.clone();

        let scale = h_flex().gap(px(space::XL)).items_end().children(
            [
                ("XS", space::XS),
                ("SM", space::SM),
                ("MD", space::MD),
                ("LG", space::LG),
                ("XL", space::XL),
                ("XXL", space::XXL),
                ("XXXL", space::XXXL),
            ]
            .into_iter()
            .map(|(name, value)| {
                v_flex()
                    .gap(px(space::XS))
                    .items_center()
                    .child(
                        div()
                            .size(px(value))
                            .rounded(px(radii(cx).xs))
                            // ds-exempt: a specimen of a spacing step, not interface
                            .bg(surface::accent(cx).opacity(0.35)),
                    )
                    .child(text::meta(format!("{name} {value}")))
            }),
        );
        let corners = h_flex().gap(px(space::XL)).children(
            [
                ("XS", radii(cx).xs),
                ("SM", radii(cx).sm),
                ("MD", radii(cx).md),
                ("LG", radii(cx).lg),
                ("MODAL", radii(cx).modal),
            ]
            .into_iter()
            .map(|(name, value)| {
                v_flex()
                    .gap(px(space::XS))
                    .items_center()
                    .child(
                        div()
                            .size(px(size::STRIP))
                            .rounded(px(value))
                            .border_1()
                            .border_color(surface::control_border(cx))
                            .bg(surface::document(cx)),
                    )
                    .child(text::meta(format!("{name} {value}")))
            }),
        );
        let swatch = |name: &'static str, fill: Hsla, shadow: bool| {
            v_flex()
                .gap(px(space::XS))
                .items_center()
                .child(
                    div()
                        .size(px(2. * size::CONTROL))
                        .rounded(px(radii(cx).md))
                        .bg(fill)
                        .border_1()
                        .border_color(surface::rule(cx))
                        .when(shadow, |d| d.shadow(ds::elevation::raised(cx))),
                )
                .child(text::meta(name))
        };
        let surfaces = h_flex()
            .gap(px(space::XL))
            .child(swatch("chrome", surface::chrome(cx), false))
            .child(swatch("document", surface::document(cx), false))
            .child(swatch("sunken", surface::sunken(cx), false))
            .child(swatch("hover", surface::hover(cx), false))
            .child(swatch(
                "selected · chrome",
                surface::selected_chrome(cx),
                false,
            ))
            .child(swatch("well", surface::well(cx), false))
            .child(swatch(
                "selected · document",
                surface::selected_document(cx),
                false,
            ))
            .child(swatch("rule", surface::rule(cx), false));
        let texts = v_flex()
            .gap(px(space::SM))
            .child(text::title("Title · Radia Knuth"))
            .child(text::body("Body · Chief Scientist at Allen Labs"))
            .child(text::muted("Muted · String? · 200 rows"))
            .child(text::meta("Meta · 2 h ago"))
            .child(text::code("Code · hb1.01M4R…EP12CTBE", cx));

        // The left dock: a header strip of icon tabs, the active panel, then the connection.
        let strip = |height: f32| {
            h_flex()
                .h(px(height))
                .flex_none()
                .px(px(space::SM))
                .gap(px(space::XXS))
                .items_center()
        };
        let sidebar = v_flex()
            .w(px(size::SIDEBAR_WIDTH))
            .rounded(px(radii(cx).lg))
            .relative()
            .overflow_hidden()
            .map(|d| {
                if glass {
                    d.bg(surface::page(cx)).child(ds::wash())
                } else {
                    d.bg(surface::chrome(cx))
                        .border_1()
                        .border_color(surface::rule(cx))
                }
            })
            .child(
                strip(size::HEADER_STRIP)
                    .border_b_1()
                    .border_color(surface::rule(cx))
                    .children(
                        [
                            (IconName::Table, "Types"),
                            (IconName::ListTree, "Explore"),
                            (IconName::GitBranch, "Branches"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (icon, title))| {
                            studio_ui::ds::icon_button(("g-dock-tab", index), icon)
                                .toggle_state(index == 0)
                                .tooltip(ui::Tooltip::text(title))
                        }),
                    ),
            )
            .child(
                v_flex()
                    .pb(px(space::LG))
                    .child(ds::section_label("Types"))
                    .child(
                        SidebarRow::new("g-company", "Company")
                            .start(studio_ui::type_swatch("Company", cx))
                            .end(ds::sidebar_count("25")),
                    )
                    .child(
                        SidebarRow::new("g-person", "Person")
                            .start(studio_ui::type_swatch("Person", cx))
                            .end(ds::sidebar_count("200"))
                            .selected(true),
                    )
                    .child(
                        SidebarRow::new("g-knows", "Knows")
                            .start(studio_ui::kind_swatch("Knows", true, cx))
                            .end(ds::sidebar_count("412"))
                            .muted(true),
                    )
                    .child(ds::sidebar_placeholder(
                        "g-explore",
                        IconName::ListTree,
                        "No node",
                        "An empty panel: one dim row, how to fill it in a tooltip",
                    )),
            )
            .child(
                strip(size::STRIP)
                    .px(px(space::MD))
                    .gap(px(space::SM))
                    .border_t_1()
                    .border_color(surface::rule(cx))
                    .child(ui::Indicator::dot().color(Color::Success))
                    .child(text::body("environment"))
                    .child(text::muted("Ready"))
                    .child(div().flex_1())
                    .child(studio_ui::ds::icon::meta(IconName::ChevronUpDown)),
            );
        let columns = [
            (IconName::CaseSensitive, "slug", "String · key"),
            (IconName::CaseSensitive, "name", "String"),
            (IconName::CaseSensitive, "title", "String?"),
        ];
        let rows = [
            ("person-000000", "Radia Knuth", "Chief Scientist"),
            ("person-000001", "Donald Dijkstra", "Researcher"),
            ("person-000002", "Grace Allen", "—"),
        ];
        // A grid card, as an agent's record cards draw one; the type table draws the same cells
        // with no card, flush in its document under its toolbar: header cells of the kind's
        // icon and a muted name (the first ordering the rows, in ink, its menu shown as under
        // the pointer), then data cells, every cell's rule on its right but the last's; the
        // selected row on its fill and its Peek shown.
        let last = columns.len() - 1;
        let ruled = |cell: Div, at: usize| {
            let cell = cell.flex_1().min_w_0();
            if at < last {
                ds::cell_rule(cell, cx)
            } else {
                cell
            }
        };
        let table = ds::card(cx)
            .child(ds::card_title(
                ds::icon_tile(IconName::Table, cx),
                "3 rows",
                Some("main".into()),
            ))
            .child(
                ds::header_row(cx).children(columns.iter().enumerate().map(
                    |(at, (icon, name, detail))| {
                        ruled(
                            ds::header_cell(Some(*icon), *name, at == 0)
                                .child(text::meta(*detail).color(Color::Disabled))
                                .when(at == 0, |cell| {
                                    cell.child(div().flex_1()).child(ds::row_icon_button(
                                        "g-column-menu",
                                        IconName::Ellipsis,
                                    ))
                                }),
                            at,
                        )
                    },
                )),
            )
            .children(rows.iter().enumerate().map(|(index, values)| {
                let values = [values.0, values.1, values.2];
                ds::grid_row(cx)
                    .when(index == 0, |row| row.bg(surface::selected_document(cx)))
                    .children(values.iter().enumerate().map(|(at, value)| {
                        ruled(
                            ds::data_cell().child(text::body(*value)).when(
                                index == 0 && at == 0,
                                |cell| {
                                    cell.child(div().flex_1())
                                        .child(ds::row_icon_button("g-peek", IconName::Eye))
                                },
                            ),
                            at,
                        )
                    }))
            }));

        // The command palette's parts: a tray of inset panels, a group's label above its own,
        // rows led by neutral tiles (a type's colour as its dot) with their keys as caps, the
        // highlighted row, and the footer's raised key hints.
        let palette_row = |id: &'static str, tile: Div, label: &'static str, end: AnyElement| {
            ds::MenuRow::new(id)
                .height(size::PALETTE_ROW)
                .radius(radii(cx).inset_row())
                .inset((size::PALETTE_ROW - size::ICON_TILE) / 2.)
                .start_slot(tile)
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(text::plain(label))
                        .child(end),
                )
        };
        let palette = v_flex()
            .w(px(size::MODAL_WIDTH))
            .p(px(space::XS))
            .gap(px(space::XS))
            .rounded(px(radii(cx).modal))
            .bg(surface::tray(cx))
            .border_1()
            .border_color(surface::floating_border(cx))
            .child(
                ds::inset_panel(cx).child(
                    h_flex()
                        .h(px(size::CONTROL))
                        .px(px(space::SM))
                        .gap(px(space::MD))
                        .child(ds::icon::lead(IconName::MagnifyingGlass))
                        .child(text::muted(
                            "Search commands, types, nodes, queries and branches…",
                        )),
                ),
            )
            .child(
                ds::segmented(cx)
                    .child(ds::segment("g-tab-all", "All", true, cx))
                    .child(ds::segment("g-tab-types", "Types", false, cx))
                    .child(ds::segment("g-tab-commands", "Commands", false, cx)),
            )
            .child(ds::inset_label("Recent", size::PALETTE_ROW))
            .child(
                ds::inset_panel(cx)
                    .child(palette_row(
                        "g-palette-agent",
                        ds::icon_tile(IconName::ZedAssistant, cx),
                        "Agent: open agent",
                        ds::KeyCaps::for_action_in(&OpenAgent, &focus).into_any_element(),
                    ))
                    .child(
                        palette_row(
                            "g-palette-person",
                            ds::tile(cx).child(ds::type_dot("Person")),
                            "Person",
                            text::muted("node · 200 rows").into_any_element(),
                        )
                        .toggle_state(true),
                    ),
            )
            .child(
                h_flex()
                    .px(px(space::SM))
                    .py(px(space::SM))
                    .child(
                        div()
                            .flex_1()
                            .child(text::meta("Open this node type's table.")),
                    )
                    .child(
                        ds::key_hints()
                            .child(
                                ds::key_hint(&menu::SelectPrevious, "Navigate", &focus, cx)
                                    .and(&menu::SelectNext, &focus)
                                    .raised(),
                            )
                            .child(ds::key_hint(&menu::Confirm, "Select", &focus, cx).raised())
                            .child(ds::key_hint(&menu::Cancel, "Close", &focus, cx).raised()),
                    ),
            );

        let card = ds::card(cx)
            .child(
                ds::card_header(cx)
                    .child(studio_ui::ds::icon::meta(IconName::Pencil))
                    .child(text::body("Retitle person-000000"))
                    .child(div().flex_1())
                    .child(ds::meta(IconName::GitBranch, "agent/20261012-095049"))
                    .child(studio_ui::ds::tag("new", cx)),
            )
            .child(
                ds::card_body()
                    .child(ds::code_block(
                        "query retitle($id: String, $title: String) {\n  update Person set { title: $title } where @id = $id\n}",
                        cx,
                    ))
                    .child(
                        ds::fields()
                            .child(ds::field("$id", text::code("person-000000", cx)))
                            .child(ds::field("$title", text::code("Retitled by the agent", cx))),
                    ),
            )
            .child(
                ds::card_footer(cx)
                    .child(
                        studio_ui::ds::button("g-allow", "Allow")
                            .start_icon(
                                Icon::new(IconName::Check),
                            )
                            .key_binding(ui::KeyBinding::for_action_in(&Allow, &focus, cx)),
                    )
                    .child(
                        studio_ui::ds::button("g-always", "Always on this branch")
                            .start_icon(
                                Icon::new(IconName::CheckDouble),
                            )
                            .key_binding(ui::KeyBinding::for_action_in(&AllowAlways, &focus, cx)),
                    )
                    .child(div().flex_1())
                    .child(
                        studio_ui::ds::button("g-reject", "Reject")
                            .start_icon(
                                Icon::new(IconName::Close),
                            )
                            .key_binding(ui::KeyBinding::for_action_in(&Reject, &focus, cx)),
                    ),
            );

        let feed_rows = vec![
            ActivityRow::new(
                "f1",
                IconName::ZedAssistant,
                "Studio agent",
                "failed to change",
                "3m ago",
            )
            .status(ds::Status::Failed)
            .object("person-000014")
            .place("on", IconName::GitBranch, "agent/20261012-095049"),
            ActivityRow::new(
                "f2",
                IconName::ZedAssistant,
                "Studio agent",
                "is proposing",
                "31m ago",
            )
            .status(ds::Status::Running)
            .object("Retitle person-000000"),
            ActivityRow::new("f3", IconName::Person, "act-dev", "committed", "1h ago")
                .object("Agent change")
                .place("to", IconName::GitBranch, "agent/20261012-084228"),
            ActivityRow::new("f4", IconName::Person, "act-dev", "created", "2h ago")
                .object("agent/20261012-084228")
                .place("from", IconName::GitBranch, "main"),
            ActivityRow::new("f5", IconName::Person, "act-dev", "merged", "3h ago")
                .object("review")
                .place("into", IconName::GitBranch, "main"),
        ];
        let feed = if glass { ds::sheet(cx) } else { ds::card(cx) }.children(feed_rows);

        let empty = div()
            .h(px(size::PREVIEW_MAX_HEIGHT))
            .rounded(px(radii(cx).md))
            .border_1()
            .border_color(surface::rule(cx))
            .child(
                ds::empty_state(IconName::Sparkle, "No tab open").child(
                    div().w(px(size::STATE_ACTIONS)).child(
                        studio_ui::ds::button("g-search", "Search")
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .start_icon(Icon::new(IconName::MagnifyingGlass)),
                    ),
                ),
            );

        // What a view shows instead of its content: reading it, and failing to.
        let state_frame = || {
            div()
                .h(px(size::PREVIEW_MAX_HEIGHT))
                .rounded(px(radii(cx).md))
                .border_1()
                .border_color(surface::rule(cx))
        };
        let states = h_flex()
            .gap(px(space::LG))
            .child(
                state_frame()
                    .w(px(size::STATE_DETAIL_WIDTH))
                    .child(ds::loading_state("Reading Person…")),
            )
            .child(
                state_frame().w(px(size::STATE_DETAIL_WIDTH)).child(
                    ds::error_state(
                        "Person could not be read",
                        "Could not reach the server: the connection was refused",
                    )
                    .child(ds::button("g-retry", "Refresh").style(ButtonStyle::Outlined)),
                ),
            );

        // The agent's composer, after the reference chat's: a notice strip attached to the top
        // of a composer card, the message then its controls, and what the agent reaches under
        // it; the reply's actions above, and the Conversations panel's launcher.
        let composer = v_flex()
            .gap(px(space::SM))
            .child(
                h_flex()
                    .gap(px(space::SM))
                    .items_center()
                    .child(ds::row_icon_button("g-copy-reply", IconName::Copy))
                    .child(text::meta("3 tool calls")),
            )
            .child(ds::attach_notice(
                ds::notice_strip(cx).child(
                    ds::notice_line()
                        .child(ds::meta(IconName::GitBranch, "agent/20261021-122659"))
                        .child(ds::meta(IconName::Pencil, "1"))
                        .child(div().flex_1())
                        .child(
                            ds::toolbar_button("g-review", "Review")
                                .start_icon(Icon::new(IconName::Eye)),
                        ),
                ),
                ds::composer_card(cx)
                    .child(text::muted("Ask about the graph, or ask for a change…"))
                    .child(
                        ds::composer_controls()
                            .child(
                                ds::pill("g-agent", cx)
                                    .child(ui::Indicator::dot().color(Color::Success))
                                    .child(text::body("Claude Agent"))
                                    .child(studio_ui::ds::icon::meta(IconName::ChevronDown)),
                            )
                            .child(ds::composer_button("g-send", IconName::ArrowUp, true, cx)),
                    ),
                cx,
            ))
            .child(div().w_full().flex().justify_center().child(ds::meta_row([
                text::meta("Reads knowledge at main").into_any_element(),
                text::meta("writes only to a branch of its own").into_any_element(),
            ])))
            .child(
                ds::button("g-ask", "Ask the agent")
                    .style(ButtonStyle::Outlined)
                    .full_width()
                    .start_icon(Icon::new(IconName::ZedAssistant))
                    .key_caps(ds::KeyCaps::for_action_in(&OpenAgent, &focus)),
            );

        let settings_rows = ds::card(cx)
            .px(px(space::XL))
            .child(
                ds::setting_row(
                    "g-setting-style",
                    "Window style",
                    "Columns on solid panels, or the document as a sheet over a faint wash.",
                    ds::segmented(cx)
                        .child(ds::segment("g-flat", "Flat", true, cx))
                        .child(ds::segment("g-glass", "Glass", false, cx)),
                    None,
                )
                .border_b_1()
                .dotted(cx),
            )
            .child(ds::setting_row(
                "g-setting-size",
                "Table page size",
                "Rows read per page in type tables.",
                text::body("100"),
                Some(Box::new(
                    |_: &gpui::ClickEvent, _: &mut Window, _: &mut App| {},
                )),
            ));

        // Buttons, after Tower's (spec 002, "Buttons"): every variant at a control's height,
        // each size by its role (a control, a toolbar, a row, a tab), icon buttons, and the
        // toggled and disabled states.
        let variants = [
            ("Primary", ds::Variant::Primary),
            ("Secondary", ds::Variant::Secondary),
            ("Outline", ds::Variant::Outline),
            ("Ghost", ds::Variant::Ghost),
            ("Destructive", ds::Variant::Destructive),
            ("Glass", ds::Variant::Glass),
        ];
        let buttons = v_flex()
            .gap(px(space::LG))
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .children(
                        variants
                            .into_iter()
                            .enumerate()
                            .map(|(index, (label, variant))| {
                                ds::Button::new(("g-variant", index), label).variant(variant)
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .items_center()
                    .child(ds::button("g-size-control", "Control").variant(ds::Variant::Outline))
                    .child(
                        ds::toolbar_button("g-size-toolbar", "Toolbar")
                            .variant(ds::Variant::Outline),
                    )
                    .child(ds::row_button("g-size-row", "Row").variant(ds::Variant::Outline))
                    .child(
                        ds::icon_button("g-icon-control", IconName::Plus)
                            .variant(ds::Variant::Outline),
                    )
                    .child(
                        ds::toolbar_icon_button("g-icon-toolbar", IconName::Plus)
                            .variant(ds::Variant::Outline),
                    )
                    .child(
                        ds::row_icon_button("g-icon-row", IconName::Plus)
                            .variant(ds::Variant::Outline),
                    )
                    .child(
                        ds::inline_icon_button("g-icon-inline", IconName::Plus)
                            .variant(ds::Variant::Outline),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(space::MD))
                    .items_center()
                    .child(ds::Button::new("g-toggled", "Toggled").toggle_state(true))
                    .child(
                        ds::Button::new("g-disabled", "Disabled")
                            .variant(ds::Variant::Primary)
                            .disabled(true),
                    )
                    .child(
                        ds::Button::new("g-icon-label", "Row")
                            .start_icon(Icon::new(IconName::Plus)),
                    )
                    .child(ds::icon_button("g-icon-ghost", IconName::Ellipsis))
                    .child(ds::icon_button("g-icon-toggled", IconName::Filter).toggle_state(true)),
            );

        // A modal's card (spec 002, "Modals"): header, body and footer.
        let modal = ds::modal(cx)
            .child(
                ds::ModalHeader::new("Delete Acme")
                    .description(SharedString::from(
                        "Removes it and its relationships from main. Also deletes 1 Person that \
                         cannot exist without it. You can undo this right after.",
                    ))
                    .meta(ds::write_target("main", cx))
                    .on_close(|_, _, _| {}),
            )
            .child(
                ds::modal_body().child(
                    ds::row_list(cx).child(
                        h_flex()
                            .h(px(size::SIDEBAR_ROW))
                            .px(px(space::MD))
                            .gap(px(space::MD))
                            .child(studio_ui::type_swatch("Person", cx))
                            .child(studio_ui::ds::text::body("Ada Lovelace"))
                            .child(div().flex_1())
                            .child(studio_ui::ds::text::meta("needs WorksAt")),
                    ),
                ),
            )
            .child(
                ds::modal_footer(cx)
                    .child(ds::cancel_button("g-modal-cancel"))
                    .child(
                        ds::action_button("g-modal-delete", "Delete")
                            .variant(ds::Variant::Destructive)
                            .key_binding(ui::KeyBinding::for_action_in(&menu::Confirm, &focus, cx)),
                    ),
            );

        // A context menu (spec 002, "Menus"): a group header, rows, the highlighted row, a
        // checked one, a disabled one, and a separator.
        let menu = ds::menu_surface(v_flex().w(px(size::STATE_ACTIONS)).p(px(space::XS)), cx)
            .child(ds::menu_header("Row".into()))
            .child(
                ds::MenuRow::new("g-menu-peek")
                    .toggle_state(true)
                    .child(text::plain("Peek")),
            )
            .child(ds::MenuRow::new("g-menu-edit").child(text::plain("Edit cell")))
            .child(
                ds::MenuRow::new("g-menu-wrap")
                    .start_slot(studio_ui::ds::icon::lead(IconName::Check).color(Color::Accent))
                    .child(text::plain("Wrap long text")),
            )
            .child(
                ds::MenuRow::new("g-menu-disabled")
                    .disabled(true)
                    .child(text::plain("Paste").color(Color::Disabled)),
            )
            .child(ds::menu_separator(cx))
            .child(ds::MenuRow::new("g-menu-delete").child(text::plain("Delete row")));

        v_flex()
            .id("design-gallery")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .p(px(space::PAGE))
            .gap(px(space::XXXL))
            .child(
                h_flex()
                    .gap(px(space::XXXL))
                    .items_start()
                    .child(section("Spacing", scale))
                    .child(section("Radii", corners)),
            )
            .child(
                h_flex()
                    .gap(px(space::XXXL))
                    .items_start()
                    .child(section("Surfaces", surfaces))
                    .child(section("Text", texts)),
            )
            .child(
                h_flex()
                    .gap(px(space::XXXL))
                    .items_start()
                    .child(section("Left dock", sidebar))
                    .child(section(
                        "Grid card · header and data cells",
                        div().w(px(size::READING_WIDTH)).child(table),
                    )),
            )
            .child(
                h_flex()
                    .gap(px(space::XXXL))
                    .items_start()
                    .child(section("Card", div().w(px(size::MODAL_WIDTH)).child(card)))
                    .child(section(
                        "Empty state",
                        div().w(px(size::STATE_DETAIL_WIDTH)).child(empty),
                    )),
            )
            .child(section(
                "Command palette · tray, inset groups, tiles, key caps",
                palette,
            ))
            .child(section("Buttons", buttons))
            .child(
                h_flex()
                    .gap(px(space::XXXL))
                    .items_start()
                    .child(section("Modal", modal))
                    .child(section("Menu", menu)),
            )
            .child(section("Loading and error states", states))
            .child(section(
                "Settings rows · segmented control",
                div().w(px(size::READING_WIDTH)).child(settings_rows),
            ))
            .child(section(
                "Agent composer · notice strip, reply actions, launcher",
                div().w(px(size::MODAL_WIDTH)).child(composer),
            ))
            .child(section(
                "Activity",
                div().w(px(size::READING_WIDTH)).child(feed),
            ))
            .child(section(
                "Callout",
                div().w(px(size::MODAL_WIDTH)).child(
                    studio_ui::ds::callout(Severity::Error, "The agent stopped")
                        .description("Errors are the one place for a sentence."),
                ),
            ))
    }
}
