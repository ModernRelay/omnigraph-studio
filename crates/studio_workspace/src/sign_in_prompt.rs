//! What a graph's tab shows while its server asks for a sign-in nobody holds (spec 016, What the
//! person sees): the server refuses every read until then, so the tab says where the sign-in is
//! and offers it, rather than each refused read's 401.

use gpui::{AnyElement, App, Entity};
use graph_store::{Connection, PromptAction};
use ui::prelude::*;

/// The sign-in `connection`'s server asks for, drawn where a tab's content would be: who asks,
/// where to sign in, and Sign in, or Cancel while the browser is open. `None` while the server
/// asks none, or a sign-in is held.
pub fn sign_in_prompt(connection: &Entity<Connection>, cx: &App) -> Option<AnyElement> {
    let prompt = connection.read(cx).sign_in_prompt()?;
    let title = match prompt.offers {
        Some(PromptAction::Cancel) => format!("Signing in to {}", prompt.server),
        _ => format!("Sign in to {}", prompt.server),
    };
    let connection = connection.clone();
    let action = prompt.offers.map(move |offers| match offers {
        PromptAction::SignIn => studio_ui::ds::button("sign-in", "Sign in")
            .style(ButtonStyle::Filled)
            .on_click(move |_, _, cx| connection.update(cx, |c, cx| c.sign_in(cx))),
        PromptAction::Cancel => studio_ui::ds::button("cancel-sign-in", "Cancel")
            .style(ButtonStyle::Outlined)
            .on_click(move |_, _, cx| connection.update(cx, |c, cx| c.cancel_sign_in(cx))),
    });
    Some(
        studio_ui::ds::empty_state(IconName::UserCheck, title)
            .debug_selector(|| "sign-in-prompt".into())
            .child(studio_ui::ds::state_detail(prompt.says))
            .children(action)
            .into_any_element(),
    )
}
