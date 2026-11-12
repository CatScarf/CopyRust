use super::{Entry, Snapshot};
use anyhow::{Context, Result, bail, ensure};
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use std::{ffi::OsString, path::Path};
use windows_sys::{
    Wdk::Storage::FileSystem as Nt,
    Win32::{
        Foundation,
        Storage::FileSystem as Win,
        System::{IO, Threading},
    },
};

pub(crate) struct Entries {
    handle: OwnedHandle,
    buffer: Vec<u64>,
    used: usize,
    offset: usize,
    restart: bool,
    finished: bool,
}

impl Entries {
    /// Open a synchronous directory handle with shared access and an aligned batch buffer.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        ensure!(
            !wide.contains(&0),
            "Directory path contains a NUL character"
        );
        wide.push(0);
        // The NUL-terminated path and null optional parameters remain valid through the call.
        let raw = unsafe {
            Win::CreateFileW(
                wide.as_ptr(),
                Win::FILE_LIST_DIRECTORY,
                Win::FILE_SHARE_READ | Win::FILE_SHARE_WRITE | Win::FILE_SHARE_DELETE,
                std::ptr::null(),
                Win::OPEN_EXISTING,
                Win::FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if raw == Foundation::INVALID_HANDLE_VALUE || raw.is_null() {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("Failed to open directory {}", path.display()));
        }
        // The successful CreateFileW call transfers one owned handle into this guard.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        Ok(Self {
            handle,
            buffer: vec![0; 8192],
            used: 0,
            offset: 0,
            restart: true,
            finished: false,
        })
    }

    /// Read multiple complete directory records per kernel transition.
    fn refill(&mut self) -> Result<bool> {
        loop {
            let (status, used) = self
                .query()
                .context("Failed to query native directory batch")?;
            if status == Foundation::STATUS_NO_MORE_FILES {
                return Ok(false);
            }
            if status == Foundation::STATUS_BUFFER_OVERFLOW {
                self.grow()
                    .context("Failed to enlarge native directory buffer")?;
                self.restart = true;
                continue;
            }
            if status < 0 {
                bail!("Native directory query failed with NTSTATUS {status:#x}");
            }
            ensure!(
                used <= self.buffer.len() * 8,
                "Native directory buffer length is invalid"
            );
            if used == 0 {
                self.grow()
                    .context("Failed to enlarge native directory buffer")?;
                continue;
            }
            self.used = used;
            self.offset = 0;
            return Ok(true);
        }
    }

    /// Keep query buffers alive through synchronous or explicitly waited completion.
    fn query(&mut self) -> Result<(i32, usize)> {
        let mut io = IO::IO_STATUS_BLOCK::default();
        // The aligned buffer and status block remain alive until synchronous completion.
        let mut status = unsafe {
            Nt::NtQueryDirectoryFile(
                self.handle.as_raw_handle(),
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &mut io,
                self.buffer.as_mut_ptr().cast(),
                (self.buffer.len() * 8) as u32,
                Nt::FileDirectoryInformation,
                false,
                std::ptr::null(),
                self.restart,
            )
        };
        self.restart = false;
        if status == Foundation::STATUS_PENDING {
            // This private handle has at most one request; waiting completes all buffer writes.
            let waited = unsafe {
                Threading::WaitForSingleObject(self.handle.as_raw_handle(), Threading::INFINITE)
            };
            ensure!(
                waited == Foundation::WAIT_OBJECT_0,
                "Failed to wait for native directory completion"
            );
            // Completion initializes the Status member of IO_STATUS_BLOCK's union.
            status = unsafe { io.Anonymous.Status };
        }
        Ok((status, io.Information))
    }

    /// Grow unusually small result buffers without silently dropping a directory entry.
    fn grow(&mut self) -> Result<()> {
        ensure!(
            self.buffer.len() < 2 * 1024 * 1024,
            "Native directory record exceeds 16 MiB"
        );
        self.buffer.resize(self.buffer.len() * 2, 0);
        Ok(())
    }

    /// Decode validated record boundaries without assuming Rust metadata layouts.
    fn record(&mut self) -> Result<Option<Entry>> {
        const HEADER: usize = std::mem::offset_of!(Nt::FILE_DIRECTORY_INFORMATION, FileName);
        // u64 storage is initialized and aligned; used was checked against its byte capacity.
        let bytes =
            unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), self.used) };
        let header = bytes
            .get(self.offset..self.offset + HEADER)
            .context("Truncated native directory header")?;
        let next = u32::from_le_bytes(Self::field(header, 0)?) as usize;
        let length = u32::from_le_bytes(Self::field(header, 60)?) as usize;
        ensure!(
            length > 0 && length % 2 == 0,
            "Invalid native filename length"
        );
        let end = self
            .offset
            .checked_add(HEADER)
            .and_then(|n| n.checked_add(length))
            .context("Native filename length overflow")?;
        let name = bytes
            .get(self.offset + HEADER..end)
            .context("Truncated native filename")?;
        // Validated record offsets are 8-byte aligned and filename byte lengths are even.
        let wide = unsafe { std::slice::from_raw_parts(name.as_ptr().cast::<u16>(), length / 2) };
        ensure!(
            !wide.contains(&0),
            "Native filename contains a NUL character"
        );
        ensure!(
            !wide.contains(&47) && !wide.contains(&92),
            "Native filename contains a path separator"
        );
        let name = OsString::from_wide(wide);
        let modified = u64::from_le_bytes(Self::field(header, 24)?);
        let size = u64::from_le_bytes(Self::field(header, 40)?);
        let attributes = u32::from_le_bytes(Self::field(header, 56)?);
        self.advance(next, length)
            .context("Failed to advance native directory record")?;
        if name == "." || name == ".." {
            return Ok(None);
        }
        Ok(Some(Entry {
            name,
            metadata: Snapshot::batch(size, modified, attributes),
        }))
    }

    /// Advance only to aligned records fully contained in the current result batch.
    fn advance(&mut self, next: usize, length: usize) -> Result<()> {
        const HEADER: usize = std::mem::offset_of!(Nt::FILE_DIRECTORY_INFORMATION, FileName);
        if next == 0 {
            self.offset = self.used;
        } else {
            ensure!(
                next >= HEADER + length && next % 8 == 0,
                "Invalid native next-entry offset"
            );
            self.offset = self
                .offset
                .checked_add(next)
                .context("Native entry offset overflow")?;
            ensure!(
                self.offset < self.used,
                "Native next entry exceeds result buffer"
            );
        }
        Ok(())
    }

    /// Read one fixed-width field only after its containing header has been validated.
    fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
        bytes
            .get(offset..offset + N)
            .context("Truncated native metadata field")?
            .try_into()
            .context("Failed to decode native metadata field")
    }
}

impl Iterator for Entries {
    type Item = Result<Entry>;
    /// Stream records and retain the first error instead of silently skipping it.
    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished {
            if self.offset == self.used {
                match self.refill() {
                    Ok(true) => {}
                    Ok(false) => {
                        self.finished = true;
                        return None;
                    }
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error.context("Failed to refill native directory batch")));
                    }
                }
            }
            match self.record() {
                Ok(Some(entry)) => return Some(Ok(entry)),
                Ok(None) => {}
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error.context("Failed to decode native directory entry")));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Malformed byte lengths and record offsets fail before any unaligned read.
    #[test]
    fn malformed_native_records_are_rejected() -> Result<()> {
        let root = tempfile::tempdir().context("Failed to create native parser fixture")?;
        let mut entries = Entries::open(root.path())?;
        entries.used = 67;
        entries.buffer[7] = 3u64 << 32;
        entries.buffer[8] = u64::from(b'x');
        assert!(entries.record().is_err());
        entries.buffer[7] = 2u64 << 32;
        entries.buffer[0] = 65;
        assert!(entries.record().is_err());
        entries.buffer[0] = 0;
        entries.used = 66;
        let valid = entries
            .record()?
            .context("Valid native record was skipped")?;
        assert_eq!(valid.name, "x");
        Ok(())
    }
}
