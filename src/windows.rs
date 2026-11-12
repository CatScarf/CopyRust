use crate::state::State;
use anyhow::{Context, Result, bail};
use std::{
    ffi::c_void,
    fs,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use windows_sys::Win32::{Foundation::HANDLE, Storage::FileSystem};

struct Transfer<'a> {
    state: &'a State,
    transferred: AtomicU64,
}

pub(crate) struct NativeCopy;

impl NativeCopy {
    /// Use the Windows copy engine with synchronous progress callbacks.
    pub(crate) fn run(source: &Path, destination: &Path, state: &State) -> Result<()> {
        let source = Self::wide(source)?;
        let target = Self::wide(destination)?;
        let transfer = Transfer {
            state,
            transferred: AtomicU64::new(0),
        };
        // CopyFileExW completes every callback before returning, keeping transfer alive.
        let copied = unsafe {
            FileSystem::CopyFileExW(
                source.as_ptr(),
                target.as_ptr(),
                Some(Self::progress),
                std::ptr::from_ref(&transfer).cast(),
                std::ptr::null_mut(),
                0,
            )
        };
        let error = (copied == 0).then(std::io::Error::last_os_error);
        Self::make_writable(destination)
            .context("Failed to prepare temporary file for cleanup or replacement")?;
        if let Some(error) = error {
            return Err(error).context("Windows native file copy failed");
        }
        Ok(())
    }

    /// Encode paths without accepting embedded NUL terminators.
    fn wide(path: &Path) -> Result<Vec<u16>> {
        let mut value: Vec<_> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            bail!("Path contains a NUL character: {}", path.display());
        }
        value.push(0);
        Ok(value)
    }

    /// Keep temporary outputs removable even when the source is read-only.
    fn make_writable(path: &Path) -> Result<()> {
        let metadata = match fs::metadata(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("Failed to inspect native copy output"),
        };
        if metadata.permissions().readonly() {
            let mut permissions = metadata.permissions();
            #[allow(
                clippy::permissions_set_readonly_false,
                reason = "Windows-only read-only attribute cleanup."
            )]
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)
                .context("Failed to clear temporary read-only attribute")?;
        }
        Ok(())
    }

    /// Count native transfer bytes and stop cooperatively without panicking across FFI.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn progress(
        _total: i64,
        copied: i64,
        _stream_size: i64,
        _stream_copied: i64,
        _stream_number: u32,
        _reason: u32,
        _source: HANDLE,
        _destination: HANDLE,
        data: *const c_void,
    ) -> u32 {
        // The callback receives the synchronous run call's borrowed transfer record.
        let transfer = unsafe { &*data.cast::<Transfer<'_>>() };
        let current = u64::try_from(copied).unwrap_or(0);
        let previous = transfer.transferred.swap(current, Ordering::Relaxed);
        transfer
            .state
            .copied
            .fetch_add(current.saturating_sub(previous), Ordering::Relaxed);
        if transfer.state.cancellation.is_cancelled() {
            FileSystem::PROGRESS_CANCEL
        } else {
            FileSystem::PROGRESS_CONTINUE
        }
    }
}
