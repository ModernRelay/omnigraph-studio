//! Blob values written from Studio (spec 004): a file the person chooses or drops stored in a
//! cell, or the cell cleared. Every place a value shows for editing (the Blob tab, the table's
//! cells, the peek's slots) writes through here.
//!
//! A write is bound to the value the view shows, as read at the commit it shows (constitution
//! III): a managed value by its ETag, which the server compares when it applies the write. The
//! ETag names the type's table version, so an ETag read at an older commit than the view's is
//! read again there first. A null cell and an external reference have no ETag: the cell is read
//! at the branch head just before the write, and the write goes only while it still holds what
//! the view shows. A value changed elsewhere ends the write unsent or refused, never
//! overwritten, with what the cell holds now; sending the change again, bound to that, is the
//! person's choice. Nothing is retried.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use gpui::{App, AsyncApp, Entity, PathPromptOptions, Task};
use gq_language::{ScalarKind, SchemaModel};
use graph_client::api::{BLOB_PUT_MAX, BlobWriteOutput, BlobWriteState};
use graph_client::{BlobCell, BlobMeta, WriteOutcome};
use graph_store::GraphSession;
use ui::prelude::*;

use crate::model::{Notice, Tone};

/// What a Blob write does to its cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlobChange {
    /// Stores the file at this path, read when the write goes.
    Upload(PathBuf),
    Clear,
}

/// How a Blob write ended (constitution III).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BlobWritten {
    /// Published: the cell holds `meta` at `commit`, made on `parent`. A clear of a cell that
    /// was already null publishes nothing, and has no commit.
    Saved {
        commit: Option<String>,
        parent: Option<String>,
        meta: BlobMeta,
    },
    /// The cell holds another value than the one the view shows, so nothing was written: `now`
    /// is what it holds at the branch head, `None` when that could not be read.
    Changed { now: Option<BlobMeta> },
    /// Nothing was written: the file could not be sent, or the server refused it.
    Refused(Notice),
    /// The answer was lost: the write may have landed.
    Unknown(Notice),
}

/// A Blob write as the view that sent it shows it, until it lands.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BlobEdit {
    /// Out: the file being read, or the write on its way.
    Saving(BlobChange),
    /// It did not land, and why. For a value changed elsewhere, `again` is the change once
    /// more, bound to what the cell holds now, which the view offers.
    Failed {
        notice: Notice,
        again: Option<(BlobChange, BlobMeta)>,
    },
}

impl BlobEdit {
    /// How a write of `change` to `property` that did not land shows; `None` for one that did.
    pub(crate) fn ended(written: BlobWritten, change: BlobChange, property: &str) -> Option<Self> {
        let (notice, again) = match written {
            BlobWritten::Saved { .. } => return None,
            BlobWritten::Changed { now } => {
                let what = match change {
                    BlobChange::Upload(_) => "Not uploaded",
                    BlobChange::Clear => "Not cleared",
                };
                let message = match &now {
                    Some(now) => format!(
                        "{what}: {property} changed since it was read, and now holds {}.",
                        held(now)
                    ),
                    None => format!(
                        "{what}: {property} changed since it was read, and what it holds now \
                         could not be read. Refresh, then try again."
                    ),
                };
                // Clearing a value already gone asks nothing more.
                let again = now
                    .filter(|now| !(change == BlobChange::Clear && *now == BlobMeta::Null))
                    .map(|now| (change, now));
                (Notice::new(Tone::Conflict, message), again)
            }
            BlobWritten::Refused(notice) | BlobWritten::Unknown(notice) => (notice, None),
        };
        Some(BlobEdit::Failed { notice, again })
    }

    pub(crate) fn is_saving(&self) -> bool {
        matches!(self, BlobEdit::Saving(_))
    }
}

/// What a cell holds, in a sentence.
fn held(meta: &BlobMeta) -> String {
    match meta {
        BlobMeta::Null => "no value".into(),
        BlobMeta::Managed { size, .. } => format!("a value of {}", studio_ui::format_bytes(*size)),
        BlobMeta::External { uri } => format!("a reference to {}", crate::blob::host(uri)),
    }
}

/// The label of the button that sends a change again over what the cell holds now.
pub(crate) fn again_label(change: &BlobChange, now: &BlobMeta) -> &'static str {
    match (change, now) {
        (BlobChange::Clear, _) => "Clear anyway",
        (BlobChange::Upload(_), BlobMeta::Null) => "Upload anyway",
        (BlobChange::Upload(_), _) => "Replace anyway",
    }
}

/// Why `cell` cannot be written from a view, or `None` when it can: the view shows a commit
/// (`pinned`), the cell is an edge's, or the schema has no Blob property by its name.
pub(crate) fn write_refusal(
    schema: Option<&SchemaModel>,
    cell: &BlobCell,
    pinned: Option<&str>,
) -> Option<String> {
    if let Some(commit) = pinned {
        return Some(format!(
            "Read-only: this shows commit {}. Open it at the branch head to change it.",
            studio_ui::short_commit_id(commit)
        ));
    }
    if cell.edge {
        return Some(format!(
            "Studio changes no edge's values; `omnigraph blob put edge {} {} {}` does.",
            cell.type_name, cell.id, cell.property
        ));
    }
    let Some(schema) = schema else {
        return Some("The schema is still being read.".into());
    };
    match schema
        .node(&cell.type_name)
        .and_then(|node| node.property(&cell.property))
    {
        Some(property) if property.kind == ScalarKind::Blob => None,
        _ => Some(format!("{} is not a Blob property.", cell.property)),
    }
}

/// Why `cell` cannot be cleared, or `None` when it can: as [`write_refusal`], and a property
/// that must hold a value, or a cell `shown` holding none (or not read yet).
pub(crate) fn clear_refusal(
    schema: Option<&SchemaModel>,
    cell: &BlobCell,
    pinned: Option<&str>,
    shown: Option<&BlobMeta>,
) -> Option<String> {
    if let Some(reason) = write_refusal(schema, cell, pinned) {
        return Some(reason);
    }
    let nullable = schema
        .and_then(|schema| schema.node(&cell.type_name))
        .and_then(|node| node.property(&cell.property))
        .is_some_and(|property| property.nullable);
    if !nullable {
        return Some(format!(
            "{} must hold a value, so it cannot be cleared.",
            cell.property
        ));
    }
    match shown {
        None => Some(format!(
            "Studio has not read what {} holds yet.",
            cell.property
        )),
        Some(BlobMeta::Null) => Some(format!("{} holds no value.", cell.property)),
        Some(_) => None,
    }
}

/// What a Choose File button's tooltip says: where the file goes, its key, and the CLI command
/// that does the same.
pub(crate) fn upload_tooltip(cell: &BlobCell, branch: &str, cx: &mut App) -> gpui::AnyView {
    ui::Tooltip::with_meta(
        format!(
            "Choose File… to store in {} on {branch}, or drop one here",
            cell.property
        ),
        Some(&studio_actions::blob::ChooseFile),
        format!(
            "omnigraph blob put node {} {} {} --file <file> --branch {branch}",
            cell.type_name, cell.id, cell.property
        ),
        cx,
    )
}

/// What a Clear button's tooltip says.
pub(crate) fn clear_tooltip(cell: &BlobCell, branch: &str, cx: &mut App) -> gpui::AnyView {
    ui::Tooltip::with_meta(
        format!(
            "Clear {} on {branch}: it holds no value then",
            cell.property
        ),
        Some(&studio_actions::blob::ClearValue),
        format!(
            "omnigraph blob clear node {} {} {} --branch {branch}",
            cell.type_name, cell.id, cell.property
        ),
        cx,
    )
}

/// Asks for one file with the system's own picker; `None` when it was cancelled or could not
/// open.
pub(crate) fn choose_file(cx: &mut App) -> Task<Option<PathBuf>> {
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Upload".into()),
    });
    cx.spawn(async move |_| match chosen.await {
        Ok(Ok(Some(paths))) => paths.into_iter().next(),
        _ => None,
    })
}

/// The one file of a drop from the file manager; a drop of several, or of nothing, is refused.
pub(crate) fn dropped_file(paths: &[PathBuf]) -> Result<PathBuf, Notice> {
    match paths {
        [path] => Ok(path.clone()),
        _ => Err(Notice::new(
            Tone::Invalid,
            "Nothing was uploaded: drop one file at a time.",
        )),
    }
}

/// A file read for an upload: its name, which the write's description gives, and its bytes.
pub(crate) struct Upload {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Reads the file at `path` whole; call it from a background task. A file over
/// [`BLOB_PUT_MAX`] is refused before its bytes are read, and one that grew past it since is
/// refused after reading one byte too many.
pub(crate) fn read_upload(path: &Path) -> Result<Upload, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let unread = |error: std::io::Error| {
        format!("Nothing was uploaded: {name} could not be read ({error}).")
    };
    let meta = std::fs::metadata(path).map_err(unread)?;
    if meta.is_dir() {
        return Err(format!("Nothing was uploaded: {name} is a folder."));
    }
    let too_large = |size: Option<u64>| match size {
        Some(size) => format!(
            "Nothing was uploaded: {name} is {}, and a Blob value holds at most 32 MiB.",
            studio_ui::format_bytes(size)
        ),
        None => {
            format!("Nothing was uploaded: {name} is over 32 MiB, the most a Blob value holds.")
        }
    };
    if meta.len() > BLOB_PUT_MAX {
        return Err(too_large(Some(meta.len())));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    std::fs::File::open(path)
        .and_then(|file| file.take(BLOB_PUT_MAX + 1).read_to_end(&mut bytes))
        .map_err(unread)?;
    if bytes.len() as u64 > BLOB_PUT_MAX {
        return Err(too_large(None));
    }
    Ok(Upload { name, bytes })
}

/// Stores the file at `path` in `cell` on `branch`, bound to `shown`, the value the view shows,
/// read at `read_at`, the commit it shows (`None`: the branch head). The file is read off the
/// main thread, and refused over [`BLOB_PUT_MAX`] before anything is sent. This is the whole of
/// an upload but the picker, so a test can drive it.
pub(crate) fn upload(
    session: &Entity<GraphSession>,
    branch: &str,
    cell: BlobCell,
    shown: BlobMeta,
    read_at: Option<String>,
    path: PathBuf,
    cx: &mut App,
) -> Task<BlobWritten> {
    let (session, branch) = (session.clone(), branch.to_owned());
    cx.spawn(async move |cx| {
        let file = cx.background_spawn(async move { read_upload(&path) }).await;
        let Upload { name, bytes } = match file {
            Ok(upload) => upload,
            Err(message) => return BlobWritten::Refused(Notice::new(Tone::Invalid, message)),
        };
        let if_match = match bind(&session, &branch, &cell, &shown, read_at.as_deref(), cx).await {
            Ok(if_match) => if_match,
            Err(unsent) => return unsent,
        };
        let size = bytes.len() as u64;
        let description = format!(
            "Upload {name} ({}) to {} of {}",
            studio_ui::format_bytes(size),
            cell.property,
            cell.id
        );
        let write = session.update(cx, |session, cx| {
            session.blob_put_on(&branch, cell.clone(), if_match, bytes, description, cx)
        });
        let outcome = write.await;
        written(outcome, &session, &branch, &cell, cx).await
    })
}

/// Sets `cell` to null on `branch`, bound to `shown` read at `read_at` as [`upload`] binds.
pub(crate) fn clear(
    session: &Entity<GraphSession>,
    branch: &str,
    cell: BlobCell,
    shown: BlobMeta,
    read_at: Option<String>,
    cx: &mut App,
) -> Task<BlobWritten> {
    let (session, branch) = (session.clone(), branch.to_owned());
    cx.spawn(async move |cx| {
        let if_match = match bind(&session, &branch, &cell, &shown, read_at.as_deref(), cx).await {
            Ok(if_match) => if_match,
            Err(unsent) => return unsent,
        };
        let description = format!("Clear {} of {}", cell.property, cell.id);
        let write = session.update(cx, |session, cx| {
            session.blob_clear_on(&branch, cell.clone(), if_match, description, cx)
        });
        let outcome = write.await;
        written(outcome, &session, &branch, &cell, cx).await
    })
}

/// The `If-Match` that binds a write to `shown`, the value read at `read_at` (the head when
/// `None`), or how the write ended unsent. A managed value is bound by its ETag at the view's
/// commit, which the server compares when it applies the write; the ETag is read again there
/// (from the session's memo, as a rule), since the view's own writes to the type move every
/// ETag of it. A null cell and an external reference have no ETag: the cell is read at the head
/// now, and the write goes only while it still holds what is shown, unbound for null (no
/// `If-Match` describes an empty cell) and with `*` for a reference.
async fn bind(
    session: &Entity<GraphSession>,
    branch: &str,
    cell: &BlobCell,
    shown: &BlobMeta,
    read_at: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<Option<String>, BlobWritten> {
    let unread = |error: graph_client::ReadError| {
        BlobWritten::Refused(Notice::new(
            Tone::Refused,
            format!(
                "Nothing was written: what {} holds could not be read. {error}",
                cell.property
            ),
        ))
    };
    let read = match (shown, read_at) {
        (BlobMeta::Managed { .. }, None) => return Ok(Some(validator(shown))),
        (BlobMeta::Managed { .. }, Some(commit)) => session.update(cx, |session, cx| {
            session.blob_meta(cell.clone(), Some(commit.to_owned()), cx)
        }),
        _ => session.update(cx, |session, cx| {
            session.blob_meta_on(branch, cell.clone(), cx)
        }),
    };
    let found = read.await.map_err(unread)?;
    if same_value(&found, shown) {
        return Ok(match &found {
            BlobMeta::Null => None,
            _ => Some(validator(&found)),
        });
    }
    // Read at the view's commit, the value is not the one shown there: what the head holds is
    // what a change would replace.
    let now = match read_at.filter(|_| matches!(shown, BlobMeta::Managed { .. })) {
        Some(_) => head(session, branch, cell, cx).await,
        None => Some(found),
    };
    Err(BlobWritten::Changed { now })
}

/// What binds a write to a value that is not null: its ETag, or `*` (any value) without one.
fn validator(meta: &BlobMeta) -> String {
    match meta {
        BlobMeta::Managed {
            etag: Some(etag), ..
        } => etag.clone(),
        _ => "*".into(),
    }
}

/// Whether two reads of a cell found the same value, as far as they can say: a managed value
/// by its size (its ETag moves with every write to its type), a reference by where it points.
fn same_value(a: &BlobMeta, b: &BlobMeta) -> bool {
    match (a, b) {
        (BlobMeta::Null, BlobMeta::Null) => true,
        (BlobMeta::Managed { size: a, .. }, BlobMeta::Managed { size: b, .. }) => a == b,
        (BlobMeta::External { uri: a }, BlobMeta::External { uri: b }) => a == b,
        _ => false,
    }
}

/// What the cell holds at the branch head now; `None` when it could not be read.
async fn head(
    session: &Entity<GraphSession>,
    branch: &str,
    cell: &BlobCell,
    cx: &mut AsyncApp,
) -> Option<BlobMeta> {
    let read = session.update(cx, |session, cx| {
        session.blob_meta_on(branch, cell.clone(), cx)
    });
    read.await.ok()
}

/// How a sent Blob write ended. A refused binding reads what the cell holds at the head now,
/// for the view to show and bind the change to if the person sends it again.
async fn written(
    outcome: WriteOutcome<BlobWriteOutput>,
    session: &Entity<GraphSession>,
    branch: &str,
    cell: &BlobCell,
    cx: &mut AsyncApp,
) -> BlobWritten {
    match outcome {
        WriteOutcome::Committed(output) => {
            let meta = match output.kind {
                BlobWriteState::Null => BlobMeta::Null,
                _ => BlobMeta::Managed {
                    size: output.size.unwrap_or_default(),
                    etag: output.etag,
                },
            };
            BlobWritten::Saved {
                commit: output.commit.as_ref().map(|c| c.graph_commit_id.clone()),
                parent: output.commit.and_then(|c| c.parent_commit_id),
                meta,
            }
        }
        WriteOutcome::Refused(error)
            if error.status == 412 || error.body.blob_precondition_failure.is_some() =>
        {
            BlobWritten::Changed {
                now: head(session, branch, cell, cx).await,
            }
        }
        WriteOutcome::Refused(error) => {
            let message = match error.status {
                403 => format!("Your token cannot change {branch}. Nothing was written."),
                413 => format!(
                    "Nothing was written: the server takes no value this large. {}",
                    error.body.error
                ),
                _ => format!("Nothing was written. {}", error.body.error),
            };
            BlobWritten::Refused(Notice::new(Tone::Refused, message))
        }
        WriteOutcome::Unknown { .. } => BlobWritten::Unknown(Notice::new(
            Tone::Unknown,
            "The server's answer was lost, so the value may have changed. Check History \
             before trying again.",
        )),
    }
}

/// A write's notice under its value: the dot and the reason in the tone's colour, then the
/// change offered again over what the cell holds now (`again`), or History, where an unknown
/// outcome is settled.
pub(crate) fn notice_row(
    id: impl Into<SharedString>,
    notice: &Notice,
    again: Option<AnyElement>,
) -> gpui::Div {
    let id = id.into();
    let unknown = notice.tone == Tone::Unknown;
    h_flex()
        .gap(px(studio_ui::ds::space::SM))
        .child(ui::Indicator::dot().color(notice.tone.color()))
        .child(
            studio_ui::ds::text::meta(notice.message.clone())
                .color(notice.tone.color())
                .flex_1(),
        )
        .children(again)
        .when(unknown, |row| {
            row.child(
                studio_ui::ds::row_button(
                    SharedString::from(format!("{id}-history")),
                    "Open history",
                )
                .style(ButtonStyle::Subtle)
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(studio_actions::graph::OpenHistory), cx)
                }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(edge: bool) -> BlobCell {
        BlobCell {
            edge,
            type_name: "Note".into(),
            id: "n1".into(),
            property: "attachment".into(),
        }
    }

    const NOTES: &str =
        "node Note {\n  slug: String @key\n  body: String?\n  attachment: Blob?\n  scan: Blob\n}\n";

    /// A Blob is written only at the head, only a node's, and only a Blob property; a clear
    /// also needs a property that may be empty and a value to clear.
    #[test]
    fn a_blob_is_written_only_where_it_can_be_and_says_why_not() {
        let schema = SchemaModel::parse(NOTES).unwrap();
        let schema = Some(&schema);
        assert_eq!(write_refusal(schema, &cell(false), None), None);
        let pinned = write_refusal(schema, &cell(false), Some("01JCOMMIT0000c5")).unwrap();
        assert!(
            pinned.starts_with("Read-only: this shows commit"),
            "{pinned}"
        );
        let edge = write_refusal(schema, &cell(true), None).unwrap();
        assert!(
            edge.contains("omnigraph blob put edge Note n1 attachment"),
            "{edge}"
        );
        let mut body = cell(false);
        body.property = "body".into();
        assert_eq!(
            write_refusal(schema, &body, None).as_deref(),
            Some("body is not a Blob property.")
        );
        assert!(write_refusal(None, &cell(false), None).is_some());

        let managed = BlobMeta::Managed {
            size: 3,
            etag: Some("\"e1\"".into()),
        };
        assert_eq!(
            clear_refusal(schema, &cell(false), None, Some(&managed)),
            None
        );
        let mut scan = cell(false);
        scan.property = "scan".into();
        assert_eq!(
            clear_refusal(schema, &scan, None, Some(&managed)).as_deref(),
            Some("scan must hold a value, so it cannot be cleared.")
        );
        assert_eq!(
            clear_refusal(schema, &cell(false), None, Some(&BlobMeta::Null)).as_deref(),
            Some("attachment holds no value.")
        );
        assert!(clear_refusal(schema, &cell(false), None, None).is_some());
    }

    /// A file over 32 MiB is refused without its bytes being read; one at the limit is read.
    #[test]
    fn a_file_over_the_limit_is_refused_before_it_is_read() {
        let dir = std::env::temp_dir().join(format!("studio-blob-upload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let large = dir.join("large.bin");
        // Sparse: no 32 MiB is written to make it.
        std::fs::File::create(&large)
            .unwrap()
            .set_len(BLOB_PUT_MAX + 1)
            .unwrap();
        let refused = read_upload(&large).err().unwrap();
        assert!(
            refused.starts_with("Nothing was uploaded: large.bin is 34 MB"),
            "{refused}"
        );
        let small = dir.join("small.txt");
        std::fs::write(&small, b"hello").unwrap();
        let read = read_upload(&small).unwrap();
        assert_eq!(
            (read.name.as_str(), read.bytes.as_slice()),
            ("small.txt", &b"hello"[..])
        );
        assert!(read_upload(&dir).err().unwrap().ends_with("is a folder."));
        assert!(read_upload(&dir.join("gone")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A conflict offers the change again over what the cell holds now; a clear of a value
    /// cleared elsewhere offers nothing more.
    #[test]
    fn a_conflict_offers_the_change_again_over_the_value_now() {
        let now = BlobMeta::Managed {
            size: 2_140,
            etag: Some("\"e2\"".into()),
        };
        let upload = BlobChange::Upload("scan.pdf".into());
        let ended = BlobEdit::ended(
            BlobWritten::Changed {
                now: Some(now.clone()),
            },
            upload.clone(),
            "attachment",
        );
        let Some(BlobEdit::Failed { notice, again }) = ended else {
            panic!("a failure");
        };
        assert_eq!(notice.tone, Tone::Conflict);
        assert_eq!(
            notice.message,
            "Not uploaded: attachment changed since it was read, and now holds a value of 2.1 KB."
        );
        assert_eq!(again, Some((upload.clone(), now.clone())));
        assert_eq!(again_label(&upload, &now), "Replace anyway");
        assert_eq!(again_label(&upload, &BlobMeta::Null), "Upload anyway");
        let cleared = BlobEdit::ended(
            BlobWritten::Changed {
                now: Some(BlobMeta::Null),
            },
            BlobChange::Clear,
            "attachment",
        );
        assert!(matches!(
            cleared,
            Some(BlobEdit::Failed { again: None, .. })
        ));
        let saved = BlobWritten::Saved {
            commit: Some("c2".into()),
            parent: Some("c1".into()),
            meta: now,
        };
        assert_eq!(BlobEdit::ended(saved, upload, "attachment"), None);
        assert!(dropped_file(&[]).is_err());
        assert!(dropped_file(&["a".into(), "b".into()]).is_err());
        assert_eq!(dropped_file(&["a".into()]), Ok(PathBuf::from("a")));
    }
}
