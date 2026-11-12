# Omnigraph Studio

The Knowledge IDE for [Omnigraph](https://github.com/ModernRelay/omnigraph): a fast,
GPU-rendered desktop app to visualize your ontology and graph, inspect knowledge, and review
what agents change before it merges.

![Studio switching the Person table to a branch, whose rows show what it adds, changes and removes inline, then opening a person as a page, exploring their relationships on the canvas and in the tree, catching a typo in a query before it runs, and reviewing the branch's changes before the merge](docs/images/demo.gif)

## Download

| Platform | File | Needs |
|---|---|---|
| macOS, Apple silicon | [Omnigraph-Studio-arm64.dmg](https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/Omnigraph-Studio-arm64.dmg) | macOS 11 or later |
| Windows, x86_64 | [omnigraph-studio-windows-x86_64-setup.exe](https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/omnigraph-studio-windows-x86_64-setup.exe) (installer) or [.zip](https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/omnigraph-studio-windows-x86_64.zip) | Windows 10 1809 or later |
| Linux, x86_64 | [omnigraph-studio-linux-x86_64.tar.gz](https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/omnigraph-studio-linux-x86_64.tar.gz) | glibc 2.35 or later |
| Linux, arm64 | [omnigraph-studio-linux-aarch64.tar.gz](https://github.com/ModernRelay/omnigraph-studio/releases/latest/download/omnigraph-studio-linux-aarch64.tar.gz) | glibc 2.35 or later |

The macOS app is signed and notarized, and installs its own updates. The Windows installer
needs no administrator rights. Neither Windows file is signed yet, so SmartScreen may ask
first: choose More info, then Run anyway. Every version is on the
[releases page](https://github.com/ModernRelay/omnigraph-studio/releases).

## Open a graph

Studio reaches your graphs through the `omnigraph` CLI, so install it first:

```bash
curl -fsSL https://raw.githubusercontent.com/ModernRelay/omnigraph/main/scripts/install.sh | bash
```

Studio lists every server you have as a CLI target (`omnigraph target list`) and uses the
CLI's credentials. To add a server, choose File → Open Graph… in Studio. It needs Omnigraph
0.14 or later.

## What you can do

- **Browse.** A table for each node and edge type, with sorting, filters, grouping and saved
  views, read page by page on the server. Edit cells, or stage edits across rows and save them
  as one change.
- **Explore.** Open any node as a page, then follow its relationships in a tree or on a canvas.
  The ontology view draws the schema's types and how they relate.
- **Query.** Write `.gq` queries with highlighting, completion and errors as you type. ⌘↵ runs
  the query under the cursor, and a mutation asks first, naming the branch it writes to.
- **Review and merge.** See what a branch adds, changes and removes, revert single changes,
  and merge into main. Conflicts are listed when the server refuses a merge.
- **Go back in time.** Open any table, query or node as it was at an older commit.
- **Work with an agent.** Claude or Codex in a side panel. The agent writes only to a branch
  Studio makes for it, and never sees your token.

⌘K opens the command palette, ⌘P goes to any type or node, and `?` lists every shortcut.

## How it's built

Studio is a Rust app built on [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui),
the framework the Zed editor is made with, and it reuses Zed's editor, UI and theme crates.
Every frame is drawn on the GPU (Metal on macOS, Vulkan on Linux, DirectX on Windows), with no
web view in between. On an M5 Pro a table frame takes about 4.4 ms
([perf reports](docs/perf/)).

If you use Zed, a lot will feel familiar: `keymap.json` takes Zed's keymap format, and
`settings.json` reloads when you save it. The query editor links `omnigraph-compiler`, the same
parser and type checker the server runs, so you get the server's errors before you run
anything. The agent panel runs Claude or Codex over the Agent Client Protocol, the protocol Zed
uses for its agents.

To build it and run it against a sample graph, see [docs/development.md](docs/development.md).

## License

GPL-3.0-or-later, because Studio links Zed's GPL-3.0 crates ([LICENSE](LICENSE)). Omnigraph
itself is MIT and is not affected: Studio talks to it over HTTP and links only the MIT-licensed
`omnigraph-compiler`. The Inter font is under the SIL Open Font License 1.1, and the
[Nucleo](https://nucleoapp.com) icons in `assets/icons/nucleo/` are used under Nucleo's own
licence, not the GPL ([NOTICE](assets/icons/nucleo/NOTICE)).
