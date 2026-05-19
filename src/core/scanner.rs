use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use tokio::{
    sync::{Semaphore, mpsc::UnboundedSender},
    task,
};

use super::{Node, NodeKind, ScanResult};

const MIN_PARALLELISM: usize = 8;

#[derive(Debug, Clone)]
pub enum ScanProgressEvent {
    Started { root: String },
    Scanned { path: String, kind: NodeKind },
    Warning { message: String },
}

pub async fn scan_directory_with_progress(
    path: PathBuf,
    progress_tx: UnboundedSender<ScanProgressEvent>,
) -> io::Result<ScanResult> {
    scan_directory_internal(path, Some(progress_tx)).await
}

pub fn current_disk_root_from(path: &Path) -> io::Result<PathBuf> {
    let cwd = path.canonicalize()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut cursor = cwd;
        let device = std::fs::metadata(&cursor)?.dev();

        while let Some(parent) = cursor.parent() {
            if std::fs::metadata(parent)?.dev() != device {
                break;
            }
            cursor = parent.to_path_buf();
        }
        Ok(cursor)
    }

    #[cfg(windows)]
    {
        use std::path::Component;

        let mut root = PathBuf::new();
        for component in cwd.components() {
            match component {
                Component::Prefix(prefix) => root.push(prefix.as_os_str()),
                Component::RootDir => {
                    root.push(std::path::MAIN_SEPARATOR.to_string());
                    break;
                }
                _ => break,
            }
        }

        if root.as_os_str().is_empty() {
            Err(io::Error::other("failed to resolve current drive root"))
        } else {
            Ok(root)
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        Ok(PathBuf::from(std::path::MAIN_SEPARATOR.to_string()))
    }
}

async fn scan_directory_internal(
    path: PathBuf,
    progress_tx: Option<UnboundedSender<ScanProgressEvent>>,
) -> io::Result<ScanResult> {
    let started = Instant::now();
    emit_progress(
        progress_tx.as_ref(),
        ScanProgressEvent::Started {
            root: path_to_string(&path),
        },
    );

    let semaphore = Arc::new(Semaphore::new(default_parallelism()));
    let (root, warnings) = scan_node(path, semaphore, progress_tx).await?;
    let scan_duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    Ok(ScanResult::new(root, warnings, scan_duration_ms))
}

fn scan_node(
    path: PathBuf,
    semaphore: Arc<Semaphore>,
    progress_tx: Option<UnboundedSender<ScanProgressEvent>>,
) -> Pin<Box<dyn Future<Output = io::Result<(Node, Vec<String>)>> + Send>> {
    Box::pin(async move {
        let metadata = task::spawn_blocking({
            let path = path.clone();
            move || std::fs::symlink_metadata(path)
        })
        .await
        .map_err(join_error_to_io)??;

        let kind = if metadata.file_type().is_symlink() {
            NodeKind::Symlink
        } else if metadata.is_dir() {
            NodeKind::Directory
        } else {
            NodeKind::File
        };

        if kind != NodeKind::Directory {
            let size = if kind == NodeKind::File {
                metadata.len()
            } else {
                0
            };
            let node = Node {
                name: display_name(&path),
                path: path_to_string(&path),
                kind,
                size,
                children: Vec::new(),
            };
            emit_progress(
                progress_tx.as_ref(),
                ScanProgressEvent::Scanned {
                    path: node.path.clone(),
                    kind: node.kind,
                },
            );
            return Ok((node, Vec::new()));
        }

        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| io::Error::other(format!("failed to acquire semaphore: {e}")))?;
        let read_result = task::spawn_blocking({
            let path = path.clone();
            move || read_directory_entries(&path)
        })
        .await
        .map_err(join_error_to_io)?;
        drop(permit);

        let DirReadResult {
            files,
            directories,
            mut warnings,
        } = read_result?;

        for warning in &warnings {
            emit_progress(
                progress_tx.as_ref(),
                ScanProgressEvent::Warning {
                    message: warning.clone(),
                },
            );
        }

        let mut children = files;
        let mut handles = Vec::with_capacity(directories.len());
        for directory in directories {
            let semaphore = semaphore.clone();
            let progress_tx = progress_tx.clone();
            handles.push(tokio::spawn(async move {
                scan_node(directory, semaphore, progress_tx).await
            }));
        }

        for handle in handles {
            match handle.await {
                Ok(Ok((child, mut child_warnings))) => {
                    children.push(child);
                    warnings.append(&mut child_warnings);
                }
                Ok(Err(error)) => {
                    let warning = error.to_string();
                    emit_progress(
                        progress_tx.as_ref(),
                        ScanProgressEvent::Warning {
                            message: warning.clone(),
                        },
                    );
                    warnings.push(warning);
                }
                Err(error) => {
                    let warning = format!("scan task join error: {error}");
                    emit_progress(
                        progress_tx.as_ref(),
                        ScanProgressEvent::Warning {
                            message: warning.clone(),
                        },
                    );
                    warnings.push(warning);
                }
            }
        }

        children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        let size = children.iter().map(|c| c.size).sum();

        let node = Node {
            name: display_name(&path),
            path: path_to_string(&path),
            kind: NodeKind::Directory,
            size,
            children,
        };
        emit_progress(
            progress_tx.as_ref(),
            ScanProgressEvent::Scanned {
                path: node.path.clone(),
                kind: node.kind,
            },
        );
        Ok((node, warnings))
    })
}

struct DirReadResult {
    files: Vec<Node>,
    directories: Vec<PathBuf>,
    warnings: Vec<String>,
}

fn read_directory_entries(path: &Path) -> io::Result<DirReadResult> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut warnings = Vec::new();

    for entry in std::fs::read_dir(path)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "failed to read entry under {}: {error}",
                    path.display()
                ));
                continue;
            }
        };

        let entry_path = entry.path();
        let metadata = match std::fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(format!(
                    "failed to read metadata for {}: {error}",
                    entry_path.display()
                ));
                continue;
            }
        };

        let name = entry.file_name().to_string_lossy().to_string();
        if metadata.file_type().is_symlink() {
            files.push(Node {
                name,
                path: path_to_string(&entry_path),
                kind: NodeKind::Symlink,
                size: 0,
                children: Vec::new(),
            });
            continue;
        }

        if metadata.is_dir() {
            directories.push(entry_path);
            continue;
        }

        files.push(Node {
            name,
            path: path_to_string(&entry_path),
            kind: NodeKind::File,
            size: metadata.len(),
            children: Vec::new(),
        });
    }

    Ok(DirReadResult {
        files,
        directories,
        warnings,
    })
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path_to_string(path))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn default_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|cpu| (cpu.get() * 8).max(MIN_PARALLELISM))
        .unwrap_or(MIN_PARALLELISM)
}

fn join_error_to_io(error: task::JoinError) -> io::Error {
    io::Error::other(format!("task join error: {error}"))
}

fn emit_progress(
    progress_tx: Option<&UnboundedSender<ScanProgressEvent>>,
    event: ScanProgressEvent,
) {
    if let Some(progress_tx) = progress_tx {
        let _ = progress_tx.send(event);
    }
}
