use std::{fs, time::SystemTime};

pub(crate) enum Snapshot {
    #[cfg(not(windows))]
    Standard(fs::Metadata),
    #[cfg(windows)]
    Standard(Box<fs::Metadata>),
    #[cfg(windows)]
    Batch {
        size: u64,
        ticks: u64,
        attributes: u32,
    },
}

impl From<fs::Metadata> for Snapshot {
    /// Retain full metadata when obtained through portable filesystem operations.
    fn from(metadata: fs::Metadata) -> Self {
        #[cfg(windows)]
        let metadata = Box::new(metadata);
        Self::Standard(metadata)
    }
}

impl Snapshot {
    /// Read regular-file byte length without an additional filesystem query.
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Standard(m) => m.len(),
            #[cfg(windows)]
            Self::Batch { size, .. } => *size,
        }
    }
    /// Preserve exact native modification time precision.
    pub(crate) fn modified(&self) -> std::io::Result<SystemTime> {
        match self {
            Self::Standard(m) => m.modified(),
            #[cfg(windows)]
            Self::Batch { ticks, .. } => Self::time(*ticks),
        }
    }
    /// Compare native write times directly without converting every file timestamp.
    pub(crate) fn same_time(&self, other: &Self) -> anyhow::Result<bool> {
        use anyhow::Context;
        #[cfg(windows)]
        if let (Self::Batch { ticks: first, .. }, Self::Batch { ticks: second, .. }) = (self, other)
        {
            return Ok(first == second);
        }
        Ok(self
            .modified()
            .context("Failed to read source modification time")?
            == other
                .modified()
                .context("Failed to read target modification time")?)
    }

    /// Identify directories from cached enumeration attributes.
    pub(crate) fn is_dir(&self) -> bool {
        match self {
            Self::Standard(m) => m.is_dir(),
            #[cfg(windows)]
            Self::Batch { attributes, .. } => attributes & 0x10 != 0,
        }
    }
    /// Exclude directories and device entries from ordinary file selection.
    pub(crate) fn is_file(&self) -> bool {
        match self {
            Self::Standard(m) => m.is_file(),
            #[cfg(windows)]
            Self::Batch { attributes, .. } => attributes & 0x50 == 0,
        }
    }
    /// Reject symlinks and all Windows reparse points without resolving them.
    pub(crate) fn is_link(&self) -> bool {
        match self {
            Self::Standard(m) => crate::io::Paths::is_link(m),
            #[cfg(windows)]
            Self::Batch { attributes, .. } => attributes & 0x400 != 0,
        }
    }
    /// Retain portable permission bits for direct-write preparation.
    #[cfg(unix)]
    pub(crate) fn permissions(&self) -> fs::Permissions {
        match self {
            Self::Standard(m) => m.permissions(),
        }
    }
    /// Retain native write-time ticks until hashing requires a portable timestamp.
    #[cfg(windows)]
    pub(crate) fn batch(size: u64, ticks: u64, attributes: u32) -> Self {
        Self::Batch {
            size,
            ticks,
            attributes,
        }
    }

    /// Convert Windows timestamps only for source stability checks and mixed metadata.
    #[cfg(windows)]
    fn time(ticks: u64) -> std::io::Result<SystemTime> {
        use std::time::{Duration, UNIX_EPOCH};
        let delta = i128::from(ticks) - 116_444_736_000_000_000;
        let magnitude = delta.unsigned_abs();
        let duration = Duration::new(
            (magnitude / 10_000_000) as u64,
            ((magnitude % 10_000_000) * 100) as u32,
        );
        let modified = if delta >= 0 {
            UNIX_EPOCH.checked_add(duration)
        } else {
            UNIX_EPOCH.checked_sub(duration)
        };
        modified.ok_or_else(|| {
            std::io::Error::other("Native modification time exceeds the supported range")
        })
    }
}
