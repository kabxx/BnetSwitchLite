use std::sync::Mutex;

use tauri::{Runtime, WebviewWindow};
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        WindowsAndMessaging::{
            DestroyIcon, GCLP_HICON, GCLP_HICONSM, HICON, ICON_BIG, ICON_SMALL, ICON_SMALL2,
            IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXSMICON, SetClassLongPtrW, WM_SETICON,
        },
    },
};
use windows::core::PCWSTR;

const APPLICATION_ICON_RESOURCE_ID: u16 = 32_512;
const BASE_DPI: u32 = 96;
const TASKBAR_ICON_LOGICAL_SIZE: u32 = 24;

struct WindowIcons {
    small: HICON,
    taskbar: HICON,
    dpi: u32,
}

/// Keep both owned icon handles alive for as long as the window uses them.
pub struct WindowIconManager(Mutex<WindowIcons>);

// HICON is an opaque kernel/user handle. The handle is only used by the window
// thread and is kept in managed state solely to tie its lifetime to the app.
unsafe impl Send for WindowIcons {}

impl Drop for WindowIcons {
    fn drop(&mut self) {
        let _ = unsafe { DestroyIcon(self.small) };
        let _ = unsafe { DestroyIcon(self.taskbar) };
    }
}

impl WindowIconManager {
    pub fn refresh<R: Runtime>(&self, window: &WebviewWindow<R>) -> tauri::Result<()> {
        let hwnd = window.hwnd()?;
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let mut current = self.0.lock().map_err(|_| {
            tauri::Error::Anyhow(std::io::Error::other("window icon lock poisoned").into())
        })?;

        if dpi == current.dpi {
            return Ok(());
        }

        let replacement = load_icons(dpi)?;
        set_icons(hwnd, &replacement);
        *current = replacement;
        Ok(())
    }
}

pub fn install<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<WindowIconManager> {
    let hwnd = window.hwnd()?;
    let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
    let icons = load_icons(unsafe { GetDpiForWindow(hwnd) })?;
    set_icons(hwnd, &icons);

    Ok(WindowIconManager(Mutex::new(icons)))
}

fn load_icons(dpi: u32) -> tauri::Result<WindowIcons> {
    let instance = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_err(|error| tauri::Error::Anyhow(error.into()))?;
    let small_size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    let taskbar_size = ((TASKBAR_ICON_LOGICAL_SIZE * dpi + BASE_DPI / 2) / BASE_DPI) as i32;

    let load = |size| unsafe {
        LoadImageW(
            Some(instance.into()),
            PCWSTR(APPLICATION_ICON_RESOURCE_ID as usize as *const u16),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
        .map(|handle| HICON(handle.0))
        .map_err(|error| tauri::Error::Anyhow(error.into()))
    };

    let small = load(small_size)?;
    let taskbar = match load(taskbar_size) {
        Ok(icon) => icon,
        Err(error) => {
            let _ = unsafe { DestroyIcon(small) };
            return Err(error);
        }
    };

    Ok(WindowIcons {
        small,
        taskbar,
        dpi,
    })
}

fn set_icons(hwnd: windows::Win32::Foundation::HWND, icons: &WindowIcons) {
    unsafe {
        // Install every window and class fallback before the initially hidden
        // window is shown. This prevents the taskbar from painting Tauri's
        // single-frame default icon and replacing it after WebView startup.
        SetClassLongPtrW(hwnd, GCLP_HICONSM, icons.small.0 as isize);
        SetClassLongPtrW(hwnd, GCLP_HICON, icons.taskbar.0 as isize);
        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_SMALL as usize)),
            Some(LPARAM(icons.small.0 as isize)),
        );
        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_SMALL2 as usize)),
            Some(LPARAM(icons.small.0 as isize)),
        );
        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_BIG as usize)),
            Some(LPARAM(icons.taskbar.0 as isize)),
        );
    }
}
