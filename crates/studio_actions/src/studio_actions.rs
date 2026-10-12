//! Every action Studio defines. Doc comments on actions become palette descriptions.
//!
//! Layer 0 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Each action gets a default binding in `assets/keymaps/` and appears in the command palette
//! wherever it is available.

pub mod workspace {
    use gpui::actions;

    actions!(
        studio,
        [
            /// Open the command palette.
            ToggleCommandPalette,
            /// Go to a type, or find a node by its id or label.
            GoTo,
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
            /// Close the active tab.
            CloseActiveItem,
            /// Activate the next tab.
            ActivateNextItem,
            /// Activate the previous tab.
            ActivatePreviousItem,
            /// Show or hide the sidebar.
            ToggleSidebar,
            /// Show or hide the inspector.
            ToggleInspector,
            /// Show or hide the activity log.
            ToggleActivity,
            /// Connect to a server or open a graph.
            OpenGraph,
            /// Reload the schema, branches and counts of the open graph.
            Refresh,
        ]
    );

    /// Activate the tab at a position, counted from zero.
    #[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
    #[action(namespace = studio)]
    pub struct ActivateItem {
        pub index: usize,
    }

    gpui::actions!(
        studio,
        [
            /// Activate the last tab.
            ActivateLastItem,
            /// Open the design system gallery: every token and component in the current window
            /// style.
            OpenDesignSystem,
        ]
    );
}

pub mod graph {
    use gpui::{Action, actions};
    use schemars::JsonSchema;
    use serde::Deserialize;

    actions!(
        studio_graph,
        [
            /// Open the query editor.
            NewQuery,
            /// Open the branch review against main.
            ReviewBranch,
            /// Open the commit history.
            OpenHistory,
            /// Open the graph canvas on the schema.
            OpenCanvas,
            /// Open the inspector for the graph's storage.
            OpenInspector,
            /// Create a branch from the current branch.
            CreateBranch,
            /// Delete the current branch.
            DeleteBranch,
            /// Merge the current branch into main.
            MergeBranch,
        ]
    );

    /// Open a node or edge type's table.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenType {
        pub type_name: String,
    }

    /// Open one node in the inspector and the relationship tree.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenNode {
        pub type_name: String,
        pub id: String,
    }

    /// Show one node on the graph canvas.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct ShowNodeOnCanvas {
        pub type_name: String,
        pub id: String,
    }

    /// Open one commit's changes.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenCommit {
        pub commit_id: String,
    }
}

pub mod table {
    use gpui::actions;

    actions!(
        studio_table,
        [
            /// Move the cell cursor up.
            MoveUp,
            /// Move the cell cursor down.
            MoveDown,
            /// Move the cell cursor left.
            MoveLeft,
            /// Move the cell cursor right.
            MoveRight,
            /// Edit the selected cell.
            EditCell,
            /// Save the cell being edited.
            ConfirmEdit,
            /// Cancel the edit, or step back one level.
            Cancel,
            /// Show the selected row in the inspector.
            PeekRow,
            /// Reload the table from the branch head.
            RefreshTable,
            /// Add a row.
            InsertRow,
            /// Delete the selected row.
            DeleteRow,
            /// Copy the selected cell's value.
            CopyCell,
            /// Add a filter on the selected column.
            AddFilter,
            /// Show only rows whose selected column equals the selected cell's value.
            FilterBySelection,
            /// Remove every filter from the table.
            ClearFilters,
        ]
    );
}

pub mod tree {
    use gpui::actions;

    actions!(
        studio_tree,
        [
            /// Select the next row.
            SelectNext,
            /// Select the previous row.
            SelectPrevious,
            /// Expand the selected row, or step into its first child.
            Expand,
            /// Collapse the selected row, or go to its parent.
            Collapse,
            /// Expand every relationship of the selected node.
            ExpandAll,
            /// Collapse the selected row's subtree.
            CollapseAll,
            /// Open the selected node in the inspector.
            OpenSelected,
            /// Make the selected node the tree's root.
            RerootHere,
            /// Show the selected node on the canvas.
            ShowOnCanvas,
            /// Clear the text typed to filter the tree.
            ClearFilter,
        ]
    );
}

pub mod query {
    use gpui::actions;

    actions!(
        studio_query,
        [
            /// Run the query under the cursor.
            Run,
            /// Show the plan of the query under the cursor.
            Explain,
            /// Copy the selected result cell's value.
            CopyCell,
            /// Copy the selected result row as JSON.
            CopyRow,
        ]
    );
}

pub mod review {
    use gpui::actions;

    actions!(
        studio_review,
        [
            /// Go to the next change.
            NextChange,
            /// Go to the previous change.
            PreviousChange,
            /// Mark the current change reviewed and go to the next.
            AcceptAndNext,
            /// Revert the current change and go to the next.
            RevertAndNext,
            /// Merge the reviewed branch.
            Merge,
        ]
    );
}

pub mod canvas {
    use gpui::actions;

    actions!(
        studio_canvas,
        [
            /// Fit the graph to the window.
            FitToWindow,
            /// Zoom in.
            ZoomIn,
            /// Zoom out.
            ZoomOut,
            /// Expand the selected node.
            ExpandNode,
            /// Collapse the selected node.
            CollapseNode,
            /// Select the nearest node to the left.
            MoveLeft,
            /// Select the nearest node to the right.
            MoveRight,
            /// Select the nearest node above.
            MoveUp,
            /// Select the nearest node below.
            MoveDown,
        ]
    );
}

pub mod agent {
    use gpui::{Action, actions};
    use schemars::JsonSchema;
    use serde::Deserialize;

    actions!(
        studio_agent,
        [
            /// Open the agent panel.
            OpenAgent,
            /// Send the message in the agent composer.
            Send,
            /// Stop the agent's current turn.
            Stop,
            /// Allow the change or request the agent is waiting on.
            Allow,
            /// Allow this and every later change on the agent's branch in this session.
            AllowAlways,
            /// Reject the change or request the agent is waiting on.
            Reject,
            /// Start the agent again after it stopped.
            Restart,
        ]
    );

    /// Ask the agent a question about the graph.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_agent)]
    pub struct Ask {
        pub text: String,
    }
}
