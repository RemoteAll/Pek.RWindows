//! 配置：`Config/Settings.toml`（TOML 可写注释；缺失自动生成、缺键补齐、损坏备份重建）。
//!
//! 读写统一走 `dhrust::config::TomlConfig<T>`——不再各项目自写模板替换/合并逻辑。

use dhrust::config::TomlConfig;
use serde::{Deserialize, Serialize};

/// 配置文件路径（相对 exe 目录，启动时会切到 exe 目录）。
pub const CONFIG_PATH: &str = "Config/Settings.toml";

/// 配置模板：首次运行生成该文件，字段缺失时也按它补齐（含注释）。
const TEMPLATE: &str = r#"# Pek 启动器配置（修改后重启程序生效）

# 全局唤起热键：Ctrl / Alt / Shift / Win 组合 + 键名，例如 Alt+Space、Ctrl+Alt+P、Ctrl+Shift+F2
Hotkey = "Alt+Space"

# 搜索结果最多显示条数（1~200）
MaxResults = 60

# 窗口尺寸（逻辑像素）
WindowWidth = 720.0
WindowHeight = 460.0

# 失去焦点时自动隐藏（更接近开始菜单搜索的手感）
AutoHideOnBlur = true

# 额外扫描目录：除开始菜单应用外，再把这里的 .lnk / .exe / .url / .bat 当成应用（含子目录）
ExtraScanDirs = []

# 不参与搜索的应用名（不区分大小写，精确匹配）
IgnoreNames = []
"#;

/// 用户配置（字段名与 TOML 键一致：PascalCase）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Settings {
    pub hotkey: String,
    pub max_results: usize,
    pub window_width: f32,
    pub window_height: f32,
    pub auto_hide_on_blur: bool,
    pub extra_scan_dirs: Vec<String>,
    pub ignore_names: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Space".to_string(),
            max_results: 60,
            window_width: 720.0,
            window_height: 460.0,
            auto_hide_on_blur: true,
            extra_scan_dirs: Vec::new(),
            ignore_names: Vec::new(),
        }
    }
}

impl Settings {
    /// 校正明显越界的值（配置是给运维手改的，容忍度要高，但运行时要有边界）。
    fn sanitize(&mut self) {
        if self.hotkey.trim().is_empty() {
            self.hotkey = Settings::default().hotkey;
        }
        self.max_results = self.max_results.clamp(1, 200);
        self.window_width = self.window_width.clamp(420.0, 2400.0);
        self.window_height = self.window_height.clamp(240.0, 1600.0);
        self.extra_scan_dirs
            .retain(|d| !d.trim().is_empty());
        self.ignore_names.retain(|n| !n.trim().is_empty());
    }
}

/// 加载配置（缺失生成模板；解析问题写入日志，不 panic）。
pub fn load() -> Settings {
    let mut inner = TomlConfig::<Settings>::load(CONFIG_PATH, TEMPLATE);
    for note in inner.take_notes() {
        dhrust::info!("配置：{note}");
    }
    let mut settings = inner.into_value();
    settings.sanitize();
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_into_defaults() {
        let s: Settings = toml::from_str(TEMPLATE).expect("模板必须是合法 TOML");
        assert_eq!(s.hotkey, "Alt+Space");
        assert_eq!(s.max_results, 60);
    }

    #[test]
    fn sanitize_clamps_values() {
        let mut s = Settings {
            max_results: 0,
            window_width: 1.0,
            hotkey: "   ".into(),
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.max_results, 1);
        assert_eq!(s.window_width, 420.0);
        assert_eq!(s.hotkey, "Alt+Space");
    }

    #[test]
    fn json_migration_is_not_needed_but_serde_roundtrip_works() {
        let s = Settings::default();
        let text = toml::to_string(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.max_results, s.max_results);
    }
}
