//! LSP exposure for atomic Python file and regular-package renames.
//!
//! Relevant entries must belong to one workspace; a renamed folder may not contain another one.
//! Candidates include indexed and open files plus every physical Python source moved with a
//! folder. Discovery, analysis, ownership, and LSP conversion failures decline the whole batch,
//! and every decline is traced with a stable reason instead of returning partial edits.

use std::collections::HashMap;

use lsp_types::{RenameFilesParams, TextEdit, Uri, WillRenameFilesRequest, WorkspaceEdit};
use percent_encoding::percent_decode_str;
use ruff_db::Db as _;
use ruff_db::files::system_path_to_file;
use ruff_db::system::{System, SystemPath, SystemPathBuf};
use ty_ide::{PathRename, will_rename_paths};
use ty_project::{Db as _, ProjectDatabase};

use crate::document::FileRangeExt;
use crate::server::api::traits::{
    BackgroundRequestHandler, RequestHandler, RetriableRequestHandler,
};
use crate::session::SessionSnapshot;
use crate::session::client::Client;

/// Handles `workspace/willRenameFiles` for supported Python modules and packages.
pub(crate) struct WillRenameFilesHandler;

impl RequestHandler for WillRenameFilesHandler {
    type RequestType = WillRenameFilesRequest;
}

impl BackgroundRequestHandler for WillRenameFilesHandler {
    fn run(
        snapshot: &SessionSnapshot,
        _client: &Client,
        params: RenameFilesParams,
    ) -> crate::server::Result<Option<WorkspaceEdit>> {
        workspace_edit(snapshot, params).map_or_else(|reason| Ok(decline(reason)), Ok)
    }
}

impl RetriableRequestHandler for WillRenameFilesHandler {
    const RETRY_ON_CANCELLATION: bool = true;
}

fn decline(reason: &'static str) -> Option<WorkspaceEdit> {
    tracing::debug!(reason, "Declining `workspace/willRenameFiles`");
    None
}

fn workspace_edit(
    snapshot: &SessionSnapshot,
    params: RenameFilesParams,
) -> Result<Option<WorkspaceEdit>, &'static str> {
    let system = snapshot
        .projects()
        .first()
        .map(ProjectDatabase::system)
        .ok_or("no project is available")?;
    let mut owner = None;
    let mut moved_paths = Vec::new();
    let mut renames = Vec::new();

    for rename in params.files {
        let old_path = match file_uri_to_path(&rename.old_uri) {
            Some(path) => path,
            None if uri_is_non_python_file(&rename.old_uri) => continue,
            None => return Err("a rename URI is not a local file path"),
        };
        let directory = system.is_directory(&old_path);
        if !directory && !matches!(old_path.extension(), Some("py" | "pyi")) {
            continue;
        }
        let new_path =
            file_uri_to_path(&rename.new_uri).ok_or("a rename URI is not a local file path")?;
        let moved = if directory {
            let files = python_files_in_directory(system, &old_path)
                .ok_or("a renamed directory cannot be read completely")?;
            if files.is_empty() {
                continue;
            }
            files
        } else {
            if new_path.extension() != old_path.extension() {
                return Err("a Python file rename changes its extension");
            }
            Vec::new()
        };
        let path_rename = if directory {
            PathRename::directory(old_path.clone(), new_path.clone())
        } else {
            PathRename::file(old_path.clone(), new_path.clone())
        };

        let project = snapshot
            .enclosing_project_index(&old_path)
            .ok_or("a renamed source is outside every workspace")?;
        if snapshot.enclosing_project_index(&new_path) != Some(project)
            || owner.is_some_and(|owner| owner != project)
            || !moved.is_empty() && snapshot.contains_other_workspace(project, &old_path)
        {
            return Err("a rename crosses workspace ownership");
        }
        owner.get_or_insert(project);
        moved_paths.extend(moved);
        renames.push(path_rename);
    }

    let Some(owner) = owner else {
        return Ok(None);
    };
    if snapshot.language_services_disabled(owner) {
        return Err("language services are disabled for the workspace");
    }
    let db = &snapshot.projects()[owner];
    let project = db.project();
    let mut moved_files = Vec::with_capacity(moved_paths.len());
    for path in moved_paths {
        if snapshot.enclosing_project_index(&path) != Some(owner) {
            return Err("a moved source belongs to another workspace");
        }
        moved_files.push(
            system_path_to_file(db, path).map_err(|_| "a moved source cannot be registered")?,
        );
    }
    let edits = will_rename_paths(
        db,
        &renames,
        project
            .files(db)
            .into_iter()
            .chain(project.open_files(db).iter().copied())
            .chain(moved_files),
        |file| {
            file.path(db)
                .as_system_path()
                .is_none_or(|path| snapshot.enclosing_project_index(path) == Some(owner))
        },
    )?;
    let changes = lsp_edits(db, snapshot.position_encoding(), edits)
        .ok_or("an edit cannot be converted to an LSP location")?;
    Ok((!changes.is_empty()).then(|| WorkspaceEdit::new(Some(changes), None, None)))
}

fn lsp_edits(
    db: &ProjectDatabase,
    encoding: crate::PositionEncoding,
    edits: Vec<ty_ide::FileRenameEdit>,
) -> Option<HashMap<Uri, Vec<TextEdit>>> {
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for edit in edits {
        let location = edit.range.to_lsp_range(db, encoding)?.into_location()?;
        let edit = TextEdit::new(location.range, edit.value);
        changes.entry(location.uri).or_default().push(edit);
    }
    #[expect(clippy::iter_over_hash_type, reason = "documents are independent")]
    for edits in changes.values_mut() {
        edits.sort_unstable_by_key(|edit| edit.range);
        edits.dedup();
        if edits
            .windows(2)
            .any(|pair| pair[1].range.start < pair[0].range.end)
        {
            return None;
        }
    }
    Some(changes)
}

fn python_files_in_directory(system: &dyn System, root: &SystemPath) -> Option<Vec<SystemPathBuf>> {
    let mut files = Vec::new();
    for entry in system.read_directory(root).ok()? {
        let entry = entry.ok()?;
        let file_type = entry.file_type();
        let path = entry.into_path();
        if file_type.is_file() && matches!(path.extension(), Some("py" | "pyi")) {
            files.push(path);
        } else if file_type.is_directory() {
            files.extend(python_files_in_directory(system, &path)?);
        }
    }
    Some(files)
}

fn file_uri_to_path(uri: &str) -> Option<SystemPathBuf> {
    let uri = Uri::parse(uri).ok()?;
    SystemPathBuf::from_path_buf(uri.to_file_path().ok()?).ok()
}

fn uri_is_non_python_file(uri: &str) -> bool {
    Uri::parse(uri)
        .ok()
        .and_then(|uri| {
            let path: Vec<_> = percent_decode_str(uri.path()).collect();
            let name = path.rsplit(|byte| *byte == b'/').next()?;
            let dot = name.iter().rposition(|byte| *byte == b'.')?;
            let extension = &name[dot + 1..];
            Some(extension != b"py" && extension != b"pyi")
        })
        .unwrap_or(false)
}
