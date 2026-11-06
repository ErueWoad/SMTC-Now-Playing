use std::env;
use std::ffi::c_void;

use windows::core::{w, PCWSTR};

const HKEY_CURRENT_USER: isize = 0x80000001u32 as i32 as isize;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const REG_SZ: u32 = 1;
const SW_SHOWNORMAL: i32 = 1;
const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VAL_NAME: PCWSTR = w!("SMTC-Now-Playing");

#[link(name = "advapi32")]
extern "system" {
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

    fn RegDeleteKeyValueW(
        hkey: isize,
        lpsubkey: *const u16,
        lpvaluename: *const u16,
    ) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        lpoperation: *const u16,
        lpfile: *const u16,
        lpparameters: *const u16,
        lpdirectory: *const u16,
        nshowcmd: i32,
    ) -> isize;
}

fn expected_startup_command() -> Option<String> {
    let exe = env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

pub fn is_startup_enabled() -> bool {
    let Some(expected) = expected_startup_command() else {
        return false;
    };
    let mut size: u32 = 0;
    unsafe {
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY.as_ptr(),
            RUN_VAL_NAME.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        if status != 0 || size < 4 {
            return false;
        }
        let mut buf = vec![0u16; (size as usize) / 2];
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY.as_ptr(),
            RUN_VAL_NAME.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        );
        if status != 0 {
            return false;
        }
        if let Some(nul) = buf.iter().position(|&c| c == 0) {
            buf.truncate(nul);
        }
        let actual = String::from_utf16_lossy(&buf);
        actual.eq_ignore_ascii_case(&expected)
    }
}

pub fn set_startup_enabled(enable: bool) -> bool {
    if enable {
        let Some(quoted) = expected_startup_command() else {
            return false;
        };
        let wide: Vec<u16> = quoted.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let status = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY.as_ptr(),
                RUN_VAL_NAME.as_ptr(),
                REG_SZ,
                wide.as_ptr().cast(),
                (wide.len() * std::mem::size_of::<u16>()) as u32,
            );
            status == 0
        }
    } else {
        unsafe {
            let _ = RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY.as_ptr(),
                RUN_VAL_NAME.as_ptr(),
            );
        }
        is_startup_enabled()
    }
}

pub fn open_url(url: &str) {
    let wide_url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            0,
            w!("open").as_ptr(),
            wide_url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}