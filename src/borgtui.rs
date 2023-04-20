use crate::profiles::Repository;
// use crate::cli::Action;
use crate::types::{BorgResult, PrettyBytes, RingBuffer};
use crate::{borg::BorgCreateProgress, profiles::Profile};
use borgbackup::asynchronous::CreateProgress;
use borgbackup::output::list::ListRepository;
use crossterm::event::KeyEvent;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, info};
use tui::layout::Rect;
use tui::style::{Color, Modifier, Style};
use tui::symbols;
use tui::text::{Span, Spans};
use tui::widgets::{Axis, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, Wrap};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    text::Text,
    widgets::{Block, Borders, List, ListItem},
    Frame, Terminal,
};

const BYTES_TO_MEGABYTES_F64: f64 = 1024.0 * 1024.0;

#[derive(Debug)]
pub(crate) enum Command {
    CreateBackup(Profile),
    SaveProfile(Profile),
    // TODO: Don't use a full repo here!
    ListArchives(Repository),
    DetermineDirectorySize(PathBuf, Arc<AtomicU64>),
    GetDirectorySuggestionsFor(String),
    Quit,
}

#[derive(Debug)]
pub(crate) enum CommandResponse {
    CreateProgress(BorgCreateProgress),
    ListArchiveResult(ListRepository),
    Info(String),
    SuggestionResults((Vec<PathBuf>, usize)),
}

#[derive(Copy, Clone, Debug)]
struct BackupStat {
    num_files: u64,
    original_size: u64,
    compressed_size: u64,
    deduplicated_size: u64,
}

impl BackupStat {
    fn new(
        num_files: u64,
        original_size: u64,
        compressed_size: u64,
        deduplicated_size: u64,
    ) -> Self {
        Self {
            num_files,
            original_size,
            compressed_size,
            deduplicated_size,
        }
    }
}

#[derive(Default)]
// TODO: Move each associated member to their own struct
struct BackupState {
    // TODO: Use an actual struct for this!
    backup_stats: HashMap<String, RingBuffer<BackupStat>>,
    recently_backed_up_files: HashMap<String, RingBuffer<String>>,
    finished_backing_up: HashSet<String>,
}

impl BackupState {
    fn mark_finished(&mut self, repo: String) {
        self.finished_backing_up.insert(repo);
    }

    fn is_finished(&self, repo: &str) -> bool {
        self.finished_backing_up.contains(repo)
    }
    fn clear_finished(&mut self) {
        self.finished_backing_up.clear();
    }
}

macro_rules! toggle_to_previous_state_or_run {
    ($self:expr, $associated_state:expr, $to_run:block) => {
        if !$self.is_a_toggle_to_previous_screen($associated_state) {
            $to_run
        } else {
            $self.toggle_to_previous_screen()
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum UIState {
    ProfileView,
    BackingUp,
    ListAllArchives,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum EditingState {
    Normal,
    Editing,
}

// TODO: Consider encapsulating these different states into their own struct
pub(crate) struct BorgTui {
    tick_rate: Duration,
    profile: Profile,
    backup_path_sizes: HashMap<PathBuf, Arc<AtomicU64>>,
    command_channel: Sender<Command>,
    recv_channel: Receiver<CommandResponse>,
    ui_state: UIState,
    previous_ui_state: Option<UIState>,
    editing_state: EditingState,
    input_buffer: String,
    input_buffer_changed: bool,
    popup_required: bool,
    // This is not an enum field to make it easier to tab while a backup is in progress.
    backup_state: BackupState,
    list_archives_state: HashMap<String, ListRepository>,
    directory_suggestions: Vec<PathBuf>,
    directory_suggestions_update_num: usize,
    info_logs: RingBuffer<String>,
    done: bool,
}

impl BorgTui {
    // The number of queued updates to pull per update tick.
    const POLLING_AMOUNT: usize = 10;
    const BACKUP_STATS_RETENTION_AMOUNT: usize = 100;

    pub(crate) fn new(
        profile: Profile,
        command_channel: Sender<Command>,
        recv_channel: Receiver<CommandResponse>,
    ) -> BorgTui {
        BorgTui {
            tick_rate: Duration::from_millis(16),
            profile,
            backup_path_sizes: HashMap::new(),
            command_channel,
            recv_channel,
            ui_state: UIState::ProfileView,
            previous_ui_state: None,
            input_buffer: String::new(),
            popup_required: false,
            editing_state: EditingState::Normal,
            backup_state: BackupState::default(),
            list_archives_state: HashMap::new(),
            directory_suggestions: Vec::new(),
            directory_suggestions_update_num: 0,
            input_buffer_changed: false,
            info_logs: RingBuffer::new(10),
            done: false,
        }
    }

    pub(crate) fn run(&mut self) -> BorgResult<()> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let res = self.run_app(&mut terminal);

        // restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        if let Err(e) = res {
            tracing::error!("Failed to run app: {}", e);
        }
        Ok(())
    }

    fn handle_input_buffer_changed(&mut self) -> BorgResult<()> {
        self.input_buffer_changed = false;
        let command = Command::GetDirectorySuggestionsFor(self.input_buffer.clone());
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn handle_keyboard_input(&mut self, key: KeyEvent) -> BorgResult<()> {
        match key.code {
            KeyCode::Char('q') => {
                self.done = true;
                // TODO: Block on any remaining borg backups
                self.send_quit_command()?;
                return Ok(());
            }
            KeyCode::Char('u') => {
                toggle_to_previous_state_or_run!(self, UIState::BackingUp, {
                    self.start_backing_up();
                    self.send_create_command()?;
                });
            }
            KeyCode::Char('l') => {
                toggle_to_previous_state_or_run!(self, UIState::ListAllArchives, {
                    self.start_list_archive_state();
                    self.send_list_archives_command()?;
                });
            }
            // TODO: Have a "previous state" variable and toggle back to that.
            KeyCode::Char('p') => {
                toggle_to_previous_state_or_run!(self, UIState::ProfileView, {
                    self.switch_ui_state(UIState::ProfileView);
                });
            }
            KeyCode::Char('a') => {
                self.editing_state = EditingState::Editing;
                self.popup_required = true;
            }
            KeyCode::Char('s') => {
                if let Err(e) = self.send_save_command() {
                    error!("Failed to save profile: {}", e);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn run_app<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> BorgResult<()> {
        let mut last_tick = Instant::now();
        self.profile
            .backup_paths()
            .to_vec()
            .iter()
            .for_each(|backup_path| {
                if let Err(e) = self.send_backup_dir_size_command(backup_path.clone()) {
                    tracing::error!(
                        "Failed to query directory size for {}: {}",
                        backup_path.display(),
                        e
                    );
                }
            });
        loop {
            if self.done {
                return Ok(());
            }
            terminal.draw(|f| {
                self.draw_ui(f);
            })?;

            let timeout = self
                .tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));
            if crossterm::event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    match self.editing_state {
                        EditingState::Normal => self.handle_keyboard_input(key)?,
                        EditingState::Editing => {
                            // TODO: How do we associate a function with the enter press?
                            match key.code {
                                KeyCode::Backspace => {
                                    self.input_buffer.pop();
                                    self.input_buffer_changed = true;
                                }
                                KeyCode::Tab => {
                                    if let Some(res) = self.directory_suggestions.first() {
                                        let res = res.to_string_lossy().to_string();
                                        // TODO: This will cycle the ending forward slash on and off
                                        if self.input_buffer == res {
                                            // TODO: Handle windows backslash maybe never?
                                            self.input_buffer.push('/');
                                        } else {
                                            self.input_buffer = res;
                                        }
                                        self.input_buffer_changed = true;
                                    }
                                }
                                KeyCode::Char(c) => {
                                    self.input_buffer.push(c);
                                    self.input_buffer_changed = true;
                                }
                                KeyCode::Enter => info!("input_buffer: {}", self.input_buffer),
                                KeyCode::Esc => {
                                    self.editing_state = EditingState::Normal;
                                }
                                _ => {}
                            };
                        }
                    }
                }
            }
            if last_tick.elapsed() >= self.tick_rate {
                self.on_tick()?;
                last_tick = Instant::now();
            }
        }
    }

    fn send_backup_dir_size_command(&mut self, dir: PathBuf) -> BorgResult<()> {
        let byte_count = self
            .backup_path_sizes
            .entry(dir.clone())
            .or_default()
            .clone();
        self.command_channel
            .blocking_send(Command::DetermineDirectorySize(dir, byte_count))?;
        Ok(())
    }

    fn send_create_command(&mut self) -> BorgResult<()> {
        let command = Command::CreateBackup(self.profile.clone());
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn send_save_command(&mut self) -> BorgResult<()> {
        let command = Command::SaveProfile(self.profile.clone());
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn send_list_archives_command(&mut self) -> BorgResult<()> {
        for repo in self.profile.repos() {
            let command = Command::ListArchives(repo.clone());
            self.command_channel.blocking_send(command)?;
        }
        Ok(())
    }

    fn send_quit_command(&mut self) -> BorgResult<()> {
        let command = Command::Quit;
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn is_a_toggle_to_previous_screen(&self, associated_state: UIState) -> bool {
        self.ui_state == associated_state
    }

    fn toggle_to_previous_screen(&mut self) {
        if let Some(previous_state) = self.previous_ui_state {
            self.previous_ui_state = Some(self.ui_state);
            self.ui_state = previous_state;
        }
    }

    fn switch_ui_state(&mut self, new_state: UIState) {
        self.previous_ui_state = Some(self.ui_state);
        self.ui_state = new_state
    }

    fn start_backing_up(&mut self) {
        self.switch_ui_state(UIState::BackingUp);
        self.backup_state.clear_finished();
    }

    fn start_list_archive_state(&mut self) {
        self.switch_ui_state(UIState::ListAllArchives);
    }

    fn record_create_progress(
        &mut self,
        repo: String,
        path: String,
        num_files: u64,
        original_size: u64,
        compressed_size: u64,
        deduplicated_size: u64,
    ) {
        self.insert_recently_backed_up_file(repo.clone(), path);
        let backup_stat =
            BackupStat::new(num_files, original_size, compressed_size, deduplicated_size);
        self.backup_state
            .backup_stats
            .entry(repo)
            .or_insert_with(|| RingBuffer::new(Self::BACKUP_STATS_RETENTION_AMOUNT))
            .push_back(backup_stat);
    }

    fn insert_recently_backed_up_file(&mut self, repo: String, path: String) {
        self.backup_state
            .recently_backed_up_files
            .entry(repo)
            .or_insert_with(|| RingBuffer::new(5))
            .push_back(path);
    }

    fn handle_command(&mut self, msg: CommandResponse) {
        tracing::debug!("Got message: {:?}", msg);
        match msg {
            CommandResponse::CreateProgress(progress) => {
                let repo = progress.repository.clone();
                match progress.create_progress {
                    CreateProgress::Progress {
                        path,
                        nfiles,
                        original_size,
                        compressed_size,
                        deduplicated_size,
                    } => {
                        self.record_create_progress(
                            repo,
                            path,
                            nfiles,
                            original_size,
                            compressed_size,
                            deduplicated_size,
                        );
                    }
                    CreateProgress::Finished => {
                        // TODO: Replace this hack with a proper notification
                        self.backup_state.mark_finished(repo.clone());
                        self.info_logs
                            .push_back(format!("Finished backing up {}", repo));
                        debug!("test: {:?}", self.info_logs.is_empty());
                        tracing::info!("Finished backing up {}", repo);
                    }
                }
            }
            CommandResponse::Info(info_string) => {
                info!(info_string); // Make sure it ends up in the logfile
                self.info_logs.push_back(info_string)
            }
            CommandResponse::ListArchiveResult(list_archive_result) => {
                self.list_archives_state.insert(
                    list_archive_result.repository.location.clone(),
                    list_archive_result,
                );
            }
            CommandResponse::SuggestionResults((suggestions, update_num)) => {
                if self.directory_suggestions_update_num < update_num {
                    self.directory_suggestions = suggestions;
                }
            }
        }
    }

    fn on_tick(&mut self) -> BorgResult<()> {
        if self.input_buffer_changed {
            self.handle_input_buffer_changed()?;
        }
        for _ in 0..Self::POLLING_AMOUNT {
            let res = self
                .recv_channel
                .try_recv()
                .map(|cmd| self.handle_command(cmd));
            let disconnected = matches!(
                res,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            );
            if disconnected {
                tracing::debug!("TUI channel closed");
                self.done = true;
            }
        }
        Ok(())
    }

    fn latest_stats_for_repo(&self, repo: &str) -> Option<BackupStat> {
        self.backup_state
            .backup_stats
            .get(repo)
            .and_then(|rb| rb.back())
            .copied()
    }

    fn get_backup_stats_for_repo(
        &self,
        repo: &str,
        metric_fn: &dyn Fn(&BackupStat) -> f64,
    ) -> Option<Vec<(f64, f64)>> {
        self.backup_state.backup_stats.get(repo).map(|ring_buffer| {
            ring_buffer
                .iter()
                .map(metric_fn)
                .zip(-99..=0)
                .map(|(metric, x_axis)| (x_axis as f64, metric))
                .collect()
        })
    }

    // TODO: Make this _much_ nicer. Do we even need a min?
    fn get_min_and_max_stat_value(
        &self,
        metric_fn: &dyn Fn(&BackupStat) -> f64,
    ) -> Option<(f64, f64)> {
        let mut min = f64::INFINITY;
        let mut max = -1.0;
        for repo in self.profile.repos() {
            if let Some(ring_buffer) = self.backup_state.backup_stats.get(&repo.path) {
                ring_buffer.iter().map(metric_fn).for_each(|value| {
                    if value < min {
                        min = value;
                    }
                    if value > max {
                        max = value;
                    }
                });
            }
        }
        if min == f64::INFINITY || max == -1.0 {
            None
        } else {
            Some((min, max))
        }
    }

    fn draw_popup<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        // Clear out the background
        frame.render_widget(tui::widgets::Clear, area);
        let input_box_size = 3;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Length(area.height - input_box_size),
                    Constraint::Max(input_box_size),
                ]
                .as_ref(),
            )
            .split(area);
        let (top_area, input_panel_area) = (chunks[0], chunks[1]);
        // TODO: Make this generic
        let list_items: Vec<_> = self
            .directory_suggestions
            .iter()
            .map(|item| ListItem::new(item.to_string_lossy().to_owned()))
            .collect();
        let content =
            List::new(list_items).block(Block::default().borders(Borders::ALL).title("Content"));
        frame.render_widget(content, top_area);

        let input_panel = Paragraph::new(self.input_buffer.clone())
            .block(Block::default().borders(Borders::ALL).title("Input"));
        frame.render_widget(input_panel, input_panel_area);
    }

    fn draw_backup_chart<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        let mut datasets = Vec::new();
        // TODO: How to make original size look good?
        let original_size_metrics: Vec<_> = self
            .profile
            .repos()
            .iter()
            .filter_map(|repo| {
                self.get_backup_stats_for_repo(&repo.path, &|stat: &BackupStat| {
                    stat.original_size as f64 / BYTES_TO_MEGABYTES_F64
                })
                .map(|item| (repo.path.clone(), item))
            })
            .collect();
        datasets.extend(original_size_metrics.iter().map(|(repo_name, points)| {
            Dataset::default()
                .name(repo_name)
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Red))
                .graph_type(GraphType::Line)
                .data(points)
        }));

        let compressed_size_metrics: Vec<_> = self
            .profile
            .repos()
            .iter()
            .filter_map(|repo| {
                self.get_backup_stats_for_repo(&repo.path, &|stat: &BackupStat| {
                    stat.compressed_size as f64 / BYTES_TO_MEGABYTES_F64
                })
                .map(|item| (repo.path.clone(), item))
            })
            .collect();
        datasets.extend(compressed_size_metrics.iter().map(|(repo_name, points)| {
            Dataset::default()
                .name(format!("Compression {}", repo_name))
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Blue))
                .graph_type(GraphType::Line)
                .data(points)
        }));
        let x_labels = vec![
            Span::styled(
                format!("{}", -99),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}", -50),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}", 0),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];

        // TODO: Do we need a min and how to make the max nice?
        let (y_min, y_max) = self
            .get_min_and_max_stat_value(&|backup_stat: &BackupStat| {
                backup_stat.original_size as f64 / BYTES_TO_MEGABYTES_F64
            })
            .unwrap_or((0.0, 1000.0));
        let chart = Chart::new(datasets)
            .block(
                Block::default()
                    .title(Span::styled(
                        "Backup Progress",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL),
            )
            .x_axis(
                Axis::default()
                    .title("Ticks")
                    .style(Style::default().fg(Color::Gray))
                    .labels(x_labels)
                    .bounds([-99.0, 0.0]),
            )
            .y_axis(
                Axis::default()
                    .title("Size")
                    .style(Style::default().fg(Color::Gray))
                    .labels(vec![
                        Span::styled(
                            format!("{}", PrettyBytes::from_megabytes_f64(y_min)),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}", PrettyBytes::from_megabytes_f64((y_min + y_max) / 2.0)),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}", PrettyBytes::from_megabytes_f64(y_max)),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ])
                    // TODO: Make these bounds dynamic based on the data!
                    .bounds([y_min, y_max]),
            );
        // .hidden_legend_constraints((Constraint::Ratio(1, 1), Constraint::Ratio(1, 1)));
        frame.render_widget(chart, area);
    }

    fn draw_backup_list<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        // TODO: Handle running out of vertical space!
        let backup_constraints = std::iter::repeat(Constraint::Percentage(
            100 / self.profile.num_repos() as u16,
        ))
        .take(self.profile.num_repos())
        .collect::<Vec<_>>();
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(backup_constraints.as_ref())
            .split(area);
        self.profile
            .repos()
            .iter()
            .zip(areas)
            .for_each(|(repo, area)| {
                let mut items = self
                    .backup_state
                    .recently_backed_up_files
                    .get(&repo.path)
                    .map(|ring| {
                        ring.iter()
                            .map(|path| {
                                let text = Text::from(format!("> {}", path));
                                ListItem::new(text)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(Vec::new);
                if let Some(backup_stat) = self.latest_stats_for_repo(&repo.path) {
                    items.insert(
                        0,
                        ListItem::new(format!(
                            "# files: {} (deduplicated: {})",
                            backup_stat.num_files,
                            PrettyBytes(backup_stat.deduplicated_size),
                        )),
                    );
                }
                let backup_span = if self.backup_state.is_finished(&repo.path) {
                    Span::styled(
                        format!("FINISHED Backup {}", repo),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled(
                        format!("Backup {}", repo),
                        Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                    )
                };
                let backup_file_list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(backup_span));
                frame.render_widget(backup_file_list, area);
            })
    }

    fn draw_all_archive_lists<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        // (RepoName, Option<ListArchive>)
        let repos_with_archives: Vec<_> = self
            .profile
            .repos()
            .iter()
            .map(|repo| {
                (
                    repo.path.clone(),
                    self.list_archives_state.get(&repo.path).cloned(),
                )
            })
            .collect();
        let backup_constraints = std::iter::repeat(Constraint::Percentage(
            100 / repos_with_archives.len() as u16,
        ))
        .take(self.profile.num_repos())
        .collect::<Vec<_>>();
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(backup_constraints.as_ref())
            .split(area);
        for ((repo_name, list_archive), area) in repos_with_archives.into_iter().zip(areas) {
            let list_items = match list_archive {
                Some(list_archive) => {
                    // TODO: Consider using a table to show the date!
                    list_archive
                        .archives
                        .iter()
                        .rev() // TODO: Don't reverse in the UI. Make the original data in descending order
                        .map(|archive| ListItem::new(archive.name.clone()))
                        .collect()
                }
                None => vec![ListItem::new("Still fetching...")],
            };
            let repo_list = List::new(list_items)
                .block(Block::default().borders(Borders::ALL).title(repo_name));
            frame.render_widget(repo_list, area)
        }
    }
    // fn draw_archive_list<B: Backend>(&self, repo: &(), frame: &mut Frame<B>, area: Rect) {}

    fn draw_info_panel<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        // TODO: Make something generic here
        let text = vec![
            Spans::from("• Press 'q' to quit"),
            Spans::from("• Press 'u' to backup"),
            Spans::from("• Press 'p' to toggle profile"),
            Spans::from("• Press 'l' to list archives"),
            Spans::from("• Press 'a' to add a backup path"),
            Spans::from("• Press 's' to save profile"),
        ];
        let info_panel = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("borgtui"));
        frame.render_widget(info_panel, area);
    }

    fn draw_info_logs<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        // TODO: Sometimes this clips text!
        let info_log_text = self
            .info_logs
            .iter()
            .map(|s| Spans::from(format!("> {}\n", s)))
            .collect::<Vec<_>>();
        let info_panel = Paragraph::new(info_log_text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Logs"));
        frame.render_widget(info_panel, area);
    }

    fn split_screen<B: Backend>(&self, frame: &mut Frame<B>) -> (Rect, Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(20), Constraint::Percentage(80)].as_ref())
            .split(frame.size());
        (chunks[0], chunks[1])
    }

    fn draw_backup_dirs<B: Backend>(&mut self, frame: &mut Frame<B>, backup_paths_area: Rect) {
        let header_cells = ["Size", "Path"].iter().map(|header| Cell::from(*header));
        let header_row = Row::new(header_cells);
        let mut total_backup_dir_size = 0;
        let rows = self.profile.backup_paths().iter().map(|path| {
            let size_cell = Cell::from(
                self.backup_path_sizes
                    .get(path)
                    .map(|byte_count| {
                        let dir_size = byte_count.load(Ordering::SeqCst);
                        total_backup_dir_size += dir_size;
                        format!("{}", PrettyBytes(dir_size))
                    })
                    .unwrap_or_else(|| "??".to_string()),
            );
            let path_name = format!("{}", path.display());
            let path_cell = Cell::from(path_name);
            Row::new([size_cell, path_cell])
        });
        let table = Table::new(rows)
            .header(header_row)
            .widths(&[Constraint::Percentage(10), Constraint::Percentage(90)])
            .block(Block::default().borders(Borders::ALL).title(format!(
                "Backup Sources ({})",
                PrettyBytes(total_backup_dir_size)
            )));
        frame.render_widget(table, backup_paths_area);
    }

    fn draw_profile_view<B: Backend>(
        &mut self,
        frame: &mut Frame<B>,
        repo_area: Rect,
        backup_paths_area: Rect,
    ) {
        let repo_items: Vec<_> = self
            .profile
            .repos()
            .iter()
            .map(|repo| ListItem::new(repo.path.clone()))
            .collect();
        let repo_list = List::new(repo_items)
            .block(Block::default().borders(Borders::ALL).title("Repositories"));
        frame.render_widget(repo_list, repo_area);
        self.draw_backup_dirs(frame, backup_paths_area);
    }

    fn draw_main_right_panel<B: Backend>(&mut self, frame: &mut Frame<B>, right_area: Rect) {
        match &self.ui_state {
            UIState::ProfileView => {
                let profile_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(40), Constraint::Percentage(60)].as_ref())
                    .split(right_area);
                let (repo_area, backup_paths_area) = (profile_chunks[0], profile_chunks[1]);
                self.draw_profile_view(frame, repo_area, backup_paths_area);
            }
            UIState::BackingUp => {
                let backing_up_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(40), Constraint::Percentage(60)].as_ref())
                    .split(right_area);
                let (top_right, bottom_right) = (backing_up_chunks[0], backing_up_chunks[1]);
                self.draw_backup_chart(frame, top_right);
                self.draw_backup_list(frame, bottom_right);
            }
            UIState::ListAllArchives => {
                self.draw_all_archive_lists(frame, right_area);
            }
        }
    }

    fn draw_ui<B: Backend>(&mut self, frame: &mut Frame<B>) {
        // TODO: Calculate chunks based on number of repos
        let (mut left, right) = self.split_screen(frame);
        if !self.info_logs.is_empty() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
                .split(left);
            let (left_top, left_bottom) = (chunks[0], chunks[1]);
            left = left_top;
            self.draw_info_logs(frame, left_bottom);
        }
        self.draw_info_panel(frame, left);
        self.draw_main_right_panel(frame, right);
        if self.popup_required {
            let top_left = Layout::default()
                .direction(Direction::Vertical)
                .constraints(
                    [
                        Constraint::Percentage(10),
                        Constraint::Percentage(80),
                        Constraint::Percentage(10),
                    ]
                    .as_ref(),
                )
                .split(frame.size())[1];
            let corner = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(
                    [
                        Constraint::Percentage(10),
                        Constraint::Percentage(80),
                        Constraint::Percentage(10),
                    ]
                    .as_ref(),
                )
                .split(top_left)[1];
            self.draw_popup(frame, corner);
        }
    }
}
