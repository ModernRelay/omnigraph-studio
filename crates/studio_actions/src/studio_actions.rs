//! Every action Studio defines. Doc comments on actions become palette descriptions.
//!
//! Layer 0 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Each action gets a default binding in `assets/keymaps/` and appears in the command palette
//! wherever it is available.

pub mod workspace {
    use gpui::actions;

    actions!(
        workspace,
        [
            /// Open the command palette.
            ToggleCommandPalette,
            /// Open Studio's settings.
            OpenSettings,
            /// Switch the active tab to another branch.
            SwitchBranch,
            /// Open a new window.
            NewWindow,
            /// Show the keyboard shortcuts.
            ShowKeyBindings,
            /// Go back to the previous location.
            GoBack,
            /// Go forward to the next location.
            GoForward,
            /// Quit Studio.
            Quit,
        ]
    );
}
