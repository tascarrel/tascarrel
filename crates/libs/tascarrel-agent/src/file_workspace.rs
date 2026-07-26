//! Revision-aware filesystem access for coding tools.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::ops::Range;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reportify::Report;
use sha2::Digest as _;
use sha2::Sha256;
use similar::ChangeTag;
use similar::TextDiff;
use tokio::fs;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::FileChange;
use crate::FileChangeOperation;
use crate::ToolError;
use crate::ToolResult;

static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Filesystem root with optimistic revision tracking for agent mutations.
pub struct FileWorkspace {
    root: PathBuf,
    observations: RwLock<HashMap<PathBuf, FileObservation>>,
    mutation: Mutex<()>,
}

impl FileWorkspace {
    /// Opens an existing directory as a file-tool workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be resolved or is not a
    /// directory.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn open(root: impl AsRef<Path>) -> ToolResult<Self> {
        let root = fs::canonicalize(root.as_ref())
            .await
            .map_err(|source| io_error("open the file workspace", source))?;
        let metadata = fs::metadata(&root)
            .await
            .map_err(|source| io_error("inspect the file workspace", source))?;
        if !metadata.is_dir() {
            return Err(Report::new(ToolError::InvalidArguments {
                tool: "file_workspace".to_owned(),
                message: "workspace root is not a directory".to_owned(),
            }));
        }
        Ok(Self {
            root,
            observations: RwLock::new(HashMap::new()),
            mutation: Mutex::new(()),
        })
    }

    /// Returns the canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
    pub(crate) async fn read_text(
        &self,
        path: &Path,
        offset: usize,
        byte_offset: usize,
        limit: usize,
        max_bytes: usize,
        cancellation: &CancellationToken,
    ) -> ToolResult<TextRead> {
        ensure_running(cancellation)?;
        let path = self.resolve_existing(path).await?;
        let bytes = fs::read(&path)
            .await
            .map_err(|source| io_error("read a file", source))?;
        ensure_running(cancellation)?;
        let revision = FileRevision::from_bytes(&bytes);
        let content = String::from_utf8(bytes)
            .map_err(|_| Report::new(ToolError::NonUtf8File { path: path.clone() }))?;
        let selection = select_text(&content, offset, byte_offset, limit, max_bytes)?;
        let mut observations = self.observations.write().await;
        let observation = observations
            .entry(path)
            .or_insert_with(|| FileObservation::new(revision));
        if observation.revision != revision {
            *observation = FileObservation::new(revision);
        }
        observation.observe(selection.byte_range.clone(), content.len());
        Ok(selection)
    }

    #[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
    pub(crate) async fn write_text(
        &self,
        path: &Path,
        content: String,
        cancellation: &CancellationToken,
    ) -> ToolResult<Vec<FileChange>> {
        let _mutation = self.mutation.lock().await;
        ensure_running(cancellation)?;
        let target = self.resolve_mutation_target(path).await?;
        let prepared = match target {
            MutationTarget::Existing(path) => {
                let original = self.require_current_observation(&path).await?;
                if !original.observation.is_complete(original.bytes.len()) {
                    return Err(Report::new(ToolError::PartiallyReadFile { path }));
                }
                PreparedMutation::existing(
                    path,
                    content.into_bytes(),
                    original.bytes,
                    original.observation.revision,
                )
                .await?
            }
            MutationTarget::New(path) => PreparedMutation::new(path, content.into_bytes()),
        };
        commit_mutations(vec![prepared], &self.root, &self.observations, cancellation).await
    }

    #[tracing::instrument(level = "debug", skip_all, fields(file_count = edits.len()))]
    pub(crate) async fn edit_text(
        &self,
        edits: Vec<TextFileEdit>,
        cancellation: &CancellationToken,
    ) -> ToolResult<Vec<FileChange>> {
        let _mutation = self.mutation.lock().await;
        ensure_running(cancellation)?;
        if edits.is_empty() {
            return Err(Report::new(ToolError::InvalidArguments {
                tool: "edit".to_owned(),
                message: "files must contain at least one edit".to_owned(),
            }));
        }

        let mut prepared = Vec::with_capacity(edits.len());
        let mut targets = HashSet::new();
        for edit in edits {
            let path = self.resolve_existing(&edit.path).await?;
            if !targets.insert(path.clone()) {
                return Err(Report::new(ToolError::InvalidEdit {
                    path,
                    message: "the same file appears more than once".to_owned(),
                }));
            }
            let original = self.require_current_observation(&path).await?;
            let content = String::from_utf8(original.bytes.clone())
                .map_err(|_| Report::new(ToolError::NonUtf8File { path: path.clone() }))?;
            let new_content =
                apply_edits(&path, &content, edit.edits, &original.observation.coverage)?;
            prepared.push(
                PreparedMutation::existing(
                    path,
                    new_content.into_bytes(),
                    original.bytes,
                    original.observation.revision,
                )
                .await?,
            );
        }

        commit_mutations(prepared, &self.root, &self.observations, cancellation).await
    }

    async fn require_current_observation(&self, path: &Path) -> ToolResult<ObservedFile> {
        let observation = self
            .observations
            .read()
            .await
            .get(path)
            .cloned()
            .ok_or_else(|| {
                Report::new(ToolError::UnreadFile {
                    path: path.to_path_buf(),
                })
            })?;
        let bytes = fs::read(path)
            .await
            .map_err(|source| io_error("read a file before changing it", source))?;
        let revision = FileRevision::from_bytes(&bytes);
        if revision != observation.revision {
            return Err(Report::new(ToolError::StaleFile {
                path: path.to_path_buf(),
            }));
        }
        Ok(ObservedFile { bytes, observation })
    }

    async fn resolve_existing(&self, path: &Path) -> ToolResult<PathBuf> {
        let requested = self.requested_path(path);
        let canonical = match fs::canonicalize(&requested).await {
            Ok(path) => path,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(Report::new(ToolError::MissingFile { path: requested }));
            }
            Err(source) => return Err(io_error("resolve a file path", source)),
        };
        self.ensure_inside(canonical)
    }

    async fn resolve_mutation_target(&self, path: &Path) -> ToolResult<MutationTarget> {
        let relative = self.safe_mutation_relative(path)?;
        let requested = self.root.join(&relative);
        match fs::canonicalize(&requested).await {
            Ok(path) => self.ensure_inside(path).map(MutationTarget::Existing),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                let parent = requested.parent().ok_or_else(|| {
                    Report::new(ToolError::PathOutsideWorkspace {
                        path: requested.clone(),
                    })
                })?;
                let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
                let parent = self
                    .resolve_or_create_parent(parent, relative_parent)
                    .await?;
                let name = relative.file_name().ok_or_else(|| {
                    Report::new(ToolError::PathOutsideWorkspace {
                        path: requested.clone(),
                    })
                })?;
                Ok(MutationTarget::New(parent.join(name)))
            }
            Err(source) => Err(io_error("resolve a file path", source)),
        }
    }

    async fn resolve_or_create_parent(
        &self,
        requested_parent: &Path,
        relative_parent: &Path,
    ) -> ToolResult<PathBuf> {
        if relative_parent.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }
        let mut current = self.root.clone();
        for component in relative_parent.components() {
            let Component::Normal(name) = component else {
                return Err(Report::new(ToolError::PathOutsideWorkspace {
                    path: requested_parent.to_path_buf(),
                }));
            };
            let candidate = current.join(name);
            match fs::create_dir(&candidate).await {
                Ok(()) => {}
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(io_error("create a new file's parent directory", source));
                }
            }
            let canonical = fs::canonicalize(&candidate)
                .await
                .map_err(|source| io_error("resolve a new file's parent directory", source))?;
            current = self.ensure_inside(canonical)?;
            let metadata = fs::metadata(&current)
                .await
                .map_err(|source| io_error("inspect a new file's parent directory", source))?;
            if !metadata.is_dir() {
                return Err(Report::new(ToolError::InvalidArguments {
                    tool: "write".to_owned(),
                    message: "a new file's parent path is not a directory".to_owned(),
                }));
            }
        }
        Ok(current)
    }

    fn safe_mutation_relative(&self, path: &Path) -> ToolResult<PathBuf> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.root).map_err(|_| {
                Report::new(ToolError::PathOutsideWorkspace {
                    path: path.to_path_buf(),
                })
            })?
        } else {
            path
        };
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(Report::new(ToolError::PathOutsideWorkspace {
                path: path.to_path_buf(),
            }));
        }
        Ok(relative.to_path_buf())
    }

    fn requested_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    fn ensure_inside(&self, path: PathBuf) -> ToolResult<PathBuf> {
        if path.starts_with(&self.root) {
            return Ok(path);
        }
        Err(Report::new(ToolError::PathOutsideWorkspace { path }))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TextFileEdit {
    pub(crate) path: PathBuf,
    pub(crate) edits: Vec<TextEdit>,
}

#[derive(Clone, Debug)]
pub(crate) struct TextEdit {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
}

/// One bounded text read and its continuation metadata.
pub(crate) struct TextRead {
    pub(crate) content: String,
    pub(crate) byte_range: Range<usize>,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) total_lines: usize,
    pub(crate) next_offset: Option<usize>,
    pub(crate) next_byte_offset: usize,
    pub(crate) byte_limited: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileRevision([u8; 32]);

impl FileRevision {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

#[derive(Clone, Debug)]
struct FileObservation {
    revision: FileRevision,
    coverage: Vec<Range<usize>>,
    observed_empty_file: bool,
}

impl FileObservation {
    fn new(revision: FileRevision) -> Self {
        Self {
            revision,
            coverage: Vec::new(),
            observed_empty_file: false,
        }
    }

    fn complete(revision: FileRevision, length: usize) -> Self {
        let mut observation = Self::new(revision);
        observation.observe(0..length, length);
        observation
    }

    fn observe(&mut self, range: Range<usize>, file_length: usize) {
        if file_length == 0 {
            self.observed_empty_file = true;
            return;
        }
        if range.is_empty() {
            return;
        }
        self.coverage.push(range);
        self.coverage.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(self.coverage.len());
        for range in self.coverage.drain(..) {
            if let Some(previous) = merged.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
                continue;
            }
            merged.push(range);
        }
        self.coverage = merged;
    }

    fn contains(&self, range: &Range<usize>) -> bool {
        self.coverage
            .iter()
            .any(|observed| observed.start <= range.start && observed.end >= range.end)
    }

    fn is_complete(&self, file_length: usize) -> bool {
        if file_length == 0 {
            return self.observed_empty_file;
        }
        self.contains(&(0..file_length))
    }
}

enum MutationTarget {
    Existing(PathBuf),
    New(PathBuf),
}

struct ObservedFile {
    bytes: Vec<u8>,
    observation: FileObservation,
}

struct PreparedMutation {
    target: PathBuf,
    content: Vec<u8>,
    original_content: Vec<u8>,
    original_revision: Option<FileRevision>,
    permissions: Option<std::fs::Permissions>,
    staged: Option<PathBuf>,
}

impl PreparedMutation {
    async fn existing(
        target: PathBuf,
        content: Vec<u8>,
        original_content: Vec<u8>,
        original_revision: FileRevision,
    ) -> ToolResult<Self> {
        let metadata = fs::metadata(&target)
            .await
            .map_err(|source| io_error("inspect a file before changing it", source))?;
        Ok(Self {
            target,
            content,
            original_content,
            original_revision: Some(original_revision),
            permissions: Some(metadata.permissions()),
            staged: None,
        })
    }

    fn new(target: PathBuf, content: Vec<u8>) -> Self {
        Self {
            target,
            content,
            original_content: Vec::new(),
            original_revision: None,
            permissions: None,
            staged: None,
        }
    }

    async fn stage(&mut self) -> ToolResult<()> {
        let parent = self.target.parent().ok_or_else(|| {
            Report::new(ToolError::PathOutsideWorkspace {
                path: self.target.clone(),
            })
        })?;
        let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".tasci-");
        name.push(std::process::id().to_string());
        name.push("-");
        name.push(sequence.to_string());
        name.push(".tmp");
        let temporary = parent.join(name);
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await
            .map_err(|source| io_error("create a staged file", source))?;
        self.staged = Some(temporary.clone());
        file.write_all(&self.content)
            .await
            .map_err(|source| io_error("write a staged file", source))?;
        file.sync_all()
            .await
            .map_err(|source| io_error("synchronize a staged file", source))?;
        drop(file);
        if let Some(permissions) = self.permissions.clone() {
            fs::set_permissions(&temporary, permissions)
                .await
                .map_err(|source| io_error("preserve file permissions", source))?;
        }
        Ok(())
    }

    async fn validate(&self) -> ToolResult<()> {
        match self.original_revision {
            Some(expected) => {
                let bytes = fs::read(&self.target)
                    .await
                    .map_err(|source| io_error("validate a file before changing it", source))?;
                if FileRevision::from_bytes(&bytes) != expected {
                    return Err(Report::new(ToolError::StaleFile {
                        path: self.target.clone(),
                    }));
                }
            }
            None => match fs::symlink_metadata(&self.target).await {
                Ok(_) => {
                    return Err(Report::new(ToolError::StaleFile {
                        path: self.target.clone(),
                    }));
                }
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(io_error("validate a new file path", source)),
            },
        }
        Ok(())
    }

    async fn commit(&mut self) -> ToolResult<()> {
        let staged = self.staged.take().ok_or_else(|| {
            Report::new(ToolError::InvalidArguments {
                tool: "file_workspace".to_owned(),
                message: "file mutation was not staged".to_owned(),
            })
        })?;
        if self.original_revision.is_some() {
            fs::rename(&staged, &self.target)
                .await
                .map_err(|source| io_error("replace a file atomically", source))?;
        } else {
            fs::hard_link(&staged, &self.target)
                .await
                .map_err(|source| io_error("create a new file atomically", source))?;
            if let Err(source) = fs::remove_file(&staged).await {
                warn!(
                    path = %staged.display(),
                    error = %source,
                    "failed to remove a staged file after committing it"
                );
            }
        }
        Ok(())
    }
}

/// Stages and validates a complete batch before entering its commit phase.
///
/// Cancellation is deliberately not observed after commit starts because
/// dropping a partially committed multi-file mutation would be less safe than
/// completing the prepared batch.
async fn commit_mutations(
    mut prepared: Vec<PreparedMutation>,
    root: &Path,
    observations: &RwLock<HashMap<PathBuf, FileObservation>>,
    cancellation: &CancellationToken,
) -> ToolResult<Vec<FileChange>> {
    for index in 0..prepared.len() {
        ensure_running(cancellation)?;
        if let Err(error) = prepared[index].stage().await {
            cleanup_staged(&mut prepared).await;
            return Err(error);
        }
    }
    if let Err(error) = ensure_running(cancellation) {
        cleanup_staged(&mut prepared).await;
        return Err(error);
    }
    for mutation in &prepared {
        if let Err(error) = mutation.validate().await {
            cleanup_staged(&mut prepared).await;
            return Err(error);
        }
    }
    if let Err(error) = ensure_running(cancellation) {
        cleanup_staged(&mut prepared).await;
        return Err(error);
    }

    for index in 0..prepared.len() {
        if let Err(error) = prepared[index].commit().await {
            cleanup_staged(&mut prepared).await;
            return Err(error);
        }
    }

    let mut observations = observations.write().await;
    let mut changes = Vec::with_capacity(prepared.len());
    for mutation in prepared {
        let revision = FileRevision::from_bytes(&mutation.content);
        observations.insert(
            mutation.target.clone(),
            FileObservation::complete(revision, mutation.content.len()),
        );
        if let Some(change) = file_change(root, &mutation)? {
            changes.push(change);
        }
    }
    Ok(changes)
}

/// Removes every staged file that has not entered the commit phase.
async fn cleanup_staged(prepared: &mut [PreparedMutation]) {
    for mutation in prepared {
        let Some(path) = mutation.staged.take() else {
            continue;
        };
        if let Err(source) = fs::remove_file(&path).await {
            warn!(
                path = %path.display(),
                error = %source,
                "failed to remove an uncommitted staged file"
            );
        }
    }
}

/// Applies unique, non-overlapping replacements against one original snapshot.
fn apply_edits(
    path: &Path,
    content: &str,
    edits: Vec<TextEdit>,
    coverage: &[Range<usize>],
) -> ToolResult<String> {
    if edits.is_empty() {
        return Err(invalid_edit(path, "edits must not be empty"));
    }
    let mut matches = Vec::with_capacity(edits.len());
    for edit in edits {
        if edit.old_text.is_empty() {
            return Err(invalid_edit(path, "oldText must not be empty"));
        }
        let positions = content
            .match_indices(&edit.old_text)
            .map(|(start, _)| start)
            .collect::<Vec<_>>();
        if positions.len() != 1 {
            return Err(invalid_edit(
                path,
                "every oldText must match exactly once in the original file",
            ));
        }
        let start = positions[0];
        let range = start..start + edit.old_text.len();
        if !coverage
            .iter()
            .any(|observed| observed.start <= range.start && observed.end >= range.end)
        {
            return Err(Report::new(ToolError::UnobservedEdit {
                path: path.to_path_buf(),
            }));
        }
        matches.push((start, range.end, edit.new_text));
    }
    matches.sort_by_key(|(start, _, _)| *start);
    if matches.windows(2).any(|window| window[0].1 > window[1].0) {
        return Err(invalid_edit(path, "edit ranges must not overlap"));
    }

    let mut result = String::with_capacity(content.len());
    let mut cursor = 0;
    for (start, end, new_text) in matches {
        result.push_str(&content[cursor..start]);
        result.push_str(&new_text);
        cursor = end;
    }
    result.push_str(&content[cursor..]);
    Ok(result)
}

fn select_text(
    content: &str,
    offset: usize,
    byte_offset: usize,
    limit: usize,
    max_bytes: usize,
) -> ToolResult<TextRead> {
    if offset == 0 {
        return Err(read_arguments_error("offset must be at least 1"));
    }
    if limit == 0 {
        return Err(read_arguments_error("limit must be at least 1"));
    }
    if max_bytes == 0 {
        return Err(read_arguments_error(
            "the configured byte limit must be positive",
        ));
    }

    let line_ranges = text_line_ranges(content);
    if !line_ranges.is_empty() && offset > line_ranges.len() {
        return Err(read_arguments_error(format!(
            "offset {offset} exceeds the file's {} lines",
            line_ranges.len()
        )));
    }
    if line_ranges.is_empty() && offset != 1 {
        return Err(read_arguments_error("an empty file only accepts offset 1"));
    }
    if line_ranges.is_empty() && byte_offset != 0 {
        return Err(read_arguments_error(
            "an empty file only accepts byteOffset 0",
        ));
    }

    let start_index = offset.saturating_sub(1);
    let end_index = start_index.saturating_add(limit).min(line_ranges.len());
    let start_line = line_ranges.get(start_index);
    let start = start_line.map_or(0, |range| range.start + byte_offset);
    if let Some(range) = start_line
        && (start > range.end || !content.is_char_boundary(start))
    {
        return Err(read_arguments_error(
            "byteOffset must identify a UTF-8 boundary within the first selected line",
        ));
    }
    let requested_end = line_ranges
        .get(end_index.saturating_sub(1))
        .map_or(start, |range| range.end);
    let mut end = requested_end.min(start.saturating_add(max_bytes));
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    if end == start && end < requested_end {
        end += content[start..].chars().next().map_or(0, char::len_utf8);
    }
    let byte_limited = end < requested_end;
    let shown_end_line = line_ranges
        .iter()
        .take(end_index)
        .rposition(|range| range.end <= end)
        .map(|index| index + 1);
    let next_cursor = if byte_limited {
        read_cursor_for_byte(&line_ranges, end)
    } else if end_index < line_ranges.len() {
        Some((end_index + 1, 0))
    } else {
        None
    };

    Ok(TextRead {
        content: content[start..end].to_owned(),
        byte_range: start..end,
        start_line: offset,
        end_line: shown_end_line.unwrap_or(offset),
        total_lines: line_ranges.len(),
        next_offset: next_cursor.map(|cursor| cursor.0),
        next_byte_offset: next_cursor.map_or(0, |cursor| cursor.1),
        byte_limited,
    })
}

fn read_cursor_for_byte(line_ranges: &[Range<usize>], byte: usize) -> Option<(usize, usize)> {
    for (index, range) in line_ranges.iter().enumerate() {
        if range.start <= byte && byte < range.end {
            return Some((index + 1, byte - range.start));
        }
        if byte == range.end && index + 1 < line_ranges.len() {
            return Some((index + 2, 0));
        }
    }
    None
}

fn text_line_ranges(content: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, _) in content.match_indices('\n') {
        ranges.push(start..index + 1);
        start = index + 1;
    }
    if start < content.len() {
        ranges.push(start..content.len());
    }
    ranges
}

fn file_change(root: &Path, mutation: &PreparedMutation) -> ToolResult<Option<FileChange>> {
    if mutation.original_content == mutation.content {
        return Ok(None);
    }
    let old_content = String::from_utf8(mutation.original_content.clone()).map_err(|_| {
        Report::new(ToolError::NonUtf8File {
            path: mutation.target.clone(),
        })
    })?;
    let new_content = String::from_utf8(mutation.content.clone()).map_err(|_| {
        Report::new(ToolError::NonUtf8File {
            path: mutation.target.clone(),
        })
    })?;
    let relative_path = mutation
        .target
        .strip_prefix(root)
        .map_err(|_| {
            Report::new(ToolError::PathOutsideWorkspace {
                path: mutation.target.clone(),
            })
        })?
        .to_path_buf();
    let display_path = relative_path.to_string_lossy();
    let old_header = mutation
        .original_revision
        .map_or_else(|| "/dev/null".to_owned(), |_| format!("a/{display_path}"));
    let new_header = format!("b/{display_path}");
    let diff = TextDiff::from_lines(&old_content, &new_content);
    let unified_diff = diff
        .unified_diff()
        .header(&old_header, &new_header)
        .to_string();
    let mut additions = 0;
    let mut deletions = 0;
    let mut first_changed_line = None;
    let mut new_line = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => new_line += 1,
            ChangeTag::Insert => {
                additions += 1;
                first_changed_line.get_or_insert(new_line + 1);
                new_line += 1;
            }
            ChangeTag::Delete => {
                deletions += 1;
                first_changed_line.get_or_insert(new_line + 1);
            }
        }
    }
    Ok(Some(FileChange {
        path: relative_path,
        operation: if mutation.original_revision.is_some() {
            FileChangeOperation::Modified
        } else {
            FileChangeOperation::Created
        },
        unified_diff,
        additions,
        deletions,
        first_changed_line,
    }))
}

fn ensure_running(cancellation: &CancellationToken) -> ToolResult<()> {
    if cancellation.is_cancelled() {
        return Err(Report::new(ToolError::Cancelled));
    }
    Ok(())
}

fn invalid_edit(path: &Path, message: impl Into<String>) -> Report<ToolError> {
    Report::new(ToolError::InvalidEdit {
        path: path.to_path_buf(),
        message: message.into(),
    })
}

fn read_arguments_error(message: impl Into<String>) -> Report<ToolError> {
    Report::new(ToolError::InvalidArguments {
        tool: "read".to_owned(),
        message: message.into(),
    })
}

fn io_error(action: &'static str, source: std::io::Error) -> Report<ToolError> {
    Report::new(ToolError::Io { action, source })
}
