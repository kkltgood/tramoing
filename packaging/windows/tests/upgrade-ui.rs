//! Disposable GUI for the installer's graceful-close and timeout regressions.
#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::sync::OnceLock;

type Hwnd = *mut c_void;

#[repr(C)]
struct WindowClass {
    style: u32,
    procedure: unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize,
    class_extra: i32,
    window_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
#[derive(Default)]
struct Message {
    window: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    x: i32,
    y: i32,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(class: *const WindowClass) -> u16;
    fn CreateWindowExW(
        extended_style: u32,
        class_name: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: *mut c_void,
        instance: *mut c_void,
        parameter: *mut c_void,
    ) -> Hwnd;
    fn DefWindowProcW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    fn DestroyWindow(window: Hwnd) -> i32;
    fn GetMessageW(message: *mut Message, window: Hwnd, min: u32, max: u32) -> i32;
    fn TranslateMessage(message: *const Message) -> i32;
    fn DispatchMessageW(message: *const Message) -> isize;
    fn PostQuitMessage(exit_code: i32);
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

static CONFIG: OnceLock<(std::path::PathBuf, bool)> = OnceLock::new();

unsafe extern "system" fn window_proc(
    window: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match message {
        0x0010 => {
            let (closed, ignore_close) = CONFIG.get().unwrap();
            if !ignore_close {
                // Simulate removing a tray icon and saving the last history
                // batch. The installer must wait for completion before forcing exit.
                std::thread::sleep(std::time::Duration::from_millis(300));
                std::fs::write(closed, "clean shutdown").unwrap();
                DestroyWindow(window);
            }
            0
        }
        0x0002 => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    CONFIG
        .set((args[2].clone().into(), args[3] == "ignore-close"))
        .unwrap();
    let class_name: Vec<u16> = "TramoingUpgradeRegression\0".encode_utf16().collect();
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = WindowClass {
            style: 0,
            procedure: window_proc,
            class_extra: 0,
            window_extra: 0,
            instance,
            icon: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            background: std::ptr::null_mut(),
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
        };
        assert_ne!(RegisterClassW(&class), 0);
        // Match the app: a visible window parked offscreen, without a taskbar
        // button. There must still be a normal WM_CLOSE path while hidden.
        let window = CreateWindowExW(
            0x00000080,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0x10000000,
            -32000,
            -32000,
            100,
            100,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        assert!(!window.is_null());
        std::fs::write(&args[1], std::process::id().to_string()).unwrap();
        let mut message = Message::default();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
