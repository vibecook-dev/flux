//! The Win32 calls flux-server makes on Windows, kept in one place. Each
//! `unsafe` block is one FFI call (or a read of what it returned), on
//! handles and buffers this module owns.

use std::ffi::OsString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::PathBuf;

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
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, SetThreadPriority, TerminateProcess, THREAD_MODE_BACKGROUND_BEGIN,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

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
    // Closed by Windows when the process ends, which ends the job.
    std::mem::forget(job);
    Ok(())
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
