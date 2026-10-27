//! Pek 启动器：常驻托盘的 Windows 应用搜索（开始菜单搜索失灵时的替代品）。
//!
//! 用法：
//! - 双击运行 → 静默缩到托盘（窗口藏在屏幕外），默认热键 `Alt+Space` 唤起；
//! - 托盘菜单：显示搜索框 / 重新扫描应用 / 开机自动启动 / 打开配置目录 / 退出；
//! - `--dump-apps`：导出应用清单到 `Data/apps-dump.txt`（自检/排障）；
//! - `--selftest`：自动 显示→隐藏→显示→退出（验证托盘/热键通道与窗口切换）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apps;
mod config;
mod icon;
mod search;
mod tray;
mod ui;
mod winutil;

use std::io::Write;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use eframe::egui;

use ui::Command;

fn main() {
    prepare_working_dir();
    // 调试构建保留控制台便于排障；发布构建是纯 GUI（无黑框）
    dhrust::logs::init_console_and_file(
        "Log",
        cfg!(debug_assertions),
        dhrust::logs::level_from_env(),
    );

    let args: Vec<String> = std::env::args().skip(1).collect();
    dhrust::info!(
        "Pek 启动器 v{} 启动，参数：{args:?}",
        env!("CARGO_PKG_VERSION")
    );

    if args.iter().any(|a| a == "--dump-apps") {
        dump_apps();
        return;
    }

    if !winutil::has_desktop() {
        dhrust::error!("当前会话没有交互桌面（服务/会话 0），无法显示界面，退出");
        return;
    }

    let mut settings = config::load();
    if args.iter().any(|a| a == "--selftest") {
        // 自检的显示不是用户操作触发的，Windows 可能拒绝抢前台焦点；关闭失焦隐藏以免干扰验证
        settings.auto_hide_on_blur = false;
    }

    let (cmd_tx, cmd_rx) = channel::<Command>();
    if !winutil::acquire_instance(cmd_tx.clone()) {
        dhrust::info!("已有实例在运行，已通知其显示搜索框，本进程退出");
        return;
    }

    if args.iter().any(|a| a == "--selftest") {
        spawn_selftest(cmd_tx.clone());
    }

    if let Err(e) = run_gui(settings, cmd_rx, cmd_tx) {
        dhrust::error!("界面异常退出：{e}");
    }
    dhrust::info!("Pek 启动器退出");
    dhrust::logs::flush();
}

/// 启动时把工作目录切到 exe 所在目录：注册表自启时工作目录是 system32，
/// 配置/日志/使用统计都按相对路径落盘，必须自己纠正。
fn prepare_working_dir() {
    match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        Some(dir) => {
            if let Err(e) = std::env::set_current_dir(&dir) {
                out(&format!("切换工作目录到 {} 失败：{e}", dir.display()));
            }
        }
        None => out("无法定位程序目录，配置/日志将使用当前目录"),
    }
    if let Err(e) = std::fs::create_dir_all("Data") {
        out(&format!("创建 Data 目录失败：{e}"));
    }
}

fn run_gui(
    settings: config::Settings,
    cmd_rx: Receiver<Command>,
    cmd_tx: Sender<Command>,
) -> eframe::Result {
    let size = [settings.window_width, settings.window_height];
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(winutil::WINDOW_TITLE)
            .with_inner_size(size)
            // 启动即在屏幕外：不抢焦点、不闪一下，第一次热键才移到鼠标所在屏幕中央
            .with_position([-30000.0, -30000.0])
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_active(false),
        centered: false,
        run_and_return: false,
        ..Default::default()
    };

    eframe::run_native(
        winutil::WINDOW_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(ui::LauncherApp::new(cc, settings, cmd_rx, cmd_tx)))),
    )
}

/// 自检：模拟托盘/热键的三次指令，最后退出。
fn spawn_selftest(tx: Sender<Command>) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        dhrust::info!("[自检] 请求显示");
        let _ = ui::send(&tx, Command::Show);

        std::thread::sleep(Duration::from_secs(2));
        dhrust::info!("[自检] 请求隐藏");
        let _ = ui::send(&tx, Command::Hide);

        std::thread::sleep(Duration::from_secs(1));
        dhrust::info!("[自检] 再次请求显示");
        let _ = ui::send(&tx, Command::Show);

        std::thread::sleep(Duration::from_millis(1500));
        dhrust::info!("[自检] 请求退出");
        let _ = ui::send(&tx, Command::Quit);
    });
}

/// 导出应用清单（不开窗口）。
fn dump_apps() {
    let settings = config::load();
    let started = std::time::Instant::now();
    let list = apps::enumerate(&settings.extra_scan_dirs, &settings.ignore_names);
    let path = std::path::Path::new("Data/apps-dump.txt");
    match apps::dump_to_file(path, &list) {
        Ok(()) => {
            let summary = format!(
                "应用清单已写入 {}（{} 项，耗时 {} ms）",
                path.display(),
                list.len(),
                started.elapsed().as_millis()
            );
            dhrust::info!("{summary}");
            out(&summary);
        }
        Err(e) => {
            let msg = format!("写入 {} 失败：{e}", path.display());
            dhrust::error!("{msg}");
            out(&msg);
        }
    }
    for app in list.iter().take(20) {
        out(&format!("  {} → {}", app.name, app.target.describe()));
    }
}

/// 安全输出（GUI 子系统下没有控制台，写 stdout 会失败，不能 panic）。
fn out(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}
