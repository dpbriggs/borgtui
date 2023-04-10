// use crate::cli::Action;
use crate::types::{BorgResult, RingBuffer};
use crate::{borg::MyCreateProgress, profiles::Profile};
use borgbackup::asynchronous::CreateProgress;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::debug;
use tui::layout::Rect;
use tui::text::Spans;
use tui::widgets::{Paragraph, Wrap};

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
    CreateProgress(MyCreateProgress),
    Info(String),
}

#[derive(Default)]
// TODO: Move each associated member to their own struct
struct BackupState {
    backup_stats: HashMap<String, (u64, u64, u64, u64)>,
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
        self.backup_state.backup_stats.insert(
            repo,
            (num_files, original_size, compressed_size, deduplicated_size),
        );
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

    fn draw_backup_list<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
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
                if let Some(backup_stat) = self.backup_state.backup_stats.get(&repo.path) {
                    items.insert(0, ListItem::new(format!("# files: {}", backup_stat.0)));
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
        debug!("drawing draw_info_logs");
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
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)].as_ref())
                .split(left);
            let (left_top, left_bottom) = (chunks[0], chunks[1]);
            left = left_top;
            self.draw_info_logs(frame, left_bottom);
        }
        self.draw_info_panel(frame, left);
        match &self.ui_state {
            UIState::ProfileView => {}
            UIState::BackingUp => {
                self.draw_backup_list(frame, right);
            }
        }
    }
}
