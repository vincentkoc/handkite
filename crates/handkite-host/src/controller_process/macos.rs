use super::{Identity, MAX_PATH_BYTES, ReadFailure, valid_path};
use std::mem::{MaybeUninit, offset_of, size_of};

// Public macOS SDK sys/proc_info.h and sys/resource.h layouts. Use libc's
// bindings rather than a locally reconstructed C struct or private flavors.
const _: () = {
    assert!(size_of::<libc::proc_bsdinfo>() == 136);
    assert!(offset_of!(libc::proc_bsdinfo, pbi_uid) == 20);
    assert!(offset_of!(libc::proc_bsdinfo, pbi_start_tvsec) == 120);
    assert!(offset_of!(libc::proc_bsdinfo, pbi_start_tvusec) == 128);
    assert!(size_of::<libc::rusage_info_v0>() == 96);
    assert!(offset_of!(libc::rusage_info_v0, ri_proc_start_abstime) == 80);
    assert!(offset_of!(libc::rusage_info_v0, ri_proc_exit_abstime) == 88);
    assert!(libc::PROC_PIDPATHINFO_MAXSIZE as usize == MAX_PATH_BYTES);
};

#[derive(PartialEq, Eq)]
struct Basic {
    start_seconds: u64,
    start_microseconds: u32,
    effective_uid: u32,
}

#[derive(PartialEq, Eq)]
struct Executable {
    uuid: [u8; 16],
    start_abstime: u64,
}

pub(super) fn sample(pid: i32) -> Result<Identity, ReadFailure> {
    sample_with(|| basic(pid), || executable(pid), || path(pid))
}

fn sample_with(
    mut basic: impl FnMut() -> Result<Basic, ReadFailure>,
    mut executable: impl FnMut() -> Result<Executable, ReadFailure>,
    path: impl FnOnce() -> Result<String, ReadFailure>,
) -> Result<Identity, ReadFailure> {
    let before = basic()?;
    // Only a missing initial lookup may be Absent. Disappearance or failure
    // after observing the PID is a race, even if a later API returns ESRCH.
    let rest = || {
        let first_executable = executable()?;
        let path = path()?;
        if first_executable != executable()? || before != basic()? {
            return Err(ReadFailure::Unknown);
        }
        Ok(Identity {
            start_seconds: before.start_seconds,
            start_microseconds: before.start_microseconds,
            start_abstime: first_executable.start_abstime,
            effective_uid: before.effective_uid,
            executable_path: path,
            executable_uuid: first_executable.uuid,
        })
    };
    rest().map_err(|_| ReadFailure::Unknown)
}

fn basic(pid: i32) -> Result<Basic, ReadFailure> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // Clear errno so a zero/short response cannot reuse an unrelated ESRCH.
    unsafe { *libc::__error() = 0 };
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    let errno = std::io::Error::last_os_error().raw_os_error();
    check_bsd_count(count, errno)?;
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid as u32
        || !(2..=4).contains(&info.pbi_status)
        || info.pbi_start_tvsec == 0
        || info.pbi_start_tvusec >= 1_000_000
    {
        return Err(ReadFailure::Unknown);
    }
    Ok(Basic {
        start_seconds: info.pbi_start_tvsec,
        start_microseconds: info.pbi_start_tvusec as u32,
        effective_uid: info.pbi_uid,
    })
}

fn check_bsd_count(count: i32, errno: Option<i32>) -> Result<(), ReadFailure> {
    if count == size_of::<libc::proc_bsdinfo>() as i32 {
        Ok(())
    } else if count == 0 && errno == Some(libc::ESRCH) {
        Err(ReadFailure::Absent)
    } else {
        Err(ReadFailure::Unknown)
    }
}

fn executable(pid: i32) -> Result<Executable, ReadFailure> {
    let mut info = MaybeUninit::<libc::rusage_info_v0>::zeroed();
    // Despite the rusage_info_t* declaration, libproc expects the struct's
    // storage itself, not a pointer to a separately allocated buffer pointer.
    let result =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, info.as_mut_ptr().cast()) };
    if result != 0 {
        return Err(ReadFailure::Unknown);
    }
    let info = unsafe { info.assume_init() };
    if info.ri_uuid == [0; 16] || info.ri_proc_start_abstime == 0 || info.ri_proc_exit_abstime != 0
    {
        return Err(ReadFailure::Unknown);
    }
    Ok(Executable {
        uuid: info.ri_uuid,
        start_abstime: info.ri_proc_start_abstime,
    })
}

fn path(pid: i32) -> Result<String, ReadFailure> {
    let mut bytes = [0_u8; MAX_PATH_BYTES];
    let count = unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    decode_path(&bytes, count).ok_or(ReadFailure::Unknown)
}

fn decode_path(bytes: &[u8], count: i32) -> Option<String> {
    let count = usize::try_from(count).ok()?;
    if count == 0 || count >= bytes.len() || bytes[count] != 0 {
        return None;
    }
    let path = std::str::from_utf8(&bytes[..count]).ok()?;
    valid_path(path).then(|| path.to_owned())
}

#[cfg(test)]
mod tests;
