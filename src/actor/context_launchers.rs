// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Named context commands for the root search in Raycast and SuperCmd.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use arc_swap::ArcSwapOption;
use serde::{Deserialize, Serialize};

use crate::actor::contexts_snapshot::ContextsSnapshot;

const MANIFEST: &str = ".sugarglider-commands.json";
const LIMIT: u64 = 4 * 1024 * 1024;

struct WriterLock(File);

impl Drop for WriterLock {
    fn drop(&mut self) {
        // A fork can retain this descriptor after the worker closes its copy.
        let _ = self.0.unlock();
    }
}

pub fn directory() -> PathBuf {
    crate::config::context_commands_dir()
}

/// Coalesces snapshots so filesystem work never holds up window management.
pub struct Publisher {
    latest: Arc<ArcSwapOption<ContextsSnapshot>>,
    wake: SyncSender<()>,
}

impl Publisher {
    pub fn send(&self, snapshot: Arc<ContextsSnapshot>) {
        self.latest.store(Some(snapshot));
        let _ = self.wake.try_send(());
    }
}

pub fn spawn() -> io::Result<Publisher> {
    let binary = sibling_cli(&std::env::current_exe()?)?;
    if !binary.is_file() {
        tracing::warn!(
            ?binary,
            "Launcher commands need the Sugarglider CLI beside the server"
        );
    }
    let dir = directory();
    let latest = Arc::new(ArcSwapOption::empty());
    let source = latest.clone();
    let (wake, events) = mpsc::sync_channel(1);
    std::thread::Builder::new().name("context-launchers".into()).spawn(move || {
        let mut attempted = None;
        let mut last_error = None;
        let mut retry_at: Option<Instant> = None;
        let mut retry_delay = Duration::from_secs(2);
        loop {
            let event = match retry_at {
                Some(deadline) => {
                    events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                }
                None => events.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            };
            match event {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let Some(snapshot) = source.load_full() else { continue };
            let desired = commands(&snapshot, &binary).map_err(|error| format!("{error:#}"));
            if attempted.as_ref() == Some(&desired) {
                if retry_at.is_none_or(|deadline| Instant::now() < deadline) {
                    continue;
                }
            } else {
                retry_delay = Duration::from_secs(2);
            }
            // A failed batch can replace some files before the next snapshot
            // reverts their names, so each changed attempt needs reconciliation.
            attempted = Some(desired.clone());
            let error = desired
                .and_then(|desired| sync(&dir, &desired).map_err(|error| format!("{error:#}")))
                .err();
            retry_at = error.as_ref().map(|_| Instant::now() + retry_delay);
            retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
            if error != last_error {
                if let Some(error) = &error {
                    tracing::warn!("Could not update launcher context commands: {error}");
                }
                last_error = error;
            }
        }
    })?;
    Ok(Publisher { latest, wake })
}

fn sibling_cli(server: &Path) -> io::Result<PathBuf> {
    Ok(server.canonicalize()?.with_file_name("sugarglider"))
}

type Commands = BTreeMap<String, String>;

fn commands(snapshot: &ContextsSnapshot, binary: &Path) -> anyhow::Result<Commands> {
    let mut result = Commands::new();
    if !snapshot.enabled {
        return Ok(result);
    }
    let binary = binary.to_str().context("The CLI path is not valid UTF-8")?;
    if !Path::new(binary).is_absolute() || binary.contains(['\n', '\r', '\0']) {
        bail!("The CLI path must be absolute and contain no line breaks");
    }
    let binary = shell_quote(binary);
    for context in &snapshot.contexts {
        let id = context.id.get();
        result.insert(
            format!("context-{id}.sh"),
            script(&context.name, &binary, &format!("switch --id {id}")),
        );
    }
    result.insert(
        "everything.sh".into(),
        script("Everything", &binary, "everything"),
    );
    if snapshot.unsorted.listed {
        result.insert(
            "unsorted.sh".into(),
            script("Unsorted", &binary, "switch --name Unsorted"),
        );
    }
    Ok(result)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn script(name: &str, binary: &str, action: &str) -> String {
    let title: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let title = if title.trim().is_empty() {
        "Unnamed context"
    } else {
        &title
    };
    format!(
        "#!/bin/bash\n\
         # @raycast.schemaVersion 1\n\
         # @raycast.title {title}\n\
         # @raycast.mode compact\n\
         # @raycast.packageName Sugarglider Contexts\n\
         # @raycast.description Switch the active Sugarglider context\n\
         # Generated by Sugarglider. Changes belong in Sugarglider's context settings.\n\
         set -euo pipefail\n\
         if [[ ! -x {binary} ]]; then\n\
         \x20 echo 'Sugarglider CLI is missing. Build or install both binaries and restart Sugarglider.' >&2\n\
         \x20 exit 1\n\
         fi\n\
         exec {binary} context {action} 2>&1\n"
    )
}

/// `previous` permits recovery when a process stops between writing the
/// ownership manifest and replacing or removing a command.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct OwnedFile {
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u8,
    files: BTreeMap<String, OwnedFile>,
}

fn read_file(path: &Path) -> anyhow::Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !meta.is_file() || meta.len() > LIMIT {
        bail!(
            "Refusing a non-file or oversized launcher file: {}",
            path.display()
        );
    }
    let mut content = String::new();
    File::open(path)?.take(LIMIT + 1).read_to_string(&mut content)?;
    if content.len() as u64 > LIMIT {
        bail!("Launcher file is too large: {}", path.display());
    }
    Ok(Some(content))
}

fn owned_name(name: &str) -> bool {
    matches!(name, "everything.sh" | "unsorted.sh")
        || name
            .strip_prefix("context-")
            .and_then(|s| s.strip_suffix(".sh"))
            .is_some_and(|id| id.parse::<u32>().is_ok_and(|id| name == format!("context-{id}.sh")))
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    if bytes.len() as u64 > LIMIT {
        bail!("Launcher file is too large: {}", path.display());
    }
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temp.as_file().set_permissions(fs::Permissions::from_mode(mode))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

/// Only files recorded in our manifest may be replaced or removed. The
/// complete batch is checked before writing, including user edits and links.
fn sync(dir: &Path, desired: &Commands) -> anyhow::Result<()> {
    if !dir.try_exists()? && desired.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    if !fs::symlink_metadata(dir)?.is_dir() {
        bail!("Launcher command directory must not be a symbolic link");
    }
    let lock_path = dir.join(".lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|meta| !meta.is_file()) {
        bail!("Launcher lock must be a regular file");
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    lock.try_lock().context("Another process is updating launcher commands")?;
    let _lock = WriterLock(lock);
    let manifest_path = dir.join(MANIFEST);
    let mut manifest = match read_file(&manifest_path)? {
        Some(content) => serde_json::from_str::<Manifest>(&content)?,
        None => Manifest {
            version: 1,
            files: BTreeMap::new(),
        },
    };
    if manifest.version != 1 || manifest.files.keys().any(|name| !owned_name(name)) {
        bail!("Unrecognized launcher ownership manifest");
    }
    let pending = manifest.files.values().any(|file| file.previous.is_some());
    let mut existing = BTreeMap::new();
    for name in manifest.files.keys().chain(desired.keys()) {
        if existing.contains_key(name) {
            continue;
        }
        let content = read_file(&dir.join(name))?;
        if let Some(content) = &content {
            let ours = manifest.files.get(name).is_some_and(|owned| {
                &owned.content == content || owned.previous.as_ref() == Some(content)
            });
            if !ours {
                bail!(
                    "Preserving an unowned or edited launcher command: {}",
                    dir.join(name).display()
                );
            }
        }
        existing.insert(name.clone(), content);
    }
    let stale: Vec<_> = manifest
        .files
        .keys()
        .filter(|name| !desired.contains_key(*name))
        .cloned()
        .collect();
    for (name, content) in desired {
        let previous = existing[name].as_ref().filter(|old| *old != content).cloned();
        manifest.files.insert(
            name.clone(),
            OwnedFile {
                content: content.clone(),
                previous,
            },
        );
    }
    let changed = pending
        || !stale.is_empty()
        || desired.iter().any(|(name, text)| existing[name].as_ref() != Some(text));
    if !changed {
        return Ok(());
    }
    write_file(&manifest_path, &serde_json::to_vec_pretty(&manifest)?, 0o600)?;
    for (name, content) in desired {
        if existing[name].as_ref() != Some(content) {
            write_file(&dir.join(name), content.as_bytes(), 0o700)?;
        }
    }
    for name in stale {
        match fs::remove_file(dir.join(&name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        manifest.files.remove(&name);
    }
    for owned in manifest.files.values_mut() {
        owned.previous = None;
    }
    write_file(&manifest_path, &serde_json::to_vec_pretty(&manifest)?, 0o600)
}

#[cfg(test)]
mod tests;
