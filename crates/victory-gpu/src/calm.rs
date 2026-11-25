//! Keeps the host driver's background threads off most of the processor.
//!
//! After a pipeline is made or loaded, NVIDIA's driver compiles its shaders
//! again on four threads the Direct3D runtime names "D3D Background Thread
//! N". They run at the title's priority, for minutes after the world's first
//! frame: 3.5 of 6 cores on a Ryzen 5 5500, and the same scene went from 1-2
//! to 6.9 frames per second when they were done. Direct3D's own switch
//! (`SetBackgroundProcessingMode`) is refused unless Windows is in Developer
//! Mode, so the threads are moved to a few processors: they still finish,
//! later. Their priority is left alone: they hold the process heap's lock
//! for half of their time (the compiler allocates all the time), and at the
//! lowest priority the one that held it was not run while the title's audio
//! thread, the submitting thread and the one that copies back waited for it.
//!
//! `VICTORY_DRIVER_PROCESSORS=N`: the last N logical processors this process
//! may use are theirs (unset or 0: they are left alone).

use std::collections::HashSet;
use std::time::Duration;

/// What the runtime calls the threads.
const NAME: &str = "D3D Background Thread";

fn processors() -> u32 {
    std::env::var("VICTORY_DRIVER_PROCESSORS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// The highest `n` processors of `mask`; all of them if there are no more.
fn last(mask: usize, n: u32) -> usize {
    let mut left = mask;
    while left.count_ones() > n {
        left &= left - 1;
    }
    left
}

/// Starts the thread that looks for the driver's threads, if asked for.
pub(crate) fn start() {
    let n = processors();
    if n == 0 {
        return;
    }
    let _ = std::thread::Builder::new().name("victory-calm".into()).spawn(move || {
        let mut seen = HashSet::new();
        loop {
            // They are made with the first pipelines and kept.
            let found = calm(n, &mut seen);
            if found > 0 {
                tracing::info!(threads = found, processors = n, "driver background threads kept on the last processors");
            }
            std::thread::sleep(Duration::from_secs(if seen.is_empty() { 2 } else { 10 }));
        }
    });
}

/// Moves the threads not seen before; how many that were.
fn calm(n: u32, seen: &mut HashSet<u32>) -> usize {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessAffinityMask, GetThreadDescription, OpenThread,
        SetThreadAffinityMask, THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION,
    };
    let (mut ours, mut system) = (0usize, 0usize);
    // SAFETY: plain queries; every handle opened is closed, and the
    // description, which the host allocates, is freed.
    unsafe {
        if GetProcessAffinityMask(GetCurrentProcess(), &mut ours, &mut system) == 0 || ours == 0 {
            return 0;
        }
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }
        let process = GetCurrentProcessId();
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut found = 0;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            let id = entry.th32ThreadID;
            if entry.th32OwnerProcessID == process && !seen.contains(&id) {
                let thread = OpenThread(THREAD_QUERY_INFORMATION | THREAD_SET_INFORMATION, 0, id);
                if !thread.is_null() {
                    let mut text: *mut u16 = std::ptr::null_mut();
                    if GetThreadDescription(thread, &mut text) >= 0 && !text.is_null() {
                        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
                        let name = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                        LocalFree(text.cast());
                        if name.starts_with(NAME) {
                            SetThreadAffinityMask(thread, last(ours, n));
                            seen.insert(id);
                            found += 1;
                        }
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_processors_are_the_highest_bits() {
        assert_eq!(last(0xfff, 2), 0xc00);
        assert_eq!(last(0xfff, 1), 0x800);
        assert_eq!(last(0b1010, 1), 0b1000);
        assert_eq!(last(0b1010, 8), 0b1010);
    }

    #[test]
    fn a_thread_of_that_name_is_moved_once() {
        let (tell, told) = std::sync::mpsc::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name(format!("{NAME} 99"))
            .spawn(move || {
                ready.send(()).unwrap();
                let _ = told.recv();
            })
            .unwrap();
        started.recv().unwrap();
        let mut seen = HashSet::new();
        assert!(calm(1, &mut seen) >= 1);
        let again = calm(1, &mut seen);
        drop(tell);
        thread.join().unwrap();
        assert_eq!(again, 0);
    }
}
