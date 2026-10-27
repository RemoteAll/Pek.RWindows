# Pek 启动器（Pek.RWindows）

Windows 11 自带搜索（左下角搜索框）经常搜不出应用时，用它顶上：**常驻托盘、热键唤出、拼音可搜、回车即开**。
整机应用清单来自开始菜单本体数据源（`shell:AppsFolder`），桌面程序与 UWP/商店应用一网打尽。

> 纯 Rust（eframe/egui + Win32），单 exe、无运行时依赖。

## 功能

- **托盘常驻**：启动后不出现在任务栏，只留右下角托盘图标（左键单击即唤出）。
- **全局热键**：默认 `Alt+Space`（可在配置里改，冲突时自动回退候选键并写日志）。
- **应用搜索**：模糊匹配 + **全拼 / 拼音首字母**（如 `wx` → 微信、`jsq` → 计算器）。
- **使用频率加权**：常用的应用自动靠前（统计在 `Data/usage.json`）。
- **渐进式加载**：应用列表几百毫秒内可用，图标在后台分批补齐，不卡输入。
- **失焦自动隐藏**：更接近开始菜单搜索的手感（可关）。
- **单实例**：重复运行只会唤起已运行的窗口。
- **开机自启**：托盘菜单一键开关（写 `HKCU\...\Run`，无需管理员）。

## 操作

| 操作 | 说明 |
| --- | --- |
| 热键 `Alt+Space` / 单击托盘 | 显示搜索框（居中到鼠标所在显示器） |
| 输入 | 应用名、全拼或拼音首字母 |
| `↑` `↓` | 选择结果（鼠标悬停也会选中） |
| `Enter` / 单击结果 | 启动应用并隐藏窗口 |
| `Esc` | 隐藏并把焦点还给之前的窗口 |
| 点击窗口外 | 自动隐藏 |
| 托盘右键菜单 | 显示搜索框 / 重新扫描应用 / 开机自启动 / 打开配置目录 / 退出 |

## 配置文件

`Config/Settings.toml`（首次运行自动生成，UTF-8、带注释；**修改后重启生效**）：

```toml
Hotkey = "Alt+Space"        # 热键：Ctrl/Alt/Shift/Win + 键名
MaxResults = 60             # 结果条数上限（1~200）
WindowWidth = 720.0         # 窗口尺寸（逻辑像素）
WindowHeight = 460.0
AutoHideOnBlur = true       # 失去焦点自动隐藏
ExtraScanDirs = []          # 额外扫描目录（便携软件的 .lnk/.exe/.url/.bat，含子目录）
IgnoreNames = []            # 不参与搜索的应用名（不区分大小写，精确匹配）
```

## 文件位置

全部位于 exe 同目录（启动时自动切换工作目录，注册表自启也不会落错地方）：

- `Config/Settings.toml`：配置
- `Log/yyyy_MM_dd.log`：运行日志（UTF-8）
- `Data/usage.json`：使用次数统计
- `Data/apps-dump.txt`：`--dump-apps` 导出的应用清单（排障用）

## 命令行

```powershell
.\pek-rwindows.exe                  # 正常启动（静默到托盘）
.\pek-rwindows.exe --autostart      # 开机自启用（行为同上）
.\pek-rwindows.exe --dump-apps      # 导出应用清单到 Data/apps-dump.txt 后退出（不开窗口）
.\pek-rwindows.exe --selftest       # 自检：显示→隐藏→显示→退出，验证托盘/热键通道
```

## 构建

```powershell
cargo check                 # 秒级检查
cargo build --profile fast  # 日常本地调试版
cargo build --release       # 发布版（fat LTO + 去符号）
cargo test                  # 单元测试（热键解析/拼音/搜索/图标编码等）
```

打包（自动升补丁版本号 → release 构建 → `dist/` 出 zip）：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\pack.ps1
```

## 实现要点

- **应用来源**：`shell:AppsFolder`（与开始菜单「所有应用」同一数据源，参照 PowerToys Run / Wox 的 UniversalApps 做法）；
  AppsFolder 不可用时回退扫描 `%ProgramData%` / `%APPDATA%` 开始菜单目录。
- **启动方式**：快捷方式/可执行文件 → `ShellExecute`；只有 AUMID 的打包应用 → `IApplicationActivationManager`
  （失败再退回 `explorer shell:AppsFolder\<AUMID>`）。
- **图标**：文件类走 `SHGetFileInfoW`（快）；打包应用走 `IShellItemImageFactory`；分批推送给界面。
- **隐藏策略**：窗口**始终可见，只把位置移到屏幕外**（-30000,-30000）。Windows 上隐藏窗口后不会再收到
  `WM_PAINT`，很容易出现「热键唤不出来」的假死；移到屏幕外可以保证任何时刻都能收到重绘与指令。
- **输入法**：启动器窗口关闭 IME（否则中文输入法会吞掉回车，第一次 Enter 只选候选字）；
  搜索用拼音/英文，需要中文时可粘贴。
- **资源占用**：隐藏空闲时 0 CPU（无轮询、无定时器）；内存约 140 MB（egui/GL 上下文 + 图标纹理）。

## 已知限制

- 只索引「开始菜单里能看到的应用」。没有开始菜单入口的程序，请用 `ExtraScanDirs` 指定目录。
- 关闭了 IME：中文输入法不能直接打汉字（拼音/首字母与粘贴不受影响）。
- UWP 应用激活依赖系统 `IApplicationActivationManager`；极少数商店应用可能需要用开始菜单打开一次后才会注册 AUMID。
