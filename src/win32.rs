use std::env;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows::{
    core::{w, PCWSTR},
    Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT},
};

const HKEY_CURRENT_USER: isize = 0x80000001u32 as i32 as isize;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const REG_SZ: u32 = 1;
const SW_SHOWNORMAL: i32 = 1;
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;
const MAX_REG_VALUE_BYTES: u32 = 65_536;

pub const WM_TIMER_MSG: u32 = 0x0113;
pub const WM_APP_SYNC: u32 = 0x8001;

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VAL_NAME: PCWSTR = w!("SMTC-Now-Playing");

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        hkey: isize,
        lpsubkey: *const u16,
        lpvalue: *const u16,
        dwflags: u32,
        pdwtype: *mut u32,
        pvdata: *mut c_void,
        pcbdata: *mut u32,
    ) -> i32;

    fn RegSetKeyValueW(
        hkey: isize,
        lpsubkey: *const u16,
        lpvaluename: *const u16,
        dwtype: u32,
        lpdata: *const c_void,
        cbdata: u32,
    ) -> i32;

    fn RegDeleteKeyValueW(hkey: isize, lpsubkey: *const u16, lpvaluename: *const u16) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        lpoperation: *const u16,
        lpfile: *const u16,
        lpparameters: *const u16,
        lpdirectory: *const u16,
        nshowcmd: i32,
    ) -> isize;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(hwndnewowner: isize) -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(uformat: u32, hmem: *mut c_void) -> *mut c_void;
    fn CloseClipboard() -> i32;
    fn PostThreadMessageW(idthread: u32, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn SetTimer(
        hwnd: isize,
        nidevent: usize,
        uelapse: u32,
        lptimerfunc: *const c_void,
    ) -> usize;
    fn KillTimer(hwnd: isize, uidevent: usize) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GlobalAlloc(uflags: u32, dwbytes: usize) -> *mut c_void;
    fn GlobalLock(hmem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(hmem: *mut c_void) -> i32;
    fn GlobalFree(hmem: *mut c_void) -> *mut c_void;
}

/// RAII guard that initializes COM on the current thread and only calls
/// `CoUninitialize` if `CoInitializeEx` succeeded.
pub struct ComGuard(bool);

impl ComGuard {
    pub fn init(coinit: COINIT) -> Self {
        // SAFETY: Valid Win32 COM initialization for the calling thread.
        let ok = unsafe { CoInitializeEx(None, coinit) }.is_ok();
        Self(ok)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: Paired strictly with a successful `CoInitializeEx` on the same thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
}

pub fn current_thread_id() -> u32 {
    // SAFETY: `GetCurrentThreadId` has no preconditions.
    unsafe { GetCurrentThreadId() }
}

pub fn post_sync_message(thread_id: u32) {
    // SAFETY: Posting a custom `WM_APP_SYNC` message to the UI thread is thread-safe.
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_APP_SYNC, 0, 0);
    }
}

pub fn set_thread_timer(existing_id: usize, delay_ms: u32) -> usize {
    // SAFETY: Creates or updates a thread-owned Win32 timer (`hwnd == 0`, no callback).
    unsafe { SetTimer(0, existing_id, delay_ms, ptr::null()) }
}

pub fn kill_thread_timer(timer_id: usize) {
    if timer_id != 0 {
        // SAFETY: Cancels a thread-owned timer previously returned by `SetTimer`.
        unsafe {
            let _ = KillTimer(0, timer_id);
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Builds the quoted executable path directly in UTF-16 (lossless for non-UTF-8 OS paths).
fn expected_startup_wide_null() -> Option<Vec<u16>> {
    let exe = env::current_exe().ok()?;
    let mut wide = Vec::with_capacity(exe.as_os_str().len() + 3);
    wide.push(b'"' as u16);
    wide.extend(exe.as_os_str().encode_wide());
    wide.push(b'"' as u16);
    wide.push(0);
    Some(wide)
}

fn wide_eq_ignore_ascii_case(a: &[u16], b: &[u16]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(&x, &y)| {
            if x == y {
                true
            } else if x < 128 && y < 128 {
                (x as u8).eq_ignore_ascii_case(&(y as u8))
            } else {
                false
            }
        })
}

/// Checks whether the Windows Registry `Run` key points to our current `.exe` path.
pub fn is_startup_enabled() -> bool {
    let Some(expected) = expected_startup_wide_null() else {
        return false;
    };
    let expected_no_nul = &expected[..expected.len() - 1];
    let mut size: u32 = 0;

    // SAFETY: `RUN_SUBKEY` and `RUN_VAL_NAME` are static null-terminated UTF-16 strings;
    // `pvdata` is null to query the required byte length.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY.as_ptr(),
            RUN_VAL_NAME.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut size,
        )
    };
    if status != 0 || size < 4 || size > MAX_REG_VALUE_BYTES {
        return false;
    }

    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    // SAFETY: `buf` has capacity for at least `size` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY.as_ptr(),
            RUN_VAL_NAME.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 {
        return false;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    wide_eq_ignore_ascii_case(&buf[..len], expected_no_nul)
}

/// Adds or removes the app from the current user's Windows Startup registry key.
pub fn set_startup_enabled(enable: bool) -> bool {
    if enable {
        let Some(wide) = expected_startup_wide_null() else {
            return false;
        };
        let Ok(byte_len) = u32::try_from(wide.len() * std::mem::size_of::<u16>()) else {
            return false;
        };
        // SAFETY: `wide` is null-terminated UTF-16 with exact byte length `byte_len`.
        unsafe {
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY.as_ptr(),
                RUN_VAL_NAME.as_ptr(),
                REG_SZ,
                wide.as_ptr().cast(),
                byte_len,
            );
        }
    } else {
        // SAFETY: Static null-terminated registry paths.
        unsafe {
            let _ = RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY.as_ptr(),
                RUN_VAL_NAME.as_ptr(),
            );
        }
    }
    is_startup_enabled()
}

/// Copies UTF-8 text to the Windows clipboard via native Win32 APIs (replaces `arboard`).
pub fn set_clipboard_text(text: &str) -> bool {
    let wide = to_wide_null(text);
    let byte_len = wide.len() * std::mem::size_of::<u16>();

    // SAFETY: Standard Win32 clipboard ownership sequence; `hmem` is freed on failure and
    // transferred to Windows on successful `SetClipboardData`.
    unsafe {
        if OpenClipboard(0) == 0 {
            return false;
        }
        let ok = (|| {
            if EmptyClipboard() == 0 {
                return false;
            }
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len);
            if hmem.is_null() {
                return false;
            }
            let ptr = GlobalLock(hmem).cast::<u16>();
            if ptr.is_null() {
                let _ = GlobalFree(hmem);
                return false;
            }
            ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hmem);

            if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
                let _ = GlobalFree(hmem);
                return false;
            }
            true
        })();
        let _ = CloseClipboard();
        ok
    }
}

/// Opens an HTTP/HTTPS URL in the user's default web browser.
pub fn open_url(url: &str) {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return;
    }
    let wide_url = to_wide_null(url);
    // SAFETY: Null-terminated UTF-16 verb and HTTP(S) URL.
    unsafe {
        ShellExecuteW(
            0,
            w!("open").as_ptr(),
            wide_url.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}