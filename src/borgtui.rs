// use crate::cli::Action;
use crate::{borg::MyCreateProgress, profiles::Profile, types::BorgResult};
use borgbackup::asynchronous::CreateProgress;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tokio::sync::mpsc::{Receiver, Sender};

use std::time::{Duration, Instant};
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::Text,
    widgets::{Block, Borders, List, ListItem},
    Frame, Terminal,
};

#[derive(Debug)]
pub(crate) enum Command {
    GetOrCreateDefaultProfile,
    CreateBackup(Profile),
    Quit,
}

#[derive(Debug)]
pub(crate) enum CommandResponse {
    Profile(Profile),
    CreateProgress(MyCreateProgress),
}

pub(crate) struct BorgTui {
    tick_rate: Duration,
    profile: Profile,
    command_channel: Sender<Command>,
    recv_channel: Receiver<CommandResponse>,
    done: bool,
    recently_backed_up_files: Vec<(String, String)>,
}

impl BorgTui {
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
            done: false,
            recently_backed_up_files: Vec::new(),
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
                        // KeyCode::Down => app.items.next(),
                        KeyCode::Up => {
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

    fn on_tick(&mut self) -> BorgResult<()> {
        // TODO: Handle several of these.
        let res = self.recv_channel.try_recv().map(|msg| {
            tracing::debug!("Got message: {:?}", msg);
            match msg {
                CommandResponse::Profile(profile) => {
                    self.profile = profile;
                }
                CommandResponse::CreateProgress(progress) => {
                    let repo = progress.repository.clone();
                    match progress.create_progress {
                        CreateProgress::Progress { path, .. } => {
                            self.recently_backed_up_files.push((path, repo));
                        }
                        CreateProgress::Finished => {
                            // TODO: Show a notification
                            tracing::info!("Finished backing up {}", repo);
                        }
                    }
                }
            }
        });
        let disconnected = matches!(
            res,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        );
        if disconnected {
            tracing::debug!("TUI channel closed");
            self.done = true;
        }
        Ok(())
    }

    fn draw_ui<B: Backend>(&mut self, frame: &mut Frame<B>) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)].as_ref())
            .split(frame.size());
        let items = self
            .recently_backed_up_files
            .iter()
            .map(|(path, repo)| -> ListItem {
                let text = Text::from(format!("{} - {}", path, repo));
                ListItem::new(text).style(Style::default().fg(Color::Black).bg(Color::White))
            })
            .collect::<Vec<_>>();
        let items = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("List"))
            .highlight_style(
                Style::default()
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");
        frame.render_widget(items, chunks[0]);
    }
}
