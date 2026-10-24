#![windows_subsystem = "windows"]

mod agent;
mod notch;
mod tools;

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
use tao::window::WindowBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use wry::{WebContext, WebViewBuilder, WebViewBuilderExtWindows};

const WIN_W: f64 = 620.0;
const WIN_H: f64 = 720.0;
const ATTACH_TOP: f64 = 140.0;
const STANDALONE_W: f64 = 420.0;

enum UserEvent {
    Js(String),
    Place { x: i32, y: i32, scale: f64 },
    Shape { x: f64, y: f64, w: f64, h: f64 },
    Focus,
    Menu(String),
}

#[derive(Default)]
struct WState {
    /// Interactive panel rect in physical *screen* coordinates.
    panel: Option<(i32, i32, i32, i32)>,
    origin: (i32, i32),
    scale: f64,
    hwnd: isize,
    dismissed: bool,
    /// Panel fully open: sit above Nusa so its invisible hit region doesn't block our input.
    above: bool,
    /// User focused our input; keep foreground/notch open until they click outside or press Esc.
    focused: bool,
}

type Proxy = Arc<Mutex<EventLoopProxy<UserEvent>>>;

fn send(p: &Proxy, e: UserEvent) {
    if let Ok(p) = p.lock() {
        let _ = p.send_event(e);
    }
}

fn js_emit(p: &Proxy, v: &Value) {
    let s = serde_json::to_string(v).unwrap_or_default();
    send(p, UserEvent::Js(format!("window.__nusa&&window.__nusa({s})")));
}

fn make_icon() -> Icon {
    let n = 32u32;
    let mut px = vec![0u8; (n * n * 4) as usize];
    let c = (n as f32 - 1.0) / 2.0;
    for y in 0..n {
        for x in 0..n {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let d = (dx * dx + dy * dy).sqrt();
            let i = ((y * n + x) * 4) as usize;
            let outer = (15.5 - d).clamp(0.0, 1.0);
            let ring = ((d - 9.0).clamp(0.0, 1.0)) * ((13.0 - d).clamp(0.0, 1.0));
            let dot = (4.5 - d).clamp(0.0, 1.0);
            let t = x as f32 / n as f32;
            if outer > 0.0 {
                let w = (ring + dot).min(1.0);
                // black disc with a cyan→violet ring and a white dot
                let (r, g, b) = (25.0 + (123.0 - 25.0) * t, 184.0 + (75.0 - 184.0) * t, 255.0);
                px[i] = (r * ring + 255.0 * dot).min(255.0) as u8;
                px[i + 1] = (g * ring + 255.0 * dot).min(255.0) as u8;
                px[i + 2] = (b * ring + 255.0 * dot).min(255.0) as u8;
                px[i + 3] = (outer * 255.0).max(w * 255.0) as u8;
            }
        }
    }
    Icon::from_rgba(px, n, n).expect("icon")
}

fn open_url(url: &str) {
    use std::os::windows::process::CommandExt;
    if url.starts_with("http://") || url.starts_with("https://") {
        let _ = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x0800_0000)
            .spawn();
    }
}

fn watcher(proxy: Proxy, ws: Arc<Mutex<WState>>, ready: Arc<AtomicBool>) {
    let mut last_layout = String::new();
    let mut last_place: Option<(i32, i32, i64)> = None;
    let mut last_in = Instant::now() - Duration::from_secs(60);
    let mut last_nt: Option<(notch::Notch, Instant)> = None;
    let mut visible = false;
    let mut was_open = false;
    loop {
        std::thread::sleep(Duration::from_millis(33));
        if ready.swap(false, Ordering::SeqCst) {
            last_layout.clear();
        }
        let n = match notch::find() {
            Some(nt) => {
                last_nt = Some((nt, Instant::now()));
                Some(nt)
            }
            None => last_nt.and_then(|(mut nt, t)| {
                if visible || notch::recently_reloaded(5000) || t.elapsed() < Duration::from_millis(3000) {
                    nt.kind = notch::Kind::Closed;
                    Some(nt)
                } else {
                    None
                }
            }),
        };

        let (hwnd, panel) = {
            let w = ws.lock().unwrap();
            (w.hwnd, w.panel)
        };
        if hwnd == 0 {
            continue;
        }
        let (cx, cy) = notch::cursor();
        let in_panel = panel.map(|(x, y, w, h)| cx >= x && cx < x + w && cy >= y && cy < y + h).unwrap_or(false);
        if in_panel {
            last_in = Instant::now();
        }

        let (mode, width, top) = match n {
            None => ("hidden", STANDALONE_W, ATTACH_TOP),
            Some(nt) => {
                let s = nt.scale;
                let center = (nt.rect.left + nt.rect.right) / 2;
                let place = (center - (WIN_W * s / 2.0).round() as i32, nt.rect.top, (s * 1000.0) as i64);
                if last_place != Some(place) {
                    last_place = Some(place);
                    send(&proxy, UserEvent::Place { x: place.0, y: place.1, scale: s });
                }
                let in_body = cx >= center - (240.0 * s) as i32
                    && cx <= center + (240.0 * s) as i32
                    && cy >= nt.rect.top
                    && cy < nt.rect.top + (195.0 * s) as i32;
                let mdown = notch::mouse_down();
                let open = match nt.kind {
                    notch::Kind::Open => true,
                    notch::Kind::Anim => in_body || in_panel,
                    _ => in_body && mdown,
                };
                if open && !was_open {
                    ws.lock().unwrap().dismissed = false;
                }
                was_open = open;
                let (dismissed, mut focused) = {
                    let w = ws.lock().unwrap();
                    (w.dismissed, w.focused)
                };
                let fg = notch::foreground() == hwnd;
                let reloading = notch::recently_reloaded(3000);
                if focused && !fg && !in_panel && !in_body && !reloading && !mdown {
                    ws.lock().unwrap().focused = false;
                    focused = false;
                }
                let active_below = !dismissed
                    && visible
                    && (in_panel || in_body || fg || focused || reloading || mdown || last_in.elapsed() < Duration::from_millis(850));
                if (open || active_below) && !dismissed {
                    ("attached", nt.body_w.max(STANDALONE_W), nt.bottom)
                } else {
                    ("hidden", STANDALONE_W, ATTACH_TOP)
                }
            }
        };
        visible = mode != "hidden";
        if visible {
            if let Some(nt) = n {
                if ws.lock().unwrap().above {
                    notch::keep_above(hwnd, nt.hwnd);
                } else {
                    notch::keep_below(hwnd, nt.hwnd);
                }
            }
        }
        let width = width.clamp(360.0, 560.0).round();
        let top = top.round();
        let key = format!("{mode}|{width}|{top}");
        if key != last_layout {
            last_layout = key;
            debug(&format!("layout {mode} w={width} top={top} fg={}", notch::foreground() == hwnd));
            js_emit(&proxy, &json!({"t":"layout","mode":mode,"top":top,"width":width}));
        }
    }
}

pub fn debug(s: &str) {
    if std::env::var_os("NUSA_DEBUG").is_none() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(agent::data_dir().join("debug.log")) {
        let _ = writeln!(f, "{} {s}", chrono::Local::now().format("%H:%M:%S%.3f"));
    }
}

/// Serve `%APPDATA%\NusaAI\attachments` at `http://nusa-att.localhost/`. WebView2 reads and
/// decodes those files in its own processes, so chat images never get copied into (or kept in)
/// this process as base64 / JSON strings.
fn map_attachments(webview: &wry::WebView) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2_3, COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW};
    use windows_core::{Interface, HSTRING};
    use wry::WebViewExtWindows;
    let dir = agent::att_dir();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(wv3) = webview.webview().cast::<ICoreWebView2_3>() else { return };
    let r = unsafe {
        wv3.SetVirtualHostNameToFolderMapping(
            &HSTRING::from(agent::ATT_HOST),
            &HSTRING::from(dir.as_os_str()),
            COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
        )
    };
    if let Err(e) = r {
        debug(&format!("attachments mapping failed: {e}"));
    }
}

fn main() {
    if !notch::single_instance() {
        return;
    }
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy: Proxy = Arc::new(Mutex::new(event_loop.create_proxy()));

    let window = WindowBuilder::new()
        .with_title("")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_visible(false)
        .with_focused(false)
        .with_skip_taskbar(true)
        .with_undecorated_shadow(false)
        .with_inner_size(LogicalSize::new(WIN_W, WIN_H))
        .build(&event_loop)
        .expect("window");
    let hwnd = window.hwnd();

    let ws = Arc::new(Mutex::new(WState { hwnd: 0, scale: window.scale_factor(), ..Default::default() }));
    let ready = Arc::new(AtomicBool::new(false));

    let p2 = proxy.clone();
    let controller = agent::Controller::new(Arc::new(move |v: Value| js_emit(&p2, &v)));

    let local = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(agent::data_dir);
    let mut ctx = WebContext::new(Some(local.join("NusaAI").join("WebView2")));

    let ipc_ctrl = controller.clone();
    let ipc_proxy = proxy.clone();
    let ipc_ws = ws.clone();
    let ipc_ready = ready.clone();
    let webview = WebViewBuilder::new_with_web_context(&mut ctx)
        .with_html(include_str!("../ui/index.html"))
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_focused(false)
        .with_devtools(cfg!(debug_assertions))
        .with_default_context_menus(false)
        .with_browser_accelerator_keys(false)
        .with_additional_browser_args("--renderer-process-limit=1 --disable-features=SpareRendererForSitePerProcess,AudioServiceOutOfProcess")
        .with_ipc_handler(move |req| {
            let Ok(msg) = serde_json::from_str::<Value>(req.body()) else { return };
            match msg["t"].as_str().unwrap_or("") {
                "shape" => send(
                    &ipc_proxy,
                    UserEvent::Shape {
                        x: msg["x"].as_f64().unwrap_or(0.0),
                        y: msg["y"].as_f64().unwrap_or(0.0),
                        w: msg["w"].as_f64().unwrap_or(0.0),
                        h: msg["h"].as_f64().unwrap_or(0.0),
                    },
                ),
                "dismiss" => {
                    let mut w = ipc_ws.lock().unwrap();
                    w.dismissed = true;
                    w.focused = false;
                }
                "z" => ipc_ws.lock().unwrap().above = msg["above"].as_bool().unwrap_or(false),
                "link" => open_url(msg["url"].as_str().unwrap_or("")),
                "focus" => {
                    ipc_ws.lock().unwrap().focused = true;
                    ipc_ctrl.handle(&json!({"t": "check_cfg"}));
                    send(&ipc_proxy, UserEvent::Focus);
                }
                "blur" => ipc_ws.lock().unwrap().focused = false,
                "ready" => {
                    ipc_ready.store(true, Ordering::SeqCst);
                    ipc_ctrl.handle(&msg);
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_millis(1200));
                        agent::trim_working_set();
                    });
                }
                _ => ipc_ctrl.handle(&msg),
            }
        })
        .build(&window)
        .expect("webview");

    map_attachments(&webview);
    notch::prepare_window(hwnd);
    ws.lock().unwrap().hwnd = hwnd;

    // Tray
    let menu = Menu::new();
    let m_new = MenuItem::with_id("new", "New conversation", true, None);
    let m_cfg = MenuItem::with_id("config", "Open config", true, None);
    let m_data = MenuItem::with_id("data", "Open data folder", true, None);
    let m_quit = MenuItem::with_id("quit", "Quit Nusa AI", true, None);
    let _ = menu.append_items(&[&m_new, &PredefinedMenuItem::separator(), &m_cfg, &m_data, &PredefinedMenuItem::separator(), &m_quit]);
    let tray = TrayIconBuilder::new()
        .with_icon(make_icon())
        .with_tooltip("Nusa AI")
        .with_menu(Box::new(menu))
        .build()
        .ok();
    let mp = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| send(&mp, UserEvent::Menu(e.id().0.clone()))));

    {
        let (p, w, r) = (proxy.clone(), ws.clone(), ready.clone());
        std::thread::spawn(move || watcher(p, w, r));
    }

    let ctx = Some(ctx);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _ = (&tray, &ctx);
        match event {
            Event::NewEvents(StartCause::Init) => {}
            Event::UserEvent(UserEvent::Js(s)) => {
                let _ = webview.evaluate_script(&s);
            }
            Event::UserEvent(UserEvent::Focus) => {
                notch::activate(hwnd);
                window.set_focus();
                let _ = webview.focus();
                let _ = webview.evaluate_script("document.getElementById('inp')?.focus()");
            }
            Event::UserEvent(UserEvent::Place { x, y, scale }) => {
                window.set_outer_position(PhysicalPosition::new(x, y));
                window.set_inner_size(PhysicalSize::new((WIN_W * scale).round() as u32, (WIN_H * scale).round() as u32));
                let mut w = ws.lock().unwrap();
                w.origin = (x, y);
                w.scale = scale;
                if w.panel.is_none() {
                    notch::set_region(hwnd, None);
                }
            }
            Event::UserEvent(UserEvent::Shape { x, y, w, h }) => {
                let mut st = ws.lock().unwrap();
                let s = window.scale_factor();
                st.scale = s;
                if w <= 0.0 || h <= 0.0 {
                    st.panel = None;
                    notch::set_region(hwnd, None);
                    agent::trim_working_set();
                } else {
                    let r = ((x * s).floor() as i32, (y * s).floor() as i32, (w * s).ceil() as i32, (h * s).ceil() as i32);
                    let o = window.outer_position().map(|p| (p.x, p.y)).unwrap_or(st.origin);
                    st.origin = o;
                    st.panel = Some((o.0 + r.0, o.1 + r.1, r.2, r.3));
                    notch::set_region(hwnd, Some(r));
                }
            }
            Event::UserEvent(UserEvent::Menu(id)) => match id.as_str() {
                "new" => controller.new_conversation(),
                "config" => agent::Config::open_in_notepad(),
                "data" => {
                    let _ = std::process::Command::new("explorer").arg(agent::data_dir()).spawn();
                }
                "quit" => {
                    tools::kill_all_bg();
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            _ => {}
        }
    });
}
