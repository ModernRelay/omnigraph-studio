//! Every action Studio defines. Doc comments on actions become palette descriptions.
//!
//! Layer 0 (`docs/architecture.md`). Plan: `specs/002-app-shell/plan.md`.
//!
//! Each action appears in the command palette wherever it is available, and each one without
//! arguments has a default binding in `assets/keymaps/`, except the few that are rare or
//! destructive, as in Zed. The keymap test (`crates/studio/tests/keymaps.rs`) lists those.

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
            /// Show Studio's log file in the file manager.
            RevealLog,
            /// Show or hide GPUI's frame-time overlay, in a build with the `perf` feature.
            ToggleFrameOverlay,
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
            /// Show or hide the sidebar, the left dock.
            ToggleSidebar,
            /// Show or hide the inspector, the right dock.
            ToggleInspector,
            /// Show or hide the bottom panel: nodes, history and the activity log.
            ToggleActivity,
            /// Zoom the active pane over the document area, or end a zoom.
            ToggleZoom,
            /// Connect to a server or open a graph.
            OpenGraph,
            /// Reload the schema, branches and counts of the open graph.
            Refresh,
        ]
    );

    /// Open an `omnigraph-studio://` link: a graph, a node, or a query to read. A link never
    /// runs a query or adds a server.
    #[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
    #[action(namespace = studio)]
    pub struct OpenLink {
        pub url: String,
    }

    /// Show a panel in its dock, as its button in the status bar does: `types`, `explore`,
    /// `queries`, `branches`, `agent-conversations`, `inspector`, `nodes`, `history`,
    /// `activity`.
    #[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
    #[action(namespace = studio)]
    pub struct ActivatePanel {
        pub id: String,
    }

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
            /// Split the document area, with a copy of the active tab on the right.
            SplitRight,
            /// Split the document area, with a copy of the active tab below.
            SplitDown,
            /// Split the document area, with a copy of the active tab on the left.
            SplitLeft,
            /// Split the document area, with a copy of the active tab above.
            SplitUp,
            /// Move to the next pane of a split.
            ActivateNextPane,
            /// Move to the previous pane of a split.
            ActivatePreviousPane,
            /// Pin or unpin the active tab; pinned tabs come first and stay on "Close others".
            TogglePinTab,
            /// Keep the active preview tab, so the next preview opens beside it.
            KeepPreviewTab,
            /// Close every unpinned tab of the pane except the active one.
            CloseOtherItems,
            /// Close every unpinned tab of the pane.
            CloseAllItems,
            /// Show the next graph open in this window.
            ActivateNextGraph,
            /// Show the previous graph open in this window.
            ActivatePreviousGraph,
            /// Close the shown graph's tabs in this window; they come back when it is opened
            /// again.
            CloseGraph,
            /// Open the design system gallery: every token and component in the current window
            /// style.
            OpenDesignSystem,
        ]
    );
}

pub mod palette {
    use gpui::{Action, actions};
    use schemars::JsonSchema;
    use serde::Deserialize;

    actions!(
        studio_palette,
        [
            /// Hand the palette's text to the agent as a question.
            AskAgent,
        ]
    );

    /// Show one scope of the palette: 0 All, 1 Types, 2 Nodes, 3 Queries, 4 Branches,
    /// 5 Commands.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_palette)]
    pub struct ShowScope {
        pub index: usize,
    }
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
            /// Open the graph explorer: the canvas around the nodes being explored.
            OpenCanvas,
            /// Open the ontology: the graph's types and relationships as a map.
            OpenOntology,
            /// Open the inspector for the graph's storage.
            OpenInspector,
            /// Create a branch from the current branch.
            CreateBranch,
            /// Delete the current branch.
            DeleteBranch,
            /// Open the review of the current branch against main, to merge it there.
            MergeBranch,
        ]
    );

    /// Open a node or edge type's table.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenType {
        pub type_name: String,
        /// Read the table at this commit, read-only (spec 009); `None` reads the branch head.
        #[serde(default)]
        pub at_commit: Option<String>,
        /// Filter the table to this node id: the node itself, or with `via`, the nodes related
        /// to it through that relationship key (`in:WorksAt`) (spec 005).
        #[serde(default)]
        pub filter_id: Option<String>,
        #[serde(default)]
        pub via: Option<String>,
        /// Open as the pane's preview tab, which the next preview replaces (spec 002).
        #[serde(default)]
        pub preview: bool,
    }

    /// Open the schema's source, read-only; with a type, at its declaration (spec 007).
    #[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenSchema {
        #[serde(default)]
        pub type_name: Option<String>,
    }

    /// One node of a query result, for [`ShowResultsOnCanvas`].
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema)]
    pub struct ResultNode {
        pub type_name: String,
        pub id: String,
        #[serde(default)]
        pub label: Option<String>,
    }

    /// Draw a query result's nodes on a canvas of their own, linking the nodes of each row
    /// (specs 006, 007).
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct ShowResultsOnCanvas {
        pub title: String,
        pub nodes: Vec<ResultNode>,
        /// Pairs of indices into `nodes` that share a row.
        pub links: Vec<(usize, usize)>,
    }

    /// Browse a type's rows in the Nodes panel under the canvas.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct BrowseType {
        pub type_name: String,
    }

    /// Open a Blob value in a tab of its own, read at a commit or the branch head.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenBlob {
        pub type_name: String,
        pub id: String,
        pub property: String,
        #[serde(default)]
        pub edge: bool,
        #[serde(default)]
        pub at_commit: Option<String>,
    }

    /// Show one node in the inspector without revealing it in the tree, read at the branch
    /// head or held at a commit.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct PeekNode {
        pub type_name: String,
        pub id: String,
        #[serde(default)]
        pub at_commit: Option<String>,
    }

    /// Open one node in the inspector and the relationship tree.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenNode {
        pub type_name: String,
        pub id: String,
    }

    /// Open a node's text property in an editor of its own with its preview, read at a commit
    /// (the one a field was being edited at) or the branch head, showing text already typed.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenTextEditor {
        pub type_name: String,
        pub id: String,
        pub property: String,
        #[serde(default)]
        pub read_at: Option<String>,
        #[serde(default)]
        pub text: Option<String>,
    }

    /// Put back what a delete removed, its nodes and their relationships, in one commit, while
    /// the branch is still at the delete's commit (spec 004). Offered by the toast a delete
    /// leaves; `what` names the deleted rows in the outcome.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct UndoDelete {
        pub commit: String,
        pub branch: String,
        pub what: String,
    }

    /// Open one node as a page in its own tab, read at the branch head or at a commit.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenNodePage {
        pub type_name: String,
        pub id: String,
        #[serde(default)]
        pub at_commit: Option<String>,
    }

    /// Open a new query tab with this text. It is not run.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct NewQueryWithText {
        pub text: String,
    }

    /// Open a `.gq` file in a query tab; without a path, choose one.
    #[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenQueryFile {
        #[serde(default)]
        pub path: Option<String>,
    }

    /// Open a query editor that reads one commit, read-only (spec 009).
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenQueryAt {
        pub commit_id: String,
    }

    /// Open a stored query, one the server serves by name, in a tab with its parameters; with
    /// an alias from the CLI config, its values filled in (spec 007).
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_graph)]
    pub struct OpenStoredQuery {
        pub name: String,
        #[serde(default)]
        pub alias: Option<String>,
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
            /// Keep the cell being edited as a draft, to save with the others in one commit.
            StageEdit,
            /// Save every draft in the table as one commit.
            SaveDrafts,
            /// Highlight the next value the filter's value field offers.
            SelectNextSuggestion,
            /// Highlight the previous value the filter's value field offers.
            SelectPreviousSuggestion,
            /// Cancel the edit, or step back one level.
            Cancel,
            /// Show the selected row in the inspector.
            PeekRow,
            /// Open the selected row's node as a page.
            OpenRowPage,
            /// Explore the graph from the row under the cursor, on the canvas.
            ExploreRow,
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

pub mod node {
    use gpui::actions;

    actions!(
        studio_node,
        [
            /// Save the node field being edited.
            SaveField,
            /// Put the node field being edited back as it was.
            CancelField,
            /// Open the text field being edited in an editor of its own, with its preview.
            ExpandField,
            /// Save the text in the editor.
            SaveText,
            /// Save every unsaved change of the node shown as one commit.
            SaveNodeDrafts,
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
            /// Show the selected node in the inspector, keeping focus in the tree.
            PeekSelected,
            /// Open a table filtered to the selected node, or to its neighbours in a group.
            OpenTable,
            /// Open the selected node's page, or a relationship group's table.
            OpenPage,
            /// Show or hide relationship groups with no relationships.
            ToggleEmptyGroups,
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
            /// Save the query to its file, or to this graph's saved queries.
            Save,
            /// Save the query to a file you choose.
            SaveAs,
        ]
    );
}

pub mod history {
    use gpui::actions;

    actions!(
        studio_history,
        [
            /// Go to the next commit matching the search.
            NextMatch,
            /// Go to the previous commit matching the search.
            PreviousMatch,
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
            /// Edit the current change's node on the reviewed branch.
            EditChange,
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
            /// Show an image at its own size, one image pixel to one screen point.
            ActualSize,
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
            /// Show the selected node in the inspector.
            PeekSelected,
            /// Open the selected node's page.
            OpenSelectedPage,
            /// Center and zoom on the selected node.
            FocusSelected,
            /// Select the node the selected one was reached from.
            WalkBack,
            /// Go back down the path walked with WalkBack.
            WalkForward,
            /// Mark every chip on the canvas.
            SelectAll,
            /// Unmark the marked chips.
            ClearSelection,
            /// Let the layout place every node again, forgetting where nodes were dragged.
            ResetLayout,
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
            /// Send the message in the agent composer; while the agent works, queue it.
            Send,
            /// Stop the agent's turn and send the message in the composer at once.
            SendNow,
            /// Drop every message waiting in the agent's queue.
            ClearQueue,
            /// Stop the agent's current turn.
            Stop,
            /// Allow the change or request the agent is waiting on.
            Allow,
            /// Allow this and every later change on the agent's branch in this session.
            AllowAlways,
            /// Reject the change or request the agent is waiting on.
            Reject,
            /// Start the agent again on this conversation after it stopped.
            Restart,
            /// Start a new conversation with the agent in a tab of its own.
            NewConversation,
            /// Show the graph's agent conversations in the sidebar.
            ShowConversations,
        ]
    );

    /// Ask the agent a question about the graph.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_agent)]
    pub struct Ask {
        pub text: String,
    }

    /// Open one of the graph's agent conversations.
    #[derive(Clone, Debug, PartialEq, Deserialize, JsonSchema, Action)]
    #[action(namespace = studio_agent)]
    pub struct OpenConversation {
        pub session: String,
    }
}
