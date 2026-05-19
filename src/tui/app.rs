use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::{
    runtime::Handle,
    sync::{
        mpsc::{UnboundedReceiver, error::TryRecvError as ProgressTryRecvError, unbounded_channel},
        oneshot::{self, error::TryRecvError as DoneTryRecvError},
    },
};

use crate::core::{
    Node, NodeKind, ScanProgressEvent, ScanResult, current_disk_root_from, load_result_sync,
    save_result_sync, scan_directory_with_progress,
};

pub fn run(initial_result: Option<ScanResult>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let app_result = run_app(&mut terminal, App::new(initial_result)?);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    app_result
}

struct FlatNode {
    name: String,
    path: String,
    kind: NodeKind,
    size: u64,
    child_count: usize,
}

struct PendingDelete {
    dir_path: String,
    child_index: usize,
    name: String,
    path: String,
    kind: NodeKind,
}

struct ScanProgressState {
    root: String,
    current_path: String,
    started_at: Instant,
    scanned_nodes: u64,
    scanned_dirs: u64,
    scanned_files: u64,
    scanned_symlinks: u64,
    warnings: u64,
}

struct App {
    result: Option<ScanResult>,
    current_dir_path: Option<String>,
    selected: usize,
    history: Vec<String>,
    history_index: usize,
    status: String,
    pending_delete: Option<PendingDelete>,
    scan_progress: Option<ScanProgressState>,
    scan_progress_rx: Option<UnboundedReceiver<ScanProgressEvent>>,
    scan_done_rx: Option<oneshot::Receiver<io::Result<ScanResult>>>,
    cwd: PathBuf,
    default_result_path: PathBuf,
}

impl App {
    fn new(initial_result: Option<ScanResult>) -> io::Result<Self> {
        let cwd = std::env::current_dir()?;
        let default_result_path = cwd.join("diskanalyzer-result.json");
        let mut app = Self {
            result: None,
            current_dir_path: None,
            selected: 0,
            history: Vec::new(),
            history_index: 0,
            status: "s: scan current dir / a: full disk scan / o: load / p: save".to_string(),
            pending_delete: None,
            scan_progress: None,
            scan_progress_rx: None,
            scan_done_rx: None,
            cwd,
            default_result_path,
        };
        if let Some(result) = initial_result {
            app.set_result(result);
        }
        Ok(app)
    }

    fn set_result(&mut self, result: ScanResult) {
        let root_path = result.root.path.clone();
        let duration_label = format_duration_ms(result.scan_duration_ms);
        self.result = Some(result);
        self.current_dir_path = Some(root_path.clone());
        self.selected = 0;
        self.history = vec![root_path];
        self.history_index = 0;
        self.pending_delete = None;
        self.status = format!("Scan loaded. Duration: {duration_label}");
    }

    fn is_scanning(&self) -> bool {
        self.scan_progress.is_some()
    }

    fn current_dir(&self) -> Option<&Node> {
        let result = self.result.as_ref()?;
        let path = self
            .current_dir_path
            .as_deref()
            .unwrap_or(&result.root.path);
        find_node_by_path(&result.root, path)
    }

    fn selected_node(&self) -> Option<FlatNode> {
        let node = self.current_dir()?.children.get(self.selected)?;
        Some(FlatNode {
            name: node.name.clone(),
            path: node.path.clone(),
            kind: node.kind,
            size: node.size,
            child_count: node.child_count(),
        })
    }

    fn current_children_len(&self) -> usize {
        self.current_dir().map(|n| n.children.len()).unwrap_or(0)
    }

    fn normalize_selection(&mut self) {
        let len = self.current_children_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn next(&mut self) {
        let len = self.current_children_len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected + 1).min(len.saturating_sub(1));
    }

    fn previous(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn open_selected_directory(&mut self) {
        let Some(node) = self
            .current_dir()
            .and_then(|dir| dir.children.get(self.selected))
        else {
            self.status = "No entry selected".to_string();
            return;
        };

        if node.kind != NodeKind::Directory {
            self.status = "Selected entry is not a directory".to_string();
            return;
        }

        self.navigate_to(node.path.clone(), true);
    }

    fn go_parent(&mut self) {
        let Some(result) = &self.result else {
            self.status = "No scan data. Press s or a to start scanning.".to_string();
            return;
        };
        let Some(current_path) = &self.current_dir_path else {
            return;
        };

        if current_path == &result.root.path {
            self.status = "Already at root".to_string();
            return;
        }

        let parent = Path::new(current_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty());

        if let Some(parent_path) = parent
            && find_node_by_path(&result.root, &parent_path).is_some()
        {
            self.navigate_to(parent_path, true);
        } else {
            self.navigate_to(result.root.path.clone(), true);
        }
    }

    fn history_back(&mut self) {
        if self.history_index == 0 {
            self.status = "No older history".to_string();
            return;
        }
        self.history_index -= 1;
        self.restore_history_path();
    }

    fn history_forward(&mut self) {
        if self.history_index + 1 >= self.history.len() {
            self.status = "No newer history".to_string();
            return;
        }
        self.history_index += 1;
        self.restore_history_path();
    }

    fn navigate_to(&mut self, next_path: String, record_history: bool) {
        self.current_dir_path = Some(next_path.clone());
        self.selected = 0;
        if record_history {
            if self.history_index + 1 < self.history.len() {
                self.history.truncate(self.history_index + 1);
            }
            self.history.push(next_path);
            self.history_index = self.history.len() - 1;
        }
        if let Some(dir) = self.current_dir() {
            self.status = format!("Browsing {}", dir.path);
        }
    }

    fn restore_history_path(&mut self) {
        let Some(path) = self.history.get(self.history_index).cloned() else {
            return;
        };
        let Some(result) = &self.result else {
            return;
        };

        if find_node_by_path(&result.root, &path).is_some() {
            self.current_dir_path = Some(path);
            self.selected = 0;
            if let Some(dir) = self.current_dir() {
                self.status = format!("Browsing {}", dir.path);
            }
        } else {
            self.current_dir_path = Some(result.root.path.clone());
            self.selected = 0;
            self.history[self.history_index] = result.root.path.clone();
            self.status = "History target no longer exists; moved to root".to_string();
        }
    }

    fn request_delete(&mut self) {
        let Some(node) = self
            .current_dir()
            .and_then(|dir| dir.children.get(self.selected))
        else {
            self.status = "No entry selected".to_string();
            return;
        };
        let Some(dir_path) = &self.current_dir_path else {
            self.status = "No directory selected".to_string();
            return;
        };

        self.pending_delete = Some(PendingDelete {
            dir_path: dir_path.clone(),
            child_index: self.selected,
            name: node.name.clone(),
            path: node.path.clone(),
            kind: node.kind,
        });
    }

    fn cancel_delete(&mut self) {
        self.pending_delete = None;
        self.status = "Delete canceled".to_string();
    }

    fn confirm_delete(&mut self) {
        let Some(pending) = self.pending_delete.take() else {
            return;
        };

        if let Err(error) = delete_path_on_disk(Path::new(&pending.path), pending.kind) {
            self.status = format!("Delete failed: {error}");
            return;
        }

        let Some(result) = self.result.as_mut() else {
            self.status = "Deleted on disk, but no loaded tree to update".to_string();
            return;
        };
        if remove_child_by_dir_path(&mut result.root, &pending.dir_path, pending.child_index)
            .is_none()
        {
            self.status = "Deleted on disk, but in-memory tree update failed".to_string();
            return;
        }

        recalculate_sizes_and_sort(&mut result.root);
        self.normalize_selection();
        self.status = format!("Deleted {}", pending.name);
    }

    fn start_scan_current_dir(&mut self) {
        let target = self
            .current_dir()
            .map(|node| PathBuf::from(&node.path))
            .unwrap_or_else(|| self.cwd.clone());
        self.start_scan(target);
    }

    fn start_scan_full_disk(&mut self) {
        let base_path = self
            .current_dir()
            .map(|node| PathBuf::from(&node.path))
            .unwrap_or_else(|| self.cwd.clone());
        match current_disk_root_from(&base_path) {
            Ok(root) => self.start_scan(root),
            Err(error) => {
                self.status = format!("Failed to resolve disk root: {error}");
            }
        }
    }

    fn start_scan(&mut self, target: PathBuf) {
        if self.is_scanning() {
            self.status = "Scan is already running".to_string();
            return;
        }

        let (progress_tx, progress_rx) = unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        let target_for_task = target.clone();
        Handle::current().spawn(async move {
            let result = scan_directory_with_progress(target_for_task, progress_tx).await;
            let _ = done_tx.send(result);
        });

        let target_str = target.to_string_lossy().to_string();
        self.scan_progress = Some(ScanProgressState {
            root: target_str.clone(),
            current_path: target_str.clone(),
            started_at: Instant::now(),
            scanned_nodes: 0,
            scanned_dirs: 0,
            scanned_files: 0,
            scanned_symlinks: 0,
            warnings: 0,
        });
        self.scan_progress_rx = Some(progress_rx);
        self.scan_done_rx = Some(done_rx);
        self.pending_delete = None;
        self.status = format!("Scanning started: {}", target_str);
    }

    fn save_default(&mut self) {
        let Some(result) = self.result.as_ref() else {
            self.status = "No scan result to save".to_string();
            return;
        };
        match save_result_sync(result, &self.default_result_path) {
            Ok(_) => {
                self.status = format!("Saved: {}", self.default_result_path.display());
            }
            Err(error) => {
                self.status = format!("Save failed: {error}");
            }
        }
    }

    fn load_default(&mut self) {
        match load_result_sync(&self.default_result_path) {
            Ok(result) => {
                self.set_result(result);
                self.status = format!("Loaded: {}", self.default_result_path.display());
            }
            Err(error) => {
                self.status = format!("Load failed: {error}");
            }
        }
    }

    fn poll_background(&mut self) {
        let mut progress_events = Vec::new();
        let mut progress_disconnected = false;
        if let Some(progress_rx) = &mut self.scan_progress_rx {
            loop {
                match progress_rx.try_recv() {
                    Ok(event) => progress_events.push(event),
                    Err(ProgressTryRecvError::Empty) => break,
                    Err(ProgressTryRecvError::Disconnected) => {
                        progress_disconnected = true;
                        break;
                    }
                }
            }
        }
        if progress_disconnected {
            self.scan_progress_rx = None;
        }
        for event in progress_events {
            self.apply_progress_event(event);
        }

        if let Some(done_rx) = &mut self.scan_done_rx {
            match done_rx.try_recv() {
                Ok(result) => {
                    self.scan_done_rx = None;
                    self.scan_progress_rx = None;
                    self.scan_progress = None;
                    match result {
                        Ok(scan_result) => {
                            let duration = format_duration_ms(scan_result.scan_duration_ms);
                            self.set_result(scan_result);
                            self.status = format!("Scan completed in {duration}");
                        }
                        Err(error) => {
                            self.status = format!("Scan failed: {error}");
                        }
                    }
                }
                Err(DoneTryRecvError::Empty) => {}
                Err(DoneTryRecvError::Closed) => {
                    self.scan_done_rx = None;
                    self.scan_progress_rx = None;
                    self.scan_progress = None;
                    self.status = "Scan task ended unexpectedly".to_string();
                }
            }
        }
    }

    fn apply_progress_event(&mut self, event: ScanProgressEvent) {
        let Some(progress) = &mut self.scan_progress else {
            return;
        };

        match event {
            ScanProgressEvent::Started { root } => {
                progress.root = root.clone();
                progress.current_path = root;
            }
            ScanProgressEvent::Scanned { path, kind } => {
                progress.current_path = path;
                progress.scanned_nodes += 1;
                match kind {
                    NodeKind::Directory => progress.scanned_dirs += 1,
                    NodeKind::File => progress.scanned_files += 1,
                    NodeKind::Symlink => progress.scanned_symlinks += 1,
                }
            }
            ScanProgressEvent::Warning { message } => {
                progress.warnings += 1;
                self.status = format!("Warning: {message}");
            }
        }
    }
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, mut app: App) -> io::Result<()> {
    loop {
        app.poll_background();
        terminal.draw(|frame| draw(frame, &app))?;

        if !event::poll(Duration::from_millis(120))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.pending_delete.is_some() {
            match key.code {
                KeyCode::Char('y') => app.confirm_delete(),
                KeyCode::Char('n') | KeyCode::Esc => app.cancel_delete(),
                _ => {}
            }
            continue;
        }

        if app.is_scanning() {
            if let KeyCode::Char('q') = key.code {
                return Ok(());
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('s') => app.start_scan_current_dir(),
            KeyCode::Char('a') => app.start_scan_full_disk(),
            KeyCode::Char('p') => app.save_default(),
            KeyCode::Char('o') => app.load_default(),
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Up | KeyCode::Char('k') => app.previous(),
            KeyCode::Home => app.selected = 0,
            KeyCode::End => app.selected = app.current_children_len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l') => app.open_selected_directory(),
            KeyCode::Backspace | KeyCode::Char('u') | KeyCode::Char('h') => app.go_parent(),
            KeyCode::Left | KeyCode::Char('b') | KeyCode::Char('[') => app.history_back(),
            KeyCode::Right | KeyCode::Char('f') | KeyCode::Char(']') => app.history_forward(),
            KeyCode::Char('d') => app.request_delete(),
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    if let Some(progress) = &app.scan_progress {
        draw_scanning(frame, app, progress);
    } else if app.result.is_some() {
        draw_browsing(frame, app);
    } else {
        draw_empty(frame, app);
    }

    if let Some(pending) = &app.pending_delete {
        let area = centered_rect(64, 30, frame.area());
        frame.render_widget(Clear, area);
        let prompt = Paragraph::new(vec![
            Line::from("Delete selected entry?"),
            Line::from(""),
            Line::from(format!("Name: {}", pending.name)),
            Line::from(format!("Path: {}", pending.path)),
            Line::from(format!("Type: {}", kind_label(pending.kind))),
            Line::from(""),
            Line::from("Press y to delete, n or Esc to cancel."),
        ])
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Delete"),
        );
        frame.render_widget(prompt, area);
    }
}

fn draw_scanning(frame: &mut Frame<'_>, app: &App, progress: &ScanProgressState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(4),
        ])
        .split(frame.area());

    let header = Paragraph::new(format!(
        "SCAN RUNNING | Root: {} | Elapsed: {}",
        progress.root,
        format_duration(progress.started_at.elapsed())
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Disk Analyzer"),
    );
    frame.render_widget(header, chunks[0]);

    let body = Paragraph::new(vec![
        Line::from(format!("Current path: {}", progress.current_path)),
        Line::from(""),
        Line::from(format!("Scanned nodes: {}", progress.scanned_nodes)),
        Line::from(format!("Directories: {}", progress.scanned_dirs)),
        Line::from(format!("Files: {}", progress.scanned_files)),
        Line::from(format!("Symlinks: {}", progress.scanned_symlinks)),
        Line::from(format!("Warnings: {}", progress.warnings)),
    ])
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Scan Progress"),
    );
    frame.render_widget(body, chunks[1]);

    let footer = Paragraph::new(vec![
        Line::from(format!("Status: {}", app.status)),
        Line::from("Scanning in background... q: quit"),
    ])
    .wrap(Wrap { trim: false })
    .block(Block::default().borders(Borders::ALL).title("Command Bar"));
    frame.render_widget(footer, chunks[2]);
}

fn draw_empty(frame: &mut Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(4)])
        .split(frame.area());

    let welcome = Paragraph::new(vec![
        Line::from("No scan data loaded."),
        Line::from(""),
        Line::from("s: scan current directory"),
        Line::from("a: scan current disk (full scan)"),
        Line::from("o: load ./diskanalyzer-result.json"),
        Line::from("q: quit"),
    ])
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Disk Analyzer"),
    );
    frame.render_widget(welcome, chunks[0]);

    let footer = Paragraph::new(vec![
        Line::from(format!("Status: {}", app.status)),
        Line::from("TUI-first mode: scanning and load/save from keyboard."),
    ])
    .wrap(Wrap { trim: false })
    .block(Block::default().borders(Borders::ALL).title("Command Bar"));
    frame.render_widget(footer, chunks[1]);
}

fn draw_browsing(frame: &mut Frame<'_>, app: &App) {
    let Some(current_dir) = app.current_dir() else {
        draw_empty(frame, app);
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(frame.area());

    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(chunks[1]);

    let scan_duration = app
        .result
        .as_ref()
        .map(|r| format_duration_ms(r.scan_duration_ms))
        .unwrap_or_else(|| "-".to_string());

    let header = Paragraph::new(format!(
        "Disk: {} | Current: {} | Entries: {} | Size: {} | Scan time: {}",
        app.result
            .as_ref()
            .map(|r| r.root.path.as_str())
            .unwrap_or("-"),
        current_dir.path,
        current_dir.children.len(),
        crate::core::format_bytes(current_dir.size),
        scan_duration
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Disk Analyzer (Browse Mode)"),
    );
    frame.render_widget(header, chunks[0]);

    let current_dir_size = current_dir.size.max(1);
    let items: Vec<ListItem<'_>> = current_dir
        .children
        .iter()
        .map(|node| {
            let (icon, color) = match node.kind {
                NodeKind::Directory => ("[D]", Color::Cyan),
                NodeKind::File => ("[F]", Color::White),
                NodeKind::Symlink => ("[L]", Color::Magenta),
            };
            let percentage = (node.size as f64 / current_dir_size as f64) * 100.0;
            ListItem::new(Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::raw(format!("{:<36}", node.name)),
                Span::styled(
                    format!("{:>10}", crate::core::format_bytes(node.size)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!("  {:>5.1}%", percentage)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Directory Entries"),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    state.select((app.current_children_len() > 0).then_some(app.selected));
    frame.render_stateful_widget(list, body_chunks[0], &mut state);

    let detail_text = if let Some(node) = app.selected_node() {
        vec![
            Line::from(format!("Name: {}", node.name)),
            Line::from(format!("Path: {}", node.path)),
            Line::from(format!("Type: {}", kind_label(node.kind))),
            Line::from(format!("Size: {}", crate::core::format_bytes(node.size))),
            Line::from(format!("Children: {}", node.child_count)),
            Line::from(format!(
                "History: {}/{}",
                app.history_index + 1,
                app.history.len()
            )),
            Line::from("Enter: open dir | u: up | b/f: back/forward"),
            Line::from("d: delete file/dir/symlink (confirm y/n)"),
        ]
    } else {
        vec![
            Line::from("No entries in this directory"),
            Line::from(""),
            Line::from("Use u to go parent, b/f for history"),
            Line::from("s: rescan current dir | a: full scan"),
            Line::from("p: save | o: load | q: quit"),
            Line::from(""),
            Line::from(""),
            Line::from(""),
        ]
    };

    let details = Paragraph::new(detail_text)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Details / Actions"),
        );
    frame.render_widget(details, body_chunks[1]);

    let footer = Paragraph::new(vec![
        Line::from(format!("Status: {}", app.status)),
        Line::from(
            "s scan-dir | a full-scan | p save | o load | j/k move | Enter open | u up | d delete | q quit",
        ),
    ])
    .wrap(Wrap { trim: false })
    .block(Block::default().borders(Borders::ALL).title("Command Bar"));
    frame.render_widget(footer, chunks[2]);
}

fn kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Directory => "Directory",
        NodeKind::File => "File",
        NodeKind::Symlink => "Symlink",
    }
}

fn find_node_by_path<'a>(node: &'a Node, target: &str) -> Option<&'a Node> {
    if node.path == target {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_node_by_path(child, target) {
            return Some(found);
        }
    }
    None
}

fn find_node_by_path_mut<'a>(node: &'a mut Node, target: &str) -> Option<&'a mut Node> {
    if node.path == target {
        return Some(node);
    }
    for child in &mut node.children {
        if let Some(found) = find_node_by_path_mut(child, target) {
            return Some(found);
        }
    }
    None
}

fn remove_child_by_dir_path(root: &mut Node, dir_path: &str, child_index: usize) -> Option<Node> {
    let directory = find_node_by_path_mut(root, dir_path)?;
    if directory.kind != NodeKind::Directory || child_index >= directory.children.len() {
        return None;
    }
    Some(directory.children.remove(child_index))
}

fn recalculate_sizes_and_sort(node: &mut Node) -> u64 {
    if node.kind != NodeKind::Directory {
        return node.size;
    }

    let mut total = 0_u64;
    for child in &mut node.children {
        total += recalculate_sizes_and_sort(child);
    }
    node.size = total;
    node.children
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    total
}

fn delete_path_on_disk(path: &Path, kind: NodeKind) -> io::Result<()> {
    match kind {
        NodeKind::File => fs::remove_file(path),
        NodeKind::Symlink => fs::remove_file(path).or_else(|_| fs::remove_dir(path)),
        NodeKind::Directory => fs::remove_dir_all(path),
    }
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();
    format!("{secs}.{millis:03}s")
}

fn format_duration_ms(ms: u64) -> String {
    format_duration(Duration::from_millis(ms))
}
