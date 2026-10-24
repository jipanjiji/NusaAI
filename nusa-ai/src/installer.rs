#![windows_subsystem = "windows"]

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_EXE: &[u8] = include_bytes!("C:/Users/Alvin/.cargo-targets/nusa-ai/release/nusa-ai.exe");
const WEBVIEW_DLL: &[u8] = include_bytes!("C:/Users/Alvin/.cargo-targets/nusa-ai/release/WebView2Loader.dll");

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MB_OK: u32 = 0x0000_0000;
const MB_YESNO: u32 = 0x0000_0004;
const MB_ICONINFORMATION: u32 = 0x0000_0040;
const MB_ICONQUESTION: u32 = 0x0000_0020;
const MB_ICONERROR: u32 = 0x0000_0010;
const IDYES: i32 = 6;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn msg_box(title: &str, text: &str, flags: u32) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;
    let t = wide(title);
    let m = wide(text);
    unsafe { MessageBoxW(std::ptr::null_mut(), m.as_ptr(), t.as_ptr(), flags) }
}

fn install_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
    base.join("Programs").join("NusaAI")
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
    base.join("NusaAI")
}

fn stop_running_instance() {
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "nusa-ai.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    std::thread::sleep(std::time::Duration::from_millis(400));
}

fn run_ps(script: &str) {
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

fn config_needs_setup(cfg_path: &Path) -> bool {
    let Ok(txt) = std::fs::read_to_string(cfg_path) else { return true };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return true };
    let k = v["api_key"].as_str().unwrap_or("").trim();
    let u = v["base_url"].as_str().unwrap_or("").trim();
    let m = v["model"].as_str().unwrap_or("").trim();
    k.is_empty() || u.is_empty() || m.is_empty()
}

fn install(silent: bool) -> Result<(), String> {
    let inst = install_dir();
    let data = data_dir();
    let cfg_path = data.join("config.json");

    if !silent {
        let prompt = format!(
            "Install Nusa AI ke komputer ini?\n\n\
             Lokasi program:\n  {}\n\n\
             Lokasi config AI:\n  {}\n\n\
             Shortcut akan dibuat di Start Menu, Desktop, dan Windows Startup.",
            inst.display(),
            cfg_path.display()
        );
        if msg_box("Nusa AI Setup", &prompt, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return Ok(());
        }
    }

    stop_running_instance();

    std::fs::create_dir_all(&inst).map_err(|e| format!("Gagal membuat folder instalasi: {e}"))?;
    std::fs::create_dir_all(data.join("conversations")).map_err(|e| format!("Gagal membuat folder data: {e}"))?;
    std::fs::create_dir_all(data.join("attachments")).map_err(|e| format!("Gagal membuat folder attachments: {e}"))?;

    let exe_path = inst.join("nusa-ai.exe");
    let dll_path = inst.join("WebView2Loader.dll");
    let uninst_path = inst.join("Uninstall-NusaAI.exe");

    std::fs::write(&exe_path, APP_EXE).map_err(|e| format!("Gagal menulis nusa-ai.exe: {e}"))?;
    std::fs::write(&dll_path, WEBVIEW_DLL).map_err(|e| format!("Gagal menulis WebView2Loader.dll: {e}"))?;
    if let Ok(self_exe) = std::env::current_exe() {
        let _ = std::fs::copy(&self_exe, &uninst_path);
    }

    if !cfg_path.exists() {
        let empty_cfg = "{\n  \"api_key\": \"\",\n  \"base_url\": \"\",\n  \"model\": \"\"\n}";
        let _ = std::fs::write(&cfg_path, empty_cfg);
    }

    // Create Start Menu & Desktop shortcuts + Startup & Uninstall registry entries
    let ps = format!(
        r#"
$exe = '{exe}'
$dir = '{dir}'
$uninst = '"{uninst}" --uninstall'
$ws = New-Object -ComObject WScript.Shell
foreach ($lnk in @(
    (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Nusa AI.lnk'),
    (Join-Path ([Environment]::GetFolderPath('Desktop')) 'Nusa AI.lnk')
)) {{
    $s = $ws.CreateShortcut($lnk)
    $s.TargetPath = $exe
    $s.WorkingDirectory = $dir
    $s.Description = 'Nusa AI Desktop Assistant'
    $s.IconLocation = "$exe,0"
    $s.Save()
}}
Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'NusaAI' -Value "`"$exe`"" -Force
$uk = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\NusaAI'
New-Item -Path $uk -Force | Out-Null
Set-ItemProperty -Path $uk -Name 'DisplayName' -Value 'Nusa AI'
Set-ItemProperty -Path $uk -Name 'DisplayVersion' -Value '0.1.0'
Set-ItemProperty -Path $uk -Name 'Publisher' -Value 'Nusa'
Set-ItemProperty -Path $uk -Name 'DisplayIcon' -Value $exe
Set-ItemProperty -Path $uk -Name 'InstallLocation' -Value $dir
Set-ItemProperty -Path $uk -Name 'UninstallString' -Value $uninst
Set-ItemProperty -Path $uk -Name 'NoModify' -Value 1 -Type DWord
Set-ItemProperty -Path $uk -Name 'NoRepair' -Value 1 -Type DWord
"#,
        exe = exe_path.display().to_string().replace('\'', "''"),
        dir = inst.display().to_string().replace('\'', "''"),
        uninst = uninst_path.display().to_string().replace('\'', "''"),
    );
    run_ps(&ps);

    // Launch Nusa AI
    let _ = Command::new(&exe_path).current_dir(&inst).spawn();

    if !silent {
        if config_needs_setup(&cfg_path) {
            let msg = format!(
                "Nusa AI berhasil di-install dan dijalankan!\n\n\
                 Sebelum menggunakan Nusa AI, silakan isi api_key, base_url, dan model Anda pada file config:\n\
                 {}\n\n\
                 (Anda juga bisa membukanya kapan saja lewat tombol Setup Config di dalam aplikasi atau ikon tray Nusa AI.)\n\n\
                 Buka config.json di Notepad sekarang?",
                cfg_path.display()
            );
            if msg_box("Nusa AI Setup — Selesai", &msg, MB_YESNO | MB_ICONINFORMATION) == IDYES {
                let _ = Command::new("notepad").arg(&cfg_path).spawn();
            }
        } else {
            msg_box(
                "Nusa AI Setup — Selesai",
                "Nusa AI berhasil di-install dan dijalankan di system tray!",
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
    Ok(())
}

fn uninstall(silent: bool) {
    let inst = install_dir();
    if !silent {
        let msg = format!(
            "Uninstall Nusa AI dari komputer ini?\n\n\
             Folder program yang akan dihapus:\n  {}\n\n\
             (Catatan: riwayat chat & config.json di %APPDATA%\\NusaAI tetap disimpan.)",
            inst.display()
        );
        if msg_box("Uninstall Nusa AI", &msg, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
    }

    stop_running_instance();

    let ps = r#"
Remove-Item (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Nusa AI.lnk') -Force -ErrorAction SilentlyContinue
Remove-Item (Join-Path ([Environment]::GetFolderPath('Desktop')) 'Nusa AI.lnk') -Force -ErrorAction SilentlyContinue
Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'NusaAI' -ErrorAction SilentlyContinue
Remove-Item -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\NusaAI' -Recurse -Force -ErrorAction SilentlyContinue
"#;
    run_ps(ps);

    let dir_str = inst.display().to_string();
    let _ = Command::new("cmd")
        .args(["/C", &format!("ping 127.0.0.1 -n 2 > nul & rmdir /s /q \"{dir_str}\"")])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    if !silent {
        msg_box("Uninstall Nusa AI", "Nusa AI berhasil di-uninstall.", MB_OK | MB_ICONINFORMATION);
    }
}

fn main() {
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<String> = std::env::args().collect();
    let silent = args.iter().any(|a| a == "--silent" || a == "/S" || a == "/s");
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default();
    let is_uninst = args.iter().any(|a| a == "--uninstall" || a == "/uninstall") || exe_name.contains("uninstall");

    if is_uninst {
        uninstall(silent);
    } else if let Err(e) = install(silent) {
        if !silent {
            msg_box("Nusa AI Setup — Error", &e, MB_OK | MB_ICONERROR);
        }
        std::process::exit(1);
    }
}
