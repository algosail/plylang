//! `plylang build --watch` after the first successful build.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use super::{open_session_over, BuildError, BuildFlags, Session, SessionOver};

pub fn build_watch(dir: &Path, flags: &BuildFlags) -> Result<(), BuildError> {
    if flags.exports {
        return Err(BuildError::plain(
            "--watch cannot be combined with --exports",
        ));
    }
    let mut session = open_session_over(
        dir,
        &SessionOver {
            entry: flags.entry.clone(),
            out_rel: flags.out.clone(),
            foreign: flags.foreign.clone(),
        },
    )?;
    session.check_all()?;
    session.write_outputs()?;
    println!("watching");
    let _ = io::stdout().flush();
    session.watch_source()
}

impl Session {
    fn watch_source(&mut self) -> Result<(), BuildError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = RecommendedWatcher::new(tx, notify::Config::default())
            .map_err(|error| BuildError::plain(error.to_string()))?;
        watcher
            .watch(&self.package.source, RecursiveMode::Recursive)
            .map_err(|error| BuildError::plain(error.to_string()))?;
        loop {
            let first = rx
                .recv()
                .map_err(|error| BuildError::plain(error.to_string()))?;
            let mut paths = Vec::new();
            push_paths(first, &mut paths);
            let deadline = Instant::now() + Duration::from_millis(40);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match rx.recv_timeout(remaining) {
                    Ok(event) => push_paths(event, &mut paths),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(BuildError::plain("watch stopped"));
                    }
                }
            }
            paths.sort();
            paths.dedup();
            // A save can land as two files. The first pass updates each one.
            // The second pass reads them again, so an importer saved in the
            // same burst sees the dependency that the first pass checked.
            for path in &paths {
                let _ = self.refresh_path(path);
            }
            for path in &paths {
                if let Err(error) = self.refresh_path(path) {
                    eprintln!("{}", error.render());
                }
            }
        }
    }
}

fn push_paths(event: Result<Event, notify::Error>, paths: &mut Vec<PathBuf>) {
    let Ok(event) = event else {
        return;
    };
    for path in event.paths {
        if super::is_ply_source(&path) {
            paths.push(path);
        }
    }
}
