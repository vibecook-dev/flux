# Starts a console program in a hidden console of its own, or stops one the
# way a service wrapper such as NSSM or WinSW does: attach to its console and
# send Ctrl-C or Ctrl-Break. smoke-test.sh uses it to check that a Windows
# package stops gracefully. Windows PowerShell 5.1 or PowerShell 7.
#
#   windows-console.ps1 start <log> <exe> [args...]
#       Starts <exe> with stdout and stderr in <log> (a new file) and prints
#       its process id. It inherits no other handle, so a caller reading this
#       script's output through a pipe isn't held open by the program.
#   windows-console.ps1 stop <pid> c|break [seconds]
#       Sends Ctrl-C or Ctrl-Break to the console of <pid>, waits up to
#       [seconds] (default 15) for it to exit and prints its exit code.
#       Exits 1 if it is still running then.
#   windows-console.ps1 kill <pid>
#       Ends <pid> at once (TerminateProcess), if it still runs.
#
# Keep this file ASCII: Windows PowerShell reads a script without a BOM in
# the system's ANSI code page.
param(
    [Parameter(Mandatory = $true, Position = 0)][string]$Action,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Rest
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;

public static class FluxConsole {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFO {
        public int cb;
        public string lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }
    [StructLayout(LayoutKind.Sequential)]
    public struct STARTUPINFOEX { public STARTUPINFO StartupInfo; public IntPtr lpAttributeList; }
    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }
    [StructLayout(LayoutKind.Sequential)]
    public struct SECURITY_ATTRIBUTES { public int nLength; public IntPtr lpSecurityDescriptor; public bool bInheritHandle; }

    public delegate bool HandlerRoutine(uint ctrlType);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessW(string app, StringBuilder cmdLine, IntPtr pa, IntPtr ta, bool inherit,
        uint flags, IntPtr env, string cwd, ref STARTUPINFOEX si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, ref SECURITY_ATTRIBUTES sa,
        uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value,
        IntPtr size, IntPtr previous, IntPtr returnSize);
    [DllImport("kernel32.dll")]
    static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AttachConsole(uint pid);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetConsoleCtrlHandler(HandlerRoutine handler, bool add);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint processGroupId);

    const uint GENERIC_WRITE = 0x40000000, FILE_SHARE_READ = 1, FILE_SHARE_WRITE = 2, FILE_SHARE_DELETE = 4;
    const uint CREATE_ALWAYS = 2, FILE_ATTRIBUTE_NORMAL = 0x80;
    const uint CREATE_NEW_CONSOLE = 0x10, EXTENDED_STARTUPINFO_PRESENT = 0x80000;
    const int STARTF_USESHOWWINDOW = 1, STARTF_USESTDHANDLES = 0x100;
    const int PROC_THREAD_ATTRIBUTE_HANDLE_LIST = 0x20002;

    // Kept in a field so the delegate outlives the native callback.
    static readonly HandlerRoutine Ignore = delegate(uint ctrlType) { return true; };

    // One argument as the C runtime's command-line parser reads it back.
    static string Quote(string arg) {
        if (arg.Length > 0 && arg.IndexOfAny(new[] { ' ', '\t', '"' }) < 0) return arg;
        var sb = new StringBuilder("\"");
        int slashes = 0;
        foreach (char c in arg) {
            if (c == '\\') { slashes++; continue; }
            sb.Append('\\', c == '"' ? slashes * 2 + 1 : slashes);
            slashes = 0;
            sb.Append(c);
        }
        sb.Append('\\', slashes * 2);
        return sb.Append('"').ToString();
    }

    public static int Start(string log, string exe, string[] args) {
        var cmd = new StringBuilder(Quote(exe));
        foreach (string a in args) cmd.Append(' ').Append(Quote(a));

        var sa = new SECURITY_ATTRIBUTES();
        sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
        sa.bInheritHandle = true;
        IntPtr file = CreateFileW(log, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
        if (file == new IntPtr(-1)) throw new Win32Exception(Marshal.GetLastWin32Error(), "opening " + log);

        // Inherit the log file and nothing else.
        IntPtr size = IntPtr.Zero;
        InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
        IntPtr list = Marshal.AllocHGlobal(size);
        IntPtr handles = Marshal.AllocHGlobal(IntPtr.Size);
        try {
            if (!InitializeProcThreadAttributeList(list, 1, 0, ref size))
                throw new Win32Exception(Marshal.GetLastWin32Error());
            try {
                Marshal.WriteIntPtr(handles, file);
                if (!UpdateProcThreadAttribute(list, 0, new IntPtr(PROC_THREAD_ATTRIBUTE_HANDLE_LIST), handles,
                        new IntPtr(IntPtr.Size), IntPtr.Zero, IntPtr.Zero))
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                var si = new STARTUPINFOEX();
                si.StartupInfo.cb = Marshal.SizeOf(typeof(STARTUPINFOEX));
                si.StartupInfo.dwFlags = STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW;
                si.StartupInfo.wShowWindow = 0; // SW_HIDE
                si.StartupInfo.hStdOutput = file;
                si.StartupInfo.hStdError = file;
                si.lpAttributeList = list;
                PROCESS_INFORMATION pi;
                if (!CreateProcessW(exe, cmd, IntPtr.Zero, IntPtr.Zero, true,
                        CREATE_NEW_CONSOLE | EXTENDED_STARTUPINFO_PRESENT, IntPtr.Zero, null, ref si, out pi))
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "starting " + exe);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return pi.dwProcessId;
            } finally {
                DeleteProcThreadAttributeList(list);
            }
        } finally {
            Marshal.FreeHGlobal(handles);
            Marshal.FreeHGlobal(list);
            CloseHandle(file);
        }
    }

    // The exit code, or null if the process still runs after `seconds`.
    public static int? Stop(int pid, uint ctrlEvent, int seconds) {
        using (Process p = Process.GetProcessById(pid)) {
            // Open the process now, so its exit code can be read after it exits.
            if (p.Handle == IntPtr.Zero) throw new InvalidOperationException("no handle for " + pid);
            FreeConsole();
            if (!AttachConsole((uint)pid)) throw new Win32Exception(Marshal.GetLastWin32Error(), "attaching to the console of " + pid);
            // The event reaches every process on that console, this one too.
            SetConsoleCtrlHandler(Ignore, true);
            bool sent = GenerateConsoleCtrlEvent(ctrlEvent, 0);
            int error = Marshal.GetLastWin32Error();
            bool exited = p.WaitForExit(seconds * 1000);
            FreeConsole();
            if (!sent) throw new Win32Exception(error, "sending the console event");
            if (!exited) return null;
            return p.ExitCode;
        }
    }
}
'@

switch ($Action) {
    'start' {
        if ($Rest.Count -lt 2) { throw 'usage: windows-console.ps1 start <log> <exe> [args...]' }
        $log = [IO.Path]::GetFullPath($Rest[0])
        $exe = [IO.Path]::GetFullPath($Rest[1])
        $argv = if ($Rest.Count -gt 2) { [string[]]$Rest[2..($Rest.Count - 1)] } else { [string[]]@() }
        [FluxConsole]::Start($log, $exe, $argv)
    }
    'stop' {
        if ($Rest.Count -lt 2) { throw 'usage: windows-console.ps1 stop <pid> c|break [seconds]' }
        $ctrlEvent = switch ($Rest[1]) { 'c' { 0 } 'break' { 1 } default { throw "unknown event '$($Rest[1])' (c or break)" } }
        $seconds = if ($Rest.Count -gt 2) { [int]$Rest[2] } else { 15 }
        $code = [FluxConsole]::Stop([int]$Rest[0], [uint32]$ctrlEvent, $seconds)
        if ($null -eq $code) {
            Write-Output "still running after $seconds s"
            exit 1
        }
        Write-Output $code
    }
    'kill' {
        if ($Rest.Count -lt 1) { throw 'usage: windows-console.ps1 kill <pid>' }
        Stop-Process -Id ([int]$Rest[0]) -Force -ErrorAction SilentlyContinue
    }
    default { throw "unknown action '$Action' (start, stop or kill)" }
}
