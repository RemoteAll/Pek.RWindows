# 打包脚本：自动递增补丁版本 → release 构建 → dist/ 生成 zip。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\pack.ps1           # 升补丁版本后打包
#   powershell -ExecutionPolicy Bypass -File scripts\pack.ps1 -NoBump   # 保持当前版本
param(
    [switch]$NoBump
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (Get-Process -Name 'pek-rwindows' -ErrorAction SilentlyContinue) {
    throw '检测到 Pek 启动器正在运行：请先从托盘菜单退出（或结束 pek-rwindows 进程），否则 exe 被占用无法构建。'
}

$cargoToml = Join-Path $root 'Cargo.toml'
$text = [System.IO.File]::ReadAllText($cargoToml)
$m = [regex]::Match($text, '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"')
if (-not $m.Success) { throw 'Cargo.toml 里找不到 version = "x.y.z"' }

$major = [int]$m.Groups[1].Value
$minor = [int]$m.Groups[2].Value
$patch = [int]$m.Groups[3].Value
if (-not $NoBump) { $patch++ }
$version = "$major.$minor.$patch"

if (-not $NoBump) {
    $newText = $text.Substring(0, $m.Index) + "version = `"$version`"" + $text.Substring($m.Index + $m.Length)
    [System.IO.File]::WriteAllText($cargoToml, $newText, (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "版本号已递增为 $version"
} else {
    Write-Host "保持当前版本 $version（-NoBump）"
}

Write-Host '构建 release（fat LTO，首次约 2~4 分钟）...'
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build --release 失败' }

$exe = Join-Path $root 'target\release\pek-rwindows.exe'
if (-not (Test-Path $exe)) { throw "找不到构建产物：$exe" }

$distDir = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $distDir | Out-Null
$stage = Join-Path $distDir "PekLauncher_$version"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item $exe $stage
Copy-Item (Join-Path $root 'README.md') $stage

$zip = Join-Path $distDir "PekLauncher_v${version}_win-x64.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -CompressionLevel Optimal
Remove-Item $stage -Recurse -Force

$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
$size = (Get-Item $zip).Length
Write-Host ''
Write-Host "打包完成：$zip"
Write-Host "版本：$version"
Write-Host "大小：$size 字节"
Write-Host "SHA256：$hash"
