use std::io::{BufRead, ErrorKind};

use anyhow::Result;

pub(super) const READ_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Default)]
pub(super) struct LineScan {
    pub(super) consumed: u64,
    pub(super) lines: u64,
    pub(super) skipped_large: u64,
    pub(super) read_failed: bool,
}

// Bytes after the last newline are a torn write: they stay unconsumed for the next pass.
pub(super) fn scan_lines<R: BufRead>(
    reader: &mut R,
    mut on_line: impl FnMut(&[u8]) -> Result<()>,
) -> Result<LineScan> {
    let mut scan = LineScan::default();
    let mut line: Vec<u8> = Vec::new();
    let mut oversized = false;
    let mut pending = 0u64;
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => {
                scan.read_failed = true;
                return Ok(scan);
            }
        };
        if buffer.is_empty() {
            return Ok(scan);
        }
        let newline = buffer.iter().position(|b| *b == b'\n');
        let take = newline.map_or(buffer.len(), |at| at + 1);
        if !oversized {
            if line.len() + take > MAX_LINE_BYTES {
                // Stop buffering at once: these lines can be hundreds of megabytes.
                oversized = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(&buffer[..take]);
            }
        }
        reader.consume(take);
        pending += take as u64;
        if newline.is_some() {
            if oversized {
                scan.skipped_large += 1;
            } else {
                on_line(line.trim_ascii_end())?;
                scan.lines += 1;
            }
            scan.consumed += pending;
            pending = 0;
            oversized = false;
            line.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn collect(input: &[u8]) -> (LineScan, Vec<Vec<u8>>) {
        let mut seen = Vec::new();
        let scan = scan_lines(&mut Cursor::new(input), |l| {
            seen.push(l.to_vec());
            Ok(())
        })
        .expect("scan");
        (scan, seen)
    }

    #[test]
    fn torn_tail_is_not_consumed() {
        let (scan, seen) = collect(b"one\r\ntwo\nthr");
        assert_eq!(seen, vec![b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(scan.consumed, 9);
    }

    #[test]
    fn oversized_line_is_skipped_but_consumed() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 10];
        input.extend_from_slice(b"\nok\n");
        let (scan, seen) = collect(&input);
        assert_eq!(seen, vec![b"ok".to_vec()]);
        assert_eq!(scan.skipped_large, 1);
        assert_eq!(scan.consumed, input.len() as u64);
    }
}
