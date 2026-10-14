// smart-paste.rs — WezTerm 智能粘贴的剪贴板分类器（Windows，Rust 单文件，无第三方依赖）
// 优先级：CF_HDROP 真实文件 -> 带引号完整路径
//         FileGroupDescriptorW / FileNameW（微信、Outlook 等虚拟文件）-> 带引号文件名
//         纯图片剪贴板（PNG/DIB/Bitmap，且无任何文本格式）-> 直接存 %TEMP%\smart-paste-*.png，输出 IMAGE:<win路径>
//           （wezterm 侧转 /mnt/c 路径以 bracketed paste 注入，CLI agent（omp/Claude Code 等）加载为图片附件；存图失败退回裸 IMAGE 走 pwsh 兜底）
//         其余（含 文本+图片 并存，如 Excel）-> 静默（wezterm 退回普通文本粘贴，与 Windows Terminal 一致）
// 每次运行追加日志到 %USERPROFILE%\.wezterm\smart-paste.log
// 始终退出 0；stdout 固定 UTF-8 字节（wezterm 按 UTF-8 解码）
// 编译：rustc -O -C strip=symbols smart-paste.rs -o smart-paste.exe（仅 std + Win32 FFI）

use std::fs::OpenOptions;
use std::io::Write;
use std::os::raw::c_void;
use std::ptr;

const CF_HDROP: u32 = 15;
const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_DIBV5: u32 = 17;

#[repr(C)]
struct SystemTime {
    year: u16,
    month: u16,
    _day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    millis: u16,
}

#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hwnd: *mut c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn GetClipboardData(format: u32) -> *mut c_void;
    fn EnumClipboardFormats(format: u32) -> u32;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    fn GetClipboardFormatNameW(format: u32, name: *mut u16, max: i32) -> i32;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn DragQueryFileW(drop: *mut c_void, idx: u32, buf: *mut u16, size: u32) -> u32;
}

extern "system" {
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalSize(mem: *mut c_void) -> usize;
    fn GetLocalTime(st: *mut SystemTime);
}

// —— 图片分支：exe 内直接落盘 PNG（优先剪贴板原生 PNG 字节，其次 GDI+ 编码），免去 pwsh ~500ms 启动 ——

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

// PNG 编码器 CLSID {557CF406-1A04-11D3-9A73-0000F81EF32E}
const CLSID_PNG_ENCODER: Guid = Guid {
    data1: 0x557CF406,
    data2: 0x1A04,
    data3: 0x11D3,
    data4: [0x9A, 0x73, 0x00, 0x00, 0xF8, 0x1E, 0xF3, 0x2E],
};

#[repr(C)]
struct GdiplusStartupInput {
    gdiplus_version: u32,
    debug_event_callback: *mut c_void,
    suppress_background_thread: i32,
    suppress_external_codecs: i32,
}

// gnu 工具链无 gdiplus 导入库也无 dlltool，改为运行时 LoadLibrary/GetProcAddress
struct Gdiplus {
    startup: unsafe extern "system" fn(*mut usize, *const GdiplusStartupInput, *mut c_void) -> i32,
    shutdown: unsafe extern "system" fn(usize),
    from_hbitmap: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> i32,
    dispose: unsafe extern "system" fn(*mut c_void) -> i32,
    save_to_file: unsafe extern "system" fn(*mut c_void, *const u16, *const Guid, *mut c_void) -> i32,
}

unsafe fn load_gdiplus() -> Option<Gdiplus> {
    unsafe fn sym<T: Copy>(h: *mut c_void, name: &'static [u8]) -> Option<T> {
        let p = GetProcAddress(h, name.as_ptr());
        if p.is_null() { None } else { Some(std::mem::transmute_copy(&p)) }
    }
    let h = LoadLibraryW(wide("gdiplus.dll").as_ptr());
    if h.is_null() {
        return None;
    }
    Some(Gdiplus {
        startup: sym(h, b"GdiplusStartup\0")?,
        shutdown: sym(h, b"GdiplusShutdown\0")?,
        from_hbitmap: sym(h, b"GdipCreateBitmapFromHBITMAP\0")?,
        dispose: sym(h, b"GdipDisposeImage\0")?,
        save_to_file: sym(h, b"GdipSaveImageToFile\0")?,
    })
}


#[link(name = "kernel32")]
extern "system" {
    fn GetTempPathW(len: u32, buf: *mut u16) -> u32;
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn quote_join(items: &[String]) -> String {
    items
        .iter()
        .map(|f| format!("'{}'", f.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 读取剪贴板某格式对应的内存块（拷贝出来，不带锁）
unsafe fn clipboard_bytes(fmt: u32) -> Option<Vec<u8>> {
    let h = GetClipboardData(fmt);
    if h.is_null() {
        return None;
    }
    let size = GlobalSize(h);
    if size == 0 {
        return None;
    }
    let p = GlobalLock(h);
    if p.is_null() {
        return None;
    }
    let v = std::slice::from_raw_parts(p as *const u8, size).to_vec();
    GlobalUnlock(h);
    Some(v)
}

/// FileGroupDescriptorW：cItems(4B) + N × FILEDESCRIPTORW(592B)，文件名在结构体偏移 72 处，260 个 WCHAR
fn parse_file_group_descriptor(buf: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    if buf.len() < 4 {
        return names;
    }
    let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    const DESC_SIZE: usize = 592;
    const NAME_OFFSET: usize = 72;
    for i in 0..count {
        let off = 4 + i * DESC_SIZE + NAME_OFFSET;
        if off + 520 > buf.len() {
            break;
        }
        let wide_chars: Vec<u16> = buf[off..off + 520]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = wide_chars.iter().position(|&c| c == 0).unwrap_or(wide_chars.len());
        let s = String::from_utf16_lossy(&wide_chars[..end]).trim().to_string();
        if !s.is_empty() {
            names.push(s);
        }
    }
    names
}

/// 宽字符内存块 -> String（截到首个 NUL）
fn wide_bytes_to_string(buf: &[u8]) -> String {
    let wide_chars: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = wide_chars.iter().position(|&c| c == 0).unwrap_or(wide_chars.len());
    String::from_utf16_lossy(&wide_chars[..end]).trim().to_string()
}

fn format_display_name(fmt: u32) -> String {
    match fmt {
        1 => "Text".into(),
        2 => "Bitmap".into(),
        8 => "DIB".into(),
        13 => "UnicodeText".into(),
        15 => "FileDrop".into(),
        16 => "Locale".into(),
        17 => "DIBV5".into(),
        _ if fmt >= 0xC000 => {
            let mut buf = [0u16; 256];
            let n = unsafe { GetClipboardFormatNameW(fmt, buf.as_mut_ptr(), 256) };
            if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                format!("#{fmt}")
            }
        }
        _ => format!("#{fmt}"),
    }
}

fn log_line(branch: &str, formats: &[String], items: &[String]) {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    let path = std::path::Path::new(&home)
        .join(".wezterm")
        .join("smart-paste.log");
    let mut st = SystemTime {
        year: 0, month: 0, _day_of_week: 0, day: 0,
        hour: 0, minute: 0, second: 0, millis: 0,
    };
    unsafe { GetLocalTime(&mut st) };
    let detail = if items.is_empty() {
        String::new()
    } else {
        format!(" -> {}", items.join(" ; "))
    };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} branch={} formats=[{}]{}\n",
        st.year, st.month, st.day, st.hour, st.minute, st.second, st.millis,
        branch, formats.join(","), detail
    );
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 尽力把剪贴板图片存成 %TEMP%\smart-paste-<时间戳>.png，返回 Windows 路径。
/// 前提：剪贴板已打开（分支 4 内调用）。
/// 1) 注册格式 PNG / image/png：字节本身就是 PNG，直接落盘（原样字节，最快）
/// 2) CF_BITMAP + GDI+ 编码（真压缩 PNG；截图工具基本都会同时放 CF_BITMAP）
/// 3) 都失败 -> None，wezterm 侧退回 pwsh 兜底
unsafe fn save_clipboard_png() -> Option<String> {
    let mut st = SystemTime {
        year: 0, month: 0, _day_of_week: 0, day: 0,
        hour: 0, minute: 0, second: 0, millis: 0,
    };
    GetLocalTime(&mut st);
    let mut tmp = [0u16; 260];
    let n = GetTempPathW(260, tmp.as_mut_ptr());
    if n == 0 || n as usize >= 259 {
        return None;
    }
    let dir = String::from_utf16_lossy(&tmp[..n as usize]);
    let name = format!(
        "{}smart-paste-{:04}{:02}{:02}-{:02}{:02}{:02}{:03}.png",
        dir, st.year, st.month, st.day, st.hour, st.minute, st.second, st.millis
    );

    // 1) 剪贴板原生图片字节（PNG/JPEG/WebP），原样落盘，跳过 GDI+ 编码
    //    Qt 类生产者（微信截图等）常同时放 image/png 等多种格式
    let stem = name.strip_suffix(".png").unwrap_or(&name);
    for (fmt_name, ext) in [
        ("PNG", ".png"), ("image/png", ".png"),
        ("image/jpeg", ".jpg"), ("JPEG", ".jpg"),
        ("image/webp", ".webp"),
    ] {
        let fmt = RegisterClipboardFormatW(wide(fmt_name).as_ptr());
        if fmt == 0 {
            continue;
        }
        if let Some(bytes) = clipboard_bytes(fmt) {
            let ok = match ext {
                ".png" => bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
                ".jpg" => bytes.starts_with(&[0xFF, 0xD8]),
                ".webp" => bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
                _ => false,
            };
            if ok {
                let fname = format!("{stem}{ext}");
                if std::fs::write(&fname, &bytes).is_ok() {
                    return Some(fname);
                }
            }
        }
    }

    // 2) GDI+ 编码
    let hbm = GetClipboardData(CF_BITMAP);
    if hbm.is_null() {
        return None;
    }
    let Some(g) = load_gdiplus() else { return None };
    let mut token: usize = 0;
    let input = GdiplusStartupInput {
        gdiplus_version: 1,
        debug_event_callback: ptr::null_mut(),
        suppress_background_thread: 0, // 置 1 会导致 GdipSaveImageToFile 失败（实测回归）
        suppress_external_codecs: 0,
    };
    if (g.startup)(&mut token, &input, ptr::null_mut()) != 0 {
        return None;
    }
    let mut bitmap: *mut c_void = ptr::null_mut();
    let mut saved = false;
    if (g.from_hbitmap)(hbm, ptr::null_mut(), &mut bitmap) == 0 {
        if (g.save_to_file)(bitmap, wide(&name).as_ptr(), &CLSID_PNG_ENCODER, ptr::null_mut()) == 0 {
            saved = true;
        }
        (g.dispose)(bitmap);
    }
    (g.shutdown)(token);
    if saved {
        Some(name)
    } else {
        let _ = std::fs::remove_file(&name); // 清掉可能的半成品
        None
    }
}

fn main() {
    let mut branch = "NONE";
    let mut out = String::new();
    let mut items: Vec<String> = Vec::new();
    let mut formats: Vec<String> = Vec::new();

    unsafe {
        if OpenClipboard(ptr::null_mut()) != 0 {
            let mut f = 0;
            loop {
                f = EnumClipboardFormats(f);
                if f == 0 {
                    break;
                }
                formats.push(format_display_name(f));
            }

            // 1) 资源管理器真实文件（CF_HDROP）
            let hdrop = GetClipboardData(CF_HDROP);
            if !hdrop.is_null() {
                let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, ptr::null_mut(), 0);
                let mut paths = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, ptr::null_mut(), 0);
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; (len + 1) as usize];
                    DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1);
                    paths.push(String::from_utf16_lossy(&buf[..len as usize]));
                }
                if !paths.is_empty() {
                    branch = "HDROP";
                    items = paths;
                    out = quote_join(&items);
                }
            }

            // 2) 微信/Outlook 虚拟文件：FileGroupDescriptorW（只有文件名）
            if out.is_empty() {
                let fmt = RegisterClipboardFormatW(wide("FileGroupDescriptorW").as_ptr());
                if fmt != 0 {
                    if let Some(buf) = clipboard_bytes(fmt) {
                        let names = parse_file_group_descriptor(&buf);
                        if !names.is_empty() {
                            branch = "VIRTUAL";
                            items = names;
                            out = quote_join(&items);
                        }
                    }
                }
            }

            // 3) FileNameW / FileName 兜底
            if out.is_empty() {
                for name in ["FileNameW", "FileName"] {
                    let fmt = RegisterClipboardFormatW(wide(name).as_ptr());
                    if fmt == 0 {
                        continue;
                    }
                    if let Some(buf) = clipboard_bytes(fmt) {
                        let s = if name.ends_with('W') {
                            wide_bytes_to_string(&buf)
                        } else {
                            String::from_utf8_lossy(&buf)
                                .trim_matches('\0')
                                .trim()
                                .to_string()
                        };
                        if !s.is_empty() {
                            branch = "VIRTUAL";
                            items = vec![s];
                            out = quote_join(&items);
                            break;
                        }
                    }
                }
            }

            // 4) 纯图片剪贴板（Win+Shift+S 截图等；文本+图片并存时让位给文本粘贴）
            if out.is_empty() {
                let has_text = IsClipboardFormatAvailable(CF_UNICODETEXT) != 0
                    || IsClipboardFormatAvailable(CF_TEXT) != 0;
                let custom_img = ["PNG", "image/png", "image/jpeg", "JPEG", "image/webp"]
                    .iter()
                    .any(|n| {
                        let f = RegisterClipboardFormatW(wide(n).as_ptr());
                        f != 0 && IsClipboardFormatAvailable(f) != 0
                    });
                let has_image = IsClipboardFormatAvailable(CF_BITMAP) != 0
                    || IsClipboardFormatAvailable(CF_DIB) != 0
                    || IsClipboardFormatAvailable(CF_DIBV5) != 0
                    || custom_img;
                if has_image && !has_text {
                    match save_clipboard_png() {
                        Some(p) => {
                            branch = "IMAGE_FILE";
                            items = vec![p.clone()];
                            out = format!("IMAGE:{p}");
                        }
                        None => {
                            branch = "IMAGE";
                            out = "IMAGE".into();
                        }
                    }
                }
            }

            CloseClipboard();
        }
    }

    log_line(branch, &formats, &items);
    print!("{out}");
    let _ = std::io::stdout().flush();
}
