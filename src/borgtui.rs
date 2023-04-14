// use crate::cli::Action;
use crate::types::{BorgResult, RingBuffer};
use crate::{borg::BorgCreateProgress, profiles::Profile};
use borgbackup::asynchronous::CreateProgress;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::debug;
use tui::layout::Rect;
use tui::style::{Color, Modifier, Style};
use tui::symbols;
use tui::text::{Span, Spans};
use tui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph, Wrap};

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    text::Text,
    widgets::{Block, Borders, List, ListItem},
    Frame, Terminal,
};

#[derive(Debug)]
pub(crate) enum Command {
    CreateBackup(Profile),
    Quit,
}

#[derive(Debug)]
pub(crate) enum CommandResponse {
    CreateProgress(BorgCreateProgress),
    Info(String),
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
}

enum UIState {
    ProfileView,
    BackingUp,
}

// TODO: Consider encapsulating these different states into their own struct
pub(crate) struct BorgTui {
    tick_rate: Duration,
    profile: Profile,
    command_channel: Sender<Command>,
    recv_channel: Receiver<CommandResponse>,
    ui_state: UIState,
    // This is not an enum field to make it easier to tab while a backup is in progress.
    backup_state: BackupState,
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
            command_channel,
            recv_channel,
            ui_state: UIState::ProfileView,
            backup_state: BackupState::default(),
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

    fn run_app<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> BorgResult<()> {
        let mut last_tick = Instant::now();
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
                    match key.code {
                        KeyCode::Char('q') => {
                            self.done = true;
                            // TODO: Block on any remaining borg backups
                            self.send_quit_command()?;
                            return Ok(());
                        }
                        KeyCode::Char('u') => {
                            self.start_backing_up();
                            self.send_create_command()?;
                        }
                        _ => {}
                    }
                }
            }
            if last_tick.elapsed() >= self.tick_rate {
                self.on_tick()?;
                last_tick = Instant::now();
            }
        }
    }

    fn send_create_command(&mut self) -> BorgResult<()> {
        let profile = self.profile.clone();
        let command = Command::CreateBackup(profile);
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn send_quit_command(&mut self) -> BorgResult<()> {
        let command = Command::Quit;
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn start_backing_up(&mut self) {
        self.ui_state = UIState::BackingUp;
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
                            .push_back(format!("Finished backing up {}", repo.clone()));
                        debug!("test: {:?}", self.info_logs.is_empty());
                        tracing::info!("Finished backing up {}", repo);
                    }
                }
            }
            CommandResponse::Info(info_string) => self.info_logs.push_back(info_string),
        }
    }

    fn on_tick(&mut self) -> BorgResult<()> {
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

    fn oldest_stats_for_repo(&self, repo: &str) -> Option<BackupStat> {
        self.backup_state
            .backup_stats
            .get(repo)
            .and_then(|rb| rb.front())
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

    // TODO: Make this _much_ nicer
    fn get_min_and_max_stat_value(
        &self,
        metric_fn: &dyn Fn(&BackupStat) -> f64,
    ) -> Option<(f64, f64)> {
        let mut min = f64::INFINITY;
        let mut max = -1.0;
        for repo in self.profile.repos() {
            self.backup_state
                .backup_stats
                .get(&repo.path)
                .map(|ring_buffer| {
                    ring_buffer.iter().map(metric_fn).for_each(|value| {
                        if value < min {
                            min = value;
                        }
                        if value > max {
                            max = value;
                        }
                    });
                });
        }
        if min == f64::INFINITY || max == -1.0 {
            None
        } else {
            Some((min, max))
        }
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
                    stat.original_size as f64 / 1048576.0
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
                    stat.compressed_size as f64 / 1048576.0
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
                backup_stat.original_size as f64 / 1048576.0
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
                    .title("size (mb)")
                    .style(Style::default().fg(Color::Gray))
                    .labels(vec![
                        Span::styled(
                            format!("{}", y_min.trunc()),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}", ((y_min + y_max) / 2.0).trunc()),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}", y_max.trunc()),
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
                if self.backup_state.is_finished(&repo.path) {
                    items.push(ListItem::new(format!("Finished backing {}", repo)))
                }
                if let Some(backup_stat) = self.latest_stats_for_repo(&repo.path) {
                    items.insert(
                        0,
                        ListItem::new(format!("# files: {}", backup_stat.num_files)),
                    );
                }
                let backup_file_list = List::new(items).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Backup {}", repo.path)),
                );
                frame.render_widget(backup_file_list, area);
            })
    }

    fn draw_info_panel<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        let text = vec![
            Spans::from("Press 'q' to quit"),
            Spans::from("Press 'u' to backup"),
        ];
        let info_panel =
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("borgtui"));
        frame.render_widget(info_panel, area);
    }

    fn draw_info_logs<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        let info_log_text = self
            .info_logs
            .iter()
            .map(|s| Spans::from(format!("> {}\n", s.to_string())))
            .collect::<Vec<_>>();
        let info_panel = Paragraph::new(info_log_text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Logs"));
        frame.render_widget(info_panel, area);
    }

    // TODO: Make this dynamic and generic over the screen
    fn split_screen<B: Backend>(&self, frame: &mut Frame<B>) -> (Rect, Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(20), Constraint::Percentage(80)].as_ref())
            .split(frame.size());
        (chunks[0], chunks[1])
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
        match &self.ui_state {
            UIState::ProfileView => {}
            UIState::BackingUp => {
                let backing_up_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
                    .split(right);
                let (top_right, bottom_right) = (backing_up_chunks[0], backing_up_chunks[1]);
                self.draw_backup_chart(frame, top_right);
                self.draw_backup_list(frame, bottom_right);
            }
        }
    }
}
