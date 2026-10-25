use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, GetRgnBox, GetWindowRgn, SetWindowRgn, HGDIOBJ,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetCursorPos, GetForegroundWindow, GetWindow, GetWindowRect, IsWindowVisible,
    SetWindowPos, GW_HWNDPREV, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOOWNERZORDER,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Closed,
    Minimal,
    Open,
    Anim,
}

#[derive(Clone, Copy)]
pub struct Notch {
    pub hwnd: isize,
    pub rect: RECT,
    pub kind: Kind,
    pub scale: f64,
    /// Width of the visible notch body in logical px (only meaningful when Open).
    pub body_w: f64,
    /// Bottom of the notch's hit region in logical px (only meaningful when Open).
    pub bottom: f64,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn find() -> Option<Notch> {
    unsafe {
        let class = wide("NotchWindow");
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if hwnd.is_null() || IsWindowVisible(hwnd) == 0 {
            return None;
        }
        let mut rect: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut rect);
        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 };

        let rgn = CreateRectRgn(0, 0, 0, 0);
        let ty = GetWindowRgn(hwnd, rgn);
        let mut b: RECT = std::mem::zeroed();
        let has = ty > 1 && GetRgnBox(rgn, &mut b) > 1;
        DeleteObject(rgn as HGDIOBJ);

        let bottom = if has { (b.bottom as f64 / scale).clamp(150.0, 230.0) } else { 186.0 };
        let (kind, body_w) = if !has {
            (Kind::Anim, 420.0)
        } else {
            let w = (b.right - b.left) as f64 / scale;
            let h = (b.bottom - b.top) as f64 / scale;
            if h <= 20.0 {
                (Kind::Minimal, 0.0)
            } else if w >= 600.0 {
                (Kind::Anim, 420.0)
            } else if h >= 100.0 {
                (Kind::Open, (w - 92.0).clamp(300.0, 560.0))
            } else {
                (Kind::Closed, 0.0)
            }
        };
        Some(Notch { hwnd: hwnd as isize, rect, kind, scale, body_w, bottom })
    }
}

pub fn cursor() -> (i32, i32) {
    unsafe {
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        (p.x, p.y)
    }
}

pub fn foreground() -> isize {
    unsafe { GetForegroundWindow() as isize }
}

pub fn mouse_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    unsafe {
        ((GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0)
            || ((GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0)
    }
}

pub fn activate(hwnd: isize) {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetWindowLongPtrW, GetWindowThreadProcessId,
        SetForegroundWindow, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };
    unsafe {
        let h = hwnd as HWND;
        // Strip WS_EX_NOACTIVATE if present so Windows allows keyboard focus
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        if (ex & (WS_EX_NOACTIVATE as isize)) != 0 {
            SetWindowLongPtrW(h, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE as isize));
        }
        // Dummy VK_NONAME (0xFC) input unlocks SetForegroundWindow for this thread
        let inp = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT { wVk: 0xFC, wScan: 0, dwFlags: 0, time: 0, dwExtraInfo: 0 },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0xFC,
                        wScan: 0,
                        dwFlags: KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
        ];
        let _ = SendInput(2, inp.as_ptr(), std::mem::size_of::<INPUT>() as i32);

        let fg = GetForegroundWindow();
        let cur_tid = GetCurrentThreadId();
        let fg_tid = if !fg.is_null() { GetWindowThreadProcessId(fg, std::ptr::null_mut()) } else { 0 };
        let attached = if fg_tid != 0 && fg_tid != cur_tid {
            AttachThreadInput(fg_tid, cur_tid, 1) != 0
        } else {
            false
        };
        let _ = BringWindowToTop(h);
        let _ = SetForegroundWindow(h);
        let _ = SetFocus(h);
        if attached {
            let _ = AttachThreadInput(fg_tid, cur_tid, 0);
        }
    }
}

/// Make sure our window sits directly underneath the notch in z-order.
pub fn keep_below(ours: isize, notch: isize) {
    unsafe {
        let ours_h = ours as HWND;
        let notch_h = notch as HWND;
        let mut w = GetWindow(notch_h, GW_HWNDPREV);
        let mut above = false;
        let mut guard = 0;
        while !w.is_null() && guard < 4096 {
            if w == ours_h {
                above = true;
                break;
            }
            w = GetWindow(w, GW_HWNDPREV);
            guard += 1;
        }
        if above {
            SetWindowPos(
                ours_h,
                notch_h,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }
}

/// Put our window above the notch (used once the panel is fully open, so Nusa's
/// invisible hit region doesn't swallow clicks on our card).
pub fn keep_above(ours: isize, notch: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOPMOST;
    unsafe {
        let ours_h = ours as HWND;
        let notch_h = notch as HWND;
        // Walk upward from our window; if we meet the notch, it is above us.
        let mut w = GetWindow(ours_h, GW_HWNDPREV);
        let mut guard = 0;
        while !w.is_null() && guard < 4096 {
            if w == notch_h {
                SetWindowPos(ours_h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
                return;
            }
            w = GetWindow(w, GW_HWNDPREV);
            guard += 1;
        }
    }
}

pub fn prepare_window(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, ShowWindow, GWL_EXSTYLE, GWL_STYLE,
        SW_SHOWNOACTIVATE, WS_BORDER, WS_CAPTION, WS_DLGFRAME, WS_EX_APPWINDOW,
        WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE, WS_EX_STATICEDGE, WS_EX_TOOLWINDOW,
        WS_EX_WINDOWEDGE, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    };
    unsafe {
        let h = hwnd as HWND;
        let st = GetWindowLongPtrW(h, GWL_STYLE);
        let clean_st = (st
            & !((WS_CAPTION | WS_THICKFRAME | WS_BORDER | WS_DLGFRAME | WS_SYSMENU) as isize))
            | (WS_POPUP as isize);
        SetWindowLongPtrW(h, GWL_STYLE, clean_st);

        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let clean_ex = (ex | (WS_EX_TOOLWINDOW as isize))
            & !((WS_EX_APPWINDOW
                | WS_EX_NOACTIVATE
                | WS_EX_WINDOWEDGE
                | WS_EX_STATICEDGE
                | WS_EX_DLGMODALFRAME) as isize);
        SetWindowLongPtrW(h, GWL_EXSTYLE, clean_ex);
        set_region(hwnd, None);
        ShowWindow(h, SW_SHOWNOACTIVATE);
    }
}

pub fn single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    unsafe {
        let name = wide("Local\\NusaAI-SingleInstance");
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        !(h.is_null() || GetLastError() == ERROR_ALREADY_EXISTS)
    }
}

/// Clip our window to the given physical client rect (empty rect = fully click-through/invisible).
pub fn set_region(hwnd: isize, r: Option<(i32, i32, i32, i32)>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_STYLE, WS_BORDER, WS_CAPTION, WS_DLGFRAME,
        WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    };
    unsafe {
        let h = hwnd as HWND;
        let st = GetWindowLongPtrW(h, GWL_STYLE);
        let mask = (WS_CAPTION | WS_THICKFRAME | WS_BORDER | WS_DLGFRAME | WS_SYSMENU) as isize;
        if (st & mask) != 0 {
            SetWindowLongPtrW(h, GWL_STYLE, (st & !mask) | (WS_POPUP as isize));
        }
        let rgn = match r {
            Some((x, y, w, h)) if w > 0 && h > 0 => CreateRectRgn(x, y, x + w, y + h),
            _ => CreateRectRgn(0, 0, 0, 0),
        };
        // The system owns the region after a successful call.
        if SetWindowRgn(h, rgn, 1) == 0 {
            DeleteObject(rgn as HGDIOBJ);
        }
    }
}

static LAST_RELOAD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn recently_reloaded(within_ms: u64) -> bool {
    let last = LAST_RELOAD_MS.load(std::sync::atomic::Ordering::SeqCst);
    last > 0 && now_ms().saturating_sub(last) < within_ms
}

/// Gracefully restart `nusa.exe` so it reloads `%APPDATA%\Nusa\todo.txt` from disk.
pub fn reload_nusa() {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
    };

    LAST_RELOAD_MS.store(now_ms(), std::sync::atomic::Ordering::SeqCst);

    let candidates = [
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("Programs").join("Nusa").join("nusa.exe")),
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|p| p.join("AppData\\Local\\Programs\\Nusa\\nusa.exe")),
        Some(PathBuf::from("C:\\Users\\Alvin\\AppData\\Local\\Programs\\Nusa\\nusa.exe")),
    ];
    let Some(exe) = candidates.into_iter().flatten().find(|p| p.exists()) else {
        return;
    };
    unsafe {
        let prev_fg = GetForegroundWindow() as isize;
        let class = wide("NotchWindow");
        let hwnd = FindWindowW(class.as_ptr(), std::ptr::null());
        if !hwnd.is_null() {
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let h_proc = if pid != 0 {
                OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid)
            } else {
                std::ptr::null_mut()
            };
            // Let main thread destroy window cleanly first (~10-40ms)
            let _ = PostMessageW(hwnd, WM_CLOSE, 0, 0);
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_millis(60) {
                if FindWindowW(class.as_ptr(), std::ptr::null()).is_null() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if !h_proc.is_null() {
                // Force-kill remaining sleeping background threads (5s/8s timers in nusa.exe)
                // so the single-instance mutex is released immediately.
                let _ = TerminateProcess(h_proc, 0);
                WaitForSingleObject(h_proc, 2000);
                CloseHandle(h_proc);
            } else {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
        for _ in 0..4 {
            let mut cmd = std::process::Command::new(&exe);
            if let Some(dir) = exe.parent() {
                cmd.current_dir(dir);
            }
            let _ = cmd.spawn();
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_millis(700) {
                let new_h = FindWindowW(class.as_ptr(), std::ptr::null());
                if !new_h.is_null() {
                    LAST_RELOAD_MS.store(now_ms(), std::sync::atomic::Ordering::SeqCst);
                    if prev_fg != 0 && prev_fg != (hwnd as isize) {
                        activate(prev_fg);
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
}

