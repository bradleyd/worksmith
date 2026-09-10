//! Session-local bounded results. Handles never resolve arbitrary filesystem paths.
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    time::{Duration, SystemTime},
};

pub(super) const DISK_BYTES: u64 = 8 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub(super) struct Store {
    directory: PathBuf,
}
impl Store {
    pub fn new(session: &std::path::Path) -> Self {
        Self {
            directory: crate::session::store::artifact_path(session, "mcp-results", "mcp-results"),
        }
    }

    fn prepare(&self) -> Result<u64> {
        if self.directory.exists() {
            if !fs::symlink_metadata(&self.directory)?.file_type().is_dir() {
                bail!("invalid result directory");
            }
        } else {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&self.directory)?;
        }
        let mut bytes = 0u64;
        for file in fs::read_dir(&self.directory)? {
            let file = file?;
            let metadata = file.path().symlink_metadata()?;
            if !metadata.file_type().is_file() {
                bail!("unexpected entry in result directory");
            }
            if SystemTime::now()
                .duration_since(metadata.modified()?)
                .unwrap_or_default()
                > RETENTION
            {
                fs::remove_file(file.path())?;
            } else {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
        Ok(bytes)
    }

    pub fn put(&self, content: &str) -> Result<String> {
        let bytes = self.prepare()?;
        if bytes.saturating_add(content.len() as u64) > DISK_BYTES {
            bail!("session result disk cap (8 MiB) reached");
        }
        let handle = uuid::Uuid::new_v4().to_string();
        let path = self.directory.join(&handle);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        if let Err(error) = file.write_all(content.as_bytes()) {
            let _ = fs::remove_file(path);
            return Err(error.into());
        }
        Ok(handle)
    }

    pub fn page(&self, handle: &str, offset: u64) -> Result<String> {
        let id = uuid::Uuid::parse_str(handle).context("invalid result handle")?;
        if id.to_string() != handle {
            bail!("invalid result handle");
        }
        if !self.directory.exists() {
            bail!("result expired or belongs to another session");
        }
        self.prepare()?;
        let path = self.directory.join(handle);
        if !path
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_file())
        {
            bail!("result expired or belongs to another session");
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(path)
            .context("result expired or belongs to another session")?;
        let total = file.metadata()?.len();
        if offset > total || total > DISK_BYTES {
            bail!("offset or stored result exceeds bounds");
        }
        // Include the requested character even when the offset lands inside it.
        let mut start = offset;
        if offset < total {
            let lookbehind = offset.saturating_sub(3);
            file.seek(SeekFrom::Start(lookbehind))?;
            let mut prefix = [0; 4];
            let length = (offset - lookbehind + 1) as usize;
            file.read_exact(&mut prefix[..length])?;
            let mut index = length - 1;
            while index > 0 && prefix[index] & 0xc0 == 0x80 {
                index -= 1;
            }
            start = lookbehind + index as u64;
        }
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![];
        file.take(6000).read_to_end(&mut bytes)?;
        let length = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => bail!("stored result contains invalid UTF-8"),
        };
        let text = std::str::from_utf8(&bytes[..length])?;
        let end = start + length as u64;
        let eof = end == total;
        let next = if eof { "null".into() } else { end.to_string() };
        Ok(format!(
            "{text}\n[bytes {start}..{end} of {total}; next_offset={next}; eof={eof}; handle {handle}]"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_bounded_session_local_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("first.jsonl"));
        let handle = store.put("fixture evidence").unwrap();
        assert!(store.page(&handle, 0).unwrap().contains("fixture evidence"));
        let other = Store::new(&dir.path().join("second.jsonl"));
        assert!(other.page(&handle, 0).is_err());
        assert!(store.page("../../first.jsonl", 0).is_err());
        assert!(store.put(&"x".repeat(DISK_BYTES as usize)).is_err());
        let file = std::fs::File::open(store.directory.join(&handle)).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        assert!(store.page(&handle, 0).is_err());
        assert!(!store.directory.join(&handle).exists());
    }

    #[test]
    fn arbitrary_offsets_include_the_requested_character() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("session.jsonl"));
        let content = "header: é中🦀 END";
        let handle = store.put(content).unwrap();
        for offset in 0..=content.len() {
            let mut start = offset;
            while !content.is_char_boundary(start) {
                start -= 1;
            }
            let page = store.page(&handle, offset as u64).unwrap();
            assert_eq!(
                page,
                format!(
                    "{}\n[bytes {start}..{} of {}; next_offset=null; eof=true; handle {handle}]",
                    &content[start..],
                    content.len(),
                    content.len()
                )
            );
        }
        assert!(store.page(&handle, content.len() as u64 + 1).is_err());
    }

    #[test]
    fn next_offsets_reconstruct_multibyte_content_without_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("session.jsonl"));
        let content = format!("header: {}END", "é中🦀".repeat(2000));
        let handle = store.put(&content).unwrap();
        let mut offset = 0;
        let mut reconstructed = String::new();
        loop {
            let page = store.page(&handle, offset).unwrap();
            let (text, metadata) = page.rsplit_once("\n[bytes ").unwrap();
            assert!(text.len() <= 6000);
            reconstructed.push_str(text);
            let next = metadata
                .split("next_offset=")
                .nth(1)
                .unwrap()
                .split(';')
                .next()
                .unwrap();
            if next == "null" {
                assert!(metadata.contains("eof=true"));
                break;
            }
            let next = next.parse::<u64>().unwrap();
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(reconstructed, content);
    }

    #[cfg(unix)]
    #[test]
    fn result_files_are_private_and_symlinks_are_not_followed() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&dir.path().join("session.jsonl"));
        let handle = store.put("private result").unwrap();
        assert_eq!(
            store.directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = store.directory.join(&handle);
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(&path).unwrap();
        let external = dir.path().join("external");
        std::fs::write(&external, "must not read").unwrap();
        symlink(external, path).unwrap();
        assert!(store.page(&handle, 0).is_err());
    }
}
