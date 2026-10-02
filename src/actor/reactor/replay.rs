// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Support for recording reactor events to a file and replaying them later.
//!
//! This is used in development.

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
#[cfg(test)]
use tempfile::NamedTempFile;
use tokio::sync::mpsc::unbounded_channel;
use tracing::Span;

use super::{Event, Reactor};
use crate::actor::app::{AppThreadHandle, Request, pid_t};
use crate::actor::layout::LayoutManager;
use crate::actor::parked_journal::{JournalEntry, ParkedJournal};
use crate::collections::HashMap;
use crate::config::Config;
use crate::model::contexts::Contexts;
use crate::sys::app::Process;

thread_local! {
    static DESERIALIZE_THREAD_HANDLE: RefCell<Option<AppThreadHandle>> = RefCell::new(None);
}

pub(super) fn deserialize_app_thread_handle() -> AppThreadHandle {
    DESERIALIZE_THREAD_HANDLE
        .with(|handle| handle.borrow().clone().expect("No deserialize thread handle set!"))
}

/// File to record incoming events.
pub struct Record {
    file: Option<File>,
    #[cfg(test)]
    temp: Option<NamedTempFile>,
    /// Where the layout line is, and how long it is, while it can still be
    /// rewritten because no event follows it.
    layout_line: Option<(u64, u64)>,
}

// The format is simple:
// One line for the config, one for the layout, one for the state read at
// launch, and then one line per event. Recordings made before the state
// line existed start their events on the third line.

/// What the reactor read at launch besides the config and the layout: the
/// parked-window journal, the contexts if they were read, and whether the
/// process of each journal entry ran then.
#[derive(Serialize, Deserialize)]
pub(super) struct LaunchState {
    pub(super) journal: Vec<JournalEntry>,
    pub(super) contexts: Option<Contexts>,
    /// The process check of each journal entry's pid at launch. A replay
    /// answers from this instead of the processes on the machine that
    /// replays it.
    #[serde(default)]
    pub(super) processes: Vec<(pid_t, Process)>,
}

impl Record {
    pub fn new(path: Option<&Path>) -> Self {
        Self {
            file: path.map(|path| File::create(path).unwrap()),
            #[cfg(test)]
            temp: None,
            layout_line: None,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(temp: NamedTempFile) -> Self {
        Self {
            file: None,
            temp: Some(temp),
            layout_line: None,
        }
    }

    #[cfg(test)]
    pub(super) fn temp(&mut self) -> Option<&mut NamedTempFile> {
        self.temp.as_mut()
    }

    #[cfg(test)]
    pub(super) fn keep(&mut self) -> Result<PathBuf, anyhow::Error> {
        let Some(temp) = self.temp.take() else {
            anyhow::bail!("no temp file")
        };
        Ok(temp.keep()?.1)
    }

    fn file(&mut self) -> Option<&mut File> {
        #[cfg(test)]
        return self.file.as_mut().or(self.temp.as_mut().map(|temp| temp.as_file_mut()));
        #[cfg(not(test))]
        self.file.as_mut()
    }

    pub(super) fn start(&mut self, config: &Config, layout: &LayoutManager) {
        let Some(file) = self.file() else { return };
        let config = ron::ser::to_string(&config).unwrap();
        let layout = ron::ser::to_string(&layout).unwrap();
        writeln!(file, "{config}").unwrap();
        let layout_at = file.stream_position().unwrap();
        writeln!(file, "{layout}").unwrap();
        let layout_end = file.stream_position().unwrap();
        self.layout_line = Some((layout_at, layout_end - layout_at));
    }

    /// Records the layout as it is now, and what the reactor read at launch.
    /// Call it after the contexts were read, and before the first event, so
    /// that the layout line shows the layouts that reading the contexts left.
    pub(super) fn launch_state(&mut self, layout: &LayoutManager, state: &LaunchState) {
        let layout = ron::ser::to_string(layout).unwrap();
        let line = ron::ser::to_string(state).unwrap();
        let layout_line = self.layout_line.take();
        let Some(file) = self.file() else { return };
        if let Some((at, len)) = layout_line {
            // Rewrite the layout line in place when nothing follows it yet.
            let end = at + len;
            if file.metadata().is_ok_and(|meta| meta.len() == end) {
                file.seek(SeekFrom::Start(at)).unwrap();
                writeln!(file, "{layout}").unwrap();
                let after = file.stream_position().unwrap();
                if after < end {
                    file.set_len(after).unwrap();
                }
                file.seek(SeekFrom::End(0)).unwrap();
            }
        }
        writeln!(file, "{line}").unwrap();
    }

    pub(super) fn on_event(&mut self, event: &Event) {
        let Some(file) = self.file() else { return };
        let line = ron::ser::to_string(&event).unwrap();
        writeln!(file, "{line}").unwrap();
    }
}

pub fn replay(
    path: &Path,
    mut on_event: impl FnMut(Span, Request) + Send + 'static,
) -> anyhow::Result<()> {
    let file = BufReader::new(File::open(path)?);
    let (tx, mut rx) = unbounded_channel();
    let handle = AppThreadHandle::new_for_test(tx);
    DESERIALIZE_THREAD_HANDLE.with(|h| h.borrow_mut().replace(handle));
    let mut lines = file.lines();
    let config = ron::de::from_str(&lines.next().expect("Empty restore file")?)?;
    let layout = ron::de::from_str(&lines.next().expect("Expected layout line")?)?;
    let mut first_event = None;
    let mut state = None;
    if let Some(line) = lines.next() {
        let line = line?;
        match ron::de::from_str::<LaunchState>(&line) {
            Ok(read) => state = Some(read),
            Err(_) => first_event = Some(line),
        }
    }
    let (group_indicators_tx, _) = crate::actor::channel();
    // A replay must not read or change the journal or the contexts of a
    // running Sugarglider, so both stay in memory.
    let journal = match &mut state {
        Some(state) => ParkedJournal::in_memory_with(std::mem::take(&mut state.journal)),
        None => ParkedJournal::in_memory(),
    };
    let mut reactor = Reactor::new(
        Arc::new(config),
        layout,
        Record::new(None),
        group_indicators_tx,
        journal,
    );
    if let Some(state) = state {
        // The process checks come from the recording: which processes run on
        // the machine that replays it says nothing about the recorded run.
        let processes: HashMap<pid_t, Process> = state.processes.into_iter().collect();
        if !processes.is_empty() {
            reactor.process_lookup =
                Box::new(move |pid| processes.get(&pid).cloned().unwrap_or(Process::Gone));
        }
        if let Some(contexts) = state.contexts {
            reactor.contexts = contexts;
        }
    }
    std::thread::spawn(move || {
        // Unfortunately we have to spawn a thread because the reactor blocks
        // on raise requests currently.
        while let Some((span, request)) = rx.blocking_recv() {
            on_event(span, request);
        }
    });
    if let Some(line) = first_event {
        reactor.handle_event(ron::de::from_str(&line)?);
    }
    for line in lines {
        reactor.handle_event(ron::de::from_str(&line?)?);
    }
    Ok(())
}
