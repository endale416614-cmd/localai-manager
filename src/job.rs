//! Windows job object: every process an app starts lives in its job, so stopping the app (or quitting
//! the manager) kills the whole tree. Without it, `cmd /c start-llm.bat` would leave llama-server
//! orphaned and still holding VRAM.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

pub struct Job(HANDLE);

// The handle is only used through thread-safe Win32 calls.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub fn new() -> io::Result<Job> {
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Job(h);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    /// Spawns suspended, joins the job, then resumes, so no grandchild can start outside the job.
    pub fn spawn(&self, cmd: &mut Command) -> io::Result<Child> {
        cmd.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = cmd.spawn()?;
        unsafe {
            if AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) == 0 {
                let err = io::Error::last_os_error();
                let _ = child.kill();
                return Err(err);
            }
        }
        resume_threads(child.id());
        Ok(child)
    }

    pub fn kill(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// cmd → powershell (a grandchild, like start-llm.bat → llama-server) holds a port open;
    /// killing the job must close it.
    #[test]
    fn kill_takes_down_grandchildren() {
        const PORT: u16 = 47123;
        let job = Job::new().unwrap();
        let mut cmd = Command::new("cmd.exe");
        // raw_arg: cmd.exe does not understand Rust's \" argument escaping.
        cmd.raw_arg("/c powershell -NoProfile -Command \"$l=[Net.Sockets.TcpListener]::new('127.0.0.1',47123);$l.Start();Start-Sleep 60\"");
        let mut child = job.spawn(&mut cmd).unwrap();
        let t = Instant::now();
        while !crate::supervisor::probe(PORT, "tcp") {
            assert!(t.elapsed() < Duration::from_secs(20), "grandchild never listened");
            std::thread::sleep(Duration::from_millis(200));
        }
        job.kill();
        let _ = child.wait();
        let t = Instant::now();
        while crate::supervisor::probe(PORT, "tcp") {
            assert!(t.elapsed() < Duration::from_secs(5), "grandchild survived the job kill");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn resume_threads(pid: u32) {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return;
        }
        let mut te: THREADENTRY32 = std::mem::zeroed();
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snap, &mut te);
        while ok != 0 {
            if te.th32OwnerProcessID == pid {
                let t = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                if !t.is_null() {
                    ResumeThread(t);
                    CloseHandle(t);
                }
            }
            ok = Thread32Next(snap, &mut te);
        }
        CloseHandle(snap);
    }
}
