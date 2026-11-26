use std::ops::Range;

#[derive(Clone, Debug, Default)]
pub(super) struct SourceLines {
    starts: Vec<usize>,
}

impl SourceLines {
    pub(super) fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut starts = Vec::with_capacity(1 + bytes.len() / 24);
        starts.push(0);
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                    index += 2;
                    starts.push(index);
                }
                b'\r' | b'\n' => {
                    index += 1;
                    starts.push(index);
                }
                _ => index += 1,
            }
        }
        Self { starts }
    }

    pub(super) fn line_of(&self, offset: usize) -> usize {
        if self.starts.is_empty() {
            return 0;
        }
        self.starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    }

    pub(super) fn blank_lines_between(
        &self,
        source: &str,
        previous: Range<usize>,
        next_start: usize,
    ) -> usize {
        let bytes = source.as_bytes();
        let end = previous.end.min(bytes.len());
        let start = previous.start.min(end);
        let from_line = match bytes[start..end].iter().rposition(|byte| !is_blank(*byte)) {
            Some(index) => self.line_of(start + index),
            None => self.line_of(start),
        };
        self.line_of(next_start).saturating_sub(from_line + 1)
    }
}

fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::SourceLines;

    #[test]
    fn line_numbers_are_zero_based_and_count_line_end_bytes() {
        let source = "a\n\nb";
        let lines = SourceLines::new(source);
        assert_eq!(lines.line_of(0), 0);
        assert_eq!(lines.line_of(1), 0);
        assert_eq!(lines.line_of(2), 1);
        assert_eq!(lines.line_of(3), 2);

        let source = "a\r\n\r\nb";
        let lines = SourceLines::new(source);
        assert_eq!(lines.line_of(1), 0);
        assert_eq!(lines.line_of(2), 0);
        assert_eq!(lines.line_of(4), 1);
        assert_eq!(lines.line_of(5), 2);

        let empty = SourceLines::new("");
        assert_eq!(empty.line_of(0), 0);
    }

    #[test]
    fn blank_lines_follow_the_previous_end_and_next_start_lines() {
        let source = "a\n\nb";
        let lines = SourceLines::new(source);
        assert_eq!(lines.blank_lines_between(source, 0..2, 3), 1);

        let source = "a\n\n\n\nb";
        let lines = SourceLines::new(source);
        assert_eq!(lines.blank_lines_between(source, 0..2, 5), 3);

        let source = "a\r\n\r\nb";
        let lines = SourceLines::new(source);
        assert_eq!(lines.blank_lines_between(source, 0..3, 5), 1);

        let source = "a\n\nb";
        let lines = SourceLines::new(source);
        assert_eq!(lines.blank_lines_between(source, 0..3, 3), 1);
        assert_eq!(lines.blank_lines_between(source, Range::default(), 0), 0);
    }
}
