pub(crate) type BorgResult<T> = anyhow::Result<T>;

use std::{
    collections::{BTreeSet, VecDeque},
    fmt::Display,
    path::PathBuf,
};

pub(crate) const EXTENDED_NOTIFICATION_DURATION: std::time::Duration =
    std::time::Duration::from_secs(60);
pub(crate) const SHORT_NOTIFICATION_DURATION: std::time::Duration =
    std::time::Duration::from_secs(15);

/// Send a CommandResponse::Info in a channel.
macro_rules! send_info {
    ($channel:expr, $info_message:expr) => {
        if let Err(e) = $channel.send(CommandResponse::Info($info_message)).await {
            tracing::error!(
                "Error occurred while sending info message \"{}\": {}",
                $info_message,
                e
            );
        }
    };
    ($channel:expr, $info_message:expr, $error_message:expr) => {
        if let Err(e) = $channel.send(CommandResponse::Info($info_message)).await {
            tracing::error!($error_message, e);
        }
    };
}
use glob::Pattern;
use notify_rust::{Notification, Timeout};
pub(crate) use send_info;

/// Send a CommandResponse::Info in a channel.
macro_rules! send_error {
    ($channel:expr, $info_message:expr) => {
        if let Err(e) = $channel.send(CommandResponse::Error($info_message)).await {
            tracing::error!(
                "Error occurred while sending error message \"{}\": {}",
                $info_message,
                e
            );
        }
    };
    ($channel:expr, $info_message:expr, $error_message:expr) => {
        if let Err(e) = $channel.send(CommandResponse::Error($info_message)).await {
            tracing::error!($error_message, e);
        }
    };
}
pub(crate) use send_error;

macro_rules! log_on_error {
    ($result_expr:expr, $log_message:expr) => {
        match $result_expr {
            Ok(res) => res,
            Err(e) => {
                tracing::error!($log_message, e);
                return;
            }
        }
    };
}
pub(crate) use log_on_error;

/// Send a CommandResponse::Info in a channel.
macro_rules! take_repo_lock {
    ($channel:expr, $repo:expr) => {
        if $repo.lock.try_lock().is_err() {
            send_info!(
                $channel,
                format!("Repo lock {} is already held, waiting...", $repo)
            );
        }
        let _backup_guard = $repo.lock.lock().await;
    };
    ($channel:expr, $repo:expr, $message:expr) => {
        if $repo.lock.try_lock().is_err() {
            send_info!($channel, format!($message, $repo));
        }
        let _backup_guard = $repo.lock.lock().await;
    };
}
pub(crate) use take_repo_lock;

#[derive(Debug, Default)]
pub(crate) struct RingBuffer<T, const N: usize> {
    deque: VecDeque<T>,
}

impl<T, const N: usize> RingBuffer<T, N> {
    pub(crate) fn new() -> Self {
        Self {
            deque: VecDeque::with_capacity(N),
        }
    }

    pub(crate) fn push_back(&mut self, item: T) {
        self.deque.push_back(item);
        if self.deque.len() > N {
            self.deque.pop_front();
        }
    }

    pub(crate) fn back(&self) -> Option<&T> {
        self.deque.back()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.deque.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.deque.iter()
    }
}

impl<T, const N: usize> FromIterator<T> for RingBuffer<T, N> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut r = RingBuffer::new();
        for item in iter.into_iter() {
            r.push_back(item)
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    #[test]
    fn test_pushes() {
        let mut r = RingBuffer::<char, 3>::new();
        for c in 'A'..='C' {
            r.push_back(c);
        }
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['A', 'B', 'C']);
        assert_eq!(r.back(), Some(&'C'));
        r.push_back('D');
        assert_eq!(r.back(), Some(&'D'));
        r.push_back('E');
        assert_eq!(r.back(), Some(&'E'));
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['C', 'D', 'E']);
        r.push_back('F');
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['D', 'E', 'F']);
        r.push_back('G');
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec!['E', 'F', 'G']);
    }

    #[test]
    fn test_empty_iter() {
        let empty: RingBuffer<u32, 256> = RingBuffer::new();
        let test: Vec<u32> = Vec::new();
        assert_eq!(empty.iter().copied().collect::<Vec<_>>(), test);
    }

    #[test]
    fn test_larger() {
        let big: RingBuffer<u32, 256> = (0..=1024).collect();
        assert_eq!(
            big.iter().copied().collect::<Vec<_>>(),
            (769..=1024).collect::<Vec<_>>()
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub(crate) struct PrettyBytes(pub(crate) u64);

impl PrettyBytes {
    const UNITS: [&'static str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

    fn scaled_with_unit(&self) -> (f64, usize, &'static str) {
        let index = ((self.0 as f64).ln() / 1024_f64.ln()).trunc() as usize;
        match Self::UNITS.get(index) {
            Some(unit) => {
                let precision = if index < 3 { 0 } else { 3 };
                (self.0 as f64 / 1024f64.powf(index as f64), precision, unit)
            }
            None => (self.0 as f64, 0, "B"),
        }
    }

    pub(crate) fn from_megabytes_f64(kb: f64) -> Self {
        PrettyBytes((kb * 1024.0 * 1024.0).trunc() as u64)
    }
}

impl Display for PrettyBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (scaled, precision, unit) = self.scaled_with_unit();
        write!(f, "{0:.1$}", scaled, precision)?;
        write!(f, " {}", unit)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DirectoryFinder {
    known_directories: BTreeSet<PathBuf>,
    num_updates: usize,
    exclude_patterns: Vec<glob::Pattern>,
}

impl DirectoryFinder {
    const UPDATE_GUESS_MAX_DEPTH: usize = 2;
    pub(crate) fn new() -> Self {
        Self {
            known_directories: BTreeSet::new(),
            num_updates: 0,
            exclude_patterns: vec![],
        }
    }

    pub(crate) fn seed_exclude_patterns(&mut self, exclude_patterns: &[String]) -> BorgResult<()> {
        self.exclude_patterns = exclude_patterns
            .iter()
            .map(|s| Pattern::new(s.as_str()))
            .collect::<Result<_, _>>()?;
        Ok(())
    }

    pub(crate) fn seed_from_directory(&mut self, directory: PathBuf, max_depth: usize) {
        let all_directories = walkdir::WalkDir::new(directory)
            .max_depth(max_depth)
            .follow_links(true)
            .into_iter()
            .filter_entry(|entry| {
                !self
                    .exclude_patterns
                    .iter()
                    .any(|pattern| pattern.matches_path(entry.path()))
            })
            .filter_map(|e| e.ok())
            .filter(|entry| entry.file_type().is_dir())
            .map(|entry| entry.path().to_owned());
        self.known_directories.extend(all_directories);
        self.num_updates += 1;
    }

    pub(crate) fn update_guess(&mut self, file_path_fragment: &str) -> BorgResult<()> {
        let path = PathBuf::from(file_path_fragment);
        self.seed_from_directory(path, Self::UPDATE_GUESS_MAX_DEPTH);
        self.num_updates += 1;
        Ok(())
    }

    pub(crate) fn suggestions(
        &self,
        starting_fragment: &str,
        max_results: usize,
    ) -> BorgResult<(Vec<PathBuf>, usize)> {
        let exclude_dot_files = !starting_fragment.contains('.');
        let path = PathBuf::from(starting_fragment);
        Ok((
            self.known_directories
                .range(path..)
                .filter(|res| !(res.to_string_lossy().contains('.') && exclude_dot_files))
                .take(max_results)
                .cloned()
                .collect(),
            self.num_updates,
        ))
    }
}

pub(crate) async fn show_notification<I: Into<Timeout>>(
    summary: &str,
    body: &str,
    duration: I,
) -> BorgResult<()> {
    Notification::new()
        .summary(summary)
        .subtitle("BorgTUI")
        .body(body)
        .timeout(duration)
        .show_async()
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) enum BackupCreationProgress {
    InProgress {
        original_size: u64,
        compressed_size: u64,
        deduplicated_size: u64,
        num_files: u64,
        current_path: String,
    },
    Finished,
}

#[derive(Debug)]
pub(crate) struct BackupCreateProgress {
    pub(crate) repository: String,
    pub(crate) create_progress: BackupCreationProgress,
}

#[derive(Debug, Clone)]
pub(crate) struct Archive {
    pub(crate) name: String,
    pub(crate) creation_date: chrono::NaiveDateTime,
}

#[derive(Debug, Clone)]
pub(crate) struct RepositoryArchives {
    pub(crate) path: String,
    pub(crate) archives: Vec<Archive>,
}
