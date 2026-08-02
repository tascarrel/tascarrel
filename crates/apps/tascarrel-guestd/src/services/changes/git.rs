//! Bounded local Git inspection for pod worktrees.
//!
//! This module runs non-networked Git subprocesses with explicit time and
//! output limits. Its primary interfaces produce lightweight repository
//! snapshots, divergent commit metadata, and complete file change sets.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::MAX_RELATIVE_PATH_BYTES;
use tascarrel_api::types::changes as api;
use tascarrel_api::types::files as file_api;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tracing::debug;
use tracing::warn;

/// Limits and executable used for one local Git command.
#[derive(Clone, Debug)]
pub(crate) struct GitConfig {
    /// Git executable resolved by guest process configuration.
    pub(crate) executable: PathBuf,
    /// Maximum duration of one subprocess.
    pub(crate) command_timeout: Duration,
    /// Maximum bytes retained from metadata-producing commands.
    pub(crate) metadata_bytes: usize,
    /// Maximum bytes retained from result-producing commands.
    pub(crate) result_bytes: usize,
    /// Maximum bytes retained from standard error.
    pub(crate) diagnostic_bytes: usize,
}

/// Lightweight status and per-path overlay produced by one coherent refresh.
#[derive(Clone, Debug)]
pub(crate) struct RepositorySnapshot {
    /// Status published through the repository inventory.
    pub(crate) status: api::RepositoryStatus,
    /// Git annotations keyed by repository-relative path.
    pub(crate) files: BTreeMap<String, file_api::FileGitStatus>,
}

/// Caller-relevant failures from local Git inspection.
#[derive(Debug, Error)]
pub(crate) enum GitInspectionError {
    /// An exact requested commit is no longer available.
    #[error("Git revision is unavailable")]
    RevisionUnavailable(api::GitObjectId),
    /// Exact commits have no common ancestor.
    #[error("Git histories are unrelated")]
    UnrelatedHistories,
    /// A configured result or subprocess-output limit was exceeded.
    #[error("Git inspection result exceeds its configured limit")]
    TooLarge,
    /// Git execution or output parsing failed.
    #[error("Git inspection failed: {0}")]
    Failed(String),
}

type GitResult<T> = Result<T, Report<GitInspectionError>>;

/// Inspects lightweight status and changed paths without contacting a remote.
///
/// # Errors
///
/// Returns a report when Git execution fails or emits malformed metadata.
pub(crate) async fn inspect(
    config: &GitConfig,
    root: &Path,
    repository_path: &str,
) -> GitResult<RepositorySnapshot> {
    let status = successful(
        run(
            config,
            root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            None,
            config.metadata_bytes,
        )
        .await
        .map_err(inspection_run_error)?,
        "inspect repository status",
    )?;
    let files = parse_status(&status, repository_path)?;
    let branch = optional_text(
        config,
        root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await?;
    let head_id = optional_object(config, root, "HEAD").await?;
    let head = match head_id.as_ref() {
        Some(head) => Some(commit_summary(config, root, head.as_str()).await?),
        None => None,
    };
    let upstream = if head_id.is_some() {
        upstream_status(config, root).await?
    } else {
        None
    };
    let conflict_count = files
        .values()
        .filter(|status| {
            status.index == Some(file_api::GitChangeKind::Unmerged)
                || status.worktree == Some(file_api::GitChangeKind::Unmerged)
        })
        .count() as u64;
    let file_count = files.len() as u64;
    Ok(RepositorySnapshot {
        status: api::RepositoryStatus {
            branch: branch.map(Into::into),
            head,
            upstream,
            working: api::RepositoryWorkingStatus {
                dirty: file_count != 0,
                file_count,
                conflict_count,
            },
        },
        files,
    })
}

/// Lists commits unique to each side of one exact revision comparison.
///
/// # Errors
///
/// Returns a typed report for unavailable revisions, resource limits, or Git
/// execution and parsing failures.
pub(crate) async fn divergent_commits(
    config: &GitConfig,
    root: &Path,
    comparison: &api::RepositoryDivergence,
) -> GitResult<api::DivergentCommits> {
    require_commit(config, root, &comparison.head).await?;
    require_commit(config, root, &comparison.upstream).await?;
    let ahead = commits_between(
        config,
        root,
        comparison.head.as_str(),
        comparison.upstream.as_str(),
    )
    .await?;
    let behind = commits_between(
        config,
        root,
        comparison.upstream.as_str(),
        comparison.head.as_str(),
    )
    .await?;
    Ok(api::DivergentCommits {
        ahead: ahead.into(),
        behind: behind.into(),
    })
}

/// Builds one complete working-tree or pull-request-style change set.
///
/// # Errors
///
/// Returns a typed report for unavailable revisions, unrelated histories,
/// resource limits, or Git execution and parsing failures.
pub(crate) async fn change_set(
    config: &GitConfig,
    root: &Path,
    comparison: &api::ChangeSetComparison,
    path_filter: Option<&api::RepositoryPath>,
) -> GitResult<api::FileChangeSet> {
    let path_filter = path_filter.map(api::RepositoryPath::as_str);
    if let Some(path_filter) = path_filter {
        validate_relative(path_filter)?;
    }
    let (resolved, before, after, include_untracked) = match comparison {
        api::ChangeSetComparison::Working => {
            let head = optional_object(config, root, "HEAD").await?;
            let before = match head.as_ref() {
                Some(head) => head.as_str().to_owned(),
                None => empty_tree(config, root).await?,
            };
            (
                api::ResolvedChangeSetComparison::Working(api::ResolvedWorkingTreeComparison {
                    head,
                }),
                before,
                None,
                true,
            )
        }
        api::ChangeSetComparison::Commits(commits) => {
            require_commit(config, root, &commits.base).await?;
            require_commit(config, root, &commits.head).await?;
            let merge_base = merge_base(config, root, &commits.base, &commits.head).await?;
            (
                api::ResolvedChangeSetComparison::Commits(api::ResolvedCommitTreeComparison {
                    base: commits.base.clone(),
                    head: commits.head.clone(),
                    merge_base: merge_base.clone(),
                }),
                merge_base.as_str().to_owned(),
                Some(commits.head.as_str().to_owned()),
                false,
            )
        }
    };
    let mut files = diff_inventory(config, root, &before, after.as_deref(), path_filter).await?;
    let mut diff = diff_text(config, root, &before, after.as_deref(), path_filter).await?;
    if include_untracked {
        let untracked = untracked_files(config, root, path_filter).await?;
        for untracked_path in untracked {
            let (file, untracked_diff) = untracked_change(config, root, &untracked_path).await?;
            files.push(file);
            if !diff.is_empty() && !diff.ends_with('\n') {
                diff.push('\n');
            }
            diff.push_str(&untracked_diff);
            if diff.len() > config.result_bytes {
                return Err(GitInspectionError::TooLarge.report());
            }
        }
    }
    files.sort_by(|left, right| {
        display_path(left)
            .cmp(&display_path(right))
            .then_with(|| old_path(left).cmp(&old_path(right)))
    });
    let lines = files.iter().fold(
        api::LineChangeSummary {
            additions: 0,
            deletions: 0,
        },
        |mut summary, file| {
            summary.additions = summary.additions.saturating_add(file.lines.additions);
            summary.deletions = summary.deletions.saturating_add(file.lines.deletions);
            summary
        },
    );
    Ok(api::FileChangeSet {
        comparison: resolved,
        summary: api::FileChangeSetSummary {
            file_count: files.len() as u64,
            lines,
        },
        files: files.into(),
        diff: api::UnifiedDiff::new(diff),
    })
}

async fn upstream_status(
    config: &GitConfig,
    root: &Path,
) -> GitResult<Option<api::RepositoryUpstreamStatus>> {
    let Some(reference) = optional_text(
        config,
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .await?
    else {
        return Ok(None);
    };
    let commit_id = optional_object(config, root, "@{upstream}")
        .await?
        .ok_or_else(|| failed("upstream tracking reference does not identify a commit"))?;
    let commit = commit_summary(config, root, commit_id.as_str()).await?;
    let counts = successful(
        run(
            config,
            root,
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
            None,
            config.metadata_bytes,
        )
        .await
        .map_err(inspection_run_error)?,
        "compare local and upstream commits",
    )?;
    let counts = utf8(&counts, "Git returned non-UTF-8 divergence counts")?;
    let mut counts = counts.split_whitespace();
    let ahead = parse_count(counts.next(), "ahead")?;
    let behind = parse_count(counts.next(), "behind")?;
    Ok(Some(api::RepositoryUpstreamStatus {
        reference: api::GitReference::new(reference),
        commit,
        ahead,
        behind,
    }))
}

async fn commit_summary(
    config: &GitConfig,
    root: &Path,
    revision: &str,
) -> GitResult<api::GitCommitSummary> {
    let output = successful(
        run_owned(
            config,
            root,
            vec![
                "show".to_owned(),
                "-s".to_owned(),
                "--no-show-signature".to_owned(),
                "--format=%H%x00%an%x00%ae%x00%aI%x00%s%x00".to_owned(),
                revision.to_owned(),
            ],
            None,
            config.metadata_bytes,
        )
        .await
        .map_err(inspection_run_error)?,
        "inspect commit summary",
    )?;
    let fields = nul_fields(&output, 5, "commit summary")?;
    Ok(api::GitCommitSummary {
        id: object_id(fields[0])?,
        author: signature(fields[1], fields[2], fields[3])?,
        subject: utf8(fields[4], "Git returned a non-UTF-8 commit subject")?.into(),
    })
}

async fn commits_between(
    config: &GitConfig,
    root: &Path,
    include: &str,
    exclude: &str,
) -> GitResult<Vec<api::GitCommit>> {
    let output = successful(
        run_owned(
            config,
            root,
            vec![
                "log".to_owned(),
                "--reverse".to_owned(),
                "--topo-order".to_owned(),
                "--no-show-signature".to_owned(),
                "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%s%x00%b%x00"
                    .to_owned(),
                include.to_owned(),
                format!("^{exclude}"),
            ],
            None,
            config.result_bytes,
        )
        .await
        .map_err(detail_run_error)?,
        "list divergent commits",
    )?;
    parse_commits(&output)
}

fn parse_commits(output: &[u8]) -> GitResult<Vec<api::GitCommit>> {
    let mut commits = Vec::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(id) = fields.next() {
        let id = id.strip_prefix(b"\n").unwrap_or(id);
        if id.iter().all(u8::is_ascii_whitespace) {
            if fields.all(|field| field.iter().all(u8::is_ascii_whitespace)) {
                break;
            }
            return Err(failed("Git returned malformed commit metadata"));
        }
        let mut record = Vec::with_capacity(10);
        record.push(id);
        for _ in 1..10 {
            record.push(
                fields
                    .next()
                    .ok_or_else(|| failed("Git returned malformed commit metadata"))?,
            );
        }
        let fields = record;
        let parents = utf8(fields[1], "Git returned a non-UTF-8 parent list")?
            .split_whitespace()
            .map(|parent| object_id(parent.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        commits.push(api::GitCommit {
            id: object_id(fields[0])?,
            parents: parents.into(),
            author: signature(fields[2], fields[3], fields[4])?,
            committer: signature(fields[5], fields[6], fields[7])?,
            subject: utf8(fields[8], "Git returned a non-UTF-8 commit subject")?.into(),
            body: utf8(fields[9], "Git returned a non-UTF-8 commit body")?.into(),
        });
    }
    Ok(commits)
}

async fn merge_base(
    config: &GitConfig,
    root: &Path,
    base: &api::GitObjectId,
    head: &api::GitObjectId,
) -> GitResult<api::GitObjectId> {
    let output = run_owned(
        config,
        root,
        vec!["merge-base".to_owned(), base.to_string(), head.to_string()],
        None,
        config.metadata_bytes,
    )
    .await
    .map_err(detail_run_error)?;
    if output.status.code() == Some(1) {
        return Err(GitInspectionError::UnrelatedHistories.report());
    }
    let output = successful(output, "find repository merge base")?;
    object_id(output.strip_suffix(b"\n").unwrap_or(&output))
}

async fn empty_tree(config: &GitConfig, root: &Path) -> GitResult<String> {
    let output = successful(
        run(
            config,
            root,
            &["hash-object", "-t", "tree", "--stdin"],
            Some(&[]),
            config.metadata_bytes,
        )
        .await
        .map_err(detail_run_error)?,
        "calculate the empty Git tree",
    )?;
    Ok(utf8(&output, "Git returned a non-UTF-8 tree identifier")?
        .trim()
        .to_owned())
}

async fn diff_inventory(
    config: &GitConfig,
    root: &Path,
    before: &str,
    after: Option<&str>,
    path: Option<&str>,
) -> GitResult<Vec<api::FileChange>> {
    let mut status_args = vec![
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--find-renames".to_owned(),
        "--find-copies".to_owned(),
        "--raw".to_owned(),
        "-z".to_owned(),
        before.to_owned(),
    ];
    if let Some(after) = after {
        status_args.push(after.to_owned());
    }
    append_path(&mut status_args, path);
    let raw = successful(
        run_owned(config, root, status_args, None, config.metadata_bytes)
            .await
            .map_err(detail_run_error)?,
        "read changed file metadata",
    )?;

    let mut stat_args = vec![
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--find-renames".to_owned(),
        "--find-copies".to_owned(),
        "--numstat".to_owned(),
        "-z".to_owned(),
        before.to_owned(),
    ];
    if let Some(after) = after {
        stat_args.push(after.to_owned());
    }
    append_path(&mut stat_args, path);
    let stats = successful(
        run_owned(config, root, stat_args, None, config.metadata_bytes)
            .await
            .map_err(detail_run_error)?,
        "read changed file statistics",
    )?;
    let stats = parse_numstat(&stats)?;
    let mut files = parse_raw_changes(&raw)?;
    for file in &mut files {
        if let Some(stat) = stats.get(display_path(file).as_str()) {
            file.binary = stat.binary;
            file.lines = api::LineChangeSummary {
                additions: stat.additions,
                deletions: stat.deletions,
            };
        }
    }
    Ok(files)
}

async fn diff_text(
    config: &GitConfig,
    root: &Path,
    before: &str,
    after: Option<&str>,
    path: Option<&str>,
) -> GitResult<String> {
    let mut args = vec![
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--no-color".to_owned(),
        "--find-renames".to_owned(),
        "--find-copies".to_owned(),
        before.to_owned(),
    ];
    if let Some(after) = after {
        args.push(after.to_owned());
    }
    append_path(&mut args, path);
    let output = successful(
        run_owned(config, root, args, None, config.result_bytes)
            .await
            .map_err(detail_run_error)?,
        "generate unified diff",
    )?;
    String::from_utf8(output).map_err(|_| failed("Git returned a unified diff which is not UTF-8"))
}

async fn untracked_files(
    config: &GitConfig,
    root: &Path,
    path: Option<&str>,
) -> GitResult<Vec<String>> {
    let mut args = vec![
        "ls-files".to_owned(),
        "--others".to_owned(),
        "--exclude-standard".to_owned(),
        "-z".to_owned(),
    ];
    append_path(&mut args, path);
    let output = successful(
        run_owned(config, root, args, None, config.metadata_bytes)
            .await
            .map_err(detail_run_error)?,
        "list untracked files",
    )?;
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = utf8(path, "repository contains a non-UTF-8 path")?;
            validate_relative(path)?;
            Ok(path.to_owned())
        })
        .collect::<GitResult<Vec<_>>>()
}

async fn untracked_change(
    config: &GitConfig,
    root: &Path,
    path: &str,
) -> GitResult<(api::FileChange, String)> {
    let args = vec![
        "diff".to_owned(),
        "--no-index".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--no-color".to_owned(),
        "--".to_owned(),
        "/dev/null".to_owned(),
        path.to_owned(),
    ];
    let output = run_owned(config, root, args, None, config.result_bytes)
        .await
        .map_err(detail_run_error)?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(failed(command_failure(
            &output,
            "generate untracked file diff",
        )));
    }
    let unified_diff = String::from_utf8(output.stdout)
        .map_err(|_| failed("Git returned a unified diff which is not UTF-8"))?;

    let stats = run_owned(
        config,
        root,
        vec![
            "diff".to_owned(),
            "--no-index".to_owned(),
            "--no-ext-diff".to_owned(),
            "--numstat".to_owned(),
            "-z".to_owned(),
            "--".to_owned(),
            "/dev/null".to_owned(),
            path.to_owned(),
        ],
        None,
        config.metadata_bytes,
    )
    .await
    .map_err(detail_run_error)?;
    if !matches!(stats.status.code(), Some(0 | 1)) {
        return Err(failed(command_failure(
            &stats,
            "read untracked file statistics",
        )));
    }
    let stat = parse_numstat(&stats.stdout)?
        .into_values()
        .next()
        .unwrap_or_default();
    let mode = match tokio::fs::symlink_metadata(root.join(path)).await {
        Ok(metadata) => Some(if metadata.file_type().is_symlink() {
            "120000"
        } else if metadata.permissions().mode() & 0o111 != 0 {
            "100755"
        } else {
            "100644"
        }),
        Err(error) => {
            debug!(%error, path, "failed to inspect untracked file mode");
            None
        }
    };
    Ok((
        api::FileChange {
            old_path: None,
            new_path: Some(api::RepositoryPath::new(path)),
            kind: file_api::GitChangeKind::Untracked,
            old_mode: None,
            new_mode: mode.map(api::GitFileMode::new),
            binary: stat.binary,
            lines: api::LineChangeSummary {
                additions: stat.additions,
                deletions: stat.deletions,
            },
        },
        unified_diff,
    ))
}

#[derive(Clone, Debug, Default)]
struct DiffStat {
    additions: u64,
    deletions: u64,
    binary: bool,
}

fn parse_numstat(bytes: &[u8]) -> GitResult<BTreeMap<String, DiffStat>> {
    let chunks = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut output = BTreeMap::new();
    let mut index = 0;
    while index < chunks.len() {
        let chunk = chunks[index];
        index += 1;
        if chunk.is_empty() {
            continue;
        }
        let mut fields = chunk.splitn(3, |byte| *byte == b'\t');
        let additions = fields.next().unwrap_or_default();
        let deletions = fields.next().unwrap_or_default();
        let binary = additions == b"-" || deletions == b"-";
        let additions = parse_numstat_count(additions)?;
        let deletions = parse_numstat_count(deletions)?;
        let embedded_path = fields.next().unwrap_or_default();
        let path = if embedded_path.is_empty() {
            index += 1;
            let current = chunks.get(index).copied().unwrap_or_default();
            index += 1;
            utf8(current, "repository contains a non-UTF-8 path")?
        } else {
            utf8(embedded_path, "repository contains a non-UTF-8 path")?
        };
        validate_relative(path)?;
        output.insert(
            path.to_owned(),
            DiffStat {
                additions,
                deletions,
                binary,
            },
        );
    }
    Ok(output)
}

fn parse_raw_changes(bytes: &[u8]) -> GitResult<Vec<api::FileChange>> {
    let chunks = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut index = 0;
    while index < chunks.len() {
        let header = chunks[index];
        index += 1;
        if header.is_empty() {
            continue;
        }
        let header = utf8(header, "Git returned non-UTF-8 change metadata")?;
        let mut fields = header.split_whitespace();
        let old_mode = fields
            .next()
            .and_then(|mode| mode.strip_prefix(':'))
            .ok_or_else(|| failed("Git omitted the old file mode"))?;
        let new_mode = fields
            .next()
            .ok_or_else(|| failed("Git omitted the new file mode"))?;
        let _old_object = fields
            .next()
            .ok_or_else(|| failed("Git omitted the old object"))?;
        let _new_object = fields
            .next()
            .ok_or_else(|| failed("Git omitted the new object"))?;
        let status = fields
            .next()
            .and_then(|status| status.as_bytes().first().copied())
            .ok_or_else(|| failed("Git omitted the file status"))?;
        let first = chunks
            .get(index)
            .copied()
            .ok_or_else(|| failed("Git omitted a changed path"))?;
        index += 1;
        let first = utf8(first, "repository contains a non-UTF-8 path")?;
        validate_relative(first)?;
        let (old_path, new_path, kind) = match status {
            b'A' => (None, Some(first), file_api::GitChangeKind::Added),
            b'D' => (Some(first), None, file_api::GitChangeKind::Deleted),
            b'R' | b'C' => {
                let second = chunks
                    .get(index)
                    .copied()
                    .ok_or_else(|| failed("Git omitted the destination path"))?;
                index += 1;
                let second = utf8(second, "repository contains a non-UTF-8 path")?;
                validate_relative(second)?;
                (
                    Some(first),
                    Some(second),
                    if status == b'R' {
                        file_api::GitChangeKind::Renamed
                    } else {
                        file_api::GitChangeKind::Copied
                    },
                )
            }
            b'T' => (
                Some(first),
                Some(first),
                file_api::GitChangeKind::TypeChanged,
            ),
            b'U' => (Some(first), Some(first), file_api::GitChangeKind::Unmerged),
            _ => (Some(first), Some(first), file_api::GitChangeKind::Modified),
        };
        output.push(api::FileChange {
            old_path: old_path.map(api::RepositoryPath::new),
            new_path: new_path.map(api::RepositoryPath::new),
            kind,
            old_mode: mode(old_mode),
            new_mode: mode(new_mode),
            binary: false,
            lines: api::LineChangeSummary {
                additions: 0,
                deletions: 0,
            },
        });
    }
    Ok(output)
}

fn parse_status(
    bytes: &[u8],
    repository_path: &str,
) -> GitResult<BTreeMap<String, file_api::FileGitStatus>> {
    let chunks = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut output = BTreeMap::new();
    let mut index = 0;
    while index < chunks.len() {
        let record = chunks[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(failed("Git returned malformed worktree status"));
        }
        let path = utf8(&record[3..], "repository contains a non-UTF-8 path")?;
        validate_relative(path)?;
        let code = &record[..2];
        let renamed = code.iter().any(|code| matches!(code, b'R' | b'C'));
        let previous_path = if renamed {
            let previous = chunks
                .get(index)
                .copied()
                .ok_or_else(|| failed("Git omitted a rename source"))?;
            index += 1;
            let previous = utf8(previous, "repository contains a non-UTF-8 path")?;
            validate_relative(previous)?;
            Some(file_api::FilePath::new(join_relative(
                repository_path,
                previous,
            )))
        } else {
            None
        };
        let unmerged = code.contains(&b'U') || matches!(code, b"AA" | b"DD");
        let (index_status, worktree_status) = if unmerged {
            (
                Some(file_api::GitChangeKind::Unmerged),
                Some(file_api::GitChangeKind::Unmerged),
            )
        } else if code == b"??" {
            (None, Some(file_api::GitChangeKind::Untracked))
        } else {
            (status_code(code[0]), status_code(code[1]))
        };
        output.insert(
            path.to_owned(),
            file_api::FileGitStatus {
                previous_path,
                index: index_status,
                worktree: worktree_status,
            },
        );
    }
    Ok(output)
}

fn status_code(code: u8) -> Option<file_api::GitChangeKind> {
    match code {
        b' ' => None,
        b'A' => Some(file_api::GitChangeKind::Added),
        b'D' => Some(file_api::GitChangeKind::Deleted),
        b'R' => Some(file_api::GitChangeKind::Renamed),
        b'C' => Some(file_api::GitChangeKind::Copied),
        b'T' => Some(file_api::GitChangeKind::TypeChanged),
        b'U' => Some(file_api::GitChangeKind::Unmerged),
        _ => Some(file_api::GitChangeKind::Modified),
    }
}

async fn require_commit(
    config: &GitConfig,
    root: &Path,
    revision: &api::GitObjectId,
) -> GitResult<()> {
    validate_object_id(revision.as_str())?;
    let output = run_owned(
        config,
        root,
        vec![
            "cat-file".to_owned(),
            "-e".to_owned(),
            format!("{}^{{commit}}", revision.as_str()),
        ],
        None,
        config.metadata_bytes,
    )
    .await
    .map_err(detail_run_error)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GitInspectionError::RevisionUnavailable(revision.clone()).report())
    }
}

async fn optional_object(
    config: &GitConfig,
    root: &Path,
    revision: &str,
) -> GitResult<Option<api::GitObjectId>> {
    let output = run_owned(
        config,
        root,
        vec![
            "rev-parse".to_owned(),
            "--verify".to_owned(),
            format!("{revision}^{{commit}}"),
        ],
        None,
        config.metadata_bytes,
    )
    .await
    .map_err(inspection_run_error)?;
    if !output.status.success() {
        return Ok(None);
    }
    object_id(output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout)).map(Some)
}

async fn optional_text(
    config: &GitConfig,
    root: &Path,
    arguments: &[&str],
) -> GitResult<Option<String>> {
    let output = run(config, root, arguments, None, config.metadata_bytes)
        .await
        .map_err(inspection_run_error)?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = utf8(&output.stdout, "Git returned non-UTF-8 reference metadata")?
        .trim()
        .to_owned();
    Ok((!text.is_empty()).then_some(text))
}

fn signature(name: &[u8], email: &[u8], timestamp: &[u8]) -> GitResult<api::GitSignature> {
    let timestamp = utf8(timestamp, "Git returned a non-UTF-8 commit timestamp")?
        .parse()
        .map_err(|error| failed(format!("Git returned an invalid commit timestamp: {error}")))?;
    Ok(api::GitSignature {
        name: utf8(name, "Git returned a non-UTF-8 signature name")?.into(),
        email: utf8(email, "Git returned a non-UTF-8 signature email")?.into(),
        timestamp,
    })
}

fn object_id(bytes: &[u8]) -> GitResult<api::GitObjectId> {
    let value = utf8(bytes, "Git returned a non-UTF-8 object identifier")?.trim();
    validate_object_id(value)?;
    Ok(api::GitObjectId::new(value))
}

fn validate_object_id(value: &str) -> GitResult<()> {
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(failed(
            "Git object identifier must be a complete hexadecimal hash",
        ))
    }
}

fn nul_fields<'a>(bytes: &'a [u8], count: usize, context: &str) -> GitResult<Vec<&'a [u8]>> {
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.len() < count
        || fields[count..]
            .iter()
            .any(|field| !field.iter().all(u8::is_ascii_whitespace))
    {
        Err(failed(format!("Git returned malformed {context}")))
    } else {
        Ok(fields[..count].to_vec())
    }
}

fn parse_count(value: Option<&str>, name: &str) -> GitResult<u64> {
    value
        .ok_or_else(|| failed(format!("Git omitted the {name} count")))?
        .parse()
        .map_err(|_| failed(format!("Git returned an invalid {name} count")))
}

fn parse_numstat_count(bytes: &[u8]) -> GitResult<u64> {
    if bytes == b"-" {
        return Ok(0);
    }
    utf8(bytes, "Git returned non-UTF-8 diff statistics")?
        .parse()
        .map_err(|_| failed("Git returned invalid diff statistics"))
}

fn append_path(arguments: &mut Vec<String>, path: Option<&str>) {
    if let Some(path) = path {
        arguments.push("--".to_owned());
        arguments.push(path.to_owned());
    }
}

fn display_path(file: &api::FileChange) -> String {
    file.new_path
        .as_ref()
        .or(file.old_path.as_ref())
        .map_or_else(String::new, ToString::to_string)
}

fn old_path(file: &api::FileChange) -> String {
    file.old_path
        .as_ref()
        .map_or_else(String::new, ToString::to_string)
}

fn mode(value: &str) -> Option<api::GitFileMode> {
    (value != "000000").then(|| api::GitFileMode::new(value))
}

fn validate_relative(path: &str) -> GitResult<()> {
    if path.is_empty()
        || path.len() > MAX_RELATIVE_PATH_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        Err(failed("repository path must be normalized and relative"))
    } else {
        Ok(())
    }
}

fn join_relative(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}

fn utf8<'a>(bytes: &'a [u8], message: &str) -> GitResult<&'a str> {
    std::str::from_utf8(bytes).map_err(|_| failed(message))
}

#[derive(Debug, Error)]
enum RunError {
    #[error("Git subprocess failed: {0}")]
    Failed(String),
    #[error("Git subprocess output exceeds its configured limit")]
    OutputTooLarge,
}

struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stderr_truncated: bool,
}

async fn run(
    config: &GitConfig,
    root: &Path,
    arguments: &[&str],
    input: Option<&[u8]>,
    limit: usize,
) -> Result<GitOutput, Report<RunError>> {
    run_owned(
        config,
        root,
        arguments.iter().map(ToString::to_string).collect(),
        input,
        limit,
    )
    .await
}

async fn run_owned(
    config: &GitConfig,
    root: &Path,
    arguments: Vec<String>,
    input: Option<&[u8]>,
    limit: usize,
) -> Result<GitOutput, Report<RunError>> {
    let mut command = Command::new(&config.executable);
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("diff.external=")
        .arg("-c")
        .arg("credential.helper=")
        .arg("-c")
        .arg(format!("safe.directory={}", root.display()))
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| run_failed(format!("failed to start Git: {error}")))?;
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| run_failed("failed to capture Git stdin"))?;
        stdin
            .write_all(input)
            .await
            .map_err(|error| run_failed(format!("failed to write Git input: {error}")))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| run_failed(format!("failed to finish Git input: {error}")))?;
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| run_failed("failed to capture Git stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| run_failed("failed to capture Git stderr"))?;
    let collection = tokio::time::timeout(config.command_timeout, async {
        tokio::join!(
            read_bounded(stdout, limit),
            read_bounded(stderr, config.diagnostic_bytes),
            child.wait(),
        )
    })
    .await;
    let Ok((stdout, stderr, status)) = collection else {
        if let Err(error) = child.kill().await {
            warn!(%error, "failed to kill timed-out Git command");
        }
        if let Err(error) = child.wait().await {
            warn!(%error, "failed to reap timed-out Git command");
        }
        return Err(run_failed("Git command timed out"));
    };
    let stdout =
        stdout.map_err(|error| run_failed(format!("failed to read Git output: {error}")))?;
    let stderr =
        stderr.map_err(|error| run_failed(format!("failed to read Git diagnostic: {error}")))?;
    let status = status.map_err(|error| run_failed(format!("failed to wait for Git: {error}")))?;
    if stdout.truncated {
        return Err(RunError::OutputTooLarge.report());
    }
    Ok(GitOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stderr_truncated: stderr.truncated,
    })
}

struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<BoundedBytes> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let retained = limit.saturating_sub(bytes.len()).min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok(BoundedBytes { bytes, truncated })
}

fn successful(output: GitOutput, action: &str) -> GitResult<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(failed(command_failure(&output, action)))
    }
}

fn command_failure(output: &GitOutput, action: &str) -> String {
    let mut diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if diagnostic.is_empty() {
        diagnostic = output.status.to_string();
    }
    if output.stderr_truncated {
        diagnostic.push_str(" [diagnostic truncated]");
    }
    format!("{action} failed: {diagnostic}")
}

fn inspection_run_error(report: Report<RunError>) -> Report<GitInspectionError> {
    let error = match report.error() {
        RunError::Failed(message) => GitInspectionError::Failed(message.clone()),
        RunError::OutputTooLarge => {
            GitInspectionError::Failed("Git metadata exceeded its configured bound".to_owned())
        }
    };
    report.escalate(error)
}

fn detail_run_error(report: Report<RunError>) -> Report<GitInspectionError> {
    let error = match report.error() {
        RunError::Failed(message) => GitInspectionError::Failed(message.clone()),
        RunError::OutputTooLarge => GitInspectionError::TooLarge,
    };
    report.escalate(error)
}

fn run_failed(message: impl Into<String>) -> Report<RunError> {
    RunError::Failed(message.into()).report()
}

fn failed(message: impl Into<String>) -> Report<GitInspectionError> {
    GitInspectionError::Failed(message.into()).report()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates bounded Git settings for local repository fixtures.
    fn fixture_config() -> GitConfig {
        GitConfig {
            executable: PathBuf::from("git"),
            command_timeout: Duration::from_secs(5),
            metadata_bytes: 1024 * 1024,
            result_bytes: 1024 * 1024,
            diagnostic_bytes: 64 * 1024,
        }
    }

    /// Runs one successful Git command against a repository fixture.
    fn fixture_git(root: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .output()
            .expect("fixture Git command starts");
        assert!(
            output.status.success(),
            "fixture Git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("fixture Git output is UTF-8")
            .trim()
            .to_owned()
    }

    /// Resolves a fixture executable from the hermetic test environment.
    fn fixture_program(name: &str) -> PathBuf {
        let path = std::env::var_os("PATH").expect("fixture PATH is configured");
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| panic!("fixture program {name} is available"))
    }

    /// Preserves staged and worktree columns while decoding porcelain status.
    #[test]
    fn porcelain_status_preserves_both_change_columns() {
        let parsed = parse_status(b"MM src/lib.rs\0?? notes.txt\0", "repository")
            .expect("status is decoded");
        assert_eq!(
            parsed["src/lib.rs"].index,
            Some(file_api::GitChangeKind::Modified)
        );
        assert_eq!(
            parsed["src/lib.rs"].worktree,
            Some(file_api::GitChangeKind::Modified)
        );
        assert_eq!(parsed["notes.txt"].index, None);
        assert_eq!(
            parsed["notes.txt"].worktree,
            Some(file_api::GitChangeKind::Untracked)
        );
    }

    /// Decodes rename paths from raw diff metadata in their old/new order.
    #[test]
    fn raw_changes_preserve_rename_paths() {
        let changes = parse_raw_changes(
            b":100644 100644 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb R100\0old.rs\0new.rs\0",
        )
        .expect("raw change is decoded");
        assert_eq!(
            changes[0]
                .old_path
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("old.rs")
        );
        assert_eq!(
            changes[0]
                .new_path
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("new.rs")
        );
        assert_eq!(changes[0].kind, file_api::GitChangeKind::Renamed);
    }

    /// Empty optional commit fields remain aligned across multiple records.
    #[test]
    fn commits_preserve_empty_fields() {
        let hash = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut output = Vec::new();
        for subject in [b"first".as_slice(), b"".as_slice()] {
            if !output.is_empty() {
                output.push(b'\n');
            }
            for field in [
                hash.as_slice(),
                b"".as_slice(),
                b"Author".as_slice(),
                b"author@example.com".as_slice(),
                b"2026-01-01T00:00:00Z".as_slice(),
                b"Committer".as_slice(),
                b"committer@example.com".as_slice(),
                b"2026-01-01T00:00:00Z".as_slice(),
                subject,
                b"".as_slice(),
            ] {
                output.extend_from_slice(field);
                output.push(0);
            }
        }
        output.push(b'\n');

        let commits = parse_commits(&output).expect("commit metadata is decoded");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject.as_ref(), "first");
        assert_eq!(commits[1].subject.as_ref(), "");
        assert_eq!(commits[1].body.as_ref(), "");
    }

    /// Exercises status, live working changes, commit comparison, and
    /// divergence against a real local repository without contacting a remote.
    #[tokio::test]
    async fn local_repository_queries_are_coherent() {
        let root = tempfile::tempdir().expect("repository fixture is created");
        fixture_git(root.path(), &["init", "--quiet"]);
        fixture_git(root.path(), &["config", "user.name", "Tascarrel Test"]);
        fixture_git(
            root.path(),
            &["config", "user.email", "tascarrel@example.test"],
        );
        std::fs::write(root.path().join("tracked.txt"), "base\n")
            .expect("tracked fixture is written");
        fixture_git(root.path(), &["add", "tracked.txt"]);
        fixture_git(root.path(), &["commit", "--quiet", "-m", "base"]);
        let base = api::GitObjectId::new(fixture_git(root.path(), &["rev-parse", "HEAD"]));

        std::fs::write(root.path().join("tracked.txt"), "base\nchanged\n")
            .expect("tracked fixture is changed");
        std::fs::write(root.path().join("untracked.txt"), "new\n")
            .expect("untracked fixture is written");

        let snapshot = inspect(&fixture_config(), root.path(), "repo")
            .await
            .expect("repository status is inspected");
        assert!(snapshot.status.working.dirty);
        assert_eq!(snapshot.status.working.file_count, 2);
        assert_eq!(
            snapshot.files["untracked.txt"].worktree,
            Some(file_api::GitChangeKind::Untracked)
        );

        let working = change_set(
            &fixture_config(),
            root.path(),
            &api::ChangeSetComparison::Working,
            None,
        )
        .await
        .expect("working change set is generated");
        assert_eq!(working.summary.file_count, 2);
        assert!(working.diff.as_str().contains("tracked.txt"));
        assert!(working.diff.as_str().contains("untracked.txt"));

        fixture_git(root.path(), &["add", "."]);
        fixture_git(
            root.path(),
            &["commit", "--quiet", "-m", "second", "-m", "commit body"],
        );
        let head = api::GitObjectId::new(fixture_git(root.path(), &["rev-parse", "HEAD"]));
        let comparison = api::RepositoryDivergence {
            head: head.clone(),
            upstream: base.clone(),
        };
        let divergence = divergent_commits(&fixture_config(), root.path(), &comparison)
            .await
            .expect("divergent commits are generated");
        assert_eq!(divergence.ahead.len(), 1);
        assert!(divergence.behind.is_empty());
        assert_eq!(divergence.ahead[0].subject.as_ref(), "second");
        assert_eq!(divergence.ahead[0].body.as_ref(), "commit body\n");

        let committed = change_set(
            &fixture_config(),
            root.path(),
            &api::ChangeSetComparison::Commits(api::CommitTreeComparison { base, head }),
            None,
        )
        .await
        .expect("committed change set is generated");
        assert_eq!(committed.summary.file_count, 2);
        assert!(matches!(
            committed.comparison,
            api::ResolvedChangeSetComparison::Commits(_)
        ));
    }

    /// Terminates a Git subprocess at the configured deadline instead of
    /// waiting for its output pipes to close.
    #[tokio::test(start_paused = true)]
    async fn git_command_timeout_kills_the_subprocess() {
        let root = tempfile::tempdir().expect("command fixture is created");
        let executable = root.path().join("slow-git");
        let shell = fixture_program("sh");
        std::fs::write(
            &executable,
            format!("#!{}\nkill -s STOP \"$$\"\n", shell.display()),
        )
        .expect("command fixture is written");
        let mut permissions = std::fs::metadata(&executable)
            .expect("command fixture metadata is read")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).expect("command fixture is executable");
        let mut config = fixture_config();
        config.executable = executable;
        config.command_timeout = Duration::from_millis(25);
        let started = std::time::Instant::now();

        let Err(error) = run(&config, root.path(), &["status"], None, 1024).await else {
            panic!("slow command did not reach its deadline");
        };

        assert!(matches!(
            error.error(),
            RunError::Failed(message) if message == "Git command timed out"
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
