//! Fallible command output and best-effort diagnostics.

use std::fmt;
use std::io::{self, Write};

/// Emit a diagnostic without panicking on an I/O failure.
#[doc(hidden)]
#[macro_export]
macro_rules! diagnostic {
    ($($arg:tt)*) => { $crate::output::diagnostic(format_args!($($arg)*)) };
}

/// Write a complete CLI response, including its trailing newline and flush.
pub fn write_output(writer: &mut impl Write, output: &str) -> io::Result<()> {
    writer.write_all(output.as_bytes())?;
    if !output.ends_with('\n') {
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

/// Diagnostics must not panic when stderr is full or disconnected.
pub fn diagnostic(message: fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter {
        remaining: usize,
        fail_flush: bool,
        bytes: Vec<u8>,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::ErrorKind::StorageFull.into());
            }
            let n = bytes.len().min(self.remaining);
            self.bytes.extend_from_slice(&bytes[..n]);
            self.remaining -= n;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::ErrorKind::StorageFull.into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn storage_full_during_write_newline_or_flush_is_returned() {
        for (remaining, fail_flush) in [(0, false), (2, false), (4, false), (usize::MAX, true)] {
            let mut writer = FailingWriter {
                remaining,
                fail_flush,
                bytes: Vec::new(),
            };
            assert_eq!(
                write_output(&mut writer, "body").unwrap_err().kind(),
                io::ErrorKind::StorageFull
            );
        }
    }

    #[test]
    fn output_has_exactly_one_trailing_newline() {
        for output in ["body", "body\n"] {
            let mut bytes = Vec::new();
            write_output(&mut bytes, output).unwrap();
            assert_eq!(bytes, b"body\n");
        }
    }
}
