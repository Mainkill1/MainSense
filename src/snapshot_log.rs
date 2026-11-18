//! Append one complete JSON line at a time without waiting for the log during sampling.

use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, Thread};
use std::time::Duration;

const MAX_PENDING: usize = 32;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(50);

type FileIdentity = (u64, u64);

pub struct SnapshotLog {
    pending: Option<SyncSender<String>>,
    completion: Receiver<io::Result<()>>,
    worker: Thread,
    abort: Arc<AtomicBool>,
    error: Option<io::Error>,
}

impl SnapshotLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        let stdout = stdout_identity();
        // Validate and create the destination before the monitor samples.
        open_log(path, stdout)?;
        let path = path.to_path_buf();
        let (sender, pending) = mpsc::sync_channel(MAX_PENDING);
        let (finished, completion) = mpsc::sync_channel(1);
        let abort = Arc::new(AtomicBool::new(false));
        let worker_abort = Arc::clone(&abort);
        let worker = thread::Builder::new()
            .name("mainsense-log".to_owned())
            .spawn(move || {
                let result = write_pending(&path, stdout, &pending, &worker_abort);
                // Publish the worker's result before disconnecting the queue.
                let _ = finished.send(result);
                drop(pending);
            })?;
        Ok(Self {
            pending: Some(sender),
            completion,
            worker: worker.thread().clone(),
            abort,
            error: None,
        })
    }

    pub fn record(&mut self, line: String) -> io::Result<()> {
        if let Some(error) = &self.error {
            return Err(copy_error(error));
        }
        let pending = self.pending.as_ref().expect("open log has a queue");
        match pending.try_send(line) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "log writer fell behind and its queue is full",
            )),
            Err(TrySendError::Disconnected(_)) => {
                let error = self
                    .completion
                    .try_recv()
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(writer_stopped);
                let report = copy_error(&error);
                self.error = Some(error);
                Err(report)
            }
        }
    }

    pub fn finish(mut self) -> io::Result<()> {
        drop(self.pending.take());
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        match self.completion.recv_timeout(CLOSE_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for pending log records",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(writer_stopped()),
        }
    }
}

impl Drop for SnapshotLog {
    fn drop(&mut self) {
        self.abort.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

fn writer_stopped() -> io::Error {
    io::Error::other("log writer stopped unexpectedly")
}

fn copy_error(error: &io::Error) -> io::Error {
    match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(error.kind(), error.to_string()),
    }
}

fn stdout_identity() -> Option<FileIdentity> {
    // SAFETY: fcntl returns a new owned descriptor or reports that duplication failed.
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fcntl returned an open descriptor that only this File owns.
    let stdout = unsafe { File::from_raw_fd(fd) };
    let metadata = stdout.metadata().ok()?;
    Some((metadata.dev(), metadata.ino()))
}

fn open_log(path: &Path, stdout: Option<FileIdentity>) -> io::Result<File> {
    // Opening a FIFO must not block before its type can be checked.
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if stdout == Some((metadata.dev(), metadata.ino())) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log file and stdout refer to the same destination",
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log destination must be a regular file",
        ));
    }
    Ok(file)
}

fn write_pending(
    path: &Path,
    stdout: Option<FileIdentity>,
    pending: &Receiver<String>,
    abort: &AtomicBool,
) -> io::Result<()> {
    while !abort.load(Ordering::Acquire) {
        let Ok(line) = pending.recv() else {
            break;
        };
        append(path, stdout, line.as_bytes(), abort)?;
    }
    Ok(())
}

fn append(
    path: &Path,
    stdout: Option<FileIdentity>,
    line: &[u8],
    abort: &AtomicBool,
) -> io::Result<()> {
    while !abort.load(Ordering::Acquire) {
        // Reopen for each append and lock retry so a rename can replace the active path.
        let file = open_log(path, stdout)?;
        // SAFETY: file owns the open descriptor for this call and the following append.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return write_line(file, line, abort);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::WouldBlock => {
                drop(file);
                thread::park_timeout(LOCK_RETRY);
            }
            io::ErrorKind::Interrupted => continue,
            _ => return Err(error),
        }
    }
    Ok(())
}

fn write_line(mut file: File, line: &[u8], abort: &AtomicBool) -> io::Result<()> {
    if abort.load(Ordering::Acquire) {
        return Ok(());
    }
    let size = file.metadata()?.len();
    let mut last = [b'\n'];
    if size > 0 {
        file.read_exact_at(&mut last, size - 1)?;
    }
    let separator = if last == [b'\n'] {
        &b""[..]
    } else {
        &b"\n"[..]
    };
    let expected = separator.len() + line.len();
    let written = loop {
        if abort.load(Ordering::Acquire) {
            return Ok(());
        }
        match file.write_vectored(&[IoSlice::new(separator), IoSlice::new(line)]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => break result?,
        }
    };
    if written == expected {
        return Ok(());
    }
    // Keep the lock during rollback and check for intervening writes before truncating.
    if written > 0 && file.metadata()?.len() == size + written as u64 {
        file.set_len(size)?;
    }
    Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "incomplete log write",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = env::temp_dir().join(format!(
                    "mainsense-log-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("{error}"),
                }
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finishes_all_accepted_records_and_repairs_a_missing_newline() {
        let dir = TestDirectory::new();
        let path = dir.path("snapshots.jsonl");
        fs::write(&path, "{\"previous\":true}").unwrap();
        let mut log = SnapshotLog::open(&path).unwrap();
        log.record("{\"sequence\":1}\n".to_owned()).unwrap();
        log.record("{\"sequence\":2}\n".to_owned()).unwrap();
        log.finish().unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "{\"previous\":true}\n{\"sequence\":1}\n{\"sequence\":2}\n"
        );
    }

    #[test]
    fn rejects_a_fifo_and_a_hard_link_to_stdout() {
        let dir = TestDirectory::new();
        let fifo = dir.path("fifo");
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_name points to a valid NUL-terminated pathname.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let error = SnapshotLog::open(&fifo).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let stdout = dir.path("stdout");
        let alias = dir.path("snapshots.jsonl");
        fs::write(&stdout, "existing").unwrap();
        fs::hard_link(&stdout, &alias).unwrap();
        let status = Command::new(env::current_exe().unwrap())
            .args(["rejects_stdout_alias_in_child", "--ignored"])
            .env("MAINSENSE_TEST_ALIAS", &alias)
            .stdout(OpenOptions::new().append(true).open(&stdout).unwrap())
            .status()
            .unwrap();
        // The libtest harness writes its report to stdout, even on success.
        assert!(status.success(), "{}", fs::read_to_string(&stdout).unwrap());
        assert!(fs::read_to_string(&alias).unwrap().starts_with("existing"));
    }

    #[test]
    #[ignore = "run in a subprocess with stdout redirected to a log alias"]
    fn rejects_stdout_alias_in_child() {
        let path = PathBuf::from(env::var_os("MAINSENSE_TEST_ALIAS").unwrap());
        let error = SnapshotLog::open(&path).err().unwrap();
        assert!(error.to_string().contains("stdout"), "{error}");
    }

    #[test]
    fn locked_queue_is_bounded_and_rotates_to_the_new_path() {
        let dir = TestDirectory::new();
        let path = dir.path("snapshots.jsonl");
        fs::write(&path, b"").unwrap();
        let held = OpenOptions::new().append(true).open(&path).unwrap();
        // SAFETY: held keeps the descriptor open until the test completes.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        let mut log = SnapshotLog::open(&path).unwrap();
        let mut accepted = 0;
        for _ in 0..34 {
            match log.record("{}\n".to_owned()) {
                Ok(()) => accepted += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        // The worker can have one record in flight in addition to the queue.
        assert!((32..=33).contains(&accepted));
        let rotated = dir.path("snapshots.old");
        fs::rename(&path, &rotated).unwrap();
        log.finish().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "{}\n".repeat(accepted));
        assert_eq!(fs::read(rotated).unwrap(), b"");
    }

    #[test]
    fn concurrent_writers_preserve_all_complete_records() {
        let dir = TestDirectory::new();
        let path = dir.path("snapshots.jsonl");
        fs::write(&path, "null").unwrap();
        let mut first = SnapshotLog::open(&path).unwrap();
        let mut second = SnapshotLog::open(&path).unwrap();
        for _ in 0..8 {
            first.record("1\n".to_owned()).unwrap();
            second.record("2\n".to_owned()).unwrap();
        }
        first.finish().unwrap();
        second.finish().unwrap();
        let output = fs::read_to_string(path).unwrap();
        let mut lines = output.lines();
        assert_eq!(lines.next(), Some("null"));
        let mut remaining: Vec<_> = lines.collect();
        remaining.sort_unstable();
        assert_eq!(
            remaining,
            ["1"; 8].into_iter().chain(["2"; 8]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rolls_back_after_a_short_write() {
        let dir = TestDirectory::new();
        let path = dir.path("snapshots.jsonl");
        fs::write(&path, b"null").unwrap();
        let output = Command::new(env::current_exe().unwrap())
            .args(["rolls_back_short_write_in_child", "--ignored"])
            .env("MAINSENSE_TEST_LIMITED_LOG", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(path).unwrap(), b"null");
    }

    #[test]
    fn shutdown_times_out_when_the_destination_stays_locked() {
        let dir = TestDirectory::new();
        let path = dir.path("snapshots.jsonl");
        fs::write(&path, b"").unwrap();
        let held = OpenOptions::new().append(true).open(&path).unwrap();
        // SAFETY: held keeps the descriptor open until the test completes.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        let mut log = SnapshotLog::open(&path).unwrap();
        log.record("{}\n".to_owned()).unwrap();
        let error = log.finish().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(fs::read(path).unwrap(), b"");
    }

    #[test]
    #[ignore = "run in a subprocess to constrain its file-size limit"]
    fn rolls_back_short_write_in_child() {
        let path = PathBuf::from(env::var_os("MAINSENSE_TEST_LIMITED_LOG").unwrap());
        let limit = libc::rlimit {
            rlim_cur: 7,
            rlim_max: 7,
        };
        // SAFETY: limit is valid and these changes only affect the test subprocess.
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
        }
        let mut log = SnapshotLog::open(&path).unwrap();
        log.record("{\"snapshot\":true}\n".to_owned()).unwrap();
        assert_eq!(log.finish().unwrap_err().kind(), io::ErrorKind::WriteZero);
    }
}
