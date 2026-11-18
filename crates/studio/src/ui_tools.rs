//! The binary's wiring of Studio's UI tools (spec 032): the one module of `studio` that names
//! `ui_tool_server`. It starts nothing itself, hands the agent panel its entries, and draws
//! the settings' rows that show the server's address. `script/check-layers.py` fails if
//! another module or crate names the tool crate, so taking the tools out of Studio is this
//! file, its two test modules and one dependency.

use gpui::{AnyElement, App, Window, px};
use studio_ui::ds::Tipped;
use studio_ui::ds::{self, space};
use ui::prelude::*;

/// Follows the settings and offers the agent panel its entries (`agent_panel::UiToolEndpoint`,
/// so that neither crate depends on the other). No server starts here: one runs from the first
/// conversation that asks for its entry, or while agents outside Studio are let in.
pub(crate) fn init(cx: &mut App) {
    ui_tool_server::init(cx);
    cx.set_global(agent_panel::UiToolEndpoint::new(|window, cx| {
        let entry = ui_tool_server::UiToolServer::entry_for(window, cx)?;
        Some(agent_panel::UiToolEntry::new(
            entry.url.clone(),
            entry.token.clone(),
            ui_tool_server::tools::is_tool,
            // The conversation's token stops working when its entry goes.
            move || drop(entry),
        ))
    }));
}

/// Asks, then replaces the token agents outside Studio use. Confirmed with ↵: nothing is lost,
/// and each agent takes the new token when its command is copied again.
pub(crate) fn replace_agent_token(window: &mut Window, cx: &mut App) {
    studio_workspace::confirm(
        window,
        cx,
        "Replace the agents' token?",
        "Every agent outside Studio that holds the current token is refused from now on. Copy \
         its command again to give it the new one.",
        "New token",
        |_, cx| ui_tool_server::UiToolServer::replace_token(cx),
    );
}

/// What the settings show under the outside-agent switch while it is on: the server's address,
/// the command that adds Studio to Claude Code and Codex's config entry, each with Copy. Both
/// read the token from its file, so neither shows it.
pub(crate) fn outside_agents_row(cx: &App) -> AnyElement {
    let Some(url) = ui_tool_server::UiToolServer::outside_url(cx) else {
        return div()
            .pb(px(space::MD))
            .child(ds::text::muted(
                "Studio's tool server is not running, so agents outside Studio cannot reach it. \
                 The log says why.",
            ))
            .into_any_element();
    };
    let entry =
        |id: &'static str, agent: &'static str, text: String, tip: &'static str| {
            let shown = text
                .lines()
                .find(|line| !line.starts_with('#'))
                .unwrap_or_default()
                .to_owned();
            h_flex()
                .w_full()
                .gap(px(space::SM))
                .child(
                    div()
                        .w(px(ds::size::FIELD_LABEL))
                        .flex_none()
                        .child(ds::text::meta(agent)),
                )
                .child(
                    div().flex_1().min_w_0().overflow_hidden().child(
                        ds::text::code(shown, cx)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
                )
                .child(ds::row_button(id, "Copy").tip(ds::tooltip(tip)).on_click(
                    move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()))
                    },
                ))
        };
    // The address in use, what did not go as asked (a port taken, no keychain), and New token.
    let address = h_flex()
        .w_full()
        .gap(px(space::SM))
        .child(
            div()
                .w(px(ds::size::FIELD_LABEL))
                .flex_none()
                .child(ds::text::meta("Address")),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(ds::text::code(url.clone(), cx).color(Color::Muted))
                .children(ui_tool_server::UiToolServer::note(cx).map(ds::text::meta)),
        )
        .child(
            ds::row_button("new-agent-token", "New token")
                .tip(ds::tooltip(
                    "Replace the token: agents holding the current one are refused until their \
                     command is copied again",
                ))
                .on_click(|_, window, cx| replace_agent_token(window, cx)),
        );
    v_flex()
        .pb(px(space::MD))
        .gap(px(space::XS))
        .child(address)
        .child(entry(
            "copy-claude-command",
            "Claude Code",
            ui_tool_server::claude_command(&url),
            "Copy the command that adds Studio to Claude Code. It reads the token from its \
             file, so Copy shows no token.",
        ))
        .child(
            div()
                .pl(px(ds::size::FIELD_LABEL + space::SM))
                .child(ds::text::meta(ui_tool_server::claude_token_note(
                    ui_tool_server::keeping(cx),
                ))),
        )
        .child(entry(
            "copy-codex-config",
            "Codex",
            ui_tool_server::codex_config(&url),
            "Copy the entry for Codex's config.toml, with an approval for each tool. Export \
             OMNIGRAPH_STUDIO_TOKEN as its first line says before starting Codex.",
        ))
        .into_any_element()
}
