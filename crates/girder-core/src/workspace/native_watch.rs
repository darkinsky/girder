//! Small platform notifier used by `workspaceFiles.changes`.
//!
//! Linux has a native notifier in the standard C library interface. The wire layer still
//! re-stat's the target after every notification, so this module only wakes a waiter; it never
//! manufactures a version from an inotify payload. Other platforms return `None` and the caller
//! keeps its bounded polling fallback.

#[cfg(target_os = "linux")]
mod linux {
    #![allow(unsafe_code)]

    use std::ffi::CString;
    use std::os::fd::RawFd;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use tokio::sync::mpsc;

    /// A native notifier and its wake receiver.
    pub struct NativeWatcher {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl NativeWatcher {
        /// Starts watching the target or its parent when the target is absent.
        ///
        /// The watcher intentionally listens broadly and lets the caller compare a fresh
        /// fingerprint. That handles renames and absent targets without trusting a platform event's
        /// optional filename.
        pub fn open(target: &str, directory: bool) -> Option<(Self, mpsc::UnboundedReceiver<()>)> {
            let (fd, watch_descriptor, stop) = prepare(target, directory)?;
            let stopped = Arc::clone(&stop);
            let (notify, receiver) = mpsc::unbounded_channel();
            let thread = match std::thread::Builder::new()
                .name("girder-fs-watch".to_owned())
                .spawn(move || {
                    run(fd, watch_descriptor, stopped, move || {
                        let _ = notify.send(());
                    })
                }) {
                Ok(thread) => thread,
                Err(_) => {
                    release(fd, watch_descriptor);
                    return None;
                }
            };
            Some((
                Self {
                    stop,
                    thread: Some(thread),
                },
                receiver,
            ))
        }

        /// Watches `target` and wakes `wake` when the kernel reports an event.
        ///
        /// The returned watcher owns the thread. Dropping it stops the watch. There is no channel
        /// to drain: a caller that only needs to re-read the tree uses this instead of [`Self::open`].
        pub fn watch(
            target: &str,
            directory: bool,
            wake: std::sync::Arc<tokio::sync::Notify>,
        ) -> Option<Self> {
            let (fd, watch_descriptor, stop) = prepare(target, directory)?;
            let stopped = std::sync::Arc::clone(&stop);
            let thread = match std::thread::Builder::new()
                .name("girder-fs-watch".to_owned())
                .spawn(move || {
                    run(fd, watch_descriptor, stopped, move || {
                        wake.notify_waiters();
                    })
                }) {
                Ok(thread) => thread,
                Err(_) => {
                    release(fd, watch_descriptor);
                    return None;
                }
            };
            Some(Self {
                stop,
                thread: Some(thread),
            })
        }
    }

    impl Drop for NativeWatcher {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn prepare(target: &str, directory: bool) -> Option<(RawFd, libc::c_int, Arc<AtomicBool>)> {
        let target_path = Path::new(target);
        let watch_path = if directory && target_path.is_dir() {
            target_path.to_owned()
        } else {
            target_path.parent()?.to_owned()
        };
        let path = CString::new(watch_path.as_os_str().as_encoded_bytes()).ok()?;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mask = libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_MODIFY
            | libc::IN_MOVE_SELF
            | libc::IN_DELETE_SELF;
        let watch_descriptor = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) };
        if watch_descriptor < 0 {
            unsafe {
                libc::close(fd);
            }
            return None;
        }
        Some((fd, watch_descriptor, Arc::new(AtomicBool::new(false))))
    }

    fn run(fd: RawFd, watch_descriptor: libc::c_int, stop: Arc<AtomicBool>, on_event: impl Fn()) {
        let mut bytes = [0_u8; 16 * 1024];
        while !stop.load(Ordering::Acquire) {
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let polled = unsafe { libc::poll(&mut pollfd, 1, 250) };
            if polled <= 0 {
                continue;
            }
            let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if read > 0 {
                on_event();
            }
        }
        release(fd, watch_descriptor);
    }

    fn release(fd: RawFd, watch_descriptor: libc::c_int) {
        unsafe {
            libc::inotify_rm_watch(fd, watch_descriptor);
            libc::close(fd);
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::NativeWatcher;

#[cfg(not(target_os = "linux"))]
mod fallback {
    use std::path::Path;

    use tokio::sync::mpsc;

    /// No portable notifier is bundled on this target; callers use polling.
    pub struct NativeWatcher;

    impl NativeWatcher {
        /// No native watcher is available.
        #[must_use]
        pub fn open(
            _target: &str,
            _directory: bool,
        ) -> Option<(Self, mpsc::UnboundedReceiver<()>)> {
            let _ = Path::new("");
            None
        }

        /// No native watcher is available.
        #[must_use]
        pub fn watch(
            _target: &str,
            _directory: bool,
            _wake: std::sync::Arc<tokio::sync::Notify>,
        ) -> Option<Self> {
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use fallback::NativeWatcher;
