use crate::profiles::{ProfileOperation, Repository};
use crate::types::{BorgResult, PrettyBytes, RingBuffer};
use crate::{borg::BorgCreateProgress, profiles::Profile};
use borgbackup::asynchronous::CreateProgress;
use borgbackup::output::list::ListRepository;
use crossterm::event::{KeyEvent, KeyModifiers};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{error, info};
use tui::layout::Rect;
use tui::style::{Color, Modifier, Style};
use tui::symbols;
use tui::text::{Span, Spans};
use tui::widgets::{Axis, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, Tabs, Wrap};

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Stdout;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
// Maximum amount of stats to retain
const BACKUP_STATS_RETENTION_AMOUNT: usize = 100;
// How often to update the TUI
const TICK_RATE_MILLIS: u64 = 16;
// How many files to remember when backing up
const NUM_RECENTLY_BACKED_UP_FILES: usize = 5;

#[derive(Debug)]
pub(crate) enum Command {
    CreateBackup(Profile),
    SaveProfile(Profile),
    UpdateProfileAndSave(Profile, ProfileOperation, Arc<AtomicBool>),
    ListArchives(Repository),
    Compact(Repository),
    Prune(Repository, crate::profiles::PruneOptions),
    DetermineDirectorySize(PathBuf, Arc<AtomicU64>, Vec<String>),
    GetDirectorySuggestionsFor(String),
    Mount(Repository, String, String),
    Unmount(String),
    Quit,
}

#[derive(Debug)]
pub(crate) enum CommandResponse {
    CreateProgress(BorgCreateProgress),
    ListArchiveResult(ListRepository),
    ProfileUpdated(Profile),
    Info(String),
    Error(String),
    // TODO: Why is this a tuple :thinking:
    SuggestionResults((Vec<PathBuf>, usize)),
    MountResult(String, String),
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
struct BackupState {
    backup_stats: HashMap<String, RingBuffer<BackupStat, BACKUP_STATS_RETENTION_AMOUNT>>,
    recently_backed_up_files: HashMap<String, RingBuffer<String, NUM_RECENTLY_BACKED_UP_FILES>>,
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

#[derive(Debug)]
struct InputFieldWithSuggestions {
    suggestions: BTreeSet<String>,
    input_buffer: String,
    input_buffer_changed: bool,
    content_title: String,
    is_editing: bool,
    is_done: bool,
    cursor: Option<usize>,
}

impl InputFieldWithSuggestions {
    fn new(initial_input_text: String, content_title: String) -> Self {
        InputFieldWithSuggestions {
            suggestions: BTreeSet::new(),
            input_buffer_changed: !initial_input_text.is_empty(),
            input_buffer: initial_input_text,
            content_title,
            is_editing: true,
            is_done: false,
            cursor: None,
        }
    }

    // TODO: Be more selective when updating suggestions
    fn update_suggestions<I: Iterator<Item = String>>(&mut self, suggestions: I) {
        self.suggestions.extend(suggestions)
    }

    fn is_done(&self) -> bool {
        self.is_done
    }

    fn on_input_buffer_changed<F>(&mut self, input_buffer_change_fn: F) -> BorgResult<()>
    where
        F: Fn(&str) -> BorgResult<()>,
    {
        if self.input_buffer_changed {
            self.input_buffer_changed = false;
            input_buffer_change_fn(self.input_buffer.as_str())
        } else {
            Ok(())
        }
    }

    fn update_cursor(&mut self, new_index: usize) {
        let new_index = new_index.clamp(0, self.suggestions.len().saturating_sub(1));
        self.cursor = Some(new_index);
        if let Some(selected_item) = self.suggestions.iter().nth(new_index).cloned() {
            self.input_buffer = selected_item;
            // This is redundant but here to enforce us not scrambling the selection while a user is pathing.
            self.input_buffer_changed = false;
        }
    }

    fn handle_key<CompletionFn, ValidationFn>(
        &mut self,
        key: KeyEvent,
        completion_fn: CompletionFn,
        validation_fn: ValidationFn,
    ) -> Option<String>
    where
        CompletionFn: Fn(&BTreeSet<String>, &mut String) -> bool,
        ValidationFn: Fn(&BTreeSet<String>, &String) -> bool,
    {
        match (key.code, key.modifiers) {
            (KeyCode::Backspace, _) => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    if let Ok(path) = PathBuf::try_from(self.input_buffer.as_str()) {
                        self.input_buffer = path
                            .parent()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(String::new);
                        if !self.input_buffer.ends_with('/') {
                            self.input_buffer.push('/');
                        }
                    }
                } else {
                    self.input_buffer.pop();
                }
                self.input_buffer_changed = true;
                None
            }
            (KeyCode::Tab, _) => {
                self.input_buffer_changed =
                    completion_fn(&self.suggestions, &mut self.input_buffer);
                None
            }
            (KeyCode::Esc, _) => {
                if self.is_editing {
                    self.is_done = true;
                } else {
                    self.is_editing = false;
                }
                None
            }
            (KeyCode::Enter, _) => {
                // TODO: Basic validation
                let validated_successfully = validation_fn(&self.suggestions, &self.input_buffer);
                if validated_successfully {
                    Some(self.input_buffer.clone())
                } else {
                    None
                }
            }
            (KeyCode::Char('n'), KeyModifiers::CONTROL) | (KeyCode::Down, _) => {
                let new_index = self.cursor.unwrap_or(0).saturating_add(1);
                self.update_cursor(new_index);
                None
            }
            (KeyCode::Char('p'), KeyModifiers::CONTROL) | (KeyCode::Up, _) => {
                let new_index = self.cursor.unwrap_or(0).saturating_sub(1);
                self.update_cursor(new_index);
                None
            }
            (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                self.is_done = true;
                None
            }
            (KeyCode::Char(c), _) => {
                if c == 'q' && !self.is_editing {
                    self.is_done = true;
                } else {
                    self.input_buffer.push(c);
                    self.input_buffer_changed = true;
                }
                None
            }
            _ => None,
        }
    }

    fn draw<B: Backend, F>(&self, frame: &mut Frame<B>, area: Rect, input_panel_style: F)
    where
        F: Fn(&BTreeSet<String>, &str) -> bool,
    {
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
            .suggestions
            .range(self.input_buffer.clone()..)
            .map(|item| ListItem::new(item.to_string()))
            .collect();
        // TODO Add the ability to change "Content" to be configurable from the caller.
        let content = List::new(list_items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(self.content_title.clone()),
        );
        frame.render_widget(content, top_area);

        let input_panel_style = if input_panel_style(&self.suggestions, &self.input_buffer) {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };

        let input_panel = Paragraph::new(self.input_buffer.clone())
            .style(input_panel_style)
            .block(Block::default().borders(Borders::ALL).title("Input"));
        frame.render_widget(input_panel, input_panel_area);
    }
}

#[derive(Debug)]
enum MountPopupSelectionState {
    Repo,
    Archive,
    MountPoint,
}

#[derive(Debug)]
struct MountPopup {
    state: MountPopupSelectionState,
    input: InputFieldWithSuggestions,
    repo_or_archive: Option<String>,
    num_list_archives: usize,
    is_done: bool,
}
impl MountPopup {
    fn new(is_repo: bool) -> Self {
        let state = if is_repo {
            MountPopupSelectionState::Repo
        } else {
            MountPopupSelectionState::Archive
        };
        MountPopup {
            state,
            input: InputFieldWithSuggestions::new(
                "".into(),
                "Archive or Repo to Mount".to_string(),
            ),
            repo_or_archive: None,
            num_list_archives: 0,
            is_done: false,
        }
    }
    fn update_list_archive_suggestions(&mut self, list_archives: &HashMap<String, ListRepository>) {
        let num_list_archives = list_archives
            .values()
            .map(|list_repo| list_repo.archives.len())
            .sum();
        if num_list_archives != self.num_list_archives {
            self.num_list_archives = num_list_archives;
            self.input
                .update_suggestions(list_archives.iter().flat_map(|(repo, list_archive)| {
                    list_archive
                        .archives
                        .iter()
                        .map(|archive| format!("{}::{}", repo.clone(), archive.name))
                }));
        }
    }
    fn handle_key_repo_or_archive(&mut self, key: KeyEvent) {
        let res = self.input.handle_key(
            key,
            |suggestions, input_buffer| {
                let mut input_buffer_changed = false;
                if let Some(res) = suggestions.range(input_buffer.clone()..).next() {
                    if res != input_buffer {
                        *input_buffer = res.clone();
                        input_buffer_changed = true;
                    }
                }
                input_buffer_changed
            },
            |suggestions, input_buffer| suggestions.contains(input_buffer),
        );
        if let Some(value) = res {
            self.repo_or_archive = Some(value);
            self.state = MountPopupSelectionState::MountPoint;
            self.input = InputFieldWithSuggestions::new(
                dirs::home_dir()
                    .map(|mut p| {
                        p.push("borg-mount");
                        p.to_string_lossy().to_string()
                    })
                    .unwrap_or_default(),
                "Mount Point".to_string(),
            );
        }
    }
    fn handle_key_mount_point_selection(&mut self, key: KeyEvent, borgtui: &mut BorgTui) {
        let res = self.input.handle_key(
            key,
            filter_directory_suggestions,
            |_suggestions, _input_buffer| true,
        );
        if let Some(mountpoint) = res {
            if let Err(e) = borgtui.mount(self.repo_or_archive.clone().unwrap(), mountpoint) {
                borgtui.add_error(format!("{}", e));
            }
            self.is_done = true;
        }
    }
}

impl Popup for MountPopup {
    fn on_tick(
        &mut self,
        command_channel: &Sender<Command>,
        directory_suggestions: &[PathBuf],
        list_archives: &HashMap<String, ListRepository>,
    ) -> BorgResult<()> {
        match &self.state {
            MountPopupSelectionState::Repo => {
                if self.input.suggestions.len() != list_archives.len() {
                    self.input.update_suggestions(list_archives.keys().cloned());
                }
            }
            MountPopupSelectionState::Archive => {
                self.update_list_archive_suggestions(list_archives);
            }
            MountPopupSelectionState::MountPoint => {
                self.input.update_suggestions(
                    directory_suggestions
                        .iter()
                        .map(|path| path.to_string_lossy().to_string()),
                );
                self.input.on_input_buffer_changed(|input_buffer| {
                    let command = Command::GetDirectorySuggestionsFor(input_buffer.to_string());
                    command_channel.blocking_send(command)?;
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, borgtui: &mut BorgTui) {
        let is_repo_or_archive_selection = matches!(
            &self.state,
            MountPopupSelectionState::Archive | MountPopupSelectionState::Repo
        );
        if is_repo_or_archive_selection {
            self.handle_key_repo_or_archive(key);
        } else {
            self.handle_key_mount_point_selection(key, borgtui);
        }
    }

    fn is_done(&self) -> bool {
        self.input.is_done() || self.is_done
    }

    fn draw(&self, frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect) {
        self.input.draw(frame, area, |suggestions, input_buffer| {
            suggestions.contains(input_buffer)
        })
    }
}

#[derive(Debug)]
struct AddFileToProfilePopup {
    path_successfully_added: Arc<AtomicBool>,
    input: InputFieldWithSuggestions,
}

fn filter_directory_suggestions(suggestions: &BTreeSet<String>, input_buffer: &mut String) -> bool {
    let mut input_buffer_changed = false;
    // TODO: Make this search fuzzier (so lower case characters work)
    if let Some(res) = suggestions.range(input_buffer.clone()..).next() {
        if input_buffer == res {
            // Given Borg doesn't support windows we won't be supporting backslashes here.
            // TODO: Add a timeout here
            // Canonicalize path
            if let Ok(res) = std::fs::canonicalize(&input_buffer) {
                *input_buffer = res.to_string_lossy().to_string();
            }
            if !input_buffer.ends_with('/') {
                input_buffer.push('/');
            }
        } else {
            *input_buffer = res.to_string();
        }
        input_buffer_changed = true;
    }
    input_buffer_changed
}

impl AddFileToProfilePopup {
    fn new(initial_text: String) -> Self {
        AddFileToProfilePopup {
            path_successfully_added: Arc::new(AtomicBool::new(false)),
            input: InputFieldWithSuggestions::new(initial_text, "Filepath to Add".to_string()),
        }
    }
}

impl Popup for AddFileToProfilePopup {
    fn handle_key(&mut self, key: KeyEvent, borgtui: &mut BorgTui) {
        let res = self.input.handle_key(
            key,
            // Completion function
            filter_directory_suggestions,
            // Validation Function
            // TODO: Check if the file or path exists
            |_, _| true,
        );
        if let Some(value) = res {
            if let Err(e) =
                borgtui.add_backup_path_to_profile(value, self.path_successfully_added.clone())
            {
                borgtui.add_error(format!("{}", e));
            }
        }
    }

    fn on_tick(
        &mut self,
        command_channel: &Sender<Command>,
        directory_suggestions: &[PathBuf],
        _list_archives: &HashMap<String, ListRepository>,
    ) -> BorgResult<()> {
        self.input.update_suggestions(
            directory_suggestions
                .iter()
                .map(|path| path.to_string_lossy().to_string()),
        );
        self.input.on_input_buffer_changed(|input_buffer| {
            let command = Command::GetDirectorySuggestionsFor(input_buffer.to_string());
            command_channel.blocking_send(command)?;
            Ok(())
        })?;
        Ok(())
    }

    fn is_done(&self) -> bool {
        // TODO: A fancy animation would be nice
        self.input.is_done() || self.path_successfully_added.load(Ordering::SeqCst)
    }

    fn draw(&self, frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect) {
        self.input.draw(frame, area, |_, input_buffer| {
            std::fs::metadata(input_buffer).is_ok()
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum ConfirmationButtonState {
    Yes,
    No,
}

type OnConfirmationFn = Box<dyn Fn(ConfirmationButtonState, &mut BorgTui)>;

struct ConfirmationPopup {
    text: String,
    button_state: ConfirmationButtonState,
    on_confirmation_fn: OnConfirmationFn,
    is_dismissed: bool,
}

impl std::fmt::Debug for ConfirmationPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfirmationPopup")
            .field("text", &self.text)
            .field("button_state", &self.button_state)
            .field("is_dismissed", &self.is_dismissed)
            .finish()
    }
}

impl ConfirmationButtonState {
    fn compliment(&self) -> ConfirmationButtonState {
        match self {
            ConfirmationButtonState::Yes => ConfirmationButtonState::No,
            ConfirmationButtonState::No => ConfirmationButtonState::Yes,
        }
    }
}

impl ConfirmationPopup {
    fn new(
        text: String,
        inital_button_state: ConfirmationButtonState,
        on_confirmation_fn: OnConfirmationFn,
    ) -> Self {
        Self {
            text,
            button_state: inital_button_state,
            is_dismissed: false,
            on_confirmation_fn,
        }
    }
}

impl Popup for ConfirmationPopup {
    fn handle_key(&mut self, key: KeyEvent, borgtui: &mut BorgTui) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('g'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                self.is_dismissed = true;
            }
            (KeyCode::Enter, _) => {
                (self.on_confirmation_fn)(self.button_state, borgtui);
                self.is_dismissed = true;
            }
            (KeyCode::Right | KeyCode::Left, _) => {
                self.button_state = self.button_state.compliment();
            }
            (KeyCode::Char('y'), _) => {
                self.button_state = ConfirmationButtonState::Yes;
                (self.on_confirmation_fn)(self.button_state, borgtui);
                self.is_dismissed = true;
            }
            (KeyCode::Char('n'), _) => {
                self.button_state = ConfirmationButtonState::No;
                (self.on_confirmation_fn)(self.button_state, borgtui);
                self.is_dismissed = true;
            }
            _ => (),
        }
    }

    fn on_tick(
        &mut self,
        _command_channel: &Sender<Command>,
        _directory_suggestions: &[PathBuf],
        _list_archives: &HashMap<String, ListRepository>,
    ) -> BorgResult<()> {
        Ok(())
    }

    fn draw(&self, frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect) {
        frame.render_widget(tui::widgets::Clear, area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(90), Constraint::Min(1)].as_ref())
            .split(area);
        let (text_area, button_area) = (chunks[0], chunks[1]);
        // Render message
        let text_panel = Paragraph::new(self.text.clone())
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Confirmation Dialog"),
            );
        frame.render_widget(text_panel, text_area);
        // Render Buttons
        let selected = if matches!(self.button_state, ConfirmationButtonState::Yes) {
            0
        } else {
            1
        };
        let buttons = Tabs::new([Spans::from("Yes (y)"), Spans::from("No (n)")].to_vec())
            .block(Block::default().title("Options").borders(Borders::ALL))
            .highlight_style(Style::default().fg(Color::Gray))
            .select(selected);
        frame.render_widget(buttons, button_area);
    }

    fn is_done(&self) -> bool {
        self.is_dismissed
    }
}

#[derive(Debug, Clone)]
struct MessagePopup {
    error_message: String,
    is_dismissed: bool,
}

impl MessagePopup {
    fn new(error_message: String) -> Self {
        MessagePopup {
            error_message,
            is_dismissed: false,
        }
    }
}
impl Popup for MessagePopup {
    fn handle_key(&mut self, key: KeyEvent, _borgtui: &mut BorgTui) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('g'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                self.is_dismissed = true;
            }
            _ => (),
        }
    }

    fn on_tick(
        &mut self,
        _command_channel: &Sender<Command>,
        _directory_suggestions: &[PathBuf],
        _list_archives: &HashMap<String, ListRepository>,
    ) -> BorgResult<()> {
        Ok(())
    }

    fn is_done(&self) -> bool {
        self.is_dismissed
    }

    fn draw(&self, frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect) {
        frame.render_widget(tui::widgets::Clear, area);
        let input_panel = Paragraph::new(self.error_message.clone())
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Error"));
        frame.render_widget(input_panel, area);
    }
}

trait Popup {
    fn handle_key(&mut self, key: KeyEvent, borgtui: &mut BorgTui);
    fn on_tick(
        &mut self,
        command_channel: &Sender<Command>,
        directory_suggestions: &[PathBuf],
        list_archives: &HashMap<String, ListRepository>,
    ) -> BorgResult<()>;
    fn draw(&self, frame: &mut Frame<CrosstermBackend<Stdout>>, area: Rect);
    fn is_done(&self) -> bool;
}

enum UserIntent {
    UnmountAllRepos,
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
    popup_stack: Vec<Box<dyn Popup>>,
    currently_mounted_items: Option<Vec<(String, String)>>,
    user_intent: Vec<UserIntent>,
    // This is not an enum field to make it easier to tab while a backup is in progress.
    backup_state: BackupState,
    list_archives_state: HashMap<String, ListRepository>,
    directory_suggestions: Vec<PathBuf>,
    directory_suggestions_update_num: usize,
    info_logs: RingBuffer<String, 10>,
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
            tick_rate: Duration::from_millis(TICK_RATE_MILLIS),
            profile,
            backup_path_sizes: HashMap::new(),
            command_channel,
            recv_channel,
            ui_state: UIState::ProfileView,
            previous_ui_state: None,
            popup_stack: Vec::new(),
            currently_mounted_items: None,
            user_intent: Vec::new(),
            backup_state: BackupState::default(),
            list_archives_state: HashMap::new(),
            directory_suggestions: Vec::new(),
            directory_suggestions_update_num: 0,
            info_logs: RingBuffer::new(),
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

    fn handle_keyboard_input(&mut self, key: KeyEvent) -> BorgResult<()> {
        match key.code {
            KeyCode::Char('q') => {
                self.done = true;
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
            KeyCode::Char('p') => {
                toggle_to_previous_state_or_run!(self, UIState::ProfileView, {
                    self.switch_ui_state(UIState::ProfileView);
                });
            }
            KeyCode::Char('a') => {
                let initial_dir = dirs::home_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.add_popup(AddFileToProfilePopup::new(initial_dir));
            }
            KeyCode::Char('s') => {
                if let Err(e) = self.send_save_command() {
                    let err_msg = format!("Failed to save profile: {}", e);
                    error!(err_msg);
                    self.add_error(err_msg);
                }
            }
            KeyCode::Char('c') => {
                self.add_info("Compacting each repo...");
                if let Err(e) = self.send_compact_command() {
                    let err_msg = format!("Failed to start compacting: {}", e);
                    error!(err_msg);
                    self.add_error(err_msg);
                }
            }
            KeyCode::Char('m') => {
                self.send_list_archives_command()?;
                self.add_popup(MountPopup::new(false));
            }
            KeyCode::Char('M') => {
                self.send_list_archives_command()?;
                self.add_popup(MountPopup::new(true));
            }
            KeyCode::Char('G') => {
                if let Some(currently_mounted_items) = self.currently_mounted_items.as_ref() {
                    let mut text_description =
                        vec!["Would you like to unmount the following mounts?\n".to_string()];
                    for (repo_or_archive, mountpoint) in currently_mounted_items {
                        text_description.push(format!("- [{}] {}", mountpoint, repo_or_archive));
                    }
                    let text_description = text_description.join("\n");
                    self.add_popup(ConfirmationPopup::new(
                        text_description,
                        ConfirmationButtonState::Yes,
                        Box::new(|state, borgtui| {
                            if let ConfirmationButtonState::Yes = state {
                                borgtui.user_intent.push(UserIntent::UnmountAllRepos);
                            }
                        }),
                    ))
                }
            }
            KeyCode::Char('\\') => {
                self.add_info("Pruning each repo...");
                if let Err(e) = self.send_prune_command() {
                    let err_msg = format!("Failed to start pruning: {}", e);
                    error!(err_msg);
                    self.add_error(err_msg);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn run_app(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> BorgResult<()> {
        let mut last_tick = Instant::now();
        self.profile
            .backup_paths()
            .to_vec()
            .iter()
            .for_each(|backup_path| {
                if let Err(e) = self.send_backup_dir_size_command(
                    backup_path.clone(),
                    self.profile.exclude_patterns().to_vec(),
                ) {
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
            // Remove finished popups
            self.popup_stack.retain(|popup| !popup.is_done());
            // Handle keyboard input
            if crossterm::event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    match self.popup_stack.pop() {
                        Some(mut popup) => {
                            popup.handle_key(key, self);
                            self.popup_stack.insert(0, popup);
                        }
                        None => self.handle_keyboard_input(key)?,
                    }
                }
            }
            if last_tick.elapsed() >= self.tick_rate {
                if let Err(e) = self.on_tick() {
                    error!("{}", e);
                    self.add_error(format!("{}", e));
                }
                last_tick = Instant::now();
            }
        }
    }

    fn add_popup<P: Popup + 'static>(&mut self, popup: P) {
        self.popup_stack.push(Box::new(popup))
    }

    fn add_info<I: Into<String>>(&mut self, info: I) {
        self.info_logs.push_back(info.into())
    }

    fn add_error(&mut self, error: String) {
        self.add_popup(MessagePopup::new(error))
    }

    fn add_backup_path_to_profile(
        &mut self,
        path: String,
        signal_success: Arc<AtomicBool>,
    ) -> BorgResult<()> {
        self.command_channel
            .blocking_send(Command::UpdateProfileAndSave(
                self.profile.clone(),
                ProfileOperation::AddBackupPath(PathBuf::try_from(path)?),
                signal_success,
            ))?;
        Ok(())
    }

    fn mount(&mut self, repo_or_archive: String, mountpoint: String) -> BorgResult<()> {
        let repo = self.profile.find_repo_from_mount_src(&repo_or_archive)?;
        self.command_channel
            .blocking_send(Command::Mount(repo, repo_or_archive, mountpoint))?;
        Ok(())
    }

    fn unmount_all(&mut self) -> BorgResult<()> {
        let mount_points: Vec<String> = self
            .currently_mounted_items
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .map(|(_, mountpoint)| mountpoint.to_string())
                    .collect()
            })
            .unwrap_or_default();
        for mountpoint in mount_points {
            self.command_channel
                .blocking_send(Command::Unmount(mountpoint))?;
        }
        self.currently_mounted_items = None;
        Ok(())
    }

    fn send_backup_dir_size_command(
        &mut self,
        dir: PathBuf,
        exclude_patterns: Vec<String>,
    ) -> BorgResult<()> {
        let byte_count = self
            .backup_path_sizes
            .entry(dir.clone())
            .or_default()
            .clone();
        self.command_channel
            .blocking_send(Command::DetermineDirectorySize(
                dir,
                byte_count,
                exclude_patterns,
            ))?;
        Ok(())
    }

    fn send_create_command(&mut self) -> BorgResult<()> {
        let command = Command::CreateBackup(self.profile.clone());
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn send_compact_command(&mut self) -> BorgResult<()> {
        for repo in self.profile.active_repositories() {
            let command = Command::Compact(repo.clone());
            self.command_channel.blocking_send(command)?;
        }
        Ok(())
    }

    fn send_prune_command(&mut self) -> BorgResult<()> {
        for repo in self.profile.active_repositories() {
            let command = Command::Prune(repo.clone(), self.profile.prune_options());
            self.command_channel.blocking_send(command)?;
        }
        Ok(())
    }

    fn send_save_command(&mut self) -> BorgResult<()> {
        let command = Command::SaveProfile(self.profile.clone());
        self.command_channel.blocking_send(command)?;
        Ok(())
    }

    fn send_list_archives_command(&mut self) -> BorgResult<()> {
        for repo in self.profile.active_repositories() {
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
            .or_insert_with(RingBuffer::new)
            .push_back(backup_stat);
    }

    fn insert_recently_backed_up_file(&mut self, repo: String, path: String) {
        self.backup_state
            .recently_backed_up_files
            .entry(repo)
            .or_default()
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
                        self.backup_state.mark_finished(repo.clone());
                        self.add_info(format!("Finished backing up {}", repo));
                        tracing::info!("Finished backing up {}", repo);
                    }
                }
            }
            CommandResponse::Info(info_string) => {
                info!(info_string);
                self.add_info(info_string)
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
            CommandResponse::MountResult(repo_or_archive, mountpoint) => {
                if self.currently_mounted_items.is_none() {
                    self.currently_mounted_items = Some(Vec::new());
                }
                if let Some(mounted_items) = self.currently_mounted_items.as_mut() {
                    mounted_items.push((repo_or_archive, mountpoint))
                }
            }
            CommandResponse::Error(error_message) => self.add_error(error_message),
            CommandResponse::ProfileUpdated(profile) => {
                self.add_info("Profile updated.");
                // TODO: Refactor this to be nicer.
                self.profile = profile;
                let paths_to_add: Vec<_> = self
                    .profile
                    .backup_paths()
                    .iter()
                    .filter(|path| !self.backup_path_sizes.contains_key(*path))
                    .cloned()
                    .collect();
                for backup_path in paths_to_add {
                    if let Err(e) = self.send_backup_dir_size_command(
                        backup_path.clone(),
                        self.profile.exclude_patterns().to_vec(),
                    ) {
                        self.add_error(format!(
                            "Failed to determine the size of backup path {}: {}",
                            backup_path.to_string_lossy(),
                            e
                        ))
                    }
                }
            }
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
        // TODO: Above or below?
        self.popup_stack.iter_mut().try_for_each(|popup| {
            popup.on_tick(
                &self.command_channel,
                &self.directory_suggestions,
                &self.list_archives_state,
            )
        })?;
        let user_intentions = self.user_intent.drain(..).collect::<Vec<_>>();
        for user_intent in user_intentions {
            match user_intent {
                UserIntent::UnmountAllRepos => self.unmount_all()?,
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

    fn get_min_and_max_stat_value(
        &self,
        metric_fn: &dyn Fn(&BackupStat) -> f64,
    ) -> Option<(f64, f64)> {
        let mut stats_iter = self
            .profile
            .active_repositories()
            .filter_map(|repo| self.backup_state.backup_stats.get(&repo.path))
            .flat_map(|ring_buffer| ring_buffer.iter().map(metric_fn));
        let first_stat = stats_iter.next()?;
        let mut min = first_stat;
        let mut max = first_stat;
        stats_iter.for_each(|value| {
            min = f64::min(min, value);
            max = f64::max(max, value);
        });
        Some((min, max))
    }

    fn draw_backup_chart<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        let mut datasets = Vec::new();
        // TODO: How to make original size look good?
        let original_size_metrics: Vec<_> = self
            .profile
            .active_repositories()
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
            .active_repositories()
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
            .repositories()
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
                    .unwrap_or_default();
                if let Some(backup_stat) = self.latest_stats_for_repo(&repo.path) {
                    items.insert(
                        0,
                        ListItem::new(format!(
                            "# files: {} (backup size after deduplication: {})",
                            backup_stat.num_files,
                            PrettyBytes(backup_stat.deduplicated_size),
                        )),
                    );
                }
                if repo.disabled() {
                    items.insert(0, ListItem::new("# Repo disabled, not backing up..."));
                }
                let is_finished = self.backup_state.is_finished(&repo.path);
                let is_disabled = repo.disabled();
                let backup_span = if is_finished {
                    Span::styled(
                        format!("FINISHED Backup {}", repo),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )
                } else if is_disabled {
                    Span::styled(
                        format!("DISABLED Backup {}", repo),
                        Style::default()
                            .fg(Color::Gray)
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

    fn repos_with_archives(&self) -> Vec<(String, Option<ListRepository>, bool)> {
        self.profile
            .repositories()
            .iter()
            .map(|repo| {
                (
                    repo.path.clone(),
                    self.list_archives_state.get(&repo.path).cloned(),
                    repo.disabled(),
                )
            })
            .collect()
    }

    fn draw_all_archive_lists<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
        // (RepoName, Option<ListArchive>)
        let repos_with_archives: Vec<_> = self.repos_with_archives();
        let backup_constraints = std::iter::repeat(Constraint::Percentage(
            100 / repos_with_archives.len() as u16,
        ))
        .take(self.profile.num_repos())
        .collect::<Vec<_>>();
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(backup_constraints.as_ref())
            .split(area);
        for ((repo_name, list_archive, repo_disabled), area) in
            repos_with_archives.into_iter().zip(areas)
        {
            let archive_rows = match list_archive {
                Some(list_archive) => list_archive
                    .archives
                    .iter()
                    .rev()
                    .map(|archive| {
                        Row::new([
                            Cell::from(format!("{}", archive.start.format("%b %d %Y %H:%M:%S"))),
                            Cell::from(archive.name.clone()),
                        ])
                    })
                    .collect::<Vec<_>>(),
                None => {
                    let cell = if repo_disabled {
                        Cell::from("Repo disabled, not fetching...")
                    } else {
                        Cell::from("Still fetching...")
                    };
                    vec![Row::new([cell, Cell::from("")])]
                }
            };
            let archive_table = Table::new(archive_rows)
                .widths(&[Constraint::Percentage(30), Constraint::Percentage(70)])
                .block(Block::default().borders(Borders::ALL).title(repo_name));
            frame.render_widget(archive_table, area)
        }
    }

    fn draw_info_panel<B: Backend>(&mut self, frame: &mut Frame<B>, area: Rect) {
        let text = vec![
            Spans::from("• Press 'q' to quit"),
            Spans::from("• Press 'u' to backup"),
            Spans::from("• Press 'p' to toggle profile"),
            Spans::from("• Press 'l' to list archives"),
            Spans::from("• Press 'a' to add a backup path"),
            Spans::from("• Press 's' to save profile"),
            Spans::from("• Press 'c' to compact"),
            Spans::from("• Press 'm' to mount"),
            Spans::from("• Press 'M' to mount a repo"),
            Spans::from("• Press 'G' to unmount all"),
            Spans::from("• Press '\\' to prune"),
        ];
        let info_panel = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("borgtui"));
        frame.render_widget(info_panel, area);
    }

    fn draw_info_logs<B: Backend>(&self, frame: &mut Frame<B>, area: Rect) {
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

    fn draw_mounted_items<B: Backend>(&mut self, frame: &mut Frame<B>, backup_paths_area: Rect) {
        let header_cells = ["Source", "Mount Point"]
            .iter()
            .map(|header| Cell::from(*header));
        let header_row = Row::new(header_cells);
        let rows = self
            .currently_mounted_items
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .map(|(repo_or_archive, mount_point)| {
                Row::new([
                    Cell::from(repo_or_archive.clone()),
                    Cell::from(mount_point.clone()),
                ])
            })
            .collect::<Vec<_>>();
        let table = Table::new(rows)
            .header(header_row)
            .widths(&[Constraint::Percentage(50), Constraint::Percentage(50)])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Mounted Directories"),
            );
        frame.render_widget(table, backup_paths_area);
    }

    fn draw_profile_view<B: Backend>(
        &mut self,
        frame: &mut Frame<B>,
        repo_area: Rect,
        backup_paths_area: Rect,
        mounted_items: Option<Rect>,
    ) {
        let repo_items: Vec<_> = self
            .profile
            .repositories()
            .iter()
            .map(|repo| {
                let text = if repo.disabled() { " -- DISABLED" } else { "" };
                ListItem::new(format!("{}{}", repo.path.clone(), text))
            })
            .collect();
        let repo_list = List::new(repo_items)
            .block(Block::default().borders(Borders::ALL).title("Repositories"));
        frame.render_widget(repo_list, repo_area);
        self.draw_backup_dirs(frame, backup_paths_area);
        if let Some(mounted_items_area) = mounted_items {
            self.draw_mounted_items(frame, mounted_items_area);
        }
    }

    fn draw_main_right_panel<B: Backend>(&mut self, frame: &mut Frame<B>, right_area: Rect) {
        match &self.ui_state {
            UIState::ProfileView => {
                let mounted_items = self
                    .currently_mounted_items
                    .as_ref()
                    .map(|mounted| mounted.len())
                    .unwrap_or(0);
                let (repo_area, backup_paths_area, mounted_items) = if mounted_items > 0 {
                    let profile_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Percentage(30),
                            Constraint::Percentage(30),
                            Constraint::Percentage(40),
                        ])
                        .split(right_area);
                    (
                        profile_chunks[0],
                        profile_chunks[1],
                        Some(profile_chunks[2]),
                    )
                } else {
                    let profile_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                        .split(right_area);
                    (profile_chunks[0], profile_chunks[1], None)
                };
                self.draw_profile_view(frame, repo_area, backup_paths_area, mounted_items);
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

    fn draw_ui(&mut self, frame: &mut Frame<CrosstermBackend<Stdout>>) {
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
        if let Some(popup) = self.popup_stack.last() {
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
            popup.draw(frame, corner);
        }
    }
}
