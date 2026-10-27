//! 界面：无边框搜索窗（PowerToys Run 风格）。
//!
//! 隐藏策略：窗口**始终可见**，只是移到屏幕外（-30000, -30000）。
//! 这样 winit 在任何时候都能收到重绘事件——托盘/热键/@第二实例 发来的指令不会因为
//! 「窗口被 `Visible(false)` 后不再产生 WM_PAINT」而卡住（这是 Windows 上最常见的坑）。

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::apps::{self, AppEntry, LaunchTarget, RgbaImage};
use crate::config::Settings;
use crate::search;

/// 屏幕外坐标（隐藏时窗口停在这里）。
const OFFSCREEN: egui::Pos2 = egui::pos2(-30000.0, -30000.0);

const ROW_H: f32 = 46.0;
const ICON_SIZE: f32 = 30.0;

const BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(28, 30, 34, 250);
const BORDER: egui::Color32 = egui::Color32::from_rgb(60, 64, 72);
const TEXT: egui::Color32 = egui::Color32::from_rgb(232, 234, 238);
const HINT: egui::Color32 = egui::Color32::from_rgb(134, 140, 150);
const HINT_SOFT: egui::Color32 = egui::Color32::from_rgb(116, 122, 132);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(110, 168, 255);
const SEL_BG: egui::Color32 = egui::Color32::from_rgb(48, 53, 62);
const HOVER_BG: egui::Color32 = egui::Color32::from_rgb(40, 44, 51);
const DANGER: egui::Color32 = egui::Color32::from_rgb(255, 128, 128);

const USAGE_PATH: &str = "Data/usage.json";

/// 托盘 / 第二实例 / 自检 发给界面的指令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Show,
    Hide,
    Rescan,
    Quit,
}

/// 界面上下文（供任意线程唤醒重绘）。
static CTX: OnceLock<egui::Context> = OnceLock::new();

/// 发送指令并唤醒界面线程（可在任意线程调用）。返回 `false` 表示界面已退出。
pub fn send(tx: &Sender<Command>, cmd: Command) -> bool {
    if tx.send(cmd).is_err() {
        dhrust::warn!("界面消息通道已关闭，指令 {cmd:?} 被丢弃");
        return false;
    }
    wake();
    true
}

/// 仅唤醒界面（命令已由其它途径送达时使用）。
pub fn wake() {
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

enum ScanMsg {
    /// 应用列表（图标后到）
    List(Vec<AppEntry>),
    /// 图标批次（`(列表下标, 图标)`）
    Icons(Vec<(usize, RgbaImage)>),
}

pub struct LauncherApp {
    settings: Settings,
    apps: Vec<AppEntry>,
    textures: Vec<Option<egui::TextureHandle>>,
    query: String,
    results: Vec<search::Scored>,
    selected: usize,
    visible: bool,
    shown_at: Option<Instant>,
    /// 本次显示后是否真的拿到过焦点（没拿到就不自动隐藏，否则窗口会一弹出就消失）
    focused_once: bool,
    /// 焦点补发计数（egui 的 Focus 命令在窗口刚移动/显示时偶尔会落空）
    focus_retries: u8,
    /// 本次显示是否已因“拿不到前台焦点”告过警
    focus_warned: bool,
    request_focus: bool,
    scroll_to_selected: bool,
    matcher: SkimMatcherV2,
    cmd_rx: Receiver<Command>,
    scan_tx: Sender<ScanMsg>,
    scan_rx: Receiver<ScanMsg>,
    status_rx: Receiver<String>,
    scanning: bool,
    usage: HashMap<String, u64>,
    launch_tx: Sender<LaunchTarget>,
    status: Option<(String, Instant)>,
    prev_foreground: isize,
}

impl LauncherApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Settings,
        cmd_rx: Receiver<Command>,
        cmd_tx: Sender<Command>,
    ) -> Self {
        let _ = CTX.set(cc.egui_ctx.clone());
        install_fonts(&cc.egui_ctx);
        setup_style(&cc.egui_ctx);

        // 关闭本窗口的输入法：启动器只收 ASCII/拼音，否则中文输入法候选框会吞掉回车；
        // 需要中文时可直接粘贴或用拼音首字母搜。
        match crate::winutil::disable_ime() {
            true => dhrust::info!("已关闭搜索框的输入法（IME），用拼音/英文搜索（或粘贴中文）"),
            false => dhrust::debug!("初始化时关闭输入法未成功（后续每帧会继续尝试）"),
        }

        let (scan_tx, scan_rx) = channel();
        let (launch_tx, launch_rx) = channel();
        let (status_tx, status_rx) = channel::<String>();
        spawn_launch_worker(launch_rx, status_tx);
        spawn_scan(scan_tx.clone(), &settings);

        crate::tray::spawn(settings.hotkey.clone(), cmd_tx);

        dhrust::info!(
            "界面已就绪（窗口隐藏于屏幕外），开始后台扫描应用；热键 {}",
            settings.hotkey
        );

        Self {
            settings,
            apps: Vec::new(),
            textures: Vec::new(),
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            visible: false,
            shown_at: None,
            focused_once: false,
            focus_retries: 0,
            focus_warned: false,
            request_focus: false,
            scroll_to_selected: false,
            matcher: SkimMatcherV2::default().ignore_case(),
            cmd_rx,
            scan_tx,
            scan_rx,
            status_rx,
            scanning: true,
            usage: load_usage(),
            launch_tx,
            status: None,
            prev_foreground: 0,
        }
    }

    // ---------------- 状态切换 ----------------

    fn show(&mut self, ctx: &egui::Context) {
        if !self.visible {
            self.prev_foreground = crate::winutil::foreground_window();
            dhrust::debug!("显示窗口（之前的前台窗口句柄 {:#x}）", self.prev_foreground);
        }
        self.visible = true;
        self.shown_at = Some(Instant::now());
        self.focused_once = false;
        self.focus_retries = 0;
        self.focus_warned = false;
        self.query.clear();
        self.selected = 0;
        self.status = None;
        self.request_focus = true;
        self.refresh_results();

        if let Some((x, y)) = crate::winutil::center_on_cursor_monitor((
            self.settings.window_width,
            self.settings.window_height,
        )) {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        if !crate::winutil::focus_own_window() {
            dhrust::debug!("Win32 前台兜底未成功（egui Focus 仍可能生效）");
        }
        ctx.request_repaint();
    }

    fn hide(&mut self, ctx: &egui::Context) {
        if self.visible {
            dhrust::debug!("隐藏窗口（移到屏幕外）");
        }
        self.visible = false;
        self.shown_at = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(OFFSCREEN));
    }

    /// 隐藏并把焦点还给唤起前的窗口（Esc 场景）。
    fn hide_and_restore(&mut self, ctx: &egui::Context) {
        self.hide(ctx);
        if self.prev_foreground != 0 {
            crate::winutil::focus_window(self.prev_foreground);
            self.prev_foreground = 0;
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let len = self.results.len() as i32;
        let mut idx = self.selected as i32 + delta;
        if idx < 0 {
            idx = len - 1;
        }
        if idx >= len {
            idx = 0;
        }
        self.selected = idx as usize;
        self.scroll_to_selected = true;
    }

    fn activate_selected(&mut self, ctx: &egui::Context) {
        let Some(item) = self.results.get(self.selected) else {
            return;
        };
        let app = &self.apps[item.index];
        let key = app.key.clone();
        let target = app.target.clone();
        let name = app.name.clone();

        *self.usage.entry(key).or_insert(0) += 1;
        self.save_usage();

        self.hide(ctx);
        if self.launch_tx.send(target).is_err() {
            let msg = format!("启动「{name}」失败：后台启动线程已退出");
            dhrust::error!("{msg}");
            self.status = Some((msg, Instant::now()));
        } else {
            dhrust::info!("用户启动应用：{name}");
        }
    }

    fn refresh_results(&mut self) {
        let max = self.settings.max_results;
        let matcher = &mut self.matcher;
        let apps = &self.apps;
        let usage = &self.usage;
        self.results = search::search(matcher, apps, usage, &self.query, max);
    }

    fn set_apps(&mut self, ctx: &egui::Context, list: Vec<AppEntry>) {
        let count = list.len();
        self.textures = list.iter().map(|_| None).collect();
        self.apps = list;
        self.scanning = false;
        self.refresh_results();
        ctx.request_repaint();
        dhrust::info!("应用列表已载入：{count} 项（图标后续分批到达）");
    }

    /// 应用一批后台提取好的图标（不阻塞输入，边到边显）。
    fn apply_icons(&mut self, ctx: &egui::Context, batch: Vec<(usize, RgbaImage)>) {
        let mut applied = 0usize;
        for (index, img) in batch {
            if index >= self.textures.len() {
                dhrust::warn!("图标下标越界（{index} >= {}），已忽略", self.textures.len());
                continue;
            }
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [img.width as usize, img.height as usize],
                &img.pixels,
            );
            self.textures[index] = Some(ctx.load_texture(
                format!("app-icon-{index}"),
                image,
                egui::TextureOptions::LINEAR,
            ));
            applied += 1;
        }
        if applied > 0 {
            ctx.request_repaint();
        }
    }

    fn save_usage(&self) {
        let Ok(text) = serde_json::to_string_pretty(&self.usage) else {
            dhrust::error!("使用统计序列化失败（Data/usage.json 未写回）");
            return;
        };
        if let Err(e) = dhrust::io::write_all_text_atomic(USAGE_PATH, &text) {
            dhrust::warn!("写入使用统计 {USAGE_PATH} 失败：{e}");
        }
    }

    // ---------------- 绘制 ----------------

    fn draw(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::NONE
            .fill(BG)
            .stroke(egui::Stroke::new(1.0_f32, BORDER))
            .corner_radius(egui::CornerRadius::same(14))
            .inner_margin(egui::Margin::symmetric(18, 14));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            self.draw_search(ui);
            ui.add_space(3.0);
            separator(ui);
            ui.add_space(5.0);
            self.draw_results(ui);
            self.draw_footer(ui);
        });
    }

    fn draw_search(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(28.0, 36.0), egui::Sense::hover());
            draw_search_icon(ui.painter(), rect);

            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text(
                        egui::RichText::new("搜索应用…（支持拼音首字母，如 wx）").color(HINT),
                    )
                    .font(egui::FontId::proportional(21.0))
                    .frame(false)
                    .margin(egui::Margin::same(0))
                    .desired_width(ui.available_width()),
            );
            if resp.changed() {
                self.selected = 0;
                self.scroll_to_selected = false;
                self.refresh_results();
            }
            if self.request_focus {
                resp.request_focus();
                self.request_focus = false;
            }
        });
    }

    fn draw_results(&mut self, ui: &mut egui::Ui) {
        let max_h = (ui.available_height() - 26.0).max(80.0);
        let selected = self.selected;
        let scroll_to_selected = self.scroll_to_selected;
        let mut hovered: Option<usize> = None;
        let mut clicked: Option<usize> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(max_h)
            .show(ui, |ui| {
                if self.results.is_empty() {
                    ui.add_space(14.0);
                    if self.scanning {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("正在扫描已安装的应用…")
                                    .color(HINT)
                                    .size(13.0),
                            );
                        });
                    } else {
                        let text = if self.query.trim().is_empty() {
                            "应用列表为空：可从托盘菜单选择「重新扫描应用」"
                        } else {
                            "没有匹配的应用"
                        };
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new(text).color(HINT).size(14.0));
                        });
                    }
                    return;
                }

                for (i, scored) in self.results.iter().enumerate() {
                    let app = &self.apps[scored.index];
                    let texture = self.textures.get(scored.index).and_then(|t| t.as_ref());
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), ROW_H),
                        egui::Sense::click(),
                    );
                    let is_sel = i == selected;
                    draw_row(
                        ui,
                        rect,
                        app,
                        texture,
                        &scored.indices,
                        is_sel,
                        resp.hovered(),
                    );
                    if resp.hovered() {
                        hovered = Some(i);
                    }
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                    if scroll_to_selected && is_sel {
                        ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    }
                }
            });

        if let Some(i) = hovered {
            self.selected = i;
        }
        if let Some(i) = clicked {
            self.selected = i;
            let ctx = ui.ctx().clone();
            self.activate_selected(&ctx);
        }
        if scroll_to_selected {
            self.scroll_to_selected = false;
        }
    }

    fn draw_footer(&mut self, ui: &mut egui::Ui) {
        let status = self
            .status
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(8))
            .map(|(msg, _)| msg.clone());
        let scanning = self.scanning;
        let count = self.apps.len();
        ui.horizontal(|ui| {
            match status {
                Some(msg) => {
                    ui.label(egui::RichText::new(msg).size(11.0).color(DANGER));
                }
                None => {
                    ui.label(
                        egui::RichText::new("↑↓ 选择 · Enter 打开 · Esc 隐藏")
                            .size(11.0)
                            .color(HINT_SOFT),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = if scanning {
                    "正在扫描应用…".to_string()
                } else {
                    format!("共 {count} 个应用")
                };
                ui.label(egui::RichText::new(text).size(11.0).color(HINT));
            });
        });
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);

        if !self.visible {
            // 隐藏时没有任何周期任务：指令到达会由 `send()` 主动唤醒重绘
            return;
        }

        // 搜索框保持 ASCII 输入（IME 关闭）：否则中文输入法会接管回车，首次 Enter 只会选候选字。
        // egui-winit 会在文本框获焦时自动开启 IME，所以这里每帧压一次关闭命令。
        ctx.send_viewport_cmd(egui::ViewportCommand::IMEAllowed(false));

        if self.settings.auto_hide_on_blur {
            // 用 Win32 前台窗口判断：egui 的 focused 标志在刚显示时可能报假值
            let focused = crate::winutil::own_window_is_foreground();
            if focused {
                self.focused_once = true;
            } else if !self.focused_once && self.focus_retries < 3 {
                // 窗口刚显示时 Focus 命令可能落空，补发几次
                let due = self.shown_at.is_some_and(|t| {
                    t.elapsed() >= Duration::from_millis(150 * (self.focus_retries as u64 + 1))
                });
                if due {
                    self.focus_retries += 1;
                    dhrust::debug!("窗口未获得焦点，补发 Focus（第 {} 次）", self.focus_retries);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    let _ = crate::winutil::focus_own_window();
                    if self.focus_retries >= 3 && !self.focus_warned {
                        self.focus_warned = true;
                        dhrust::warn!("窗口未能获得前台焦点（可能被系统前台锁限制），点击窗口即可输入");
                    }
                }
            }
            let settled = self
                .shown_at
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(400));
            if settled && self.focused_once && !focused {
                self.hide(ctx);
                return;
            }
        }

        let (down, up, enter, esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if esc {
            self.hide_and_restore(ctx);
            return;
        }
        if down {
            self.move_selection(1);
        }
        if up {
            self.move_selection(-1);
        }
        if enter {
            self.activate_selected(ctx);
            if !self.visible {
                return;
            }
        }

        self.draw(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        dhrust::info!("界面退出，保存使用统计");
        self.save_usage();
        dhrust::logs::flush();
    }
}

impl LauncherApp {
    fn pump(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                Command::Show => self.show(ctx),
                Command::Hide => self.hide(ctx),
                Command::Rescan => {
                    dhrust::info!("手动重新扫描应用");
                    self.scanning = true;
                    spawn_scan(self.scan_tx.clone(), &self.settings);
                }
                Command::Quit => {
                    dhrust::info!("收到退出指令");
                    self.save_usage();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        while let Ok(msg) = self.scan_rx.try_recv() {
            match msg {
                ScanMsg::List(list) => self.set_apps(ctx, list),
                ScanMsg::Icons(batch) => self.apply_icons(ctx, batch),
            }
        }

        while let Ok(err) = self.status_rx.try_recv() {
            dhrust::warn!("启动失败：{err}");
            self.status = Some((err, Instant::now()));
        }
    }
}

// ---------------- 扫描 / 启动线程 ----------------

fn spawn_scan(tx: Sender<ScanMsg>, settings: &Settings) {
    let extra = settings.extra_scan_dirs.clone();
    let ignore = settings.ignore_names.clone();
    let fallback = tx.clone();
    let spawned = std::thread::Builder::new()
        .name("app-scan".into())
        .spawn(move || {
            let tx_list = tx.clone();
            let tx_icons = tx.clone();
            apps::scan_progressive(
                &extra,
                &ignore,
                move |list| {
                    if tx_list.send(ScanMsg::List(list)).is_ok() {
                        wake();
                    }
                },
                move |batch| {
                    if tx_icons.send(ScanMsg::Icons(batch)).is_ok() {
                        wake();
                    }
                },
            );
        });
    if let Err(e) = spawned {
        dhrust::error!("启动扫描线程失败（应用列表将为空）：{e}");
        // 让界面退出「正在扫描」状态，展示空列表提示
        let _ = fallback.send(ScanMsg::List(Vec::new()));
        wake();
    }
}

fn spawn_launch_worker(rx: Receiver<LaunchTarget>, status_tx: Sender<String>) {
    let spawned = std::thread::Builder::new()
        .name("app-launch".into())
        .spawn(move || {
            for target in rx {
                match apps::launch(&target) {
                    Ok(()) => dhrust::info!("已启动：{}", target.describe()),
                    Err(e) => {
                        dhrust::error!("{e}");
                        let _ = status_tx.send(e);
                    }
                }
            }
        });
    if let Err(e) = spawned {
        dhrust::error!("启动应用启动线程失败（之后无法打开任何应用）：{e}");
    }
}

// ---------------- 使用统计 ----------------

fn load_usage() -> HashMap<String, u64> {
    match std::fs::read_to_string(USAGE_PATH) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            dhrust::warn!("使用统计 {USAGE_PATH} 解析失败（按空统计继续）：{e}");
            HashMap::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => {
            dhrust::warn!("读取使用统计 {USAGE_PATH} 失败：{e}");
            HashMap::new()
        }
    }
}

// ---------------- 字体与样式 ----------------

/// 加载系统中文字体（egui 内置字体不含 CJK，不装就是方块）。
fn install_fonts(ctx: &egui::Context) {
    const CANDIDATES: [(&str, u32); 5] = [
        ("C:\\Windows\\Fonts\\msyh.ttc", 0),   // 微软雅黑
        ("C:\\Windows\\Fonts\\msyhbd.ttc", 0), // 微软雅黑 Bold
        ("C:\\Windows\\Fonts\\simhei.ttf", 0), // 黑体
        ("C:\\Windows\\Fonts\\Deng.ttf", 0),   // 等线
        ("C:\\Windows\\Fonts\\simsun.ttc", 0), // 宋体
    ];
    for (path, index) in CANDIDATES {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut fonts = egui::FontDefinitions::default();
                let mut data = egui::FontData::from_owned(bytes);
                data.index = index;
                fonts
                    .font_data
                    .insert("cjk".to_owned(), Arc::new(data));
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .push("cjk".to_owned());
                }
                ctx.set_fonts(fonts);
                dhrust::info!("已加载中文字体：{path}");
                return;
            }
            Err(e) => dhrust::debug!("字体 {path} 不可用：{e}"),
        }
    }
    dhrust::warn!("未找到系统中文字体（微软雅黑/黑体/等线/宋体），中文可能显示为方块");
}

fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::WHITE);
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::proportional(15.0),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::proportional(14.0),
    );
    ctx.set_style(style);
}

// ---------------- 行与图标绘制 ----------------

fn draw_search_icon(painter: &egui::Painter, rect: egui::Rect) {
    let center = egui::pos2(rect.left() + 9.0, rect.center().y);
    let stroke = egui::Stroke::new(2.0_f32, HINT);
    painter.circle_stroke(center, 6.0, stroke);
    painter.line_segment(
        [
            egui::pos2(center.x + 4.2, center.y + 4.2),
            egui::pos2(center.x + 9.0, center.y + 9.0),
        ],
        stroke,
    );
}

fn separator(ui: &mut egui::Ui) {
    let y = ui.cursor().min.y;
    let rect = egui::Rect::from_min_max(
        egui::pos2(ui.max_rect().left(), y),
        egui::pos2(ui.max_rect().right(), y + 1.0),
    );
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, egui::Color32::from_rgb(48, 51, 58));
    ui.add_space(1.0);
}

fn draw_row(
    ui: &egui::Ui,
    rect: egui::Rect,
    app: &AppEntry,
    texture: Option<&egui::TextureHandle>,
    indices: &[usize],
    selected: bool,
    hovered: bool,
) {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, egui::CornerRadius::same(8), SEL_BG);
    } else if hovered {
        painter.rect_filled(rect, egui::CornerRadius::same(8), HOVER_BG);
    }

    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 8.0, rect.center().y - ICON_SIZE / 2.0),
        egui::vec2(ICON_SIZE, ICON_SIZE),
    );
    match texture {
        Some(tex) => {
            painter.image(
                tex.id(),
                icon_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => draw_placeholder(ui, icon_rect, &app.name),
    }

    let left = icon_rect.right() + 12.0;
    let width = (rect.right() - 10.0 - left).max(40.0);

    let name = layout_text(
        ui.ctx(),
        &app.name,
        indices,
        egui::FontId::proportional(15.5),
        TEXT,
        width,
    );
    let name_y = rect.center().y - name.size().y - 1.0;
    ui.painter().galley(egui::pos2(left, name_y), name, TEXT);

    let subtitle = layout_text(
        ui.ctx(),
        &app.target.describe(),
        &[],
        egui::FontId::proportional(11.0),
        HINT_SOFT,
        width,
    );
    ui.painter()
        .galley(egui::pos2(left, rect.center().y + 2.0), subtitle, HINT_SOFT);
}

/// 单行文本 + 命中字符高亮（超出宽度以省略号截断）。
fn layout_text(
    ctx: &egui::Context,
    text: &str,
    indices: &[usize],
    font: egui::FontId,
    color: egui::Color32,
    max_width: f32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = max_width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');

    if indices.is_empty() {
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: font,
                color,
                ..Default::default()
            },
        );
    } else {
        let hit: std::collections::HashSet<usize> = indices.iter().copied().collect();
        for (ci, ch) in text.chars().enumerate() {
            let mut buf = [0u8; 4];
            let piece = ch.encode_utf8(&mut buf);
            job.append(
                piece,
                0.0,
                egui::TextFormat {
                    font_id: font.clone(),
                    color: if hit.contains(&ci) { ACCENT } else { color },
                    ..Default::default()
                },
            );
        }
    }
    ctx.fonts(|f| f.layout_job(job))
}

fn draw_placeholder(ui: &egui::Ui, rect: egui::Rect, name: &str) {
    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(6), palette_color(name));
    let text = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    let galley = painter.layout_no_wrap(
        text,
        egui::FontId::proportional(14.0),
        egui::Color32::WHITE,
    );
    let pos = rect.center() - galley.size() / 2.0;
    painter.galley(pos, galley, egui::Color32::WHITE);
}

fn palette_color(name: &str) -> egui::Color32 {
    const PALETTE: [egui::Color32; 8] = [
        egui::Color32::from_rgb(72, 120, 220),
        egui::Color32::from_rgb(60, 160, 150),
        egui::Color32::from_rgb(190, 110, 70),
        egui::Color32::from_rgb(140, 100, 210),
        egui::Color32::from_rgb(60, 140, 100),
        egui::Color32::from_rgb(190, 80, 110),
        egui::Color32::from_rgb(90, 130, 180),
        egui::Color32::from_rgb(150, 130, 60),
    ];
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(hash as usize) % PALETTE.len()]
}
