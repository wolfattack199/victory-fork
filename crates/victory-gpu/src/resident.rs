//! Keeps this process's pages from Windows' trimming (`VICTORY_KEEP_RESIDENT`:
//! `1` on, `alt` twenty seconds with and twenty without, in turn; off by
//! default).
//!
//! With the PC's memory nearly all in use (about 2 GB available of 32 in the
//! opening cutscene of run c7), Windows takes pages out of the working sets
//! of the processes that hold the most, and the next touch of each brings it
//! back: 39,000 such faults a second in that run, besides 34,000 for fresh
//! pages. The copies of title memory meet them. Looked up before the copy
//! (`VICTORY_COPY_PROBE=1`), 1.5 to 23 % of the title's pages a copy read
//! were out of the working set, mostly the title's private memory, and none
//! of the host copies' pages it wrote; in runs c3-c5 the cutscene's frame
//! length followed the page faults (62,000 a long frame) as closely as it
//! followed the bytes copied.
//!
//! With the switch a thread of ours sets the process's minimum working set,
//! every two seconds, to what the process holds then (at most the PC's
//! memory less [`LEFT_MB`]), so Windows trims other processes and its file
//! cache first. The limits stay soft: the working set grows past the
//! minimum as before, and Windows may still go below it when memory runs
//! out. Turned off, the minimum goes back to what it was. Raising a minimum
//! needs the account's "increase a process working set" right
//! (`SeIncreaseWorkingSetPrivilege`, which Windows gives every user and
//! leaves disabled): it is enabled for this process alone if the first try
//! is refused. No setting of the system changes.
//!
//! Measured in run c8 (`alt`, the opening cutscene): no gain. The title's
//! pages out of the working set before a copy were as many with it as
//! without (7.3 against 7.2 %, 4.9 against 4.1 %), and the frame rate went
//! with the scene: what takes those pages away is not Windows' trimming.
//! Hence off by default.

use std::time::Duration;

/// Memory left to the rest of the PC: the minimum never goes past the PC's
/// memory less this.
const LEFT_MB: u64 = 6 * 1024;

/// Steps under which the minimum is not changed again.
const STEP_MB: u64 = 64;

static SWITCH: crate::turns::Switch = crate::turns::Switch::new(
    "VICTORY_KEEP_RESIDENT",
    false,
    "the process's pages are kept from Windows' trimming (a minimum working set)",
);

/// What the minimum is set to, for what the process holds and the PC's
/// memory, in bytes.
fn minimum(holds: u64, physical: u64) -> u64 {
    holds.min(physical.saturating_sub(LEFT_MB << 20))
}

/// Starts the thread, if the switch is ever on.
pub(crate) fn start() {
    if !SWITCH.ever() {
        return;
    }
    let _ = std::thread::Builder::new().name("victory-resident".into()).spawn(run);
}

fn run() {
    let Some((was_min, was_max)) = limits() else {
        tracing::warn!("the process's working set limits could not be read: VICTORY_KEEP_RESIDENT does nothing");
        return;
    };
    let physical = physical();
    tracing::info!(
        was_min_mb = was_min >> 20,
        was_max_mb = was_max >> 20,
        physical_mb = physical >> 20,
        left_mb = LEFT_MB,
        "the process's pages are to be kept from Windows' trimming (VICTORY_KEEP_RESIDENT)"
    );
    let mut set = was_min as u64;
    let mut said = 0u32;
    let mut enabled = false;
    loop {
        let target = if SWITCH.on() { minimum(holds(), physical).max(was_min as u64) } else { was_min as u64 };
        if target.abs_diff(set) >= STEP_MB << 20 || (target == was_min as u64 && set != target) {
            let max = (physical as usize).max(was_max).max(target as usize);
            let mut ok = set_limits(target as usize, max);
            if !ok && !enabled {
                enabled = enable_right();
                ok = enabled && set_limits(target as usize, max);
            }
            let error = if ok { String::new() } else { std::io::Error::last_os_error().to_string() };
            if ok {
                set = target;
            }
            if !ok || said < 4 {
                said += 1;
                tracing::info!(
                    ok,
                    minimum_mb = target >> 20,
                    holds_mb = holds() >> 20,
                    error,
                    "minimum working set (VICTORY_KEEP_RESIDENT)"
                );
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// The process's working set limits now, in bytes.
fn limits() -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::GetProcessWorkingSetSizeEx;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let (mut min, mut max, mut flags) = (0usize, 0usize, 0u32);
    // SAFETY: our own process; out pointers to locals.
    (unsafe { GetProcessWorkingSetSizeEx(GetCurrentProcess(), &mut min, &mut max, &mut flags) } != 0).then_some((min, max))
}

/// Soft limits: Windows may go past both.
fn set_limits(min: usize, max: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_DISABLE, SetProcessWorkingSetSizeEx,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: our own process; neither size is the "empty it" value (all ones).
    unsafe {
        SetProcessWorkingSetSizeEx(
            GetCurrentProcess(),
            min,
            max,
            QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
        ) != 0
    }
}

/// Enables the account's right to raise a working set for this process's
/// token (it holds it, disabled). Whether it is enabled now.
fn enable_right() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_INC_WORKING_SET_NAME, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: our own process; an out pointer to a local.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    // SAFETY: a constant name; an out pointer to a local.
    let found = unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_INC_WORKING_SET_NAME, &mut luid) } != 0;
    let state = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
    };
    // SAFETY: the token is ours and open for adjusting; the state is one
    // privilege. The call succeeds without enabling it if the token does not
    // hold it: ERROR_NOT_ALL_ASSIGNED (1300) then.
    let enabled = found
        && unsafe { AdjustTokenPrivileges(token, 0, &state, 0, std::ptr::null_mut(), std::ptr::null_mut()) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(1300);
    // SAFETY: the handle opened above.
    unsafe { CloseHandle(token) };
    tracing::info!(enabled, "the right to raise this process's working set (SeIncreaseWorkingSetPrivilege)");
    enabled
}

/// What the process holds in its working set now, in bytes.
fn holds() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain data the call fills in.
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: our own process; a sized structure.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
        return 0;
    }
    c.WorkingSetSize as u64
}

/// The PC's memory, in bytes.
fn physical() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    m.ullTotalPhys
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_minimum_leaves_room_for_the_rest_of_the_pc() {
        const GB: u64 = 1 << 30;
        assert_eq!(super::minimum(14 * GB, 32 * GB), 14 * GB);
        assert_eq!(super::minimum(30 * GB, 32 * GB), 26 * GB);
        assert_eq!(super::minimum(3 * GB, 4 * GB), 0);
    }

    #[test]
    fn the_limits_can_be_read_and_set_back() {
        let (min, max) = super::limits().expect("limits");
        assert!(max >= min);
        assert!(super::set_limits(min, max));
    }
}
