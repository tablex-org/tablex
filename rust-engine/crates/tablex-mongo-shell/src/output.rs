//! The lines a script prints, with a cap so a runaway loop cannot flood the host.

/// Most lines kept.
pub const MAX_LINES: usize = 10_000;
/// Most bytes kept (4 MiB).
pub const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct Output {
    lines: Vec<String>,
    line_count: usize,
    bytes: usize,
    truncated: bool,
}

impl Output {
    /// Adds one printed message (it may hold newlines). Past a cap the rest is
    /// dropped and a single note says so.
    pub fn push(&mut self, text: &str) {
        self.push_with_limits(text, MAX_LINES, MAX_BYTES);
    }

    pub(crate) fn push_with_limits(&mut self, text: &str, max_lines: usize, max_bytes: usize) {
        if self.truncated {
            return;
        }
        let lines = text.matches('\n').count() + 1;
        if self.line_count + lines > max_lines || self.bytes + text.len() > max_bytes {
            self.truncated = true;
            self.lines.push(format!(
                "... output truncated (limit {max_lines} lines / {} MiB)",
                max_bytes >> 20
            ));
            return;
        }
        self.line_count += lines;
        self.bytes += text.len();
        self.lines.push(text.to_owned());
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn into_lines(self) -> Vec<String> {
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_cap_keeps_the_head_and_notes_the_cut_once() {
        let mut output = Output::default();
        for n in 0..(MAX_LINES + 50) {
            output.push(&n.to_string());
        }
        assert!(output.is_truncated());
        let lines = output.into_lines();
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines[MAX_LINES - 1], (MAX_LINES - 1).to_string());
        assert!(lines[MAX_LINES].contains("truncated"));
    }

    #[test]
    fn the_byte_cap_counts_multi_line_messages() {
        let mut output = Output::default();
        let big = "x".repeat(1024 * 1024);
        for _ in 0..6 {
            output.push(&big);
        }
        assert!(output.is_truncated());
        assert_eq!(output.into_lines().len(), 5);
        let mut lines = Output::default();
        lines.push(&"a\n".repeat(MAX_LINES));
        assert!(lines.is_truncated(), "10001 lines in one message is over the cap");
    }
}
