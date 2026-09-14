//! Small crash-safe JSON writer shared by receipts and operation records.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use atomicwrites::{AtomicFile, OverwriteBehavior};
use serde::Serialize;

pub fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("durable JSON path has no parent")?;
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec(value)?;
    AtomicFile::new(path, OverwriteBehavior::AllowOverwrite)
        .write(|file| -> io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()
        })
        .map_err(|error| match error {
            atomicwrites::Error::Internal(error) | atomicwrites::Error::User(error) => error,
        })?;
    // Directory fsync makes the rename durable on Unix. Windows replacement
    // is handled by atomicwrites through MoveFileExW.
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
