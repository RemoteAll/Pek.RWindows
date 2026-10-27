//! 托盘图标 + 全局热键（独立线程：自带 Win32 消息泵，与 egui 主循环互不干扰）。
//!
//! 事件全部走回调（`TrayIconEvent::set_event_handler` / `MenuEvent::set_event_handler`），
//! 消息泵阻塞在 `GetMessageW` 上——空闲时零 CPU 占用。

use std::sync::mpsc::Sender;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage, WM_HOTKEY,
};

use crate::ui::Command;

/// 热键编号（线程内唯一即可）。
const HOTKEY_ID: i32 = 0x504B; // 'P''K'

thread_local! {
    /// 开机自启菜单项：muda 的菜单句柄内部是 `Rc`（!Send），只能留在托盘线程本地，
    /// 由托盘线程自己的菜单回调修改勾选状态。
    static AUTOSTART_ITEM: std::cell::RefCell<Option<CheckMenuItem>> =
        const { std::cell::RefCell::new(None) };
}

/// 启动托盘线程；失败只影响托盘/热键，不影响主窗口逻辑。
pub fn spawn(hotkey: String, cmd_tx: Sender<Command>) {
    let _ = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            if let Err(e) = run(hotkey, cmd_tx) {
                dhrust::error!("托盘线程退出：{e}");
            }
        });
}

fn run(hotkey: String, cmd_tx: Sender<Command>) -> Result<(), String> {
    // ---------- 菜单 ----------
    let show_item = MenuItem::with_id("show", "显示搜索框", true, None);
    let rescan_item = MenuItem::with_id("rescan", "重新扫描应用", true, None);
    let config_item = MenuItem::with_id("config", "打开配置目录", true, None);
    let autostart_item = CheckMenuItem::with_id(
        "autostart",
        "开机自动启动",
        true,
        crate::winutil::startup_enabled(),
        None,
    );
    let quit_item = MenuItem::with_id("quit", "退出", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &show_item,
        &rescan_item,
        &PredefinedMenuItem::separator(),
        &autostart_item,
        &config_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .map_err(|e| format!("构建托盘菜单失败：{e}"))?;

    // ---------- 图标 ----------
    let rgba = crate::icon::draw_rgba(32);
    let icon = tray_icon::Icon::from_rgba(rgba, 32, 32)
        .map_err(|e| format!("生成托盘图标失败（图标数据非法）：{e}"))?;

    // ---------- 热键 ----------
    let registered = register_hotkey(&hotkey);

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("Pek 启动器 · {registered}"))
        .with_icon(icon)
        .build()
        .map_err(|e| format!("创建托盘图标失败（Shell_NotifyIcon）：{e}"))?;

    // ---------- 事件回调 ----------
    {
        let cmd_tx = cmd_tx.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::ui::send(&cmd_tx, Command::Show);
            }
        }));
    }
    {
        let cmd_tx = cmd_tx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            match event.id.0.as_str() {
                "show" => {
                    crate::ui::send(&cmd_tx, Command::Show);
                }
                "rescan" => {
                    crate::ui::send(&cmd_tx, Command::Rescan);
                }
                "config" => {
                    if let Err(e) = crate::winutil::open_path("Config") {
                        dhrust::error!("打开配置目录失败：{e}");
                    }
                }
                "autostart" => {
                    let enable = !crate::winutil::startup_enabled();
                    match crate::winutil::set_startup(enable) {
                        Ok(()) => AUTOSTART_ITEM.with(|cell| {
                            if let Some(item) = cell.borrow().as_ref() {
                                item.set_checked(enable);
                            }
                        }),
                        Err(e) => dhrust::error!("切换开机自启失败：{e}"),
                    }
                }
                "quit" => {
                    dhrust::info!("托盘菜单请求退出");
                    crate::ui::send(&cmd_tx, Command::Quit);
                }
                other => dhrust::warn!("收到未知托盘菜单项：{other}"),
            }
        }));
    }

    AUTOSTART_ITEM.with(|cell| {
        *cell.borrow_mut() = Some(autostart_item.clone());
    });

    dhrust::info!("托盘已就绪（热键：{registered}）");

    // ---------- 消息泵 ----------
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        if msg.message == WM_HOTKEY {
            dhrust::debug!("全局热键触发");
            crate::ui::send(&cmd_tx, Command::Show);
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    drop(tray);
    dhrust::info!("托盘线程结束");
    Ok(())
}

/// 注册全局热键；配置值不可用时按候选列表依次尝试，返回最终生效的写法。
fn register_hotkey(spec: &str) -> String {
    let mut candidates = vec![spec.trim().to_string()];
    for fallback in ["Alt+Space", "Ctrl+Alt+Space", "Ctrl+Alt+P", "Ctrl+Shift+F12"] {
        if !candidates.iter().any(|c| c == fallback) {
            candidates.push(fallback.to_string());
        }
    }

    for (i, candidate) in candidates.iter().enumerate() {
        match parse_hotkey(candidate) {
            Ok((mods, vk)) => match unsafe { RegisterHotKey(None, HOTKEY_ID, mods, vk) } {
                Ok(()) => {
                    if i > 0 {
                        dhrust::warn!("配置热键 \"{spec}\" 不可用，已改用 \"{candidate}\"");
                    }
                    return candidate.clone();
                }
                Err(e) => {
                    dhrust::warn!("注册热键 \"{candidate}\" 失败（可能被其它程序占用）：{e}")
                }
            },
            Err(e) => dhrust::warn!("热键配置 \"{candidate}\" 解析失败：{e}"),
        }
    }
    dhrust::error!("没有可用的全局热键，窗口只能从托盘菜单打开");
    "热键不可用".to_string()
}

/// 解析形如 `Ctrl+Alt+P` 的热键。
fn parse_hotkey(spec: &str) -> Result<(HOT_KEY_MODIFIERS, u32), String> {
    let parts: Vec<&str> = spec
        .split('+')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let (key, modifiers) = parts
        .split_last()
        .ok_or_else(|| "热键为空（应形如 Alt+Space）".to_string())?;

    let mut mods = HOT_KEY_MODIFIERS(0);
    for m in modifiers {
        mods |= match m.to_lowercase().as_str() {
            "ctrl" | "control" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" | "meta" | "super" | "cmd" => MOD_WIN,
            other => return Err(format!("未知修饰键 \"{other}\"")),
        };
    }
    let vk = parse_vk(key)?;
    Ok((mods | MOD_NOREPEAT, vk))
}

fn parse_vk(key: &str) -> Result<u32, String> {
    let k = key.to_lowercase();
    let mut chars = k.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Ok(c.to_ascii_uppercase() as u32);
        }
        if c.is_ascii_digit() {
            return Ok(c as u32);
        }
        return Err(format!("不支持的按键 \"{key}\"（仅支持字母/数字/F1~F24/常用命名键）"));
    }

    let vk = match k.as_str() {
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        _ => match k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
            Some(n) if (1..=24).contains(&n) => 0x70 + (n - 1),
            _ => return Err(format!("不支持的按键名 \"{key}\"")),
        },
    };
    Ok(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alt_space() {
        let (mods, vk) = parse_hotkey("Alt+Space").unwrap();
        assert_eq!(mods.0, (MOD_ALT | MOD_NOREPEAT).0);
        assert_eq!(vk, 0x20);
    }

    #[test]
    fn parses_ctrl_alt_letter_and_fkeys() {
        let (mods, vk) = parse_hotkey("Ctrl+Alt+P").unwrap();
        assert_eq!(mods.0, (MOD_CONTROL | MOD_ALT | MOD_NOREPEAT).0);
        assert_eq!(vk, 'P' as u32);

        let (_, vk) = parse_hotkey("Ctrl+Shift+F12").unwrap();
        assert_eq!(vk, 0x70 + 11);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_hotkey("").is_err());
        assert!(parse_hotkey("Hyper+X").is_err());
        assert!(parse_hotkey("Ctrl+F25").is_err());
        assert!(parse_hotkey("Ctrl+Space+Alt").is_err(), "Space 在中间会被当作修饰键解析失败");
    }
}
