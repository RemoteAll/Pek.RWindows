//! 应用枚举与启动。
//!
//! 数据来源分三层（互为兜底）：
//! 1. `shell:AppsFolder`（开始菜单「所有应用」的同一数据源：桌面程序 + UWP/商店应用，参照
//!    PowerToys Run / Wox 的 UniversalApps 插件做法）；
//! 2. 开始菜单目录扫描（`%ProgramData%` / `%APPDATA%` 下的 `.lnk/.url/.exe`），AppsFolder 不可用时兜底；
//! 3. 配置里的额外扫描目录（便携软件）。
//!
//! 图标走 `IShellItemImageFactory`（系统缩略图/图标服务），与资源管理器显示一致。

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use pinyin::ToPinyin;
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{PROPERTYKEY, SIZE, HWND};
use windows::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HGDIOBJ, ReleaseDC,
};use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    IBindCtx,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ApplicationActivationManager, AO_NONE, BHID_SFObject, FOLDERID_AppsFolder, IApplicationActivationManager,
    IEnumIDList, IShellFolder, IShellItem, IShellItem2, IShellItemImageFactory, KF_FLAG_DEFAULT,
    SHCONTF_FOLDERS, SHCONTF_NONFOLDERS, SHCreateItemWithParent,
    SHGetKnownFolderItem, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_FLAGS, SHGetFileInfoW, SHFILEINFOW,
    SHGetPathFromIDListW, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

/// 图标尺寸（逻辑像素；系统会按 DPI 取最接近的位图）。
const ICON_SIZE: i32 = 48;

/// 应用启动目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchTarget {
    /// 文件 / 快捷方式路径，用 ShellExecute 打开
    Path(String),
    /// 打包应用（UWP / 商店应用）AUMID，用 IApplicationActivationManager 激活
    Aumid(String),
}

impl LaunchTarget {
    /// 唯一键（去重用）。
    pub fn key(&self) -> String {
        match self {
            LaunchTarget::Path(p) => p.to_lowercase(),
            LaunchTarget::Aumid(a) => format!("uwp:{a}"),
        }
    }

    /// 界面上显示的副标题。
    pub fn describe(&self) -> String {
        match self {
            LaunchTarget::Path(p) => p.clone(),
            LaunchTarget::Aumid(a) => format!("AUMID · {a}"),
        }
    }
}

/// 应用图标（RGBA，未预乘 alpha）。
#[derive(Clone, Debug)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// 一条可搜索的应用记录。
pub struct AppEntry {
    /// 显示名（开始菜单里的名字）
    pub name: String,
    /// 唯一键（启动目标）
    pub key: String,
    pub target: LaunchTarget,
    pub icon: Option<RgbaImage>,
    /// 小写名（搜索排序用）
    pub name_lower: String,
    /// 全拼（如「微信输入法」→ weixinshurufa）
    pub py_full: String,
    /// 拼音首字母（如「微信输入法」→ wxsrf）
    pub py_short: String,
}

/// 一条待处理条目（图标延迟到列表发出后再取）。
struct RawApp {
    name: String,
    target: LaunchTarget,
    icon_job: Option<IconJob>,
}

/// 图标来源（只在扫描线程内使用：COM 对象不能跨套间）。
enum IconJob {
    File(String),
    ShellItem(IShellItem2),
}

/// 一次性枚举（含图标；`--dump-apps` 自检用）。
pub fn enumerate(extra_dirs: &[String], ignore_names: &[String]) -> Vec<AppEntry> {
    let mut collected: Vec<AppEntry> = Vec::new();
    let mut icons: std::collections::HashMap<usize, RgbaImage> = std::collections::HashMap::new();
    scan_progressive(
        extra_dirs,
        ignore_names,
        |list| collected = list,
        |batch| {
            for (index, image) in batch {
                icons.insert(index, image);
            }
        },
    );
    for (index, app) in collected.iter_mut().enumerate() {
        app.icon = icons.remove(&index);
    }
    collected
}

/// 渐进式扫描：先回调完整应用列表，再分批回调图标（`(列表下标, 图标)`）。
///
/// 两个回调都在调用线程执行；界面侧应把结果转成消息并主动唤醒界面。
pub fn scan_progressive(
    extra_dirs: &[String],
    ignore_names: &[String],
    mut on_list: impl FnMut(Vec<AppEntry>),
    mut on_icons: impl FnMut(Vec<(usize, RgbaImage)>),
) {
    let started = std::time::Instant::now();
    let mut raw: Vec<RawApp> = Vec::new();

    match unsafe { enumerate_apps_folder() } {
        Ok(list) => {
            dhrust::info!(
                "shell:AppsFolder 枚举到 {} 项（耗时 {} ms）",
                list.len(),
                started.elapsed().as_millis()
            );
            raw.extend(list);
        }
        Err(e) => dhrust::warn!("shell:AppsFolder 枚举失败（改用开始菜单目录扫描）：{e}"),
    }

    if raw.is_empty() {
        let t = std::time::Instant::now();
        raw.extend(scan_start_menu());
        dhrust::info!(
            "开始菜单目录扫描到 {} 项（耗时 {} ms）",
            raw.len(),
            t.elapsed().as_millis()
        );
    }

    for dir in extra_dirs {
        let before = raw.len();
        let t = std::time::Instant::now();
        scan_files(Path::new(dir), 0, 6, &mut raw);
        dhrust::info!(
            "额外目录 {dir} 扫描到 {} 项（耗时 {} ms）",
            raw.len() - before,
            t.elapsed().as_millis()
        );
    }

    // 过滤 + 去重 + 拼音预计算
    let ignores: Vec<String> = ignore_names
        .iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut pairs: Vec<(AppEntry, Option<IconJob>)> = Vec::with_capacity(raw.len());
    for item in raw {
        let name = item.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let name_lower = name.to_lowercase();
        if ignores.contains(&name_lower) {
            continue;
        }
        let key = item.target.key();
        if !seen.insert(key.clone()) {
            continue;
        }
        let (py_full, py_short) = pinyin_of(&name);
        pairs.push((
            AppEntry {
                name,
                key,
                target: item.target,
                icon: None,
                name_lower,
                py_full,
                py_short,
            },
            item.icon_job,
        ));
    }
    pairs.sort_by(|a, b| a.0.name_lower.cmp(&b.0.name_lower));

    let count = pairs.len();
    let mut apps: Vec<AppEntry> = Vec::with_capacity(count);
    let mut jobs: Vec<Option<IconJob>> = Vec::with_capacity(count);
    for (app, job) in pairs {
        apps.push(app);
        jobs.push(job);
    }

    dhrust::info!(
        "应用列表就绪：{count} 项（耗时 {} ms），图标转后台提取",
        started.elapsed().as_millis()
    );
    on_list(apps);

    // 图标：先取文件图标（快，绝大多数），再取打包应用图标（慢），每 8 个推送一批
    let mut batch: Vec<(usize, RgbaImage)> = Vec::with_capacity(8);
    for pass in 0..2 {
        for (index, job) in jobs.iter().enumerate() {
            let is_file = matches!(job, Some(IconJob::File(_)));
            if (pass == 0) != is_file {
                continue;
            }
            let image = match job {
                Some(IconJob::File(path)) => unsafe { icon_from_file(path) },
                Some(IconJob::ShellItem(item)) => unsafe { item_icon(item) },
                None => None,
            };
            if let Some(image) = image {
                batch.push((index, image));
                if batch.len() >= 8 {
                    on_icons(std::mem::take(&mut batch));
                }
            }
        }
    }
    if !batch.is_empty() {
        on_icons(batch);
    }

    dhrust::info!(
        "图标提取完成（{count} 项，总耗时 {} ms）",
        started.elapsed().as_millis()
    );
}

/// 启动应用（失败时返回含具体原因的中文说明）。
pub fn launch(target: &LaunchTarget) -> Result<(), String> {
    match target {
        LaunchTarget::Path(p) => crate::winutil::open_path(p),
        LaunchTarget::Aumid(aumid) => activate_aumid(aumid),
    }
}

/// 把应用清单导出到文本文件（`--dump-apps` 自检用）。
pub fn dump_to_file(path: &Path, apps: &[AppEntry]) -> std::io::Result<()> {
    let mut text = format!("# Pek 启动器应用清单（共 {} 项）\n", apps.len());
    for a in apps {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            a.name,
            a.target.describe(),
            if a.icon.is_some() { "icon" } else { "no-icon" },
            a.py_short
        ));
    }
    std::fs::write(path, text)
}

// ---------------- AppsFolder 枚举 ----------------

const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
    pid: 5,
};

unsafe fn enumerate_apps_folder() -> windows::core::Result<Vec<RawApp>> {
    // 调用方线程已初始化 COM 也无妨：RPC_E_CHANGED_MODE 时继续用现有套间
    let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    if hr.0 < 0 {
        dhrust::debug!("COM 初始化返回 {hr:?}（沿用线程既有套间）");
    }

    let folder: IShellItem = SHGetKnownFolderItem::<IShellItem>(
        &FOLDERID_AppsFolder,
        KF_FLAG_DEFAULT,
        None,
    )?;
    let shell_folder: IShellFolder = folder.BindToHandler(None::<&IBindCtx>, &BHID_SFObject)?;

    let mut raw_enum: Option<IEnumIDList> = None;
    let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32;
    shell_folder
        .EnumObjects(HWND::default(), flags, &mut raw_enum)
        .ok()?;
    let Some(enumerator) = raw_enum else {
        return Ok(Vec::new());
    };

    let mut items: Vec<RawApp> = Vec::new();
    loop {
        let mut arr: [*mut ITEMIDLIST; 1] = [std::ptr::null_mut()];
        let mut fetched: u32 = 0;
        let hr = enumerator.Next(&mut arr, Some(&mut fetched));
        if hr.0 < 0 || fetched == 0 || arr[0].is_null() {
            break;
        }
        let pidl = arr[0];

        // 先取真实文件路径（快捷方式/可执行文件）：AppsFolder 项的 FILESYSPATH 属性取不到，
        // 但绝对 PIDL 能换算出磁盘路径；UWP 项这里会失败，后面用 AUMID 激活。
        let mut path_buf = [0u16; 260];
        let fs_path = if SHGetPathFromIDListW(pidl, &mut path_buf).as_bool() {
            let len = path_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(path_buf.len());
            if len > 0 {
                Some(String::from_utf16_lossy(&path_buf[..len]))
            } else {
                None
            }
        } else {
            None
        };

        // 单项失败不影响整体
        if let Ok(item) = SHCreateItemWithParent::<_, IShellItem2>(None, &shell_folder, pidl) {
            if let Some(entry) = unsafe { read_item(&item, fs_path.as_deref()) } {
                items.push(entry);
            }
        } else {
            dhrust::warn!("跳过一项无法解析的开始菜单条目（PIDL 解析失败）");
        }

        CoTaskMemFree(Some(pidl as *const c_void));
    }
    Ok(items)
}

unsafe fn read_item(item: &IShellItem2, fs_path: Option<&str>) -> Option<RawApp> {
    let name = pwstr_take(item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?);
    if name.trim().is_empty() {
        return None;
    }
    let parsing = item
        .GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING)
        .map(|p| pwstr_take(p))
        .unwrap_or_default();
    let aumid = item
        .GetString(&PKEY_APP_USER_MODEL_ID)
        .map(|p| pwstr_take(p))
        .unwrap_or_default();

    let target = classify(fs_path, &parsing, &aumid)?;
    let icon_job = match fs_path {
        Some(path) => Some(IconJob::File(path.to_string())),
        None => Some(IconJob::ShellItem(item.clone())),
    };
    dhrust::debug!(
        "条目「{name}」| PATH={fs_path:?} | AUMID={aumid:?} | PARSING={parsing} → {}",
        target.describe()
    );
    Some(RawApp {
        name,
        target,
        icon_job,
    })
}

/// 判断条目类型与启动方式。
///
/// - 有磁盘路径（PIDL 换算成功）→ ShellExecute 打开（快捷方式/可执行文件）；
/// - 否则有 AUMID → IApplicationActivationManager 激活（UWP / 商店应用 / 注册了 AUMID 的经典程序）；
/// - AUMID 本身是路径的（部分安装器把 AUMID 设成 exe 路径）→ 按路径启动；
/// - 其余（帮助/网页等壳条目）丢弃。
fn classify(fs_path: Option<&str>, parsing: &str, aumid: &str) -> Option<LaunchTarget> {
    if let Some(path) = fs_path.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(LaunchTarget::Path(path.to_string()));
    }
    let aumid = aumid.trim();
    if is_url(aumid) {
        return None;
    }
    if !aumid.is_empty() && !aumid.starts_with('{') {
        if looks_like_path(aumid) {
            return Some(LaunchTarget::Path(aumid.to_string()));
        }
        return Some(LaunchTarget::Aumid(aumid.to_string()));
    }
    let parsing = parsing.trim();
    if looks_like_path(parsing) {
        return Some(LaunchTarget::Path(parsing.to_string()));
    }
    if parsing.contains('!') && !parsing.starts_with('{') {
        return Some(LaunchTarget::Aumid(parsing.to_string()));
    }
    None
}

/// 像不像磁盘路径（`X:\...` 或 UNC）。
fn looks_like_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    (bytes.len() > 2 && bytes[1] == b':') || text.starts_with("\\\\")
}

fn is_url(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("ms-")
}

unsafe fn pwstr_take(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s
}

// ---------------- 图标 ----------------

unsafe fn item_icon(item: &IShellItem2) -> Option<RgbaImage> {
    let factory: IShellItemImageFactory = item.cast().ok()?;
    let hbmp = factory
        .GetImage(
            SIZE {
                cx: ICON_SIZE,
                cy: ICON_SIZE,
            },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        )
        .ok()?;
    let image = hbitmap_to_rgba(hbmp);
    let _ = DeleteObject(HGDIOBJ(hbmp.0));
    image
}

/// 从真实文件/快捷方式路径取 32×32 图标（`SHGetFileInfoW`，比缩略图服务快一个量级）。
unsafe fn icon_from_file(path: &str) -> Option<RgbaImage> {
    let wide = crate::winutil::wide(path);
    let mut info = SHFILEINFOW::default();
    let flags = SHGFI_FLAGS(SHGFI_ICON.0 | SHGFI_LARGEICON.0);
    let ok = SHGetFileInfoW(
        PCWSTR(wide.as_ptr()),
        Default::default(),
        Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32,
        flags,
    );
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let hicon = info.hIcon;
    let mut icon_info = ICONINFO::default();
    let image = if GetIconInfo(hicon, &mut icon_info).is_ok() && !icon_info.hbmColor.is_invalid() {
        let image = hbitmap_to_rgba(icon_info.hbmColor);
        let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
        if !icon_info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
        }
        image
    } else {
        None
    };
    let _ = DestroyIcon(hicon);
    image
}

unsafe fn hbitmap_to_rgba(hbmp: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<RgbaImage> {
    let mut bmp = BITMAP::default();
    if GetObjectW(
        HGDIOBJ(hbmp.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bmp as *mut _ as *mut c_void),
    ) == 0
    {
        return None;
    }
    let width = bmp.bmWidth.max(0) as u32;
    let height = bmp.bmHeight.max(0) as u32;
    if width == 0 || height == 0 || width > 512 || height > 512 {
        return None;
    }

    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // 负值 = 自上而下，省一次翻转
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: width * height * 4,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let hdc = GetDC(None);
    let lines = GetDIBits(
        hdc,
        hbmp,
        0,
        height,
        Some(pixels.as_mut_ptr() as *mut c_void),
        &mut info,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, hdc);
    if lines == 0 {
        return None;
    }

    // 无 alpha（图标服务偶尔给全 0）→ 视为不透明
    let has_alpha = pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0);
    if !has_alpha {
        for px in pixels.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    } else {
        // 预乘检测：任一通道 > alpha 说明不是预乘，反之做反预乘
        let straight = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[3] != 0 && (px[0] > px[3] || px[1] > px[3] || px[2] > px[3]));
        if !straight {
            for px in pixels.as_chunks_mut::<4>().0 {
                let a = px[3] as u32;
                if a == 0 {
                    continue;
                }
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255) / a).min(255) as u8;
                }
            }
        }
    }
    // BGRA → RGBA
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }

    Some(RgbaImage {
        width,
        height,
        pixels,
    })
}

// ---------------- 开始菜单目录 / 额外目录 ----------------

fn scan_start_menu() -> Vec<RawApp> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(pd) = std::env::var("ProgramData") {
        roots.push(PathBuf::from(pd).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        roots.push(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    let mut out = Vec::new();
    for root in roots {
        scan_files(&root, 0, 8, &mut out);
    }
    out
}

const SCAN_EXTS: &[&str] = &["lnk", "url", "exe", "bat", "cmd", "appref-ms"];

fn scan_files(dir: &Path, depth: u32, max_depth: u32, out: &mut Vec<RawApp>) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_files(&path, depth + 1, max_depth, out);
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let ext = ext.to_lowercase();
        if !SCAN_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let name = stem.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let path_str = path.to_string_lossy().into_owned();
        out.push(RawApp {
            name,
            target: LaunchTarget::Path(path_str.clone()),
            icon_job: Some(IconJob::File(path_str)),
        });
    }
}

// ---------------- UWP 激活 ----------------

fn activate_aumid(aumid: &str) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let manager: IApplicationActivationManager = CoCreateInstance(
            &ApplicationActivationManager,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
        .map_err(|e| format!("创建 UWP 激活管理器失败（启动 {aumid} 不可用）：{e}"))?;
        let id = HSTRING::from(aumid);
        match manager.ActivateApplication(PCWSTR(id.as_ptr()), PCWSTR::null(), AO_NONE) {
            Ok(_) => Ok(()),
            Err(e) => {
                // 兜底：让资源管理器按 AppsFolder 打开（对注册了 AUMID 的经典程序更可靠）
                dhrust::warn!(
                    "ActivateApplication({aumid}) 失败：{e}；改用 explorer shell:AppsFolder 兜底"
                );
                let arg = format!("shell:AppsFolder\\{aumid}");
                std::process::Command::new("explorer.exe")
                    .arg(&arg)
                    .spawn()
                    .map(|_| ())
                    .map_err(|e2| {
                        format!(
                            "启动 {aumid} 失败：ActivateApplication 报 {e}，explorer 兜底也失败：{e2}"
                        )
                    })
            }
        }
    }
}

// ---------------- 拼音 ----------------

/// 生成「全拼」与「首字母」两套检索串（非中文字符原样保留，便于中英混排名称）。
fn pinyin_of(name: &str) -> (String, String) {
    let mut full = String::with_capacity(name.len() * 2);
    let mut short = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch.to_pinyin() {
            Some(py) => {
                full.push_str(py.plain());
                short.push_str(py.first_letter());
            }
            None => {
                for lc in ch.to_lowercase() {
                    full.push(lc);
                    short.push(lc);
                }
            }
        }
    }
    (full, short)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_variants() {
        let (full, short) = pinyin_of("微信输入法");
        assert_eq!(full, "weixinshurufa");
        assert_eq!(short, "wxsrf");

        let (full, short) = pinyin_of("Visual Studio Code");
        assert_eq!(full, "visual studio code");
        assert_eq!(short, "visual studio code");
    }

    #[test]
    fn classify_targets() {
        assert_eq!(
            classify(
                Some("C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\Foo.lnk"),
                "{6D809377-6AF0-444B-8957-A3773F02200E}\\Foo\\Foo.lnk",
                ""
            ),
            Some(LaunchTarget::Path(
                "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\Foo.lnk".into()
            )),
            "有真实路径时优先用路径启动"
        );
        assert_eq!(
            classify(None, "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App", "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"),
            Some(LaunchTarget::Aumid(
                "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into()
            )),
            "打包应用用 AUMID"
        );
        assert_eq!(
            classify(None, "Pkg_123!App", ""),
            Some(LaunchTarget::Aumid("Pkg_123!App".into())),
            "解析名本身就是 AUMID 时也能启动"
        );
        assert_eq!(
            classify(None, "D:\\Tor\\firefox.exe", "D:\\Tor\\firefox.exe"),
            Some(LaunchTarget::Path("D:\\Tor\\firefox.exe".into())),
            "AUMID 被安装器写成 exe 路径时仍按路径启动"
        );
        assert_eq!(
            classify(None, "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\help.chm", ""),
            None,
            "既无路径也无 AUMID 的壳命名空间条目（帮助/网页）丢弃"
        );
        assert_eq!(classify(None, "", ""), None);
    }
}
