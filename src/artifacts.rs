use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub id: String,
    pub path: String,
    pub bytes: u64,
    pub incomplete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactChunk {
    pub id: String,
    pub text: String,
    pub next_offset: u64,
    pub eof: bool,
    pub path: String,
    pub incomplete: bool,
}

/// Files are append-only. Unregistered files survive failed SQLite transactions
/// and are recovered explicitly, rather than deleted or excluded from quotas.
pub(crate) struct ArtifactFiles {
    directory: PathBuf,
    used: u64,
    capped: HashSet<String>,
}

impl ArtifactFiles {
    pub fn open(session: &Path) -> Result<Self> {
        let mut name = session.as_os_str().to_os_string();
        name.push(".artifacts");
        let directory = PathBuf::from(name);
        if !directory.exists() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&directory)?;
            File::open(
                directory
                    .parent()
                    .context("artifact directory has no parent")?,
            )?
            .sync_all()?;
        }
        ensure!(
            fs::symlink_metadata(&directory)?.is_dir(),
            "artifact directory must not be a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        let mut files = Self {
            directory,
            used: 0,
            capped: HashSet::new(),
        };
        for entry in fs::read_dir(&files.directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            ensure!(
                metadata.is_file(),
                "unexpected non-file in artifact directory"
            );
            files.used = files
                .used
                .checked_add(metadata.len())
                .context("artifact size overflow")?;
        }
        Ok(files)
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        let parsed = Uuid::parse_str(id).context("invalid artifact ID")?;
        ensure!(parsed.to_string() == id, "invalid artifact ID");
        Ok(self.directory.join(format!("{id}.txt")))
    }

    fn file(&self, id: &str, write: bool) -> Result<File> {
        let path = self.path(id)?;
        ensure!(
            fs::symlink_metadata(&path)?.is_file(),
            "artifact must be a regular file"
        );
        let mut options = OpenOptions::new();
        options.read(true).append(write);
        // O_NOFOLLOW also closes the symlink race after the metadata check.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(0x20000);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            ensure!(
                file.metadata()?.nlink() == 1,
                "hard-linked artifacts are not supported"
            );
            if write {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(file)
    }

    pub fn create(&self, id: &str) -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(self.path(id)?)?.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    pub fn admit(&self, session_limit: u64) -> Result<()> {
        ensure!(
            self.used < session_limit,
            "artifact session quota exhausted; existing output is retained"
        );
        Ok(())
    }

    pub fn append(
        &mut self,
        id: &str,
        text: &str,
        output_limit: u64,
        session_limit: u64,
    ) -> Result<()> {
        // Once output is omitted, never append a later fragment as if it were
        // contiguous. UTF-8 rounding can leave a few quota bytes unused.
        if self.capped.contains(id) {
            return Ok(());
        }
        let mut file = self.file(id, true)?;
        let remaining = output_limit
            .saturating_sub(file.metadata()?.len())
            .min(session_limit.saturating_sub(self.used));
        let mut bytes = (text.len() as u64).min(remaining) as usize;
        while !text.is_char_boundary(bytes) {
            bytes -= 1;
        }
        // Account actual bytes even when a disk write fails partway through.
        let before = file.metadata()?.len();
        let result = file.write_all(&text.as_bytes()[..bytes]);
        self.used += file.metadata()?.len().saturating_sub(before);
        result?;
        if bytes < text.len() {
            self.capped.insert(id.to_owned());
        }
        Ok(())
    }

    pub fn record(&self, id: &str, incomplete: bool) -> Result<ArtifactRecord> {
        let file = self.file(id, false)?;
        Ok(ArtifactRecord {
            id: id.to_owned(),
            path: self.path(id)?.display().to_string(),
            bytes: file.metadata()?.len(),
            incomplete: incomplete || self.capped.contains(id),
        })
    }

    pub fn sync(&self, id: &str) -> Result<()> {
        self.file(id, false)?.sync_all()?;
        Ok(())
    }

    pub fn ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".txt"))
                && self.path(id).is_ok()
            {
                ids.push(id.to_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn read(&self, record: ArtifactRecord, offset: u64, limit: usize) -> Result<ArtifactChunk> {
        ensure!(
            offset <= record.bytes,
            "artifact offset exceeds output size"
        );
        let mut file = self.file(&record.id, false)?;
        file.seek(SeekFrom::Start(offset))?;
        let limit = limit.clamp(1, MAX_READ_BYTES);
        let mut bytes = vec![0; (record.bytes - offset).min(limit as u64) as usize];
        file.read_exact(&mut bytes)?;
        // Byte offsets must be UTF-8 boundaries. Pages never split a codepoint.
        let valid = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(error)
                if error.error_len().is_none()
                    && record.incomplete
                    && offset + bytes.len() as u64 == record.bytes =>
            {
                // A failed write/crash can leave a partial final codepoint. Show
                // its replacement explicitly while advancing by source bytes.
                let mut text = String::from_utf8_lossy(&bytes).into_owned();
                let mut consumed = bytes.len();
                if text.len() > limit {
                    consumed = error.valid_up_to();
                    ensure!(
                        consumed != 0,
                        "artifact page limit is too small for the incomplete UTF-8 marker"
                    );
                    text.truncate(consumed);
                }
                let next_offset = offset + consumed as u64;
                return Ok(ArtifactChunk {
                    id: record.id,
                    text,
                    next_offset,
                    eof: next_offset == record.bytes,
                    path: record.path,
                    incomplete: true,
                });
            }
            Err(error)
                if error.error_len().is_none() && offset + bytes.len() as u64 == record.bytes =>
            {
                anyhow::bail!("completed artifact has an incomplete UTF-8 character")
            }
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => anyhow::bail!("artifact offset is not a UTF-8 boundary or output is corrupt"),
        };
        ensure!(
            valid != 0 || bytes.is_empty(),
            "artifact page limit is too small for the next UTF-8 character"
        );
        bytes.truncate(valid);
        let next_offset = offset + valid as u64;
        Ok(ArtifactChunk {
            id: record.id,
            text: String::from_utf8(bytes)?,
            next_offset,
            eof: next_offset == record.bytes,
            path: record.path,
            incomplete: record.incomplete,
        })
    }

    pub fn read_before(
        &self,
        record: ArtifactRecord,
        before: u64,
        limit: usize,
    ) -> Result<(u64, ArtifactChunk)> {
        ensure!(
            before <= record.bytes,
            "artifact before offset exceeds output size"
        );
        // Four source bytes guarantee progress even for a final four-byte character.
        let limit = limit.clamp(4, MAX_READ_BYTES);
        let start = before.saturating_sub(limit as u64);
        let length = (before - start) as usize;
        let mut file = self.file(&record.id, false)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; length + usize::from(before < record.bytes)];
        file.read_exact(&mut bytes)?;
        if before < record.bytes {
            ensure!(
                bytes[length] & 0xc0 != 0x80,
                "artifact before offset is not a UTF-8 boundary"
            );
        }
        bytes.truncate(length);
        let skip = bytes
            .iter()
            .take(3)
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count();
        ensure!(
            bytes.get(skip).is_none_or(|byte| *byte & 0xc0 != 0x80),
            "artifact output is corrupt"
        );
        let offset = start + skip as u64;
        ensure!(
            offset < before || before == 0,
            "artifact backward page made no progress"
        );
        let text = match std::str::from_utf8(&bytes[skip..]) {
            Ok(text) => text.to_owned(),
            Err(error)
                if error.error_len().is_none() && record.incomplete && before == record.bytes =>
            {
                String::from_utf8_lossy(&bytes[skip..]).into_owned()
            }
            Err(_) => {
                anyhow::bail!("artifact before offset is not a UTF-8 boundary or output is corrupt")
            }
        };
        Ok((
            offset,
            ArtifactChunk {
                id: record.id,
                text,
                next_offset: before,
                eof: before == record.bytes,
                path: record.path,
                incomplete: record.incomplete,
            },
        ))
    }
}
