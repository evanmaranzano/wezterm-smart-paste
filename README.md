# wezterm-smart-paste

WezTerm(Windows / WSL)智能粘贴:一个 ~8ms 的 Rust 单文件剪贴板分类器 + Lua 回调,让 `Ctrl+V` 按剪贴板内容自动分发——**文件 → 带引号路径,截图 → 存 PNG 并注入路径(CLI agent 直接附图),文本 → 普通粘贴**。

## 为什么需要它

终端到应用只有文本通道,WezTerm 的 `PasteFrom` 只认文本格式:

- 资源管理器复制的文件粘贴不出来(没有 CF_HDROP→路径的转换);
- `Win+Shift+S` 截图后粘贴毫无反应——Windows Terminal 1.22+ 会在剪贴板无文本时发送空 bracketed paste 序列通知 agent 自行读剪贴板,WezTerm 没有等价机制,粘贴事件被整个吞掉。

本项目在 WezTerm 侧补齐这条链路:按一次 `Ctrl+V`,Lua 回调调用 `smart-paste.exe` 分类剪贴板,再按结果分发。

## 分支与耗时

| 剪贴板内容 | 行为 | 耗时 |
|---|---|---|
| 文件(CF_HDROP,微信/Outlook 虚拟文件) | 带引号完整路径(POSIX 引号转义) | ~8ms |
| 纯图片(PNG/DIB/Bitmap,无任何文本格式) | 存 `%TEMP%\smart-paste-<时间戳>.png`,以 bracketed paste 注入 `/mnt/c/...` 路径 | ~40-80ms(内置 GDI+ 编码,无 pwsh) |
| 文本,或文本+图片并存(如 Excel) | WezTerm 原生 `PasteFrom`(与 Windows Terminal 一致) | ~8ms |

图片分支优先尝试剪贴板原生 PNG/JPEG/WebP 字节(原样落盘,零编码),其次 `CF_BITMAP` 经 GDI+ `GdipSaveImageToFile` 编码;两者都失败才退回裸 `IMAGE` 标记由 pwsh 兜底。**文本+图片并存时让位文本粘贴**,不误伤 Excel 复制。

omp、Claude Code 等 CLI agent 会把"粘贴的单个图片文件路径"自动加载为图片附件,因此注入路径即完成贴图。普通 shell 里注入的就是一个可直接使用的图片路径。

## 安装

### 1. 编译 exe(仅 std + Win32 FFI,无第三方依赖)

```powershell
rustc -O -C strip=symbols smart-paste.rs -o smart-paste.exe
```

放到 `%USERPROFILE%\.wezterm\smart-paste.exe`。gnu/msvc 工具链均可(gdiplus 通过运行时 `LoadLibrary` 加载,不依赖导入库)。

### 2. wezterm.lua 配置

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

local smart_paste_exe = wezterm.home_dir .. '/.wezterm/smart-paste.exe'

-- Windows 路径 -> WSL 路径(C:\Users\x -> /mnt/c/Users/x)
local function to_wsl_path(p)
  return (p:gsub('^([A-Za-z]):', function(d) return '/mnt/' .. d:lower() end):gsub('\\', '/'))
end

-- 剪贴板图片 -> 存 PNG -> 以 bracketed paste 注入 WSL 路径
-- (CLI agent 会把单个图片路径加载为附件;普通 shell 里就是贴出一个可用路径)
local function paste_clipboard_image(window, pane, winpath)
  local p = winpath
  if not p then
    local ok, stdout = wezterm.run_child_process {
      'pwsh.exe', '-NoProfile', '-NonInteractive', '-Command',
      [[Add-Type -AssemblyName System.Windows.Forms,System.Drawing
        $i = [System.Windows.Forms.Clipboard]::GetImage()
        if ($i) {
          $p = Join-Path $env:TEMP ('smart-paste-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff') + '.png')
          $i.Save($p, [System.Drawing.Imaging.ImageFormat]::Png)
          Write-Output $p
        }]],
    }
    p = ok and stdout and stdout:gsub('%s+$', '') or ''
  end
  if p:match('%S') then
    window:perform_action(act.SendString('\27[200~' .. to_wsl_path(p) .. '\27[201~'), pane)
  end
end

-- exe 输出契约:带引号路径 = 文件列表;IMAGE = 纯图片(需 Lua 存图);
-- IMAGE:<win路径> = exe 已存好图;空 = 文本/空,退回原生粘贴
local function paste_file_paths_or_text(window, pane, ok, stdout)
  local s = ok and stdout and stdout:gsub('%s+$', '') or ''
  local img_path = s:match('^IMAGE:(.*)$')
  if s == 'IMAGE' then
    paste_clipboard_image(window, pane, nil)
  elseif img_path and img_path:match('%S') then
    paste_clipboard_image(window, pane, img_path)
  elseif s:match('%S') then
    window:perform_action(act.SendString(s), pane)
  else
    window:perform_action(act.PasteFrom 'Clipboard', pane)
  end
end

local function smart_paste(window, pane)
  local ok, stdout = wezterm.run_child_process { smart_paste_exe }
  if ok then
    paste_file_paths_or_text(window, pane, ok, stdout)
    return
  end
  -- exe 缺失/损坏时的兜底(慢,但功能可用)
  local ok2, stdout2 = wezterm.run_child_process {
    'pwsh.exe', '-NoProfile', '-NonInteractive', '-Command',
    [[Add-Type -AssemblyName System.Windows.Forms,System.Drawing
      $cl = [System.Windows.Forms.Clipboard]
      if ($cl::ContainsFileDropList()) {
        ($cl::GetFileDropList() | ForEach-Object { "'" + $_.Replace("'", "''") + "'" }) -join ' '
      } elseif (-not $cl::ContainsText()) {
        $i = $cl::GetImage()
        if ($i) {
          $p = Join-Path $env:TEMP ('smart-paste-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff') + '.png')
          $i.Save($p, [System.Drawing.Imaging.ImageFormat]::Png)
          'IMAGE:' + $p
        }
      }]],
  }
  paste_file_paths_or_text(window, pane, ok2, stdout2)
end

config.keys = {
  { key = 'v', mods = 'CTRL', action = wezterm.action_callback(smart_paste) },
}
```

## 可选:omp 的兜底快捷键

若你的 wezterm 配置把 `Ctrl+V` 绑为终端级粘贴(拦截了发往应用的和弦),可给 omp 增加一个不会被拦截的图片粘贴键(`~/.omp/agent/keybindings.yml`):

```yaml
app.clipboard.pasteImage:
  - Ctrl+V
  - Alt+V
```

`app.clipboard.pasteImage` 由 omp 自己读剪贴板(图片优先、文本兜底),不依赖终端。

## 调试

每次运行追加一行到 `%USERPROFILE%\.wezterm\smart-paste.log`,记录命中的分支与剪贴板全部格式:

```
2026-10-14 17:07:28.894 branch=IMAGE_FILE formats=[DataObject,Bitmap,System.Drawing.Bitmap,Ole Private Data,DIB,DIBV5] -> C:\Users\...\Temp\smart-paste-20261014-170728894.png
```

## 已知限制

- 仅支持真实图片剪贴板生产者。.NET `DataObject` 延迟渲染的自定义 `PNG` 格式经 `GetClipboardData` 读不到字节(枚举可见但取值为空),此类对象会落到 GDI+/pwsh 兜底;真实截图工具(微信截图、Win+Shift+S、浏览器复制图片)都同时提供 `CF_BITMAP`/`CF_DIB`,不受影响。
- 图片经 `CF_BITMAP` 路径编码时 alpha 通道不保留(GDI+ 从 HBITMAP 建 32bppRGB);剪贴板带原生 PNG 字节时原样保留。
- `wezterm.run_child_process` 为同步调用,图片分支会阻塞约 40-80ms。

## License

MIT
