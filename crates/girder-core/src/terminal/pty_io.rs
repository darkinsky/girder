//! The few syscalls a pty needs that the standard library does not wrap.
//!
//! The crate denies `unsafe_code`. This module is the exception, and it only opens a pty, makes it
//! the child's controlling terminal, and changes its window size. Everything above this file talks
//! to the resulting [`std::fs::File`].

#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};

/// Opens a pty and sets its initial window size.
///
/// The master is the parent's end. The slave is what the child will use as its standard streams.
pub(super) fn open_pty(columns: u16, rows: u16) -> io::Result<(File, File)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` writes two fresh descriptors into the pointers we pass, and on success
    // those descriptors are open and owned by this process.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if opened < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openpty` just returned these descriptors, and `File` takes ownership of each one.
    unsafe { Ok((File::from_raw_fd(master), File::from_raw_fd(slave))) }
}

/// Installs the child hook that attaches `slave` and closes `master` after `fork`.
pub(super) fn prepare_child(
    command: &mut tokio::process::Command,
    slave: libc::c_int,
    master: libc::c_int,
) {
    // SAFETY: `attach_slave` only calls `setsid`, `ioctl`, `dup2`, and `close`, which are
    // async-signal-safe. The closure runs in the forked child before `exec`.
    unsafe {
        command.pre_exec(move || attach_slave(slave, master));
    }
}

/// Makes `slave` the controlling terminal and the standard streams of the calling process.
///
/// Called in the forked child, before `exec`. `master` is closed so the shell does not keep the
/// parent's end open.
pub(super) fn attach_slave(slave: libc::c_int, master: libc::c_int) -> io::Result<()> {
    // SAFETY: the descriptors belong to this process, and these calls are the ones `forkpty` makes
    // in the child: a new session, this terminal as the controlling one, then the standard streams.
    unsafe {
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::ioctl(slave, libc::TIOCSCTTY, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
        for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            if libc::dup2(slave, target) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if slave > libc::STDERR_FILENO {
            libc::close(slave);
        }
        if master >= 0 {
            libc::close(master);
        }
    }
    Ok(())
}

/// Sets the window size of an open pty and returns whether the kernel accepted it.
pub(super) fn set_window_size(file: &File, columns: u16, rows: u16) -> io::Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the file is still open, and `TIOCSWINSZ` only writes the size struct.
    let updated = unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    if updated < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
