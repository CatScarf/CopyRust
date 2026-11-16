use crate::Progress;

impl Progress {
    /// Format one cumulative batch line without estimating work that is not yet known.
    pub fn format_line(&self) -> String {
        let skipped = self.skipped_mtime + self.skipped_hash + self.skipped_same_file;
        let done = self.copied_files + skipped;
        let count = match self.total_files {
            Some(total) => format!(
                "{done}/{total} ({:.1}%)",
                if total == 0 {
                    100.0
                } else {
                    done as f64 * 100.0 / total as f64
                }
            ),
            None => format!("scan {}", self.scanned_files),
        };
        let rate = self.copied_bytes as f64 / self.elapsed_seconds.max(0.001);
        let eta = if self.complete {
            "0.00s".into()
        } else if self.phase == "copying" && rate > 0.0 {
            self.total_copy_bytes
                .map(|total| Self::duration(total.saturating_sub(self.copied_bytes) as f64 / rate))
                .unwrap_or_else(|| "--".into())
        } else {
            "--".into()
        };
        format!(
            "copyrust {}: {count}, copy {} ({}), skip {skipped} ({}), {}/s, elapse {}, eta {eta}, delete {}, failed {}",
            self.phase,
            self.copied_files,
            Self::size(self.copied_bytes as f64),
            Self::size(self.skipped_bytes as f64),
            Self::size(rate),
            Self::duration(self.elapsed_seconds),
            self.deleted_files,
            self.failed_files,
        )
    }

    /// Render binary byte units with a fixed two-decimal precision.
    fn size(mut bytes: f64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB", "PB"];
        let mut index = 0;
        while bytes >= 1024.0 && index + 1 < units.len() {
            bytes /= 1024.0;
            index += 1;
        }
        format!("{bytes:.2}{}", units[index])
    }

    /// Render elapsed time and ETA using seconds, minutes, or hours.
    fn duration(seconds: f64) -> String {
        if seconds >= 3600.0 {
            format!("{:.2}h", seconds / 3600.0)
        } else if seconds >= 60.0 {
            format!("{:.2}m", seconds / 60.0)
        } else {
            format!("{seconds:.2}s")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keep skipped sizes separate from transferred bytes and hide unknown ETA values.
    #[test]
    fn batch_progress_distinguishes_copy_and_skip() {
        let mut progress = Progress {
            phase: "copying".into(),
            elapsed_seconds: 10.0,
            total_files: Some(110),
            copied_files: 50,
            copied_bytes: 1024 * 1024 * 1024,
            skipped_mtime: 10,
            skipped_bytes: 1024 * 1024,
            total_copy_bytes: Some(2 * 1024 * 1024 * 1024),
            ..Progress::default()
        };
        let line = progress.format_line();
        assert!(line.contains("60/110 (54.5%)"));
        assert!(line.contains("copy 50 (1.00GB), skip 10 (1.00MB)"));
        assert!(line.contains("102.40MB/s, elapse 10.00s, eta 10.00s"));
        progress.phase = "comparing".into();
        assert!(progress.format_line().contains("eta --"));
        progress.complete = true;
        assert!(progress.format_line().contains("eta 0.00s"));
    }
}
