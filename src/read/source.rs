//! Request-local immutable inputs for conditional reads. MCP dispatch stays in
//! one synchronous worker thread, even if its caller times out.
use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Default)]
struct Snapshot {
    files: HashMap<PathBuf, Arc<[u8]>>,
    incomplete: bool,
    captured_bytes: usize,
}
// Bound retained owned data across the entire request, including multi-file batches.
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
thread_local! { static REQUEST: RefCell<Option<Snapshot>> = const { RefCell::new(None) }; }
struct SnapshotGuard(Option<Snapshot>);
impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        REQUEST.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
pub(crate) fn with_snapshot<T>(render: impl FnOnce() -> T) -> (T, bool) {
    let guard = SnapshotGuard(REQUEST.with(|slot| slot.replace(Some(Snapshot::default()))));
    let result = render();
    let complete = REQUEST.with(|slot| !slot.borrow().as_ref().unwrap().incomplete);
    drop(guard);
    (result, complete)
}
pub(crate) fn active() -> bool {
    REQUEST.with(|slot| slot.borrow().is_some())
}
/// Preserve partial/error status even when a budget hides its diagnostic.
pub(crate) fn mark_incomplete() {
    REQUEST.with(|slot| {
        if let Some(r) = slot.borrow_mut().as_mut() {
            r.incomplete = true;
        }
    });
}
pub(crate) fn bytes(path: &Path) -> io::Result<Arc<[u8]>> {
    if !active() {
        return std::fs::read(path).map(Arc::from);
    }
    if let Some(bytes) = REQUEST.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|r| r.files.get(path).cloned())
    }) {
        return Ok(bytes);
    }
    let key = path.canonicalize()?;
    if let Some(bytes) = REQUEST.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|r| r.files.get(&key).cloned())
    }) {
        REQUEST.with(|slot| {
            slot.borrow_mut()
                .as_mut()
                .unwrap()
                .files
                .insert(path.into(), bytes.clone());
        });
        return Ok(bytes);
    }
    let remaining =
        REQUEST.with(|slot| MAX_SNAPSHOT_BYTES - slot.borrow().as_ref().unwrap().captured_bytes);
    let bytes = capture(&key, remaining)?;
    REQUEST.with(|slot| {
        let mut slot = slot.borrow_mut();
        let snapshot = slot.as_mut().unwrap();
        snapshot.captured_bytes += bytes.len();
        snapshot.files.insert(key, bytes.clone());
        snapshot.files.insert(path.into(), bytes.clone());
    });
    Ok(bytes)
}

/// Read one opened handle and bound retained memory. Detect common in-place
/// races, but do not promise a filesystem transaction or adversarial detection.
fn capture(path: &Path, remaining: usize) -> io::Result<Arc<[u8]>> {
    use io::Read as _;
    let too_large = || {
        io::Error::other("tagged read snapshots exceed 64 MiB per request; omit tag options to read without snapshots")
    };
    for _ in 0..2 {
        let mut file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshot requires a regular file",
            ));
        }
        if before.len() > remaining as u64 {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(remaining as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > remaining {
            return Err(too_large());
        }
        let after = file.metadata()?;
        if before.len() == after.len()
            && after.len() == bytes.len() as u64
            && before.modified().ok() == after.modified().ok()
        {
            return Ok(bytes.into());
        }
    }
    Err(io::Error::other(
        "file changed while capturing snapshot; retry",
    ))
}

pub(crate) fn text(path: &Path) -> io::Result<String> {
    String::from_utf8(bytes(path)?.to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
/// Ordinary full reads retain their mmap path; tagged reads own their bytes.
pub(crate) enum Buffer {
    Mapped(memmap2::Mmap),
    Owned(Arc<[u8]>),
}
impl std::ops::Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped(b) => b,
            Self::Owned(b) => b,
        }
    }
}
pub(crate) fn map(path: &Path) -> io::Result<Buffer> {
    if active() {
        return bytes(path).map(Buffer::Owned);
    }
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(Buffer::Owned(Arc::from([])));
    }
    // SAFETY: preserves the existing untagged mmap behavior. Tagged reads never
    // map mutable filesystem bytes beneath their captured view.
    unsafe { memmap2::Mmap::map(&file) }.map(Buffer::Mapped)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reuse_and_unwind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "old").unwrap();
        assert!(
            with_snapshot(|| {
                assert_eq!(&*bytes(&path).unwrap(), b"old");
                std::fs::write(&path, "new").unwrap();
                assert_eq!(&*bytes(&path).unwrap(), b"old");
            })
            .1
        );
        assert_eq!(&*bytes(&path).unwrap(), b"new");
        let _ = std::panic::catch_unwind(|| with_snapshot(|| panic!("test")));
        assert!(!active());
    }
    #[test]
    fn snapshot_limit_rejects_large_inputs_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .unwrap();
        let (result, _) = with_snapshot(|| bytes(&path));
        assert!(result.unwrap_err().to_string().contains("64 MiB"));
    }
    #[cfg(unix)]
    #[test]
    fn retargeted_alias_stays_pinned_until_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let link = dir.path().join("link");
        std::fs::write(&a, "old").unwrap();
        std::fs::write(&b, "new").unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        with_snapshot(|| {
            assert_eq!(&*bytes(&link).unwrap(), b"old");
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&b, &link).unwrap();
            assert_eq!(&*bytes(&link).unwrap(), b"old");
            assert_eq!(&*bytes(&a).unwrap(), b"old");
        });
        assert_eq!(&*with_snapshot(|| bytes(&link).unwrap()).0, b"new");
    }
}
