//! 与界面无关的 Win32 小工具：单实例互斥、开机自启、Shell 打开、按光标所在显示器定位。

use std::sync::mpsc::Sender;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE,
    HWND, LPARAM, POINT,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcessId, INFINITE, OpenEventW, SetEvent,
    WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetCursorPos, GetForegroundWindow, GetWindowTextW,
    GetWindowThreadProcessId, SW_SHOWNORMAL, SetForegroundWindow,
};

use crate::ui::Command;

/// 单实例名（Local 会话级即可：同一用户同一桌面只会有一个启动器）。
const MUTEX_NAME: &str = "Local\\Pek.RWindows.Launcher";
const SHOW_EVENT_NAME: &str = "Local\\Pek.RWindows.Show";

// ---------------- 单实例 ----------------

/// 获取单实例所有权。
///
/// - 返回 `true`：本进程是主实例，已启动「显示窗口」事件监听线程；
/// - 返回 `false`：已有实例在运行，已通知它显示搜索框（调用方应直接退出）。
pub fn acquire_instance(cmd_tx: Sender<Command>) -> bool {
    let mutex_name = HSTRING::from(MUTEX_NAME);
    let event_name = HSTRING::from(SHOW_EVENT_NAME);
    unsafe {
        let mutex = match CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr())) {
            Ok(h) => h,
            Err(e) => {
                dhrust::warn!("创建单实例互斥体失败（本次按主实例继续运行）：{e}");
                return true;
            }
        };

        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            // 通知已运行的实例把窗口弹出来（失败则只是不弹窗，不影响退出）
            match OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event_name.as_ptr())) {
                Ok(event) => {
                    let _ = SetEvent(event);
                    let _ = CloseHandle(event);
                }
                Err(e) => dhrust::warn!("通知已运行实例失败（事件 {SHOW_EVENT_NAME} 打不开）：{e}"),
            }
            return false;
        }

        // 主实例：`HANDLE` 为 Copy、无 Drop，不关闭句柄即随进程存活（进程退出由系统回收）
        let _keep_alive = mutex;

        match CreateEventW(None, false, false, PCWSTR(event_name.as_ptr())) {
            Ok(event) => {
                let raw = event.0 as isize;
                std::thread::Builder::new()
                    .name("instance-show".into())
                    .spawn(move || {
                        let event = HANDLE(raw as _);
                        loop {
                            let r = WaitForSingleObject(event, INFINITE);
                            // WAIT_OBJECT_0 = 0：被第二实例唤醒 → 请求显示窗口
                            if r.0 != 0 {
                                dhrust::warn!("等待单实例显示事件异常结束（WaitForSingleObject 返回 {}）", r.0);
                                break;
                            }
                            if !crate::ui::send(&cmd_tx, Command::Show) {
                                break;
                            }
                        }
                    })
                    .ok();
            }
            Err(e) => dhrust::warn!("创建单实例显示事件失败（第二次启动将不会唤起窗口）：{e}"),
        }
        true
    }
}

// ---------------- 开机自启 ----------------

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "PekRWindows";

/// 是否已注册开机自启。
pub fn startup_enabled() -> bool {
    let key = HSTRING::from(RUN_KEY);
    let value = HSTRING::from(RUN_VALUE);
    unsafe {
        let mut size: u32 = 0;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        );
        r == ERROR_SUCCESS
    }
}

/// 写入 / 删除开机自启项（HKCU，无需管理员）。
pub fn set_startup(enable: bool) -> Result<(), String> {
    let key = HSTRING::from(RUN_KEY);
    let value = HSTRING::from(RUN_VALUE);
    unsafe {
        if enable {
            let exe = std::env::current_exe()
                .map_err(|e| format!("取当前程序路径失败，无法注册开机自启：{e}"))?;
            let cmd = format!("\"{}\" --autostart", exe.display());
            let wide = wide(&cmd);
            let bytes = (wide.len() * 2) as u32;
            let r = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                REG_SZ.0,
                Some(wide.as_ptr() as *const _),
                bytes,
            );
            if r == ERROR_SUCCESS {
                dhrust::info!("已注册开机自启：{cmd}");
                Ok(())
            } else {
                Err(format!(
                    "写入注册表 Run 项失败（HKCU\\...\\Run\\{RUN_VALUE}，错误码 {}）",
                    r.0
                ))
            }
        } else {
            let r = RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
            );
            if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
                dhrust::info!("已取消开机自启");
                Ok(())
            } else {
                Err(format!(
                    "删除注册表 Run 项失败（HKCU\\...\\Run\\{RUN_VALUE}，错误码 {}）",
                    r.0
                ))
            }
        }
    }
}

/// 主窗口标题（与 `main.rs` 传给 eframe 的标题一致）。
pub const WINDOW_TITLE: &str = "Pek 启动器";

// ---------------- 前台窗口 ----------------

/// 取当前前台窗口句柄（原始值，0 表示取不到）。
pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow().0 as isize }
}

/// 把焦点还给指定窗口（尽力而为：Windows 允许前台进程转移焦点）。
pub fn focus_window(raw: isize) {
    if raw == 0 {
        return;
    }
    unsafe {
        if !SetForegroundWindow(windows::Win32::Foundation::HWND(raw as _)).as_bool() {
            dhrust::debug!("归还焦点失败（目标窗口句柄 {raw:#x} 可能已关闭）");
        }
    }
}

/// 按标题找到本进程的主窗口。
pub fn own_window() -> Option<HWND> {
    struct Ctx {
        found: HWND,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            let mut buf = [0u16; 256];
            let len = GetWindowTextW(hwnd, &mut buf);
            if len > 0 {
                let title = String::from_utf16_lossy(&buf[..len as usize]);
                if title == WINDOW_TITLE {
                    ctx.found = hwnd;
                    return windows::core::BOOL(0);
                }
            }
        }
        windows::core::BOOL(1)
    }

    let mut ctx = Ctx {
        found: HWND::default(),
    };
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut ctx as *mut _ as isize));
    }
    if ctx.found.is_invalid() {
        None
    } else {
        Some(ctx.found)
    }
}

/// 本进程主窗口句柄缓存（窗口随进程存活，查一次即可）。
static OWN_HWND: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

/// 取本进程主窗口（首次找到后缓存；找不到不能缓存失败结果——窗口可能是后来才有标题）。
pub fn own_window_cached() -> Option<HWND> {
    if let Some(raw) = OWN_HWND.get() {
        return Some(HWND(*raw as _));
    }
    match own_window() {
        Some(hwnd) => {
            let _ = OWN_HWND.set(hwnd.0 as isize);
            Some(hwnd)
        }
        None => None,
    }
}

/// 本进程主窗口当前是否是前台窗口（Win32 口径，比 egui 的 focused 标志可靠）。
pub fn own_window_is_foreground() -> bool {
    match own_window_cached() {
        Some(hwnd) => unsafe { GetForegroundWindow() == hwnd },
        None => false,
    }
}

/// 把本进程主窗口提到前台（egui 的 Focus 命令偶尔不生效时的兵底）。
pub fn focus_own_window() -> bool {
    match own_window_cached() {
        Some(hwnd) => unsafe {
            let _ = BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd).as_bool()
        },
        None => false,
    }
}

// ---------------- 窗口定位 ----------------

/// 计算「让窗口居中到鼠标所在显示器」的逻辑像素坐标（窗口上沿略偏上，接近开始菜单搜索）。
///
/// 返回 `None` 表示取不到光标/显示器信息，调用方保持原位即可。
pub fn center_on_cursor_monitor(logical_size: (f32, f32)) -> Option<(f32, f32)> {
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        let mon: HMONITOR = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if mon.0.is_null() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut info).as_bool() {
            return None;
        }

        // 按目标显示器的缩放换算逻辑像素（Per-Monitor V2 下每个屏可能不同）
        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let scale = (dpi_x.max(48) as f32) / 96.0;

        let work = info.rcWork;
        let w_phys = logical_size.0 * scale;
        let h_phys = logical_size.1 * scale;
        let x = work.left as f32 + ((work.right - work.left) as f32 - w_phys) / 2.0;
        let y = work.top as f32 + ((work.bottom - work.top) as f32 - h_phys) * 0.30;
        Some((
            (x / scale).round(),
            (y / scale).clamp(0.0, 20000.0).round(),
        ))
    }
}

// ---------------- Shell 打开 ----------------

/// 用系统默认方式打开文件/文件夹（ShellExecute）。
pub fn open_path(path: &str) -> Result<(), String> {
    let file = HSTRING::from(path);
    let op = wide("open");
    let ret = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    let code = ret.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!(
            "ShellExecute 打开 \"{path}\" 失败（返回值 {code}，常见原因：目标不存在或无权限）"
        ))
    }
}

// ---------------- 杂项 ----------------

/// UTF-8 → 以 NUL 结尾的 UTF-16（Win32 API 入参）。
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 判断当前会话是否具备桌面（服务/会话 0 没有光标队列，无法弹窗或建托盘图标）。
pub fn has_desktop() -> bool {
    unsafe { GetCursorPos(&mut POINT::default()).is_ok() }
}
