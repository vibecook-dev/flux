//! The Win32 calls flux-server makes on Windows, kept in one place. Each
//! `unsafe` block is one FFI call (or a read of what it returned), on
//! handles and buffers this module owns.

use std::ffi::OsString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_BUFFER_OVERFLOW, HANDLE, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_FRIENDLY_NAME,
    GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, SetThreadPriority, TerminateProcess, THREAD_MODE_BACKGROUND_BEGIN,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

/// Leaves `path` readable and writable by its owner (the account flux runs
/// as), SYSTEM and Administrators only, and by nobody through inheritance:
/// a data dir under `C:\ProgramData` otherwise lets every local user read
/// its files. For the files that hold keys (`devices`).
pub fn restrict_to_owner(path: &std::path::Path) -> io::Result<()> {
    // Protected (no inherited entries): full control for the owner, SYSTEM
    // and the Administrators group.
    set_protected_dacl(path, "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)")
}

/// Leaves the folder `dir`, and what is made in it from now on, to SYSTEM
/// and the Administrators group: for what flux downloads and runs as an
/// administrator (the Windows installer of an update). Other users can't
/// add or change anything in it.
pub fn restrict_to_admins(dir: &std::path::Path) -> io::Result<()> {
    set_protected_dacl(dir, "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
}

/// Sets `path`'s access list from `sddl` (its `D:` part), protected from
/// inheriting its parent's.
#[allow(unsafe_code)]
fn set_protected_dacl(path: &std::path::Path, sddl: &str) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };
    let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let name: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated string; `sd` receives a LocalAlloc'd descriptor (freed below).
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `sd` is the descriptor made above; the DACL points into it.
    let got = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) };
    let result = if got == 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a NUL-terminated path, and the DACL from `sd`, alive until LocalFree.
        let err = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if err == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err as i32))
        }
    };
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(sd) };
    result
}

/// A job object that ends every process in it when its last handle closes.
pub struct KillOnCloseJob(HANDLE);

impl KillOnCloseJob {
    #[allow(unsafe_code)]
    pub fn new() -> io::Result<KillOnCloseJob> {
        // SAFETY: no attributes (the handle is not inherited) and no name.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = KillOnCloseJob(handle);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is the struct this information class takes, with
        // its size; the call only reads it.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Adds `process` (a process handle with PROCESS_SET_QUOTA and
    /// PROCESS_TERMINATE access, e.g. a `Child`'s) to the job. Processes it
    /// starts later are in the job too.
    #[allow(unsafe_code)]
    pub fn assign(&self, process: BorrowedHandle<'_>) -> io::Result<()> {
        // SAFETY: both handles are valid for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.0, process.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for KillOnCloseJob {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The job [`end_helpers_with_this_process`] put this process in (its
/// handle, as an integer: raw handles aren't `Send`).
static HELPERS_JOB: OnceLock<usize> = OnceLock::new();

/// Puts this process in a [`KillOnCloseJob`] whose handle stays open until
/// the process ends: then Windows ends every helper flux started (ffmpeg,
/// ffprobe), however flux ended (a crash, `taskkill /F`, the console
/// closing), instead of leaving them running without it.
#[allow(unsafe_code)]
pub fn end_helpers_with_this_process() -> io::Result<()> {
    let job = KillOnCloseJob::new()?;
    // SAFETY: GetCurrentProcess returns a pseudo handle, valid for as long
    // as the process runs, that needs no closing.
    job.assign(unsafe { BorrowedHandle::borrow_raw(GetCurrentProcess()) })?;
    let _ = HELPERS_JOB.set(job.0 as usize);
    // Closed by Windows when the process ends, which ends the job.
    std::mem::forget(job);
    Ok(())
}

/// Lets the processes flux starts from now on outlive it: clears
/// kill-on-close on the job [`end_helpers_with_this_process`] made. For the
/// flux that a restart starts just before this one exits (it makes a job of
/// its own for its helpers).
#[allow(unsafe_code)]
pub fn let_children_outlive_this_process() -> io::Result<()> {
    let Some(&job) = HELPERS_JOB.get() else {
        return Ok(());
    };
    // No limits at all: in particular no JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
    let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: the job handle stays open until the process ends; `info` is
    // the struct this information class takes, with its size.
    let ok = unsafe {
        SetInformationJobObject(
            job as HANDLE,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Lets the processes flux starts from now on leave its job
/// (`CREATE_BREAKAWAY_FROM_JOB`), keeping kill-on-close for the rest: for
/// the runner of a Windows Installer update, which must outlive the service
/// it has stopped, while ffmpeg must not.
#[allow(unsafe_code)]
pub fn let_children_break_away() -> io::Result<()> {
    use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let Some(&job) = HELPERS_JOB.get() else {
        return Ok(());
    };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    // SAFETY: the job handle stays open until the process ends; `info` is
    // the struct this information class takes, with its size.
    let ok = unsafe {
        SetInformationJobObject(
            job as HANDLE,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether this process acts as an administrator: elevated, or a service
/// account such as LocalSystem (Windows Installer needs one).
#[allow(unsafe_code)]
pub fn is_admin() -> bool {
    use windows_sys::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, WinBuiltinAdministratorsSid, SECURITY_MAX_SID_SIZE,
    };
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = SECURITY_MAX_SID_SIZE;
    // SAFETY: `sid` is a buffer of `size` bytes, the most a SID takes.
    let made = unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if made == 0 {
        return false;
    }
    let mut member = 0;
    // SAFETY: no token (this thread's or process's), the SID made above,
    // and a valid out pointer.
    let ok = unsafe { CheckTokenMembership(std::ptr::null_mut(), sid.as_mut_ptr().cast(), &mut member) };
    ok != 0 && member != 0
}

/// Waits up to `timeout` for process `pid` to end. True when it has (or
/// can't be opened: it's gone, or was never there).
#[allow(unsafe_code)]
pub fn wait_for_exit(pid: u32, timeout: std::time::Duration) -> bool {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: a plain call; a null handle means it couldn't be opened.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return true;
    }
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: the handle opened above, closed right after.
    let ended = unsafe { WaitForSingleObject(handle, ms) } == WAIT_OBJECT_0;
    // SAFETY: we own the handle and close it once.
    unsafe {
        CloseHandle(handle);
    }
    ended
}

/// This process runs in session 0, where services run (and service wrappers
/// such as NSSM), not in a person's session.
#[allow(unsafe_code)]
pub fn in_service_session() -> bool {
    let mut session = u32::MAX;
    // SAFETY: `session` is a valid out pointer for the call.
    let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
    ok != 0 && session == 0
}

/// Makes Ctrl-C reach this process. A process started in a new process
/// group (`start /b`, some service wrappers and ssh sessions) inherits
/// "ignore Ctrl-C", and then the Ctrl-C a service manager such as NSSM or
/// WinSW sends to stop flux would never arrive.
#[allow(unsafe_code)]
pub fn accept_ctrl_c() -> io::Result<()> {
    // SAFETY: with no handler routine, FALSE only clears the process's
    // "ignore Ctrl-C" flag.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Ends this process at once with `code`: no exit handlers, no waiting for
/// other threads (`TerminateProcess` on itself).
#[allow(unsafe_code)]
pub fn terminate_this_process(code: u32) -> ! {
    // SAFETY: terminating the current process through its pseudo handle;
    // the call does not return when it succeeds.
    unsafe {
        TerminateProcess(GetCurrentProcess(), code);
    }
    std::process::exit(code as i32)
}

/// Puts the calling thread in background mode: lower CPU, I/O and memory
/// priority, so its work yields to playback and other programs.
#[allow(unsafe_code)]
pub fn background_this_thread() -> io::Result<()> {
    // SAFETY: changes only the calling thread, through its pseudo handle.
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The user's local application data folder (`%LOCALAPPDATA%`, usually
/// `C:\Users\<name>\AppData\Local`), from the known-folder API: it does not
/// depend on environment variables such as `HOME`.
#[allow(unsafe_code)]
pub fn local_app_data() -> io::Result<PathBuf> {
    let mut path: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: on return `path` holds a NUL-terminated string allocated by
    // the shell (or null), which we copy and then free once, as the API
    // requires whether or not the call succeeded.
    unsafe {
        let hr = SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut path);
        let result = if hr >= 0 && !path.is_null() {
            let len = (0..).take_while(|&i| *path.add(i) != 0).count();
            let wide = std::slice::from_raw_parts(path, len);
            Ok(PathBuf::from(OsString::from_wide(wide)))
        } else {
            Err(hresult_error(hr))
        };
        CoTaskMemFree(path.cast_const().cast());
        result
    }
}

/// An `io::Error` for a failed HRESULT: its message, and the HRESULT in hex
/// as Microsoft documents it ("The system cannot find the path specified.
/// (HRESULT 0x80070003)"). As a raw OS error it printed as a negative
/// decimal ("os error -2147024893"). A Win32 error wrapped in an HRESULT
/// (facility 7, `0x8007xxxx`) also gets that error's kind (`NotFound`).
pub fn hresult_error(hr: i32) -> io::Error {
    let code = hr as u32;
    let os = if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::from_raw_os_error(hr)
    };
    let text = os.to_string();
    // Rust appends " (os error N)" to the system's message.
    let message = text.rsplit_once(" (os error ").map_or(text.as_str(), |(m, _)| m);
    io::Error::new(os.kind(), format!("{message} (HRESULT 0x{code:08X})"))
}

/// The unicast addresses of this machine's network adapters, as
/// `GetAdaptersAddresses` lists them (including loopback and adapters that
/// are down, like `getifaddrs` on Unix).
#[allow(unsafe_code)]
pub fn interface_ips() -> Vec<IpAddr> {
    let flags = GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_SKIP_FRIENDLY_NAME;
    // 16 KiB first, as Microsoft suggests; u64s align the buffer for the
    // structs it receives.
    let mut buf: Vec<u64> = vec![0; 2048];
    for _ in 0..4 {
        let mut len = (buf.len() * 8) as u32;
        // SAFETY: `buf` is writable for `len` bytes and suitably aligned.
        let rc = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        match rc {
            NO_ERROR => {}
            ERROR_BUFFER_OVERFLOW => {
                buf = vec![0; (len as usize).div_ceil(8) + 1];
                continue;
            }
            // ERROR_NO_DATA (no adapters), or the call failed.
            _ => return Vec::new(),
        }
        let mut out = Vec::new();
        let mut adapter = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: the call filled `buf` with a linked list of adapters, each
        // with a linked list of unicast addresses, all inside `buf`, which
        // outlives this loop; every pointer is checked for null and each
        // sockaddr is read as the type its family says it is.
        unsafe {
            while !adapter.is_null() {
                let mut unicast = (*adapter).FirstUnicastAddress;
                while !unicast.is_null() {
                    let sa = (*unicast).Address.lpSockaddr;
                    if !sa.is_null() {
                        match (*sa).sa_family {
                            AF_INET => {
                                let sin = &*sa.cast::<SOCKADDR_IN>();
                                out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr))));
                            }
                            AF_INET6 => {
                                let sin6 = &*sa.cast::<SOCKADDR_IN6>();
                                out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.u.Byte)));
                            }
                            _ => {}
                        }
                    }
                    unicast = (*unicast).Next;
                }
                adapter = (*adapter).Next;
            }
        }
        out.sort();
        out.dedup();
        return out;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsHandle;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::{GetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS};

    use super::*;

    /// A helper that runs for about `seconds` and prints nothing.
    fn sleeper(seconds: u32, background: bool) -> Child {
        let mut cmd = if background {
            flux_core::tool::background_command("ping")
        } else {
            flux_core::tool::command("ping")
        };
        cmd.args(["-n", &seconds.to_string(), "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.spawn().expect("ping runs")
    }

    fn exits_within(child: &mut Child, limit: Duration) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn closing_the_job_ends_its_processes() {
        let mut child = sleeper(30, false);
        let job = KillOnCloseJob::new().unwrap();
        job.assign(child.as_handle()).unwrap();
        assert!(
            !exits_within(&mut child, Duration::from_millis(300)),
            "still running"
        );
        drop(job);
        let ended = exits_within(&mut child, Duration::from_secs(5));
        if !ended {
            let _ = child.kill();
        }
        assert!(ended, "the job's close ended it");
    }

    #[test]
    #[allow(unsafe_code)]
    fn background_helpers_run_below_normal() {
        let mut child = sleeper(3, true);
        // SAFETY: the child's handle is valid until it is waited for.
        let class = unsafe { GetPriorityClass(child.as_raw_handle()) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(class, BELOW_NORMAL_PRIORITY_CLASS);
    }

    #[test]
    fn local_app_data_is_an_absolute_folder() {
        let dir = local_app_data().unwrap();
        assert!(dir.is_absolute() && dir.is_dir(), "{}", dir.display());
    }

    #[test]
    fn ctrl_c_can_be_turned_back_on() {
        accept_ctrl_c().unwrap();
    }

    /// The known-folder error printed "(os error -2147024893)".
    #[test]
    fn hresults_read_as_a_message_and_hex() {
        let e = hresult_error(0x8007_0003_u32 as i32);
        let text = e.to_string();
        assert!(text.ends_with("(HRESULT 0x80070003)"), "{text}");
        assert!(
            !text.contains("-2147024893") && !text.contains("os error"),
            "{text}"
        );
        assert!(
            text.len() > "(HRESULT 0x80070003)".len() + 5,
            "a message too: {text}"
        );
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        // Not a Win32 error: still hex, never negative.
        let text = hresult_error(0x8000_4005_u32 as i32).to_string();
        assert!(
            text.ends_with("(HRESULT 0x80004005)") && !text.contains("-2147"),
            "{text}"
        );
    }
}
