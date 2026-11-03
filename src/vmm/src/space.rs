// Copyright 2026 Spacebox authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The fill of guest memory (`PUT /space/fill`).
//!
//! A fill populates every page of the plugged guest memory for writing
//! (`madvise(MADV_POPULATE_WRITE)`), so that afterwards no page of the guest is a page of the
//! page cache of the host or a page that is not there. It runs on a thread of its own while the
//! guest runs on: on the VMM thread it would stop every device for seconds for each GiB. The
//! thread is started before the seccomp filter of the VMM thread is set (that filter allows no
//! new thread) and sets its own filter, of the thread category `fill`.
//!
//! The fill goes slot by slot. The guest may unplug a slot of its hotplug region while the fill
//! runs: the slot is then `PROT_NONE` and its populate fails (`EINVAL`). Such a slot, one that is
//! no longer plugged when its populate fails, is skipped and the fill goes on; a failure of a
//! slot that is still plugged ends the fill as `failed`.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::logger::{error, info};
use crate::seccomp::BpfProgram;
use crate::vmm_config::space::{FillState, FillStatus};

/// `MADV_POPULATE_WRITE` of Linux 5.14.
const MADV_POPULATE_WRITE: libc::c_int = 23;

/// Whether a slot of guest memory is still plugged, read when its populate fails.
pub type StillPlugged = Box<dyn Fn() -> bool + Send>;

/// One slot of guest memory in the address space of the process: its address, its length, and
/// for a slot of a hotplug region the reading of whether it is still plugged (none: a slot that
/// cannot be unplugged, whose failure always ends the fill).
pub struct HostRange {
    /// The address of the slot.
    pub addr: usize,
    /// Its length.
    pub len: usize,
    /// Whether it is still plugged.
    pub plugged: Option<StillPlugged>,
}

impl std::fmt::Debug for HostRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostRange")
            .field("addr", &self.addr)
            .field("len", &self.len)
            .field("hotplug", &self.plugged.is_some())
            .finish()
    }
}

impl HostRange {
    /// A slot that cannot be unplugged.
    pub fn fixed(addr: usize, len: usize) -> HostRange {
        HostRange {
            addr,
            len,
            plugged: None,
        }
    }
}

/// Errors of a fill.
#[derive(Debug, thiserror::Error, displaydoc::Display, PartialEq, Eq)]
pub enum FillError {
    /// A fill is already running.
    AlreadyRunning,
    /// The fill thread is not running.
    NoThread,
}

/// The handle of the fill thread.
#[derive(Debug, Default)]
pub struct Filler {
    jobs: Option<Sender<Vec<HostRange>>>,
    status: Arc<Mutex<FillStatus>>,
}

impl Filler {
    /// Starts the fill thread. It sets `filter` and waits for a fill.
    pub fn spawn(filter: Arc<BpfProgram>) -> std::io::Result<Filler> {
        let (jobs, receiver) = channel::<Vec<HostRange>>();
        let status = Arc::new(Mutex::new(FillStatus::default()));
        let thread_status = status.clone();
        thread::Builder::new()
            .name("fc_fill".to_owned())
            .spawn(move || {
                if let Err(err) = crate::seccomp::apply_filter(&filter) {
                    panic!("Failed to set the seccomp filter of the fill thread: {err}");
                }
                // The thread ends when the Vmm, which holds the sender, is dropped.
                while let Ok(ranges) = receiver.recv() {
                    let result = populate_write(&ranges);
                    let mut status = thread_status.lock().expect("Poisoned lock");
                    *status = match result {
                        Ok(skipped) => {
                            if skipped > 0 {
                                info!(
                                    "The fill of guest memory is done; {skipped} slots unplugged \
                                     during it were skipped."
                                );
                            }
                            info!("The fill of guest memory is done.");
                            FillStatus {
                                state: FillState::Done,
                                error: None,
                            }
                        }
                        Err(err) => {
                            error!("The fill of guest memory failed: {err}");
                            FillStatus {
                                state: FillState::Failed,
                                error: Some(err.to_string()),
                            }
                        }
                    };
                }
            })?;
        Ok(Filler {
            jobs: Some(jobs),
            status,
        })
    }

    /// Starts a fill of `ranges`. A fill that runs is not started again.
    pub fn start(&self, ranges: Vec<HostRange>) -> Result<(), FillError> {
        let jobs = self.jobs.as_ref().ok_or(FillError::NoThread)?;
        let mut status = self.status.lock().expect("Poisoned lock");
        if status.state == FillState::Running {
            return Err(FillError::AlreadyRunning);
        }
        // The thread writes the end of the fill under the same lock, so the state that is set
        // here is never written after that end.
        jobs.send(ranges).map_err(|_| FillError::NoThread)?;
        *status = FillStatus {
            state: FillState::Running,
            error: None,
        };
        drop(status);
        Ok(())
    }

    /// What the fill is doing.
    pub fn status(&self) -> FillStatus {
        self.status.lock().expect("Poisoned lock").clone()
    }
}

/// Populates each slot for writing. A page that is there keeps its bytes; a page of a private
/// file mapping gets a copy of its own; a page that is not there becomes a zero page, or is asked
/// for from the UFFD handler of a range that one serves. A slot whose populate fails and that is
/// no longer plugged (the guest unplugged it during the fill) is skipped. Returns the number of
/// slots skipped.
fn populate_write(ranges: &[HostRange]) -> std::io::Result<usize> {
    let mut skipped = 0;
    for r in ranges {
        loop {
            // SAFETY: the range is guest memory that the process mapped and keeps mapped while
            // the Vmm lives; the advice changes no byte of it.
            let ret =
                unsafe { libc::madvise(r.addr as *mut libc::c_void, r.len, MADV_POPULATE_WRITE) };
            if ret == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
                _ if r.plugged.as_ref().is_some_and(|plugged| !plugged()) => {
                    skipped += 1;
                    break;
                }
                _ => return Err(err),
            }
        }
    }
    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn wait_done(filler: &Filler) -> FillStatus {
        let start = Instant::now();
        loop {
            let status = filler.status();
            if status.state != FillState::Running {
                return status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "fill did not end"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The number of pages of `[addr, addr + len)` that are in memory.
    fn resident_pages(addr: usize, len: usize) -> usize {
        let pages = len / 4096;
        let mut vec = vec![0u8; pages];
        // SAFETY: the range is a mapping of this test and `vec` has a byte for each page.
        let ret = unsafe { libc::mincore(addr as *mut libc::c_void, len, vec.as_mut_ptr()) };
        assert_eq!(ret, 0);
        vec.iter().filter(|b| **b & 1 != 0).count()
    }

    #[test]
    fn test_fill_populates_every_page() {
        let len = 64 * 4096;
        // SAFETY: an anonymous private mapping of this test.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        let addr = addr as usize;
        // One page holds data before the fill and keeps it.
        // SAFETY: the address is in the mapping above.
        unsafe { *((addr + 4096) as *mut u8) = 0x5a };
        assert_eq!(resident_pages(addr, len), 1);

        let filler = Filler::spawn(Arc::new(vec![])).unwrap();
        assert_eq!(filler.status().state, FillState::Idle);
        filler.start(vec![HostRange::fixed(addr, len)]).unwrap();
        let status = wait_done(&filler);
        assert_eq!(status.state, FillState::Done, "{:?}", status.error);
        assert_eq!(resident_pages(addr, len), 64);
        // SAFETY: the address is in the mapping above.
        assert_eq!(unsafe { *((addr + 4096) as *const u8) }, 0x5a);

        // A fill can run again after one ended.
        filler.start(vec![HostRange::fixed(addr, len)]).unwrap();
        assert_eq!(wait_done(&filler).state, FillState::Done);

        // SAFETY: the mapping of this test.
        unsafe { libc::munmap(addr as *mut libc::c_void, len) };
    }

    #[test]
    fn test_fill_failure_is_reported() {
        let len = 4 * 4096;
        // SAFETY: an anonymous mapping of this test with no access, which cannot be populated.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        let filler = Filler::spawn(Arc::new(vec![])).unwrap();
        filler.start(vec![HostRange::fixed(addr as usize, len)]).unwrap();
        let status = wait_done(&filler);
        assert_eq!(status.state, FillState::Failed);
        assert!(status.error.is_some());
        // A slot of a hotplug region that is still plugged fails the fill in the same way.
        filler
            .start(vec![HostRange {
                addr: addr as usize,
                len,
                plugged: Some(Box::new(|| true)),
            }])
            .unwrap();
        assert_eq!(wait_done(&filler).state, FillState::Failed);
        // SAFETY: the mapping of this test.
        unsafe { libc::munmap(addr, len) };
    }

    /// Memory groups' S0 (2026-10-22): an unplug during a fill made the fill fail with
    /// "Invalid argument". A slot that is no longer plugged when its populate fails is skipped,
    /// and the slots after it are filled.
    #[test]
    fn test_fill_skips_a_slot_unplugged_during_it() {
        let len = 16 * 4096;
        let map = |prot| {
            // SAFETY: an anonymous private mapping of this test.
            let a = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    prot,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                    -1,
                    0,
                )
            };
            assert_ne!(a, libc::MAP_FAILED);
            a as usize
        };
        // the first slot was unplugged before its turn came (PROT_NONE), the second is plugged
        let (gone, kept) = (map(libc::PROT_NONE), map(libc::PROT_READ | libc::PROT_WRITE));
        let filler = Filler::spawn(Arc::new(vec![])).unwrap();
        filler
            .start(vec![
                HostRange {
                    addr: gone,
                    len,
                    plugged: Some(Box::new(|| false)),
                },
                HostRange {
                    addr: kept,
                    len,
                    plugged: Some(Box::new(|| true)),
                },
            ])
            .unwrap();
        let status = wait_done(&filler);
        assert_eq!(status.state, FillState::Done, "{:?}", status.error);
        assert_eq!(resident_pages(kept, len), 16);
        // SAFETY: the mappings of this test.
        unsafe {
            libc::munmap(gone as *mut libc::c_void, len);
            libc::munmap(kept as *mut libc::c_void, len);
        }
    }

    #[test]
    fn test_fill_with_no_thread() {
        let filler = Filler::default();
        assert_eq!(filler.start(vec![]), Err(FillError::NoThread));
    }
}
