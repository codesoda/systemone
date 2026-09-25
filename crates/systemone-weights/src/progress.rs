//! Download progress reporting.

use std::{
    io::Write,
    time::{Duration, Instant},
};

/// Receives download progress. Implementations must be cheap: `advance` is
/// called once per chunk.
pub trait Progress {
    /// A download of `total` bytes is starting (some may already be present).
    fn begin(&mut self, label: &str, total: u64);
    /// The current file changed.
    fn file(&mut self, path: &str);
    /// `bytes` more of the total are done (downloaded or verified).
    fn advance(&mut self, bytes: u64);
    fn finish(&mut self);
}

/// Reports nothing.
pub struct NoProgress;

impl Progress for NoProgress {
    fn begin(&mut self, _label: &str, _total: u64) {}
    fn file(&mut self, _path: &str) {}
    fn advance(&mut self, _bytes: u64) {}
    fn finish(&mut self) {}
}

/// One redrawn status line on a terminal:
/// `  42% [########------] 1.2 GB / 2.9 GB  24.1 MB/s  1m 10s left  encoder.onnx`
pub struct TerminalProgress<W: Write> {
    output: W,
    total: u64,
    done: u64,
    current: String,
    started: Instant,
    last_draw: Option<Instant>,
}

const REDRAW_EVERY: Duration = Duration::from_millis(100);
const BAR_WIDTH: usize = 24;

impl<W: Write> TerminalProgress<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            total: 0,
            done: 0,
            current: String::new(),
            started: Instant::now(),
            last_draw: None,
        }
    }

    fn draw(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && self
                .last_draw
                .is_some_and(|last| now.duration_since(last) < REDRAW_EVERY)
        {
            return;
        }
        self.last_draw = Some(now);
        let line = render(
            self.done,
            self.total,
            now.duration_since(self.started),
            &self.current,
        );
        // \r + clear line; a progress line is best effort and never fails
        // the download.
        let _ = write!(self.output, "\r\x1b[2K{line}");
        let _ = self.output.flush();
    }
}

impl<W: Write> Progress for TerminalProgress<W> {
    fn begin(&mut self, label: &str, total: u64) {
        self.total = total;
        self.done = 0;
        self.started = Instant::now();
        let _ = writeln!(self.output, "Downloading {label} ({})", format_bytes(total));
        self.draw(true);
    }

    fn file(&mut self, path: &str) {
        path.clone_into(&mut self.current);
        self.draw(true);
    }

    fn advance(&mut self, bytes: u64) {
        self.done = self
            .done
            .saturating_add(bytes)
            .min(self.total.max(self.done));
        self.draw(false);
    }

    fn finish(&mut self) {
        self.current.clear();
        self.draw(true);
        let _ = writeln!(self.output);
    }
}

fn render(done: u64, total: u64, elapsed: Duration, current: &str) -> String {
    #[allow(clippy::cast_precision_loss)]
    let fraction = if total == 0 {
        1.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = (fraction * BAR_WIDTH as f64).round() as usize;
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(BAR_WIDTH - filled));
    let seconds = elapsed.as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let rate = if seconds > 0.5 {
        done as f64 / seconds
    } else {
        0.0
    };
    let speed = if rate > 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let per_second = rate as u64;
        format!("  {}/s", format_bytes(per_second))
    } else {
        String::new()
    };
    let left = if rate > 0.0 && done < total {
        #[allow(clippy::cast_precision_loss)]
        let remaining = (total - done) as f64 / rate;
        format!("  {} left", format_duration(remaining))
    } else {
        String::new()
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let percent = (fraction * 100.0).floor() as u64;
    format!(
        "{percent:>3}% [{bar}] {} / {}{speed}{left}  {current}",
        format_bytes(done),
        format_bytes(total)
    )
}

fn format_duration(seconds: f64) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let seconds = seconds.round() as u64;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// Human-readable size with decimal units, e.g. `1.2 GB`.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_decimal_units() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "1.5 kB");
        assert_eq!(format_bytes(2_900_000_000), "2.9 GB");
    }

    #[test]
    fn line_shows_percent_sizes_rate_and_file() {
        let line = render(500, 1_000, Duration::from_secs(1), "a.bin");
        assert!(line.starts_with(" 50% ["), "{line}");
        assert!(line.contains("500 B / 1.0 kB"), "{line}");
        assert!(line.contains("500 B/s"), "{line}");
        assert!(line.contains("1s left"), "{line}");
        assert!(line.ends_with("a.bin"), "{line}");
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(format_duration(59.0), "59s");
        assert_eq!(format_duration(125.0), "2m 05s");
        assert_eq!(format_duration(7_300.0), "2h 01m");
    }

    #[test]
    fn terminal_progress_ends_with_a_newline() {
        let mut buffer = Vec::new();
        {
            let mut progress = TerminalProgress::new(&mut buffer);
            progress.begin("x", 10);
            progress.file("f");
            progress.advance(10);
            progress.finish();
        }
        let text = String::from_utf8(buffer).unwrap();
        assert!(text.starts_with("Downloading x (10 B)\n"), "{text:?}");
        assert!(text.contains("100%"), "{text:?}");
        assert!(text.ends_with('\n'));
    }
}
