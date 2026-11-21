//! Bounds for the in-process engine. Exceeding one refuses the command, never truncates it.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::path::Path;

pub(crate) const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 10_000;
pub(crate) const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub(crate) const MAX_COMMANDS: usize = 128;
pub(crate) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum ReadError {
    Io(io::Error),
    Refused(String),
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Read regular files only; never open a known FIFO/device or allocate for an unbounded file.
pub(crate) fn read_regular_file(path: &Path) -> Result<Vec<u8>, ReadError> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        // Preserve the system command's ordinary error for a directory.
        if metadata.is_dir() {
            return Err(ReadError::Io(io::Error::from_raw_os_error(21)));
        }
        return Err(ReadError::Refused("non-regular file".to_string()));
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(ReadError::Refused("file exceeds 8 MiB".to_string()));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    // Darwin's O_NONBLOCK avoids blocking at open if a regular path becomes a FIFO.
    // It has no effect on regular-file reads. Other platforms are not a supported target.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0004);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(ReadError::Refused("non-regular file".to_string()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ReadError::Refused("file exceeds 8 MiB".to_string()));
    }
    Ok(bytes)
}

pub(crate) fn check_input(total: &mut usize, added: usize) -> Result<(), String> {
    *total = total.saturating_add(added);
    if *total > MAX_INPUT_BYTES {
        return Err("input exceeds 16 MiB".to_string());
    }
    Ok(())
}

pub(crate) fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    if out.len().saturating_add(bytes.len()) > MAX_OUTPUT_BYTES {
        return Err("output exceeds 2 MiB".to_string());
    }
    out.extend_from_slice(bytes);
    Ok(())
}

pub(crate) fn append_error(out: &mut String, text: &str) -> Result<(), String> {
    if out.len().saturating_add(text.len()) > MAX_OUTPUT_BYTES {
        return Err("error output exceeds 2 MiB".to_string());
    }
    out.push_str(text);
    Ok(())
}
