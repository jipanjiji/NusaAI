use crate::tools;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(tools::home);
    let d = base.join("NusaAI");
    let _ = std::fs::create_dir_all(d.join("conversations"));
    d
}

fn conv_dir() -> PathBuf {
    data_dir().join("conversations")
}

pub fn att_dir() -> PathBuf {
    data_dir().join("attachments")
}

/// Virtual host that WebView2 maps straight onto `att_dir()` (see main.rs). Images are then
/// streamed and decoded by the WebView2 processes from disk; their bytes never pass through
/// (or stay resident in) this process.
pub const ATT_HOST: &str = "nusa-att.localhost";

/// Ask the OS to reclaim unused pages from our private working set and our WebView2 child processes.
pub fn trim_working_set() {
    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetCurrentProcessId() -> u32;
        fn SetProcessWorkingSetSize(h: isize, min: usize, max: usize) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snap: isize, pe: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snap: isize, pe: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(h: isize) -> i32;
    }
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
        let me = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(0x0000_0002, 0); // TH32CS_SNAPPROCESS
        if snap == -1 || snap == 0 {
            return;
        }
        let mut procs: Vec<(u32, u32)> = Vec::with_capacity(64);
        let mut pe: ProcessEntry32W = std::mem::zeroed();
        pe.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                procs.push((pe.th32_process_id, pe.th32_parent_process_id));
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        let mut tree = vec![me];
        let mut idx = 0;
        while idx < tree.len() {
            let p = tree[idx];
            idx += 1;
            for &(cid, ppid) in &procs {
                if ppid == p && !tree.contains(&cid) {
                    tree.push(cid);
                }
            }
        }
        // PROCESS_SET_QUOTA (0x0100) | PROCESS_QUERY_LIMITED_INFORMATION (0x1000)
        for &pid in tree.iter().skip(1) {
            let h = OpenProcess(0x0100 | 0x1000, 0, pid);
            if h != 0 {
                let _ = SetProcessWorkingSetSize(h, usize::MAX, usize::MAX);
                let _ = CloseHandle(h);
            }
        }
    }
}

/// `Some(file name)` if `p` lives in (or has a matching file inside) `att_dir()`.
fn att_file_name(p: &str) -> Option<String> {
    let path = std::path::Path::new(p);
    let fname = path.file_name()?.to_str()?;
    if fname.is_empty() {
        return None;
    }
    let dir = att_dir();
    if dir.join(fname).exists() {
        return Some(fname.to_string());
    }
    let parent = path.parent()?;
    if parent == dir.as_path() || parent.to_string_lossy().eq_ignore_ascii_case(&dir.to_string_lossy()) {
        return Some(fname.to_string());
    }
    None
}

fn att_url(name: &str) -> String {
    let mut u = format!("http://{ATT_HOST}/");
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            u.push(b as char);
        } else {
            u.push_str(&format!("%{b:02X}"));
        }
    }
    u
}

static MIGRATED_FLAG: AtomicBool = AtomicBool::new(false);

/// Drop inline base64 (`data`) from stored image entries once the bytes are safely on disk in
/// `att_dir()`. Legacy entries whose file is missing are written out first, so nothing is lost.
/// Returns true if anything changed.
fn strip_inline_image(im: &mut Value) -> bool {
    let path = im["path"].as_str().unwrap_or("").to_string();
    let dir = att_dir();
    if let Some(fname) = std::path::Path::new(&path).file_name().and_then(|n| n.to_str()).filter(|s| !s.is_empty()) {
        let in_dir = dir.join(fname);
        if in_dir.exists() {
            let mut changed = false;
            let canon = in_dir.to_string_lossy().into_owned();
            if path != canon {
                im["path"] = json!(canon);
                changed = true;
            }
            if im.get("data").is_some() {
                if let Some(o) = im.as_object_mut() {
                    o.remove("data");
                }
                changed = true;
            }
            return changed;
        }
    }
    let data = im["data"].as_str().unwrap_or("");
    if data.is_empty() {
        return false;
    }
    let in_att = att_file_name(&path).is_some();
    if !in_att {
        return false;
    }
    if !std::path::Path::new(&path).exists() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let b64 = data.split_once(";base64,").map(|x| x.1).unwrap_or(data);
        let Ok(bytes) = STANDARD.decode(b64.trim()) else { return false };
        let _ = std::fs::create_dir_all(&dir);
        if std::fs::write(&path, &bytes).is_err() {
            return false;
        }
    }
    if let Some(o) = im.as_object_mut() {
        o.remove("data");
    }
    true
}

/// Stream-deserialize `messages` and strip inline base64 per message as it is read, so opening a
/// legacy multi-photo conversation never holds more than a single message's base64 in memory.
fn deserialize_messages<'de, D>(de: D) -> Result<Vec<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct MsgVisitor;
    impl<'de> serde::de::Visitor<'de> for MsgVisitor {
        type Value = Vec<Value>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a sequence of messages")
        }
        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut out = Vec::new();
            while let Some(mut m) = seq.next_element::<Value>()? {
                if let Some(imgs) = m.get_mut("images").and_then(|v| v.as_array_mut()) {
                    for im in imgs.iter_mut() {
                        if strip_inline_image(im) {
                            MIGRATED_FLAG.store(true, Ordering::Relaxed);
                        }
                    }
                }
                out.push(m);
            }
            Ok(out)
        }
    }
    de.deserialize_seq(MsgVisitor)
}

/// Load an image as a `data:` URL only when it is actually needed (API request), then let it go.
fn image_data_url(im: &Value) -> Option<String> {
    if let Some(d) = im["data"].as_str().filter(|d| !d.is_empty()) {
        return Some(d.to_string());
    }
    let p = std::path::Path::new(im["path"].as_str()?);
    let bytes = std::fs::read(p).ok()?;
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    };
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    Some(format!("data:{mime};base64,{}", STANDARD.encode(&bytes)))
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Config {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("config.json")
    }
    pub fn load() -> Config {
        let def = Config {
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
        };
        let p = Self::path();
        let mut c = match std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Config>(&t).ok()) {
            Some(c) => c,
            None => {
                let _ = std::fs::write(&p, serde_json::to_string_pretty(&def).unwrap());
                def
            }
        };
        if let Ok(k) = std::env::var("NUSA_API_KEY").or_else(|_| std::env::var("OPENAI_API_KEY")) {
            if !k.trim().is_empty() {
                c.api_key = k.trim().to_string();
            }
        }
        c
    }
    pub fn is_configured(&self) -> bool {
        !self.api_key.trim().is_empty()
            && !self.base_url.trim().is_empty()
            && !self.model.trim().is_empty()
    }
    pub fn open_in_notepad() {
        let _ = Self::load();
        let _ = std::process::Command::new("notepad").arg(Self::path()).spawn();
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub created: i64,
    pub updated: i64,
    #[serde(default, deserialize_with = "deserialize_messages")]
    pub messages: Vec<Value>,
    #[serde(default)]
    pub todos: Vec<Value>,
}

const UI_PAGE: usize = 24;

impl Conversation {
    fn new() -> Self {
        let now = chrono::Local::now();
        Conversation {
            id: now.format("%Y%m%d-%H%M%S-%3f").to_string(),
            title: String::new(),
            created: now.timestamp(),
            updated: now.timestamp(),
            messages: vec![],
            todos: vec![],
        }
    }
    fn save(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        self.updated = chrono::Local::now().timestamp();
        self.write();
    }
    fn write(&self) {
        if self.messages.is_empty() {
            return;
        }
        let p = conv_dir().join(format!("{}.json", self.id));
        let tmp = p.with_extension("tmp");
        if let Ok(f) = std::fs::File::create(&tmp) {
            let mut w = std::io::BufWriter::new(f);
            if serde_json::to_writer(&mut w, self).is_ok() && w.into_inner().is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
    fn load(id: &str) -> Option<Self> {
        let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        MIGRATED_FLAG.store(false, Ordering::Relaxed);
        let f = std::fs::File::open(conv_dir().join(format!("{safe}.json"))).ok()?;
        let c: Conversation = serde_json::from_reader(BufReader::new(f)).ok()?;
        if MIGRATED_FLAG.swap(false, Ordering::Relaxed) {
            // Rewrite migrated file without touching `updated` so history order is preserved.
            c.write();
            trim_working_set();
        }
        Some(c)
    }
}

struct Shared {
    conv: Conversation,
    running: bool,
    cancel: Arc<AtomicBool>,
    partial: String,
    pending: Option<(String, mpsc::Sender<Value>)>,
    allow_all: bool,
}

#[derive(Clone)]
pub struct Controller {
    emit: Emit,
    st: Arc<Mutex<Shared>>,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

impl Controller {
    pub fn new(emit: Emit) -> Self {
        // Resume the most recent conversation if it is fresh (< 12h).
        let mut conv = Conversation::new();
        if let Some(first) = list_history().first() {
            let age = chrono::Local::now().timestamp() - first["updated"].as_i64().unwrap_or(0);
            if age < 12 * 3600 {
                if let Some(mut c) = Conversation::load(first["id"].as_str().unwrap_or("")) {
                    if !c.todos.is_empty() {
                        c.todos = tools::sync_todos_to_nusa(&c.todos);
                        c.save();
                    }
                    conv = c;
                }
            }
        }
        Controller {
            emit,
            st: Arc::new(Mutex::new(Shared {
                conv,
                running: false,
                cancel: Arc::new(AtomicBool::new(false)),
                partial: String::new(),
                pending: None,
                allow_all: false,
            })),
        }
    }

    pub fn handle(&self, msg: &Value) {
        match msg["t"].as_str().unwrap_or("") {
            "ready" => self.push_conv(),
            "config" => Config::open_in_notepad(),
            "check_cfg" => (self.emit)(json!({"t": "cfg", "ok": Config::load().is_configured()})),
            "send" => {
                if !Config::load().is_configured() {
                    (self.emit)(json!({"t": "setup"}));
                    return;
                }
                let text = s(msg, "text");
                let atts = msg["attachments"].as_array().cloned().unwrap_or_default();
                if !text.trim().is_empty() || !atts.is_empty() {
                    self.send(text, atts);
                }
            }
            "stop" => self.stop(),
            "new" => {
                self.stop();
                let mut st = self.st.lock().unwrap();
                st.conv = Conversation::new();
                st.allow_all = false;
                drop(st);
                self.push_conv();
            }
            "history" => (self.emit)(json!({"t":"history","items":list_history()})),
            "open" => {
                let id = s(msg, "id");
                if let Some(c) = Conversation::load(&id) {
                    self.stop();
                    let mut st = self.st.lock().unwrap();
                    st.conv = c;
                    st.allow_all = false;
                    drop(st);
                    self.push_conv();
                    trim_working_set();
                }
            }
            "more" => {
                let before = msg["before"].as_u64().unwrap_or(0) as usize;
                let st = self.st.lock().unwrap();
                let all = ui_items(&st.conv.messages);
                let end = before.min(all.len());
                let start = end.saturating_sub(UI_PAGE);
                let items: Vec<Value> = all.into_iter().skip(start).take(end - start).collect();
                let id = st.conv.id.clone();
                drop(st);
                (self.emit)(json!({"t": "more", "id": id, "items": items, "start": start}));
            }
            "delete" => {
                let id = s(msg, "id");
                let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
                let _ = std::fs::remove_file(conv_dir().join(format!("{safe}.json")));
                let current = self.st.lock().unwrap().conv.id == safe;
                if current {
                    self.handle(&json!({"t":"new"}));
                }
                (self.emit)(json!({"t":"history","items":list_history()}));
            }
            "reply" => {
                let id = s(msg, "id");
                let mut st = self.st.lock().unwrap();
                if let Some((pid, tx)) = st.pending.take() {
                    if pid == id {
                        let _ = tx.send(msg["value"].clone());
                    } else {
                        st.pending = Some((pid, tx));
                    }
                }
            }
            _ => {}
        }
    }

    pub fn new_conversation(&self) {
        self.handle(&json!({"t":"new"}));
    }

    fn push_conv(&self) {
        let st = self.st.lock().unwrap();
        let all = ui_items(&st.conv.messages);
        let start = all.len().saturating_sub(UI_PAGE);
        let items: Vec<Value> = all.into_iter().skip(start).collect();
        let mut ev = json!({
            "t": "conv",
            "id": st.conv.id,
            "title": st.conv.title,
            "items": items,
            "start": start,
            "todos": st.conv.todos,
            "running": st.running,
            "configured": Config::load().is_configured(),
        });
        if st.running && !st.partial.is_empty() {
            ev["partial"] = json!(st.partial);
        }
        drop(st);
        (self.emit)(ev);
    }

    fn stop(&self) {
        let mut st = self.st.lock().unwrap();
        if !st.running {
            return;
        }
        st.cancel.store(true, Ordering::SeqCst);
        st.pending = None;
        let partial = std::mem::take(&mut st.partial);
        if !partial.trim().is_empty() {
            st.conv.messages.push(json!({"role":"assistant","content":partial}));
        }
        repair(&mut st.conv.messages);
        st.conv.save();
        st.running = false;
        drop(st);
        (self.emit)(json!({"t":"done","stopped":true}));
    }

    fn send(&self, text: String, atts: Vec<Value>) {
        let cancel = {
            let mut st = self.st.lock().unwrap();
            if st.running {
                return;
            }
            st.running = true;
            st.partial.clear();
            st.cancel = Arc::new(AtomicBool::new(false));
            if st.conv.title.is_empty() {
                let raw_t = if !text.trim().is_empty() {
                    text.trim().replace('\n', " ")
                } else {
                    atts.first().and_then(|a| a["name"].as_str()).unwrap_or("Attachment").to_string()
                };
                st.conv.title = raw_t.chars().take(60).collect();
            }
            let att_dir = data_dir().join("attachments");
            let _ = std::fs::create_dir_all(&att_dir);
            let mut img_list = Vec::new();
            let mut doc_list = Vec::new();
            for (idx, a) in atts.iter().enumerate() {
                let name = a["name"].as_str().unwrap_or("file").to_string();
                let data_url: &str = a["data"].as_str().unwrap_or("");
                let given_path = a["path"].as_str().unwrap_or("").to_string();
                let fpath = if !given_path.is_empty() && std::path::Path::new(&given_path).exists() {
                    std::path::PathBuf::from(&given_path)
                } else {
                    let b64 = data_url.split_once(";base64,").map(|x| x.1).unwrap_or(data_url);
                    use base64::{engine::general_purpose::STANDARD, Engine as _};
                    let safe_name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
                    let fname = format!("{}_{}_{}", chrono::Local::now().timestamp_millis(), idx, if safe_name.is_empty() { "file.bin" } else { &safe_name });
                    let p = att_dir.join(&fname);
                    if let Ok(bytes) = STANDARD.decode(b64.trim()) {
                        let _ = std::fs::write(&p, &bytes);
                    }
                    p
                };
                let ext = std::path::Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if tools::is_image_ext(&ext) || data_url.starts_with("data:image/") {
                    let mut im = json!({"name": name, "path": fpath.to_string_lossy(), "data": data_url});
                    // Small display thumbnail generated by the UI (already downscaled there).
                    let thumb = a["thumb"].as_str().unwrap_or("");
                    if let Some(tb64) = thumb.split_once(";base64,").map(|x| x.1) {
                        use base64::{engine::general_purpose::STANDARD, Engine as _};
                        let tp = att_dir.join(format!("{}_{}_thumb.webp", chrono::Local::now().timestamp_millis(), idx));
                        if let Ok(bytes) = STANDARD.decode(tb64.trim()) {
                            if std::fs::write(&tp, &bytes).is_ok() {
                                im["thumb"] = json!(tp.to_string_lossy());
                            }
                        }
                    }
                    strip_inline_image(&mut im);
                    img_list.push(im);
                } else {
                    doc_list.push(json!({"name": name, "path": fpath.to_string_lossy()}));
                }
            }
            let mut u_msg = json!({"role":"user","content":text});
            if !img_list.is_empty() {
                u_msg["images"] = Value::Array(img_list);
            }
            if !doc_list.is_empty() {
                u_msg["docs"] = Value::Array(doc_list);
            }
            st.conv.messages.push(u_msg);
            st.conv.save();
            st.cancel.clone()
        };
        let me = self.clone();
        std::thread::spawn(move || {
            let r = me.run_turn(&cancel);
            let mut st = me.st.lock().unwrap();
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            st.running = false;
            st.partial.clear();
            repair(&mut st.conv.messages);
            st.conv.save();
            drop(st);
            match r {
                Ok(()) => (me.emit)(json!({"t":"done"})),
                Err(e) => (me.emit)(json!({"t":"error","text":e})),
            }
        });
    }

    /// Lock state only if this turn is still the active one.
    fn live(&self, cancel: &Arc<AtomicBool>) -> Option<std::sync::MutexGuard<'_, Shared>> {
        let g = self.st.lock().unwrap();
        if cancel.load(Ordering::SeqCst) { None } else { Some(g) }
    }

    fn preprocess_attachments(&self, _force_ocr: bool, _cfg: &Config, cancel: &Arc<AtomicBool>) {
        // Always extract OCR text for attached images as well as sending image_url when vision is active,
        // because many OpenAI-compatible proxy gateways silently strip `image_url` blocks
        // with 200 OK instead of returning an HTTP 400 error.
        let tasks: Vec<(usize, Vec<Value>, Vec<Value>)> = {
            let Some(st) = self.live(cancel) else { return };
            st.conv
                .messages
                .iter()
                .enumerate()
                .filter(|(_, m)| m["role"] == "user")
                .map(|(i, m)| {
                    let imgs = if m.get("ocr_done").is_none() {
                        m["images"].as_array().cloned().unwrap_or_default()
                    } else {
                        vec![]
                    };
                    let docs = if m.get("docs_done").is_none() {
                        m["docs"].as_array().cloned().unwrap_or_default()
                    } else {
                        vec![]
                    };
                    (i, imgs, docs)
                })
                .filter(|(_, imgs, docs)| !imgs.is_empty() || !docs.is_empty())
                .collect()
        };
        for (msg_idx, imgs, docs) in tasks {
            let mut extra_blocks = Vec::new();
            for (k, im) in imgs.iter().enumerate() {
                if cancel.load(Ordering::SeqCst) { return; }
                let name = im["name"].as_str().unwrap_or("image.png");
                let path = std::path::PathBuf::from(im["path"].as_str().unwrap_or(""));
                let tid = format!("ocr_{}_{}", msg_idx, k);
                (self.emit)(json!({"t":"tool","id":tid,"verb":"OCR","arg":name,"state":"run"}));
                let t0 = std::time::Instant::now();
                let res = tools::ocr_file(&path);
                let ms = t0.elapsed().as_millis() as u64;
                match res {
                    Ok(txt) => {
                        (self.emit)(json!({"t":"tool","id":tid,"verb":"OCR","arg":name,"state":"ok","out":tools::truncate(&txt, 600),"ms":ms}));
                        extra_blocks.push(format!("[Attached Image `{name}` — Extracted via OCR]:\n{txt}"));
                    }
                    Err(e) => {
                        (self.emit)(json!({"t":"tool","id":tid,"verb":"OCR","arg":name,"state":"err","out":e,"ms":ms}));
                        extra_blocks.push(format!("[Attached Image `{name}` — OCR failed: {e}]"));
                    }
                }
            }
            for (k, dc) in docs.iter().enumerate() {
                if cancel.load(Ordering::SeqCst) { return; }
                let name = dc["name"].as_str().unwrap_or("document");
                let path = std::path::PathBuf::from(dc["path"].as_str().unwrap_or(""));
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                let tid = format!("doc_{}_{}", msg_idx, k);
                (self.emit)(json!({"t":"tool","id":tid,"verb":"Read","arg":name,"state":"run"}));
                let t0 = std::time::Instant::now();
                let res = if tools::is_doc_ext(&ext) {
                    tools::extract_document(&path)
                } else {
                    std::fs::read_to_string(&path).map_err(|e| e.to_string())
                };
                let ms = t0.elapsed().as_millis() as u64;
                match res {
                    Ok(txt) => {
                        let clipped = tools::truncate(&txt, 18_000);
                        (self.emit)(json!({"t":"tool","id":tid,"verb":"Read","arg":name,"state":"ok","out":tools::truncate(&clipped, 600),"ms":ms}));
                        extra_blocks.push(format!("[Attached Document `{name}`]:\n{clipped}"));
                    }
                    Err(e) => {
                        (self.emit)(json!({"t":"tool","id":tid,"verb":"Read","arg":name,"state":"err","out":e,"ms":ms}));
                        extra_blocks.push(format!("[Attached Document `{name}` — Error: {e}]"));
                    }
                }
            }
            if let Some(mut st) = self.live(cancel) {
                if let Some(m) = st.conv.messages.get_mut(msg_idx) {
                    if !imgs.is_empty() {
                        m["ocr_done"] = json!(true);
                    }
                    if !docs.is_empty() {
                        m["docs_done"] = json!(true);
                    }
                    if !extra_blocks.is_empty() {
                        let prev = m["extracted"].as_str().unwrap_or("").to_string();
                        let combined = if prev.is_empty() {
                            extra_blocks.join("\n\n")
                        } else {
                            format!("{prev}\n\n{}", extra_blocks.join("\n\n"))
                        };
                        m["extracted"] = json!(combined);
                    }
                    st.conv.save();
                }
            }
        }
    }

    fn run_turn(&self, cancel: &Arc<AtomicBool>) -> Result<(), String> {
        let cfg = Config::load();
        let specs = tools::specs();
        let mut vision_ok = !is_vision_rejected(&cfg);
        self.preprocess_attachments(!vision_ok, &cfg, cancel);
        for _step in 0..60 {
            let (messages, has_images) = {
                let Some(mut st) = self.live(cancel) else { return Ok(()) };
                repair(&mut st.conv.messages);
                let has_img = st.conv.messages.iter().any(|m| {
                    m["role"] == "user" && m["images"].as_array().map(|a| !a.is_empty()).unwrap_or(false)
                });
                (build_messages_ex(&st.conv.messages, vision_ok), has_img)
            };
            (self.emit)(json!({"t":"thinking","on":true}));
            let res = self.stream(&cfg, &messages, &specs, cancel);
            let (content, calls, reasoning, think_ms) = match res {
                Ok(v) => v,
                Err(_e) if vision_ok && has_images => {
                    // Server API rejected multimodal image_url -> remember for this model & fallback to Windows Native OCR!
                    mark_vision_rejected(&cfg);
                    vision_ok = false;
                    self.preprocess_attachments(true, &cfg, cancel);
                    let messages_ocr = {
                        let Some(mut st) = self.live(cancel) else { return Ok(()) };
                        repair(&mut st.conv.messages);
                        build_messages_ex(&st.conv.messages, false)
                    };
                    (self.emit)(json!({"t":"thinking","on":true}));
                    self.stream(&cfg, &messages_ocr, &specs, cancel)?
                }
                Err(e) => return Err(e),
            };
            if cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            let mut msg = json!({"role":"assistant","content": content, "ms": think_ms});
            if !reasoning.trim().is_empty() {
                msg["reasoning"] = json!(reasoning);
            }
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(calls.clone());
            }
            {
                let Some(mut st) = self.live(cancel) else { return Ok(()) };
                st.partial.clear();
                st.conv.messages.push(msg);
                st.conv.save();
            }
            if calls.is_empty() {
                return Ok(());
            }
            for call in &calls {
                if cancel.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let id = s(call, "id");
                let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let raw = call["function"]["arguments"].as_str().unwrap_or("{}");
                let args: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
                let result = self.run_tool(&id, &name, &args, cancel);
                let Some(result) = result else { return Ok(()) };
                let Some(mut st) = self.live(cancel) else { return Ok(()) };
                st.conv.messages.push(json!({"role":"tool","tool_call_id":id,"content":result}));
                st.conv.save();
            }
        }
        Err("Stopped after too many steps.".into())
    }

    fn wait_reply(&self, id: &str, ev: Value, cancel: &Arc<AtomicBool>) -> Option<Value> {
        let (tx, rx) = mpsc::channel();
        {
            let mut st = self.live(cancel)?;
            st.pending = Some((id.to_string(), tx));
        }
        (self.emit)(ev);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(v) => return Some(v),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::SeqCst) {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    fn run_tool(&self, id: &str, name: &str, args: &Value, cancel: &Arc<AtomicBool>) -> Option<String> {
        let (verb, arg) = tools::label(name, args);
        match name {
            "todo_write" => {
                let raw_todos = args["todos"].as_array().cloned().unwrap_or_default();
                let todos = tools::sync_todos_to_nusa(&raw_todos);
                if let Some(mut st) = self.live(cancel) {
                    st.conv.todos = todos.clone();
                }
                (self.emit)(json!({"t":"todos","items":todos}));
                return Some("Synced to Nusa Todo List and updated checklist.".into());
            }
            "ask_user" => {
                let q = s(args, "question");
                let opts = args["options"].clone();
                let v = self.wait_reply(id, json!({"t":"ask","id":id,"question":q,"options":opts}), cancel)?;
                let ans = v.as_str().unwrap_or("").to_string();
                (self.emit)(json!({"t":"tool","id":id,"verb":"Asked","arg":tools::truncate(&q, 50),"state":"ok"}));
                return Some(if ans.trim().is_empty() { "(no answer)".into() } else { format!("User answered: {ans}") });
            }
            "request_permission" => {
                let action = s(args, "action");
                let v = self.wait_reply(
                    id,
                    json!({"t":"approve","id":id,"verb":"Permission","title":"Nusa AI asks permission","detail":action,"always":false}),
                    cancel,
                )?;
                let ok = matches!(v.as_str(), Some("allow") | Some("always"));
                (self.emit)(json!({"t":"tool","id":id,"verb":"Permission","arg":if ok {"granted"} else {"denied"},"state":if ok {"ok"} else {"err"}}));
                return Some(if ok { "Permission granted by user.".into() } else { "Permission denied by user. Do not proceed with that action.".into() });
            }
            _ => {}
        }

        if tools::needs_approval(name) {
            let allow_all = self.st.lock().unwrap().allow_all;
            if !allow_all {
                let v = self.wait_reply(
                    id,
                    json!({"t":"approve","id":id,"verb":verb,"title":format!("{verb} {arg}"),"detail":tools::approval_detail(name, args),"always":true}),
                    cancel,
                )?;
                match v.as_str() {
                    Some("allow") => {}
                    Some("always") => self.st.lock().unwrap().allow_all = true,
                    _ => {
                        (self.emit)(json!({"t":"tool","id":id,"verb":verb,"arg":arg,"state":"deny"}));
                        return Some("User denied this action. Ask what they want instead, or continue without it.".into());
                    }
                }
            }
        }

        (self.emit)(json!({"t":"tool","id":id,"verb":verb,"arg":arg,"state":"run"}));
        let t0 = std::time::Instant::now();
        let r = tools::execute(name, args, cancel);
        let tool_ms = t0.elapsed().as_millis() as u64;
        if cancel.load(Ordering::SeqCst) {
            return None;
        }
        let (state, text) = match r {
            Ok(t) => ("ok", t),
            Err(e) => ("err", format!("Error: {e}")),
        };
        if name == "nusa_todo" && state == "ok" {
            let todos = tools::nusa_todos_as_json(&tools::load_nusa_todos());
            if let Some(mut st) = self.live(cancel) {
                st.conv.todos = todos.clone();
            }
            (self.emit)(json!({"t":"todos","items":todos}));
        }
        (self.emit)(json!({"t":"tool","id":id,"verb":verb,"arg":arg,"state":state,"out":tools::truncate(&text, 600),"ms":tool_ms}));
        Some(text)
    }

    fn stream(&self, cfg: &Config, messages: &[Value], specs: &[Value], cancel: &Arc<AtomicBool>) -> Result<(String, Vec<Value>, String, u64), String> {
        let t0 = std::time::Instant::now();
        let body = json!({
            "model": cfg.model,
            "messages": messages,
            "tools": specs,
            "stream": true,
        });
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .ip_family(ureq::config::IpFamily::Ipv4Only)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(180)))
            .build()
            .new_agent();
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let body_s = serde_json::to_string(&body).map_err(|e| e.to_string())?;
        let resp = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", cfg.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .send(body_s.as_bytes())
            .map_err(|e| format!("Connection failed: {e}"))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let mut resp = resp;
            let t = resp.body_mut().read_to_string().unwrap_or_default();
            let m = serde_json::from_str::<Value>(&t)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(|x| x.to_string()))
                .unwrap_or(t);
            return Err(format!("API error {status}: {}", tools::truncate(&m, 400)));
        }
        let reader = BufReader::new(resp.into_body().into_reader());
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls: Vec<Value> = vec![];
        let mut thinking_off = false;
        let mut think_ms: Option<u64> = None;
        for line in reader.lines() {
            if cancel.load(Ordering::SeqCst) {
                return Ok((content, vec![], reasoning, t0.elapsed().as_millis() as u64));
            }
            let line = line.map_err(|e| format!("Stream error: {e}"))?;
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(err) = v.get("error") {
                return Err(format!("API error: {}", err["message"].as_str().unwrap_or(&err.to_string())));
            }
            let delta = &v["choices"][0]["delta"];
            if let Some(r) = delta["reasoning_content"].as_str().or_else(|| delta["reasoning"].as_str()) {
                if !r.is_empty() {
                    reasoning.push_str(r);
                    (self.emit)(json!({"t":"reason","text":r}));
                }
            }
            if let Some(t) = delta["content"].as_str() {
                if !t.is_empty() {
                    if !thinking_off {
                        thinking_off = true;
                        let ms = t0.elapsed().as_millis() as u64;
                        think_ms = Some(ms);
                        (self.emit)(json!({"t":"thinking","on":false,"ms":ms}));
                    }
                    content.push_str(t);
                    if let Some(mut st) = self.live(cancel) {
                        st.partial.push_str(t);
                    }
                    (self.emit)(json!({"t":"delta","text":t}));
                }
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    let idx = tc["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
                    while calls.len() <= idx {
                        calls.push(json!({"id":"","type":"function","function":{"name":"","arguments":""}}));
                    }
                    let c = &mut calls[idx];
                    if let Some(id) = tc["id"].as_str() {
                        if !id.is_empty() {
                            c["id"] = json!(id);
                        }
                    }
                    if let Some(nm) = tc["function"]["name"].as_str() {
                        let cur = c["function"]["name"].as_str().unwrap_or("").to_string();
                        c["function"]["name"] = json!(cur + nm);
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        let cur = c["function"]["arguments"].as_str().unwrap_or("").to_string();
                        c["function"]["arguments"] = json!(cur + a);
                    }
                }
            }
        }
        let final_ms = think_ms.unwrap_or_else(|| t0.elapsed().as_millis() as u64);
        if !thinking_off {
            (self.emit)(json!({"t":"thinking","on":false,"ms":final_ms}));
        }
        for (i, c) in calls.iter_mut().enumerate() {
            if c["id"].as_str().unwrap_or("").is_empty() {
                c["id"] = json!(format!("call_{}_{}", chrono::Local::now().timestamp_millis(), i));
            }
            if c["function"]["arguments"].as_str().unwrap_or("").trim().is_empty() {
                c["function"]["arguments"] = json!("{}");
            }
        }
        calls.retain(|c| !c["function"]["name"].as_str().unwrap_or("").is_empty());
        Ok((content, calls, reasoning, final_ms))
    }
}

/// Make sure every assistant tool call has a matching tool result.
fn repair(msgs: &mut Vec<Value>) {
    let mut i = 0;
    while i < msgs.len() {
        if msgs[i]["role"] == "assistant" {
            if let Some(calls) = msgs[i]["tool_calls"].as_array().cloned() {
                let mut j = i + 1;
                let mut have = vec![];
                while j < msgs.len() && msgs[j]["role"] == "tool" {
                    have.push(s(&msgs[j], "tool_call_id"));
                    j += 1;
                }
                let mut insert = vec![];
                for c in calls {
                    let id = s(&c, "id");
                    if !have.contains(&id) {
                        insert.push(json!({"role":"tool","tool_call_id":id,"content":"Cancelled by user."}));
                    }
                }
                let k = insert.len();
                for (n, m) in insert.into_iter().enumerate() {
                    msgs.insert(j + n, m);
                }
                i = j + k;
                continue;
            }
        }
        i += 1;
    }
}

fn is_indonesian(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphabetic())
        .filter(|s| !s.is_empty())
        .collect();
    const ID_WORDS: &[&str] = &[
        "aku", "saya", "gue", "gua", "kamu", "tolong", "bisa", "ada", "tidak", "nggak", "gak",
        "ga", "apa", "apakah", "gimana", "bagaimana", "kenapa", "kapan", "dimana", "yang", "ini",
        "itu", "untuk", "buat", "dari", "dengan", "dan", "atau", "tapi", "kalau", "kalo", "sudah",
        "udah", "belum", "blm", "mau", "ingin", "catat", "catet", "masukin", "hapus", "cari",
        "buka", "lihat", "cek", "hari", "jam", "pagi", "siang", "sore", "malam", "besok",
        "kemarin", "terima", "kasih", "makasih", "halo", "hai", "berapa", "mana", "dong", "ya",
        "kok", "sih", "nih", "deh", "aja", "saja", "rapat", "jadwal", "ingetin",
    ];
    words.iter().any(|w| ID_WORDS.contains(w))
}

fn system_prompt(last_user_msg: &str) -> String {
    let now = chrono::Local::now();
    let user = std::env::var("USERNAME").unwrap_or_default();
    let lang_rule = if !last_user_msg.trim().is_empty() && is_indonesian(last_user_msg) {
        "CRITICAL LANGUAGE RULE: The user's latest message is in Indonesian. Reply entirely in Indonesian."
    } else {
        "CRITICAL LANGUAGE RULE: The user's latest message is in English (or non-Indonesian). You MUST reply strictly in the exact same language as the user's latest message (English if they wrote in English). Do NOT reply in Indonesian, Korean, or any other language."
    };
    format!(
        "You are Nusa AI, a helpful personal desktop AI assistant that lives in the top screen notch on the user's Windows PC. \
You can use tools to work with the computer: read/search/write files, run PowerShell or Python, manage background processes, search and read the web, manage the notch's Todo List widget, keep a task checklist, and ask the user questions.\n\n\
Guidelines:\n\
- {lang_rule}\n\
- The chat panel is compact: be concise and friendly; use short paragraphs, bullets and markdown only when useful.\n\
- Act, don't just describe: when a task needs the computer or the web, use tools directly. Read-only tools run automatically; write/edit/delete/shell/python ask the user for approval automatically, so just call them.\n\
- When the user asks to add, record, list, complete, or delete a todo/reminder/schedule/meeting/note, call `nusa_todo` (with `action: \"add\"`, `title`, and optional `start_time`/`end_time` in 24h `HH:MM` format like `\"12:00\"`). `nusa_todo` writes directly to the built-in Todo List widget inside the notch without requiring approval.\n\
- For multi-step agent tasks use `todo_write` and keep it updated.\n\
- Use `ask_user` when something is ambiguous instead of guessing about important details. Use `request_permission` before sensitive actions that are not covered by approvals (e.g. sending data somewhere, closing apps).\n\
- Prefer non-destructive approaches. Never delete or overwrite user data without being sure.\n\
- When you use web info, mention the source briefly.\n\n\
Environment: Windows, user '{user}', home folder {home}. Current date/time: {date}.",
        home = tools::home().display(),
        date = now.format("%A, %d %B %Y %H:%M (%:z)")
    )
}

fn no_vision_cache() -> &'static Mutex<std::collections::HashSet<String>> {
    static CACHE: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn is_vision_rejected(cfg: &Config) -> bool {
    let key = format!("{}|{}", cfg.base_url.trim(), cfg.model.trim().to_ascii_lowercase());
    no_vision_cache().lock().map(|s| s.contains(&key)).unwrap_or(false)
}

fn mark_vision_rejected(cfg: &Config) {
    let key = format!("{}|{}", cfg.base_url.trim(), cfg.model.trim().to_ascii_lowercase());
    if let Ok(mut s) = no_vision_cache().lock() {
        s.insert(key);
    }
}

#[cfg(test)]
fn build_messages(msgs: &[Value]) -> Vec<Value> {
    build_messages_ex(msgs, false)
}

fn build_messages_ex(msgs: &[Value], use_vision: bool) -> Vec<Value> {
    let last_user = msgs
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("");
    let lang_tag = if !last_user.trim().is_empty() && is_indonesian(last_user) {
        "\n\n[Reply in Indonesian]"
    } else {
        "\n\n[Reply strictly in English]"
    };
    let last_user_idx = msgs.iter().rposition(|m| m["role"] == "user");
    let mut out = vec![json!({"role":"system","content":system_prompt(last_user)})];
    let n = msgs.len();
    for (i, m) in msgs.iter().enumerate() {
        let mut m = m.clone();
        let imgs = m["images"].as_array().cloned().unwrap_or_default();
        let extracted = m["extracted"].as_str().unwrap_or("").to_string();
        if let Some(obj) = m.as_object_mut() {
            obj.remove("ms");
            obj.remove("reasoning");
            obj.remove("images");
            obj.remove("docs");
            obj.remove("ocr_done");
            obj.remove("docs_done");
            obj.remove("extracted");
        }
        if m["role"] == "tool" && i + 16 < n {
            if let Some(c) = m["content"].as_str() {
                if c.len() > 1500 {
                    m["content"] = json!(tools::truncate(c, 1500));
                }
            }
        }
        if m["role"] == "assistant" && m["content"].is_null() {
            m["content"] = json!("");
        }
        if m["role"] == "user" && !extracted.is_empty() {
            let base = m["content"].as_str().unwrap_or("");
            m["content"] = json!(if base.trim().is_empty() {
                extracted.clone()
            } else {
                format!("{base}\n\n{extracted}")
            });
        }
        if Some(i) == last_user_idx || (i + 1 == n && m["role"] == "tool") {
            if let Some(c) = m["content"].as_str() {
                m["content"] = json!(format!("{c}{lang_tag}"));
            }
        }
        if m["role"] == "user" && use_vision && !imgs.is_empty() {
            let txt = m["content"].as_str().unwrap_or("").to_string();
            let mut parts = vec![json!({"type": "text", "text": txt})];
            for im in &imgs {
                if let Some(url) = image_data_url(im) {
                    if !url.is_empty() {
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                }
            }
            m["content"] = Value::Array(parts);
        }
        out.push(m);
    }
    out
}

/// What the chat UI should load for a stored image: the small thumbnail if we have one, else the
/// original file — both as ATT_HOST URLs (loaded by WebView2 from disk). Inline `data` is only a
/// fallback for legacy entries that live outside the attachments folder.
fn ui_image_src(im: &Value) -> Value {
    for k in ["thumb", "path"] {
        if let Some(n) = im[k].as_str().and_then(att_file_name) {
            return json!(att_url(&n));
        }
    }
    im["data"].clone()
}

fn ui_image_full(im: &Value) -> Value {
    if let Some(n) = im["path"].as_str().and_then(att_file_name) {
        return json!(att_url(&n));
    }
    ui_image_src(im)
}

fn ui_items(msgs: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    for (i, m) in msgs.iter().enumerate() {
        match m["role"].as_str().unwrap_or("") {
            "user" => {
                let imgs: Vec<Value> = m["images"]
                    .as_array()
                    .map(|a| a.iter().map(|x| json!({"name": x["name"], "src": ui_image_src(x), "full": ui_image_full(x)})).collect())
                    .unwrap_or_default();
                let docs: Vec<Value> = m["docs"]
                    .as_array()
                    .map(|a| a.iter().map(|x| json!({"name": x["name"]})).collect())
                    .unwrap_or_default();
                out.push(json!({"k":"user","text":s(m,"content"),"images":imgs,"docs":docs}));
            }
            "assistant" => {
                let c = s(m, "content");
                let ms = m["ms"].as_u64().unwrap_or(0);
                let reasoning = s(m, "reasoning");
                let mut used_meta = false;
                if let Some(calls) = m["tool_calls"].as_array() {
                    let mut first = true;
                    for tc in calls {
                        let name = tc["function"]["name"].as_str().unwrap_or("");
                        if name == "todo_write" {
                            continue;
                        }
                        let args: Value = serde_json::from_str(tc["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                        let (verb, arg) = tools::label(name, &args);
                        let id = s(tc, "id");
                        let res = msgs[i + 1..]
                            .iter()
                            .take_while(|x| x["role"] == "tool")
                            .find(|x| s(x, "tool_call_id") == id)
                            .map(|x| s(x, "content"))
                            .unwrap_or_default();
                        let state = if res.starts_with("Error") {
                            "err"
                        } else if res.starts_with("User denied") || res.starts_with("Cancelled") || res.starts_with("Permission denied") {
                            "deny"
                        } else {
                            "ok"
                        };
                        let think_ms = if first { ms } else { 0 };
                        let r_txt = if first { reasoning.as_str() } else { "" };
                        first = false;
                        used_meta = true;
                        out.push(json!({
                            "k":"tool",
                            "id":id,
                            "verb":verb,
                            "arg":arg,
                            "state":state,
                            "out":tools::truncate(&res, 600),
                            "think_ms":think_ms,
                            "reasoning":r_txt
                        }));
                    }
                }
                if !c.trim().is_empty() {
                    out.push(json!({
                        "k":"ai",
                        "text":c,
                        "ms":if used_meta { 0 } else { ms },
                        "reasoning":if used_meta { "" } else { reasoning.as_str() }
                    }));
                }
            }
            _ => {}
        }
    }
    out
}

pub fn list_history() -> Vec<Value> {
    // Only the summary fields are materialised; everything else (message bodies, legacy inline
    // base64 images, …) is skipped by serde while streaming, so memory stays flat no matter how
    // big the conversation files are.
    #[derive(Deserialize)]
    struct HMsg {
        #[serde(default)]
        role: Option<String>,
    }
    #[derive(Deserialize)]
    struct HConv {
        #[serde(default)]
        id: Value,
        #[serde(default)]
        title: Value,
        #[serde(default)]
        updated: Value,
        #[serde(default)]
        messages: Option<Vec<HMsg>>,
    }
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir(conv_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(f) = std::fs::File::open(&p) {
                if let Ok(c) = serde_json::from_reader::<_, HConv>(BufReader::new(f)) {
                    v.push(json!({
                        "id": c.id,
                        "title": c.title,
                        "updated": c.updated,
                        "count": c.messages.map(|a| a.iter().filter(|m| m.role.as_deref() == Some("user")).count()).unwrap_or(0),
                    }));
                }
            }
        }
    }
    v.sort_by(|a, b| b["updated"].as_i64().cmp(&a["updated"].as_i64()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_fills_missing_tool_results() {
        let mut m = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":"","tool_calls":[
                {"id":"a","type":"function","function":{"name":"shell","arguments":"{}"}},
                {"id":"b","type":"function","function":{"name":"glob","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"a","content":"ok"}),
        ];
        repair(&mut m);
        assert_eq!(m.len(), 4);
        assert_eq!(m[3]["tool_call_id"], "b");
        assert_eq!(m[3]["content"], "Cancelled by user.");
        repair(&mut m);
        assert_eq!(m.len(), 4, "repair must be idempotent");
    }

    #[test]
    fn ui_items_restore_chat() {
        let m = vec![
            json!({"role":"user","content":"cek disk"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"x","type":"function","function":{"name":"shell","arguments":"{\"command\":\"Get-PSDrive C\"}"}},
                {"id":"t","type":"function","function":{"name":"todo_write","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"x","content":"User denied this action."}),
            json!({"role":"tool","tool_call_id":"t","content":"ok"}),
            json!({"role":"assistant","content":"Sisa 120 GB."}),
        ];
        let u = ui_items(&m);
        assert_eq!(u.len(), 3, "{u:?}");
        assert_eq!(u[0]["k"], "user");
        assert_eq!(u[1]["k"], "tool");
        assert_eq!(u[1]["verb"], "Run");
        assert_eq!(u[1]["state"], "deny");
        assert_eq!(u[2]["text"], "Sisa 120 GB.");
    }

    #[test]
    fn image_urls_never_inline_attachment_files() {
        assert_eq!(att_url("1_0_IMG 1.jpg"), "http://nusa-att.localhost/1_0_IMG%201.jpg");
        let p = att_dir().join("x_0_a.png");
        let im = json!({"name":"a.png","path":p.to_string_lossy(),"thumb":att_dir().join("x_0_thumb.webp").to_string_lossy()});
        assert_eq!(ui_image_src(&im), "http://nusa-att.localhost/x_0_thumb.webp");
        let legacy_outside = json!({"name":"b.png","path":"C:\\elsewhere\\b.png","data":"data:image/png;base64,AA=="});
        assert_eq!(ui_image_src(&legacy_outside), "data:image/png;base64,AA==");
    }

    #[test]
    fn build_messages_has_system_and_no_null_content() {
        let m = vec![json!({"role":"assistant","content":null})];
        let b = build_messages(&m);
        assert_eq!(b[0]["role"], "system");
        assert!(b[0]["content"].as_str().unwrap().contains("Nusa AI"));
        assert_eq!(b[1]["content"], "");
    }
}
