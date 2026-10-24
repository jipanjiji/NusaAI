use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_OUT: usize = 30_000;
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";

pub type Res = Result<String, String>;

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": desc,
            "parameters": { "type": "object", "properties": props, "required": required }
        }
    })
}

pub fn specs() -> Vec<Value> {
    vec![
        tool("read_file", "Read a text file, document (.pdf, .docx, .xlsx, .pptx, .odt, .rtf), or image via OCR (.png, .jpg, .webp, .bmp). Returns numbered lines. Use offset/limit for large files.",
            json!({"path":{"type":"string"},"offset":{"type":"integer","description":"1-based start line"},"limit":{"type":"integer","description":"max lines (default 400)"}}), &["path"]),
        tool("ocr_image", "Extract text from an image or screenshot file (.png, .jpg, .webp, .bmp) using Windows built-in OCR.",
            json!({"path":{"type":"string"}}), &["path"]),
        tool("write_file", "Create or overwrite a file with the given content. Requires user approval.",
            json!({"path":{"type":"string"},"content":{"type":"string"}}), &["path","content"]),
        tool("edit_file", "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Requires user approval.",
            json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}}), &["path","old_string","new_string"]),
        tool("list_dir", "List a directory as a tree.",
            json!({"path":{"type":"string"},"depth":{"type":"integer","description":"default 2, max 5"}}), &["path"]),
        tool("glob", "Find files by glob pattern (e.g. **/*.pdf) under a directory.",
            json!({"pattern":{"type":"string"},"path":{"type":"string","description":"base directory, default home"}}), &["pattern"]),
        tool("grep", "Search file contents with a regex under a directory.",
            json!({"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string","description":"glob filter e.g. *.txt"},"case_sensitive":{"type":"boolean"}}), &["pattern"]),
        tool("delete_path", "Move a file or folder to the Recycle Bin. Requires user approval.",
            json!({"path":{"type":"string"}}), &["path"]),
        tool("move_path", "Move or rename a file/folder. Requires user approval.",
            json!({"from":{"type":"string"},"to":{"type":"string"}}), &["from","to"]),
        tool("shell", "Run a PowerShell command on the user's Windows PC and return its output. Requires user approval.",
            json!({"command":{"type":"string"},"cwd":{"type":"string"},"timeout_sec":{"type":"integer","description":"default 60, max 600"}}), &["command"]),
        tool("run_python", "Run a Python 3 script and return stdout/stderr. Requires user approval.",
            json!({"code":{"type":"string"},"timeout_sec":{"type":"integer"}}), &["code"]),
        tool("bg_start", "Start a long-running PowerShell command in the background. Returns an id. Requires user approval.",
            json!({"command":{"type":"string"},"cwd":{"type":"string"}}), &["command"]),
        tool("bg_output", "Get recent output and status of a background process.",
            json!({"id":{"type":"integer"},"tail":{"type":"integer","description":"chars, default 4000"}}), &["id"]),
        tool("bg_stop", "Stop a background process.",
            json!({"id":{"type":"integer"}}), &["id"]),
        tool("web_search", "Search the web (DuckDuckGo). Returns titles, urls and snippets.",
            json!({"query":{"type":"string"},"max_results":{"type":"integer"}}), &["query"]),
        tool("web_fetch", "Fetch a URL and return its content as markdown/text.",
            json!({"url":{"type":"string"},"max_chars":{"type":"integer","description":"default 20000"}}), &["url"]),
        tool("todo_write", "Create/replace the task checklist shown to the user and sync to Nusa's built-in Todo List widget in the notch.",
            json!({"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]},"start_time":{"type":"string","description":"optional HH:MM (24h), e.g. 12:00"},"end_time":{"type":"string","description":"optional HH:MM (24h), e.g. 13:00"}},"required":["content","status"]}}}), &["todos"]),
        tool("nusa_todo", "Manage the user's Todo List widget inside the Nusa notch (list, add, complete, uncomplete, delete tasks/reminders with optional start/end times). Runs automatically without approval.",
            json!({"action":{"type":"string","enum":["list","add","complete","uncomplete","delete","clear_completed"]},"title":{"type":"string","description":"Task title (for add) or title substring / 1-based #index (for complete/uncomplete/delete)"},"start_time":{"type":"string","description":"Optional start time in HH:MM (24h), e.g. 12:00"},"end_time":{"type":"string","description":"Optional end time in HH:MM (24h), e.g. 13:00"}}), &["action"]),
        tool("ask_user", "Ask the user a question and wait for the answer. Optionally provide choices.",
            json!({"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}}), &["question"]),
        tool("request_permission", "Explicitly ask the user for permission before doing something sensitive. Returns granted/denied.",
            json!({"action":{"type":"string","description":"what you want to do and why"}}), &["action"]),
    ]
}

pub fn needs_approval(name: &str) -> bool {
    matches!(
        name,
        "write_file" | "edit_file" | "delete_path" | "move_path" | "shell" | "run_python" | "bg_start"
    )
}

fn s<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}
fn n(a: &Value, k: &str) -> Option<i64> {
    a.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|x| x.parse().ok())))
}

fn short(t: &str, max: usize) -> String {
    let t = t.trim().replace(['\r', '\n'], " ");
    if t.chars().count() > max {
        format!("{}…", t.chars().take(max).collect::<String>())
    } else {
        t
    }
}

fn file_name(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string())
}

/// Short human label for a tool call, shown as a chip in the UI.
pub fn label(name: &str, args: &Value) -> (String, String) {
    match name {
        "read_file" => ("Read".into(), file_name(s(args, "path"))),
        "ocr_image" => ("OCR".into(), file_name(s(args, "path"))),
        "write_file" => ("Write".into(), file_name(s(args, "path"))),
        "edit_file" => ("Edit".into(), file_name(s(args, "path"))),
        "list_dir" => ("List".into(), short(s(args, "path"), 40)),
        "glob" => ("Find".into(), short(s(args, "pattern"), 40)),
        "grep" => ("Search".into(), short(s(args, "pattern"), 40)),
        "delete_path" => ("Delete".into(), file_name(s(args, "path"))),
        "move_path" => ("Move".into(), format!("{} → {}", file_name(s(args, "from")), file_name(s(args, "to")))),
        "shell" => ("Run".into(), short(s(args, "command"), 60)),
        "run_python" => ("Python".into(), short(s(args, "code"), 60)),
        "bg_start" => ("Background".into(), short(s(args, "command"), 60)),
        "bg_output" => ("Check".into(), format!("#{}", n(args, "id").unwrap_or(0))),
        "bg_stop" => ("Stop".into(), format!("#{}", n(args, "id").unwrap_or(0))),
        "web_search" => ("Web".into(), short(s(args, "query"), 50)),
        "web_fetch" => ("Fetch".into(), short(s(args, "url"), 50)),
        "todo_write" => ("Tasks".into(), String::new()),
        "nusa_todo" => ("Todo".into(), short(s(args, "title"), 45)),
        "ask_user" => ("Ask".into(), short(s(args, "question"), 50)),
        "request_permission" => ("Permission".into(), short(s(args, "action"), 50)),
        _ => (name.to_string(), String::new()),
    }
}

/// Detail text shown on approval cards.
pub fn approval_detail(name: &str, args: &Value) -> String {
    match name {
        "write_file" => {
            let c = s(args, "content");
            format!("{}\n\n{}", s(args, "path"), truncate(c, 1200))
        }
        "edit_file" => format!(
            "{}\n\n- {}\n+ {}",
            s(args, "path"),
            truncate(s(args, "old_string"), 500),
            truncate(s(args, "new_string"), 500)
        ),
        "delete_path" => s(args, "path").to_string(),
        "move_path" => format!("{}\n→ {}", s(args, "from"), s(args, "to")),
        "shell" | "bg_start" => {
            let cwd = s(args, "cwd");
            if cwd.is_empty() { s(args, "command").to_string() } else { format!("{}\n\n(in {})", s(args, "command"), cwd) }
        }
        "run_python" => truncate(s(args, "code"), 1500),
        _ => serde_json::to_string_pretty(args).unwrap_or_default(),
    }
}

pub fn truncate(t: &str, max: usize) -> String {
    if t.len() <= max {
        return t.to_string();
    }
    let mut cut = max;
    while !t.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…[truncated {} bytes]", &t[..cut], t.len() - cut)
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\"))
}

fn resolve(p: &str) -> PathBuf {
    let p = p.trim().trim_matches('"');
    let p = if let Some(rest) = p.strip_prefix("~") {
        home().join(rest.trim_start_matches(['/', '\\']))
    } else {
        PathBuf::from(p)
    };
    if p.is_absolute() { p } else { home().join(p) }
}

pub fn execute(name: &str, args: &Value, cancel: &Arc<AtomicBool>) -> Res {
    match name {
        "read_file" => read_file(args),
        "ocr_image" => ocr_file(&resolve(s(args, "path"))),
        "write_file" => write_file(args),
        "edit_file" => edit_file(args),
        "list_dir" => list_dir(args),
        "glob" => glob(args),
        "grep" => grep(args, cancel),
        "delete_path" => {
            let p = resolve(s(args, "path"));
            if !p.exists() {
                return Err(format!("Not found: {}", p.display()));
            }
            trash::delete(&p).map_err(|e| e.to_string())?;
            Ok(format!("Moved to Recycle Bin: {}", p.display()))
        }
        "move_path" => {
            let (a, b) = (resolve(s(args, "from")), resolve(s(args, "to")));
            if let Some(parent) = b.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::rename(&a, &b).map_err(|e| e.to_string())?;
            Ok(format!("Moved {} → {}", a.display(), b.display()))
        }
        "shell" => {
            let cwd = s(args, "cwd");
            let t = n(args, "timeout_sec").unwrap_or(60).clamp(1, 600) as u64;
            run_ps(s(args, "command"), cwd, t, cancel)
        }
        "run_python" => run_python(args, cancel),
        "bg_start" => bg_start(args),
        "bg_output" => bg_output(args),
        "bg_stop" => bg_stop(args),
        "web_search" => web_search(args),
        "web_fetch" => web_fetch(args),
        "nusa_todo" => nusa_todo(args),
        _ => Err(format!("Unknown tool: {name}")),
    }
}

// ---------- files ----------

pub fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "tif" | "tiff"
    )
}

pub fn is_doc_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "pdf" | "docx" | "xlsx" | "pptx" | "odt" | "rtf"
    )
}

fn read_file(a: &Value) -> Res {
    let p = resolve(s(a, "path"));
    if p.is_dir() {
        return list_dir(&json!({"path": p.to_string_lossy(), "depth": 1}));
    }
    if !p.exists() {
        return Err(format!("Not found: {}", p.display()));
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let extracted = if is_doc_ext(&ext) {
        Some(extract_document(&p)?)
    } else if is_image_ext(&ext) {
        Some(ocr_file(&p)?)
    } else {
        None
    };
    let text_owned;
    let text: &str = if let Some(ref ex) = extracted {
        ex
    } else {
        let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        if bytes.iter().take(8000).any(|&b| b == 0) {
            return Err(format!("{} looks like a binary file ({} bytes).", p.display(), bytes.len()));
        }
        text_owned = String::from_utf8_lossy(&bytes).into_owned();
        &text_owned
    };
    let offset = n(a, "offset").unwrap_or(1).max(1) as usize;
    let limit = n(a, "limit").unwrap_or(400).clamp(1, 4000) as usize;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate().skip(offset - 1).take(limit) {
        let l = if l.len() > 2000 { &l[..l.char_indices().nth(2000).map(|x| x.0).unwrap_or(l.len())] } else { l };
        out.push_str(&format!("{:>5}\t{}\n", i + 1, l));
    }
    let end = (offset - 1 + limit).min(total);
    if end < total {
        out.push_str(&format!("\n[showing lines {offset}-{end} of {total}; use offset to read more]"));
    }
    if out.is_empty() {
        out = format!("[empty file or offset beyond end; {total} lines]");
    }
    Ok(truncate(&out, MAX_OUT))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#10;", "\n")
        .replace("&#13;", "\r")
        .replace("&#9;", "\t")
}

fn read_zip_entries(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    use flate2::read::DeflateDecoder;
    let mut map = HashMap::new();
    let len = bytes.len();
    // Try Central Directory first (handles Office data-descriptor entries with size=0 in local header)
    let mut eocd = None;
    let min_pos = len.saturating_sub(65557);
    for i in (min_pos..=len.saturating_sub(22)).rev() {
        if &bytes[i..i + 4] == b"PK\x05\x06" {
            eocd = Some(i);
            break;
        }
    }
    if let Some(pos) = eocd {
        let cd_size = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap_or([0; 4])) as usize;
        let cd_off = u32::from_le_bytes(bytes[pos + 16..pos + 20].try_into().unwrap_or([0; 4])) as usize;
        let mut p = cd_off;
        let end = (cd_off + cd_size).min(len);
        while p + 46 <= end && &bytes[p..p + 4] == b"PK\x01\x02" {
            let method = u16::from_le_bytes([bytes[p + 10], bytes[p + 11]]);
            let comp_sz = u32::from_le_bytes(bytes[p + 20..p + 24].try_into().unwrap_or([0; 4])) as usize;
            let name_len = u16::from_le_bytes([bytes[p + 28], bytes[p + 29]]) as usize;
            let extra_len = u16::from_le_bytes([bytes[p + 30], bytes[p + 31]]) as usize;
            let comm_len = u16::from_le_bytes([bytes[p + 32], bytes[p + 33]]) as usize;
            let lhoff = u32::from_le_bytes(bytes[p + 42..p + 46].try_into().unwrap_or([0; 4])) as usize;
            if p + 46 + name_len <= len {
                let name = String::from_utf8_lossy(&bytes[p + 46..p + 46 + name_len]).replace('\\', "/");
                if lhoff + 30 <= len && &bytes[lhoff..lhoff + 4] == b"PK\x03\x04" {
                    let ln = u16::from_le_bytes([bytes[lhoff + 26], bytes[lhoff + 27]]) as usize;
                    let le = u16::from_le_bytes([bytes[lhoff + 28], bytes[lhoff + 29]]) as usize;
                    let data_start = lhoff + 30 + ln + le;
                    if data_start + comp_sz <= len {
                        let slice = &bytes[data_start..data_start + comp_sz];
                        let mut out = Vec::new();
                        if method == 0 {
                            out.extend_from_slice(slice);
                            map.insert(name, out);
                        } else if method == 8 {
                            if DeflateDecoder::new(slice).read_to_end(&mut out).is_ok() {
                                map.insert(name, out);
                            }
                        }
                    }
                }
            }
            p += 46 + name_len + extra_len + comm_len;
        }
    }
    map
}

pub fn extract_document(p: &Path) -> Res {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let bytes = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    match ext.as_str() {
        "docx" => {
            let entries = read_zip_entries(&bytes);
            let Some(xml_b) = entries.get("word/document.xml") else {
                return Err("Invalid .docx file (missing word/document.xml)".into());
            };
            let xml = String::from_utf8_lossy(xml_b);
            let s = xml
                .replace("</w:p>", "\n")
                .replace("<w:br/>", "\n")
                .replace("<w:br ", "\n<w:br ")
                .replace("<w:cr/>", "\n")
                .replace("</w:tc>", "\t")
                .replace("<w:tab/>", "\t")
                .replace("</w:tr>", "\n");
            let re = regex::Regex::new(r"<[^>]+>").unwrap();
            let clean = xml_unescape(&re.replace_all(&s, ""));
            Ok(clean.lines().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n").trim().to_string())
        }
        "pptx" => {
            let entries = read_zip_entries(&bytes);
            let mut slides: Vec<(usize, String)> = vec![];
            let re = regex::Regex::new(r"<[^>]+>").unwrap();
            for (k, v) in &entries {
                if let Some(rest) = k.strip_prefix("ppt/slides/slide").and_then(|s| s.strip_suffix(".xml")) {
                    if let Ok(num) = rest.parse::<usize>() {
                        let xml = String::from_utf8_lossy(v);
                        let s = xml.replace("</a:p>", "\n").replace("<a:br/>", "\n");
                        let txt = xml_unescape(&re.replace_all(&s, ""));
                        let lines: Vec<&str> = txt.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
                        slides.push((num, format!("--- Slide {num} ---\n{}", lines.join("\n"))));
                    }
                }
            }
            slides.sort_by_key(|x| x.0);
            if slides.is_empty() {
                return Err("No slides found in .pptx".into());
            }
            Ok(slides.into_iter().map(|x| x.1).collect::<Vec<_>>().join("\n\n"))
        }
        "xlsx" => {
            let entries = read_zip_entries(&bytes);
            let re_tag = regex::Regex::new(r"<[^>]+>").unwrap();
            let mut shared: Vec<String> = vec![];
            if let Some(ss_b) = entries.get("xl/sharedStrings.xml") {
                let ss = String::from_utf8_lossy(ss_b);
                let re_si = regex::Regex::new(r"(?s)<si[^>]*>(.*?)</si>").unwrap();
                for c in re_si.captures_iter(&ss) {
                    shared.push(xml_unescape(&re_tag.replace_all(&c[1], "")));
                }
            }
            let mut sheets: Vec<(String, String)> = vec![];
            let re_row = regex::Regex::new(r"(?s)<row[^>]*>(.*?)</row>").unwrap();
            let re_cell = regex::Regex::new(r#"(?s)<c\b([^>]*)>(.*?)</c>"#).unwrap();
            let re_v = regex::Regex::new(r"(?s)<v[^>]*>(.*?)</v>").unwrap();
            for (k, v) in &entries {
                if k.starts_with("xl/worksheets/sheet") && k.ends_with(".xml") {
                    let xml = String::from_utf8_lossy(v);
                    let mut rows_out = vec![];
                    for rcap in re_row.captures_iter(&xml) {
                        let mut cells = vec![];
                        for ccap in re_cell.captures_iter(&rcap[1]) {
                            let attrs = &ccap[1];
                            let inner = &ccap[2];
                            let is_shared = attrs.contains(r#"t="s""#) || attrs.contains("t='s'");
                            let val = if is_shared {
                                re_v.captures(inner)
                                    .and_then(|vc| vc[1].trim().parse::<usize>().ok())
                                    .and_then(|idx| shared.get(idx).cloned())
                                    .unwrap_or_default()
                            } else if let Some(vc) = re_v.captures(inner) {
                                xml_unescape(vc[1].trim())
                            } else {
                                xml_unescape(&re_tag.replace_all(inner, ""))
                            };
                            cells.push(val);
                        }
                        if cells.iter().any(|c| !c.trim().is_empty()) {
                            rows_out.push(cells.join("\t"));
                        }
                    }
                    let sname = k.trim_start_matches("xl/worksheets/").trim_end_matches(".xml").to_string();
                    if !rows_out.is_empty() {
                        sheets.push((sname.clone(), format!("--- {sname} ---\n{}", rows_out.join("\n"))));
                    }
                }
            }
            sheets.sort_by(|a, b| a.0.cmp(&b.0));
            if sheets.is_empty() {
                return Ok("[Empty .xlsx workbook]".into());
            }
            Ok(sheets.into_iter().map(|x| x.1).collect::<Vec<_>>().join("\n\n"))
        }
        "odt" => {
            let entries = read_zip_entries(&bytes);
            let Some(xml_b) = entries.get("content.xml") else {
                return Err("Invalid .odt file".into());
            };
            let xml = String::from_utf8_lossy(xml_b);
            let s = xml.replace("</text:p>", "\n").replace("</text:h>", "\n").replace("<text:line-break/>", "\n").replace("<text:tab/>", "\t");
            let re = regex::Regex::new(r"<[^>]+>").unwrap();
            Ok(xml_unescape(&re.replace_all(&s, "")).trim().to_string())
        }
        "rtf" => {
            let raw = String::from_utf8_lossy(&bytes);
            let s = raw.replace("\\par", "\n").replace("\\line", "\n").replace("\\tab", "\t");
            let re = regex::Regex::new(r"\\[a-zA-Z]+-?\d* ?|[{}]").unwrap();
            Ok(re.replace_all(&s, "").trim().to_string())
        }
        "pdf" => extract_pdf(&bytes, p),
        _ => Err(format!("Unsupported document format: .{ext}")),
    }
}

fn extract_pdf(bytes: &[u8], path: &Path) -> Res {
    use flate2::read::{DeflateDecoder, ZlibDecoder};
    let mut out = String::new();
    let len = bytes.len();
    let mut i = 0;
    let re_tj = regex::Regex::new(r"(?s)\((?:\\.|[^\\)])*\)|\([0-9A-Fa-f\s]+\)").ok();
    while i + 6 < len {
        if &bytes[i..i + 6] == b"stream" {
            let mut s_start = i + 6;
            if s_start < len && bytes[s_start] == b'\r' { s_start += 1; }
            if s_start < len && bytes[s_start] == b'\n' { s_start += 1; }
            let mut s_end = s_start;
            while s_end + 9 <= len && &bytes[s_end..s_end + 9] != b"endstream" {
                s_end += 1;
            }
            if s_end > s_start && s_end + 9 <= len {
                let raw_stream = &bytes[s_start..s_end];
                let mut decomp = Vec::new();
                if ZlibDecoder::new(raw_stream).read_to_end(&mut decomp).is_err() {
                    decomp.clear();
                    let _ = DeflateDecoder::new(raw_stream).read_to_end(&mut decomp);
                }
                let data = if decomp.is_empty() { raw_stream } else { &decomp };
                let s = String::from_utf8_lossy(data);
                if s.contains("BT") && s.contains("ET") {
                    for blk in s.split("BT").skip(1) {
                        let bt = blk.split("ET").next().unwrap_or(blk);
                        for line in bt.lines() {
                            let l = line.trim();
                            if l.ends_with("Td") || l.ends_with("TD") || l == "T*" || l.ends_with("'") {
                                out.push('\n');
                            }
                            if let Some(ref re) = re_tj {
                                for m in re.find_iter(l) {
                                    let tok = m.as_str();
                                    if tok.starts_with('(') && tok.ends_with(')') {
                                        let inner = &tok[1..tok.len() - 1];
                                        let unesc = inner
                                            .replace("\\n", "\n")
                                            .replace("\\r", "\r")
                                            .replace("\\t", "\t")
                                            .replace("\\(", "(")
                                            .replace("\\)", ")")
                                            .replace("\\\\", "\\");
                                        out.push_str(&unesc);
                                    }
                                }
                            }
                        }
                        out.push('\n');
                    }
                }
            }
            i = s_end + 9;
            continue;
        }
        i += 1;
    }
    let alpha_cnt = out.chars().filter(|c| c.is_alphanumeric()).count();
    if alpha_cnt >= 25 {
        let collapse = regex::Regex::new(r"\n{3,}").unwrap();
        return Ok(collapse.replace_all(out.trim(), "\n\n").to_string());
    }
    // Fallback for scanned PDFs or custom CID font PDFs: render pages with Windows.Data.Pdf + Windows.Media.Ocr
    ocr_pdf_winrt(path)
}

fn ocr_pdf_winrt(p: &Path) -> Res {
    let ps = format!(
        r#"$ErrorActionPreference = 'Stop'
$path = '{}'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Data.Pdf.PdfDocument, Windows.Data.Pdf, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Graphics, ContentType = WindowsRuntime]
$null = [Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]
$null = [Windows.Storage.Streams.InMemoryRandomAccessStream, Windows.Storage.Streams, ContentType = WindowsRuntime]
$asTaskG = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {{ $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' }})[0]
$asTaskA = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {{ $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncAction' }})[0]
function Await($op, $t) {{ $m = $asTaskG.MakeGenericMethod($t); $tk = $m.Invoke($null, @($op)); $tk.Wait(); return $tk.Result }}
function AwaitAct($op) {{ $tk = $asTaskA.Invoke($null, @($op)); $tk.Wait() }}
$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($path)) ([Windows.Storage.StorageFile])
$pdf = Await ([Windows.Data.Pdf.PdfDocument]::LoadFromFileAsync($file)) ([Windows.Data.Pdf.PdfDocument])
$eng = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
$max = [Math]::Min([int]$pdf.PageCount, 15)
for ($i = 0; $i -lt $max; $i++) {{
    $page = $pdf.GetPage([uint32]$i)
    $mem = New-Object Windows.Storage.Streams.InMemoryRandomAccessStream
    AwaitAct ($page.RenderToStreamAsync($mem))
    $dec = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($mem)) ([Windows.Graphics.Imaging.BitmapDecoder])
    $bmp = Await ($dec.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
    $res = Await ($eng.RecognizeAsync($bmp)) ([Windows.Media.Ocr.OcrResult])
    Write-Output ("--- Page " + ($i + 1) + " ---")
    foreach ($line in $res.Lines) {{ Write-Output $line.Text }}
    $mem.Dispose()
    $page.Dispose()
}}"#,
        p.display().to_string().replace('\'', "''")
    );
    let cancel = Arc::new(AtomicBool::new(false));
    run_ps(&ps, "", 45, &cancel)
}

pub fn ocr_file(p: &Path) -> Res {
    if !p.exists() {
        return Err(format!("Image not found: {}", p.display()));
    }
    let ps = format!(
        r#"$ErrorActionPreference = 'Stop'
$path = '{}'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Graphics, ContentType = WindowsRuntime]
$null = [Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]
$asTaskG = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {{ $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' }})[0]
function Await($op, $t) {{ $m = $asTaskG.MakeGenericMethod($t); $tk = $m.Invoke($null, @($op)); $tk.Wait(); return $tk.Result }}
$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($path)) ([Windows.Storage.StorageFile])
$stream = Await ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream])
$dec = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])
$bmp = Await ($dec.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
$eng = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
$res = Await ($eng.RecognizeAsync($bmp)) ([Windows.Media.Ocr.OcrResult])
$stream.Dispose()
Write-Output ("[Image " + $dec.PixelWidth + "x" + $dec.PixelHeight + "]")
foreach ($line in $res.Lines) {{ Write-Output $line.Text }}"#,
        p.display().to_string().replace('\'', "''")
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let out = run_ps(&ps, "", 30, &cancel)?;
    if out.trim().is_empty() {
        Ok("[No text detected by OCR in image]".into())
    } else {
        Ok(out.trim().to_string())
    }
}

fn write_file(a: &Value) -> Res {
    let p = resolve(s(a, "path"));
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let c = s(a, "content");
    std::fs::write(&p, c).map_err(|e| e.to_string())?;
    Ok(format!("Wrote {} bytes to {}", c.len(), p.display()))
}

fn edit_file(a: &Value) -> Res {
    let p = resolve(s(a, "path"));
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let (old, new) = (s(a, "old_string"), s(a, "new_string"));
    if old.is_empty() {
        return Err("old_string is empty".into());
    }
    let all = a.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
    let count = text.matches(old).count();
    if count == 0 {
        return Err("old_string not found in file".into());
    }
    if count > 1 && !all {
        return Err(format!("old_string found {count} times; add context or set replace_all"));
    }
    let out = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    std::fs::write(&p, out).map_err(|e| e.to_string())?;
    Ok(format!("Edited {} ({} replacement{})", p.display(), if all { count } else { 1 }, if all && count > 1 { "s" } else { "" }))
}

fn list_dir(a: &Value) -> Res {
    let root = resolve(if s(a, "path").is_empty() { "~" } else { s(a, "path") });
    let depth = n(a, "depth").unwrap_or(2).clamp(1, 5) as usize;
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()));
    }
    let mut out = format!("{}\n", root.display());
    let mut count = 0usize;
    fn walk(dir: &Path, prefix: &str, depth: usize, max: usize, out: &mut String, count: &mut usize) {
        if depth == 0 || *count > 600 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| (!e.path().is_dir(), e.file_name().to_ascii_lowercase()));
        let len = entries.len();
        for (i, e) in entries.iter().enumerate() {
            *count += 1;
            if *count > 600 {
                out.push_str(&format!("{prefix}… (more)\n"));
                return;
            }
            let last = i == len - 1;
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.path().is_dir();
            let size = if is_dir { String::new() } else {
                e.metadata().map(|m| format!("  ({})", human(m.len()))).unwrap_or_default()
            };
            out.push_str(&format!("{prefix}{}{}{}{}\n", if last { "└─ " } else { "├─ " }, name, if is_dir { "/" } else { "" }, size));
            if is_dir && !matches!(name.as_str(), "node_modules" | ".git" | "target" | "$RECYCLE.BIN") {
                let np = format!("{prefix}{}", if last { "   " } else { "│  " });
                walk(&e.path(), &np, depth - 1, max, out, count);
            }
        }
    }
    walk(&root, "", depth, 600, &mut out, &mut count);
    Ok(truncate(&out, MAX_OUT))
}

fn human(b: u64) -> String {
    let f = b as f64;
    if f < 1024.0 { format!("{b} B") }
    else if f < 1024.0 * 1024.0 { format!("{:.1} KB", f / 1024.0) }
    else if f < 1024.0 * 1024.0 * 1024.0 { format!("{:.1} MB", f / 1048576.0) }
    else { format!("{:.2} GB", f / 1073741824.0) }
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .max_depth(Some(12))
        .build()
}

fn glob(a: &Value) -> Res {
    let root = resolve(if s(a, "path").is_empty() { "~" } else { s(a, "path") });
    let pat = s(a, "pattern");
    let g = globset::GlobBuilder::new(pat)
        .case_insensitive(true)
        .literal_separator(false)
        .build()
        .map_err(|e| e.to_string())?
        .compile_matcher();
    let start = Instant::now();
    let mut hits = Vec::new();
    for e in walker(&root).flatten() {
        if start.elapsed() > Duration::from_secs(20) || hits.len() >= 300 {
            break;
        }
        let p = e.path();
        if let Ok(rel) = p.strip_prefix(&root) {
            if g.is_match(rel) || g.is_match(p.file_name().unwrap_or_default()) {
                hits.push(p.display().to_string());
            }
        }
    }
    if hits.is_empty() {
        return Ok(format!("No matches for {pat} under {}", root.display()));
    }
    let more = if hits.len() >= 300 { "\n[limited to 300 results]" } else { "" };
    Ok(format!("{}{}", hits.join("\n"), more))
}

fn grep(a: &Value, cancel: &Arc<AtomicBool>) -> Res {
    let root = resolve(if s(a, "path").is_empty() { "~" } else { s(a, "path") });
    let cs = a.get("case_sensitive").and_then(|v| v.as_bool()).unwrap_or(false);
    let re = regex::RegexBuilder::new(s(a, "pattern"))
        .case_insensitive(!cs)
        .build()
        .map_err(|e| e.to_string())?;
    let inc = s(a, "include");
    let incm = if inc.is_empty() { None } else {
        Some(globset::GlobBuilder::new(inc).case_insensitive(true).build().map_err(|e| e.to_string())?.compile_matcher())
    };
    let start = Instant::now();
    let mut out = Vec::new();
    let files: Box<dyn Iterator<Item = PathBuf>> = if root.is_file() {
        Box::new(std::iter::once(root.clone()))
    } else {
        Box::new(walker(&root).flatten().filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false)).map(|e| e.into_path()))
    };
    'outer: for p in files {
        if cancel.load(Ordering::Relaxed) || start.elapsed() > Duration::from_secs(25) {
            break;
        }
        if let Some(m) = &incm {
            if !m.is_match(p.file_name().unwrap_or_default()) {
                continue;
            }
        }
        let Ok(meta) = p.metadata() else { continue };
        if meta.len() > 5_000_000 {
            continue;
        }
        let Ok(bytes) = std::fs::read(&p) else { continue };
        if bytes.iter().take(4000).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                out.push(format!("{}:{}: {}", p.display(), i + 1, short(line, 200)));
                if out.len() >= 200 {
                    break 'outer;
                }
            }
        }
    }
    if out.is_empty() {
        return Ok("No matches.".into());
    }
    Ok(truncate(&out.join("\n"), MAX_OUT))
}

// ---------- processes ----------

fn ps_command(cmd: &str, cwd: &str) -> Command {
    let mut c = Command::new("powershell.exe");
    let script = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $OutputEncoding=[System.Text.Encoding]::UTF8; $ProgressPreference='SilentlyContinue'; {cmd}"
    );
    c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script]);
    let dir = if cwd.is_empty() { home() } else { resolve(cwd) };
    c.current_dir(dir);
    c.creation_flags(CREATE_NO_WINDOW);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

fn pipe_reader<R: Read + Send + 'static>(mut r: R, buf: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(k) => {
                    let mut b = buf.lock().unwrap();
                    b.extend_from_slice(&chunk[..k]);
                    if b.len() > 4_000_000 {
                        let cut = b.len() - 2_000_000;
                        b.drain(..cut);
                    }
                }
            }
        }
    });
}

fn wait_child(mut child: Child, timeout: u64, cancel: &Arc<AtomicBool>) -> Res {
    let out = Arc::new(Mutex::new(Vec::new()));
    if let Some(o) = child.stdout.take() { pipe_reader(o, out.clone()); }
    if let Some(e) = child.stderr.take() { pipe_reader(e, out.clone()); }
    let start = Instant::now();
    let status = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break Some(st);
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            std::thread::sleep(Duration::from_millis(100));
            let o = String::from_utf8_lossy(&out.lock().unwrap()).to_string();
            return Err(format!("Cancelled by user.\n{}", truncate(&o, MAX_OUT)));
        }
        if start.elapsed() > Duration::from_secs(timeout) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    std::thread::sleep(Duration::from_millis(60));
    let o = String::from_utf8_lossy(&out.lock().unwrap()).trim_end().to_string();
    let o = if o.is_empty() { "(no output)".to_string() } else { o };
    match status {
        None => Err(format!("Timed out after {timeout}s.\n{}", tail(&o, MAX_OUT))),
        Some(st) if st.success() => Ok(tail(&o, MAX_OUT)),
        Some(st) => Ok(format!("[exit code {}]\n{}", st.code().unwrap_or(-1), tail(&o, MAX_OUT))),
    }
}

fn tail(t: &str, max: usize) -> String {
    if t.len() <= max {
        return t.to_string();
    }
    let mut cut = t.len() - max;
    while !t.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…[{} bytes omitted]\n{}", cut, &t[cut..])
}

fn run_ps(cmd: &str, cwd: &str, timeout: u64, cancel: &Arc<AtomicBool>) -> Res {
    let child = ps_command(cmd, cwd).spawn().map_err(|e| e.to_string())?;
    wait_child(child, timeout, cancel)
}

fn run_python(a: &Value, cancel: &Arc<AtomicBool>) -> Res {
    let dir = std::env::temp_dir().join("nusa-ai");
    let _ = std::fs::create_dir_all(&dir);
    let f = dir.join(format!("run_{}.py", chrono::Local::now().format("%H%M%S%3f")));
    std::fs::write(&f, s(a, "code")).map_err(|e| e.to_string())?;
    let t = n(a, "timeout_sec").unwrap_or(120).clamp(1, 900) as u64;
    let mut c = Command::new("python");
    c.arg("-X").arg("utf8").arg(&f)
        .current_dir(home())
        .env("PYTHONIOENCODING", "utf-8")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| format!("Could not start python: {e}"))?;
    let r = wait_child(child, t, cancel);
    let _ = std::fs::remove_file(&f);
    r
}

struct Bg {
    child: Child,
    out: Arc<Mutex<Vec<u8>>>,
    cmd: String,
}

fn bg() -> &'static Mutex<HashMap<u32, Bg>> {
    static B: OnceLock<Mutex<HashMap<u32, Bg>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}
static BG_ID: AtomicU32 = AtomicU32::new(1);

fn bg_start(a: &Value) -> Res {
    let cmd = s(a, "command").to_string();
    let mut child = ps_command(&cmd, s(a, "cwd")).spawn().map_err(|e| e.to_string())?;
    let out = Arc::new(Mutex::new(Vec::new()));
    if let Some(o) = child.stdout.take() { pipe_reader(o, out.clone()); }
    if let Some(e) = child.stderr.take() { pipe_reader(e, out.clone()); }
    let id = BG_ID.fetch_add(1, Ordering::Relaxed);
    let pid = child.id();
    bg().lock().unwrap().insert(id, Bg { child, out, cmd });
    Ok(format!("Started background process id={id} (pid {pid}). Use bg_output to check it."))
}

fn bg_output(a: &Value) -> Res {
    let id = n(a, "id").unwrap_or(0) as u32;
    let t = n(a, "tail").unwrap_or(4000).clamp(200, MAX_OUT as i64) as usize;
    let mut map = bg().lock().unwrap();
    let b = map.get_mut(&id).ok_or_else(|| format!("No background process {id}"))?;
    let status = match b.child.try_wait() {
        Ok(Some(st)) => format!("exited ({})", st.code().unwrap_or(-1)),
        Ok(None) => "running".into(),
        Err(e) => e.to_string(),
    };
    let o = String::from_utf8_lossy(&b.out.lock().unwrap()).to_string();
    Ok(format!("[{status}] {}\n{}", short(&b.cmd, 80), tail(o.trim_end(), t)))
}

fn bg_stop(a: &Value) -> Res {
    let id = n(a, "id").unwrap_or(0) as u32;
    let mut map = bg().lock().unwrap();
    let mut b = map.remove(&id).ok_or_else(|| format!("No background process {id}"))?;
    let pid = b.child.id();
    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(CREATE_NO_WINDOW).output();
    let _ = b.child.kill();
    Ok(format!("Stopped background process {id}"))
}

pub fn kill_all_bg() {
    let mut map = bg().lock().unwrap();
    for (_, mut b) in map.drain() {
        let pid = b.child.id();
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(CREATE_NO_WINDOW).output();
        let _ = b.child.kill();
    }
}

// ---------- web ----------

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(UA)
        .build()
        .new_agent()
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn url_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn strip_tags(s: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"<[^>]+>").unwrap());
    let t = re.replace_all(s, "");
    t.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn web_search(a: &Value) -> Res {
    let q = s(a, "query");
    let max = n(a, "max_results").unwrap_or(8).clamp(1, 20) as usize;
    let body = format!("q={}&kl=wt-wt", url_encode(q));
    let mut resp = http()
        .post("https://html.duckduckgo.com/html/")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept-Language", "en-US,en;q=0.9,id;q=0.8")
        .send(body.as_bytes())
        .map_err(|e| e.to_string())?;
    let html = resp.body_mut().with_config().limit(5_000_000).read_to_string().map_err(|e| e.to_string())?;
    let re_a = regex::Regex::new(r#"(?s)class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let re_s = regex::Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();
    let links: Vec<_> = re_a.captures_iter(&html).collect();
    let snippets: Vec<String> = re_s.captures_iter(&html).map(|c| strip_tags(&c[1])).collect();
    let mut out = Vec::new();
    for (i, c) in links.iter().enumerate() {
        let mut href = strip_tags(&c[1]);
        if href.contains("duckduckgo.com/y.js") {
            continue;
        }
        if let Some(pos) = href.find("uddg=") {
            let rest = &href[pos + 5..];
            let end = rest.find('&').unwrap_or(rest.len());
            href = url_decode(&rest[..end]);
        } else if href.starts_with("//") {
            href = format!("https:{href}");
        }
        let title = strip_tags(&c[2]);
        let snip = snippets.get(i).cloned().unwrap_or_default();
        out.push(format!("{}. {}\n   {}\n   {}", out.len() + 1, title, href, snip));
        if out.len() >= max {
            break;
        }
    }
    if out.is_empty() {
        return Ok(format!("No results for \"{q}\"."));
    }
    Ok(out.join("\n\n"))
}

fn web_fetch(a: &Value) -> Res {
    let mut url = s(a, "url").trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("https://{url}");
    }
    let max = n(a, "max_chars").unwrap_or(20_000).clamp(1000, 100_000) as usize;
    let mut resp = http()
        .get(&url)
        .header("Accept", "text/html,application/xhtml+xml,text/plain,application/json;q=0.9,*/*;q=0.8")
        .call()
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let bytes = resp.body_mut().with_config().limit(8_000_000).read_to_vec().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let body = if ctype.contains("html") || text.trim_start().starts_with('<') {
        let cleaned = regex::Regex::new(r"(?is)<(script|style|noscript|svg|iframe|head)[^>]*>.*?</(script|style|noscript|svg|iframe|head)>")
            .unwrap()
            .replace_all(&text, "");
        let md = htmd::convert(&cleaned).unwrap_or_else(|_| strip_tags(&cleaned));
        let collapse = regex::Regex::new(r"\n{3,}").unwrap();
        collapse.replace_all(&md, "\n\n").to_string()
    } else {
        text
    };
    let head = if status >= 400 { format!("[HTTP {status}]\n") } else { String::new() };
    Ok(format!("{head}{}", truncate(body.trim(), max)))
}

// ---------- Nusa built-in Todo List (%APPDATA%\Nusa\todo.txt) ----------

#[derive(Clone, Debug, PartialEq)]
pub struct NusaTodo {
    pub completed: bool,
    pub start_min: Option<u16>,
    pub end_min: Option<u16>,
    pub done_date: u32,
    pub title: String,
}

pub fn nusa_todo_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData\\Roaming"))
        .join("Nusa")
        .join("todo.txt")
}

fn today_yyyymmdd() -> u32 {
    use chrono::Datelike;
    let d = chrono::Local::now().date_naive();
    (d.year() as u32) * 10_000 + d.month() * 100 + d.day()
}

pub fn fmt_min(m: u16) -> String {
    format!("{:02}:{:02}", (m / 60) % 24, m % 60)
}

pub fn parse_time_min(raw: &str) -> Option<u16> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() || s == "-" {
        return None;
    }
    let is_pm = s.contains("pm") || s.contains("siang") || s.contains("sore") || s.contains("malam");
    let is_am = s.contains("am") || s.contains("pagi");
    let clean: String = s
        .chars()
        .map(|c| if c == '.' { ':' } else { c })
        .filter(|c| c.is_ascii_digit() || *c == ':')
        .collect();
    let (mut h, m) = if let Some((hs, ms)) = clean.split_once(':') {
        (hs.parse::<u16>().ok()?, ms.get(..2).unwrap_or(ms).parse::<u16>().ok()?)
    } else {
        let v = clean.parse::<u16>().ok()?;
        if v <= 24 {
            (v, 0)
        } else if v < 1440 {
            return Some(v);
        } else {
            return None;
        }
    };
    if m >= 60 || h > 24 {
        return None;
    }
    if is_pm && h < 12 {
        h += 12;
    } else if is_am && h == 12 {
        h = 0;
    }
    let total = (h % 24) * 60 + m;
    Some(total)
}

pub fn extract_time_from_text(text: &str) -> (Option<u16>, Option<u16>) {
    // Range like "12:00-13:00" or "12.00 - 13.30"
    if let Ok(re_range) = regex::Regex::new(r"(?i)\b(\d{1,2}[:.]\d{2})\s*[-–—s/d]+\s*(\d{1,2}[:.]\d{2})\b") {
        if let Some(c) = re_range.captures(text) {
            let st = parse_time_min(&c[1]);
            let en = parse_time_min(&c[2]);
            if st.is_some() {
                return (st, en);
            }
        }
    }
    // Indonesian "jam/pukul X[:YY] [pagi/siang/sore/malam]"
    if let Ok(re_id) = regex::Regex::new(r"(?i)\b(?:jam|pukul|pkl)\s*(\d{1,2}(?:[:.]\d{2})?)\s*(pagi|siang|sore|malam|am|pm)?\b") {
        if let Some(c) = re_id.captures(text) {
            let raw = format!("{} {}", &c[1], c.get(2).map(|m| m.as_str()).unwrap_or(""));
            if let Some(st) = parse_time_min(&raw) {
                return (Some(st), None);
            }
        }
    }
    // Plain "HH:MM"
    if let Ok(re_hm) = regex::Regex::new(r"\b([01]?\d|2[0-3]):([0-5]\d)\b") {
        if let Some(c) = re_hm.captures(text) {
            if let Some(st) = parse_time_min(&c[0]) {
                return (Some(st), None);
            }
        }
    }
    (None, None)
}

pub fn parse_nusa_todos_str(text: &str) -> Vec<NusaTodo> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }
        let completed = parts[0].trim() == "1" || parts[0].trim().eq_ignore_ascii_case("true");
        let start_min = parts[1].trim().parse::<u16>().ok().filter(|&m| m < 1440);
        let end_min = parts[2].trim().parse::<u16>().ok().filter(|&m| m < 1440);
        let done_date = parts[3].trim().parse::<u32>().unwrap_or(0);
        let title = parts[4..].join(" ").trim().to_string();
        if !title.is_empty() {
            out.push(NusaTodo { completed, start_min, end_min, done_date, title });
        }
    }
    out
}

pub fn format_nusa_todos_str(items: &[NusaTodo]) -> String {
    let mut out = String::new();
    for item in items {
        let c = if item.completed { 1 } else { 0 };
        let st = item.start_min.map(|m| m.to_string()).unwrap_or_else(|| "-".into());
        let en = item.end_min.map(|m| m.to_string()).unwrap_or_else(|| "-".into());
        let dt = if item.completed {
            if item.done_date > 0 { item.done_date } else { today_yyyymmdd() }
        } else {
            0
        };
        let clean = item.title.replace(['\t', '\r', '\n'], " ").trim().to_string();
        if !clean.is_empty() {
            out.push_str(&format!("{c}\t{st}\t{en}\t{dt}\t{clean}\n"));
        }
    }
    out
}

pub fn load_nusa_todos() -> Vec<NusaTodo> {
    let p = nusa_todo_path();
    std::fs::read_to_string(&p).map(|s| parse_nusa_todos_str(&s)).unwrap_or_default()
}

pub fn save_nusa_todos(items: &[NusaTodo]) -> Result<(), String> {
    let p = nusa_todo_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = format_nusa_todos_str(items);
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    if old == body {
        return Ok(());
    }
    std::fs::write(&p, body).map_err(|e| e.to_string())?;
    #[cfg(not(test))]
    crate::notch::reload_nusa();
    Ok(())
}

pub fn nusa_todos_as_json(items: &[NusaTodo]) -> Vec<Value> {
    items
        .iter()
        .map(|t| {
            let mut v = json!({
                "content": t.title,
                "status": if t.completed { "completed" } else { "pending" },
            });
            if let Some(st) = t.start_min {
                v["start_time"] = json!(fmt_min(st));
            }
            if let Some(en) = t.end_min {
                v["end_time"] = json!(fmt_min(en));
            }
            v
        })
        .collect()
}

/// Sync `todo_write` checklist items into `%APPDATA%\Nusa\todo.txt` (preserving existing items that aren't in the list, or updating matching ones).
pub fn sync_todos_to_nusa(todos: &[Value]) -> Vec<Value> {
    let mut existing = load_nusa_todos();
    for td in todos {
        let title = s(td, "content").trim();
        if title.is_empty() {
            continue;
        }
        let completed = s(td, "status") == "completed";
        let explicit_st = parse_time_min(s(td, "start_time"));
        let explicit_en = parse_time_min(s(td, "end_time"));
        let (inferred_st, inferred_en) = extract_time_from_text(title);
        let start_min = explicit_st.or(inferred_st);
        let end_min = explicit_en.or(inferred_en);

        if let Some(found) = existing.iter_mut().find(|x| x.title.eq_ignore_ascii_case(title)) {
            found.completed = completed;
            if start_min.is_some() {
                found.start_min = start_min;
            }
            if end_min.is_some() {
                found.end_min = end_min;
            }
            if completed && found.done_date == 0 {
                found.done_date = today_yyyymmdd();
            } else if !completed {
                found.done_date = 0;
            }
        } else {
            existing.push(NusaTodo {
                completed,
                start_min,
                end_min,
                done_date: if completed { today_yyyymmdd() } else { 0 },
                title: title.to_string(),
            });
        }
    }
    let _ = save_nusa_todos(&existing);
    nusa_todos_as_json(&existing)
}

fn format_nusa_todo_summary(items: &[NusaTodo]) -> String {
    if items.is_empty() {
        return "Nusa Todo List is empty.".into();
    }
    let mut lines = Vec::new();
    for (i, t) in items.iter().enumerate() {
        let mark = if t.completed { "[x]" } else { "[ ]" };
        let time = match (t.start_min, t.end_min) {
            (Some(s), Some(e)) => format!(" ({}-{})", fmt_min(s), fmt_min(e)),
            (Some(s), None) => format!(" ({})", fmt_min(s)),
            _ => String::new(),
        };
        lines.push(format!("{}. {}{} {}", i + 1, mark, time, t.title));
    }
    lines.join("\n")
}

fn nusa_todo(a: &Value) -> Res {
    let action = s(a, "action").trim().to_lowercase();
    let title = s(a, "title").trim();
    let mut items = load_nusa_todos();

    match action.as_str() {
        "list" | "" => Ok(format_nusa_todo_summary(&items)),
        "add" => {
            if title.is_empty() {
                return Err("title is required for action=add".into());
            }
            let explicit_st = parse_time_min(s(a, "start_time"));
            let explicit_en = parse_time_min(s(a, "end_time"));
            let (inf_st, inf_en) = extract_time_from_text(title);
            let start_min = explicit_st.or(inf_st);
            let end_min = explicit_en.or(inf_en);
            if let Some(found) = items.iter_mut().find(|x| x.title.eq_ignore_ascii_case(title)) {
                found.completed = false;
                found.done_date = 0;
                if start_min.is_some() {
                    found.start_min = start_min;
                }
                if end_min.is_some() {
                    found.end_min = end_min;
                }
            } else {
                items.push(NusaTodo {
                    completed: false,
                    start_min,
                    end_min,
                    done_date: 0,
                    title: title.to_string(),
                });
            }
            save_nusa_todos(&items)?;
            Ok(format!("Added to Nusa Todo List:\n{}", format_nusa_todo_summary(&items)))
        }
        "complete" | "uncomplete" => {
            let done = action == "complete";
            let idx = title.trim_start_matches('#').parse::<usize>().ok();
            let q = title.to_lowercase();
            let mut matched = false;
            for (i, item) in items.iter_mut().enumerate() {
                if idx == Some(i + 1) || (!q.is_empty() && item.title.to_lowercase().contains(&q)) {
                    item.completed = done;
                    item.done_date = if done { today_yyyymmdd() } else { 0 };
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Err(format!("Task not found: {title}"));
            }
            save_nusa_todos(&items)?;
            Ok(format!("Updated Nusa Todo List:\n{}", format_nusa_todo_summary(&items)))
        }
        "delete" => {
            let idx = title.trim_start_matches('#').parse::<usize>().ok();
            let q = title.to_lowercase();
            let before = items.len();
            let mut removed_one = false;
            items.retain(|item| {
                if removed_one {
                    return true;
                }
                let hit = !q.is_empty() && item.title.to_lowercase().contains(&q);
                if hit {
                    removed_one = true;
                    false
                } else {
                    true
                }
            });
            if !removed_one {
                if let Some(k) = idx {
                    if k >= 1 && k <= before {
                        items.remove(k - 1);
                        removed_one = true;
                    }
                }
            }
            if !removed_one {
                return Err(format!("Task not found: {title}"));
            }
            save_nusa_todos(&items)?;
            Ok(format!("Removed from Nusa Todo List:\n{}", format_nusa_todo_summary(&items)))
        }
        "clear_completed" => {
            items.retain(|x| !x.completed);
            save_nusa_todos(&items)?;
            Ok(format!("Cleared completed tasks:\n{}", format_nusa_todo_summary(&items)))
        }
        other => Err(format!("Unknown nusa_todo action: {other}")),
    }
}


#[allow(dead_code)]
pub fn flush_stdout() {
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nusa-ai-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn c() -> Arc<AtomicBool> { Arc::new(AtomicBool::new(false)) }
    fn run(name: &str, a: Value) -> Res { execute(name, &a, &c()) }

    #[test]
    fn approval_policy() {
        for t in ["shell", "write_file", "edit_file", "delete_path", "move_path", "run_python", "bg_start"] {
            assert!(needs_approval(t), "{t} must need approval");
        }
        for t in ["read_file", "list_dir", "glob", "grep", "web_search", "web_fetch", "bg_output", "todo_write"] {
            assert!(!needs_approval(t), "{t} must run automatically");
        }
    }

    #[test]
    fn specs_are_valid() {
        let sp = specs();
        assert!(sp.len() >= 18);
        for t in &sp {
            assert_eq!(t["type"], "function");
            assert!(t["function"]["name"].as_str().unwrap().len() > 2);
            assert_eq!(t["function"]["parameters"]["type"], "object");
        }
    }

    #[test]
    fn write_read_edit_roundtrip() {
        let d = tmp("rw");
        let f = d.join("sub").join("a.txt");
        let fs = f.to_string_lossy().to_string();
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        run("write_file", json!({"path": fs, "content": body})).unwrap();
        let r = run("read_file", json!({"path": fs, "offset": 3, "limit": 2})).unwrap();
        assert!(r.contains("    3\tline 3") && r.contains("    4\tline 4") && !r.contains("line 5\n"), "{r}");
        assert!(r.contains("of 10"));
        assert!(run("edit_file", json!({"path": fs, "old_string": "line", "new_string": "x"})).is_err());
        run("edit_file", json!({"path": fs, "old_string": "line 7\n", "new_string": "SEVEN\n"})).unwrap();
        assert!(std::fs::read_to_string(&f).unwrap().contains("SEVEN"));
        run("edit_file", json!({"path": fs, "old_string": "line", "new_string": "L", "replace_all": true})).unwrap();
        assert!(!std::fs::read_to_string(&f).unwrap().contains("line"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn list_glob_grep_move() {
        let d = tmp("lg");
        std::fs::create_dir_all(d.join("docs")).unwrap();
        std::fs::write(d.join("docs").join("report.pdf"), b"%PDF").unwrap();
        std::fs::write(d.join("notes.txt"), "hello Nusa\nbye").unwrap();
        let ds = d.to_string_lossy().to_string();
        let l = run("list_dir", json!({"path": ds})).unwrap();
        assert!(l.contains("docs") && l.contains("notes.txt"), "{l}");
        let g = run("glob", json!({"pattern": "**/*.pdf", "path": ds})).unwrap();
        assert!(g.contains("report.pdf"), "{g}");
        let s = run("grep", json!({"pattern": "nusa", "path": ds})).unwrap();
        assert!(s.contains("notes.txt") && s.contains("hello Nusa"), "{s}");
        let to = d.join("moved").join("n.txt");
        run("move_path", json!({"from": d.join("notes.txt").to_string_lossy(), "to": to.to_string_lossy()})).unwrap();
        assert!(to.exists() && !d.join("notes.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shell_runs_powershell() {
        let r = run("shell", json!({"command": "Write-Output (40+2)"})).unwrap();
        assert!(r.contains("42"), "{r}");
    }

    #[test]
    fn background_process() {
        let r = run("bg_start", json!({"command": "Write-Output started; Start-Sleep 30"})).unwrap();
        let id: i64 = r.split("id=").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        let mut out = String::new();
        for _ in 0..40 {
            out = run("bg_output", json!({"id": id})).unwrap();
            if out.contains("started") { break; }
            std::thread::sleep(Duration::from_millis(250));
        }
        assert!(out.contains("[running]") && out.contains("started"), "{out}");
        run("bg_stop", json!({"id": id})).unwrap();
        assert!(run("bg_output", json!({"id": id})).is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(url_encode("a b&c"), "a+b%26c");
        assert_eq!(strip_tags("<b>Tom &amp; Jerry</b>"), "Tom & Jerry");
        let t = truncate("ééééé", 3);
        assert!(t.starts_with('é') && t.contains("truncated"));
        assert_eq!(label("shell", &json!({"command": "dir"})).0, "Run");
    }

    #[test]
    fn nusa_todo_format_and_time_parser() {
        assert_eq!(parse_time_min("12:00"), Some(720));
        assert_eq!(parse_time_min("2 siang"), Some(840));
        assert_eq!(parse_time_min("9:30 pagi"), Some(570));
        assert_eq!(extract_time_from_text("Rapat jam 12 siang hari ini"), (Some(720), None));
        assert_eq!(extract_time_from_text("Meeting 14:00-15:30"), (Some(840), Some(930)));

        let items = vec![
            NusaTodo { completed: false, start_min: Some(720), end_min: None, done_date: 0, title: "Rapat jam 12 siang".into() },
            NusaTodo { completed: true, start_min: Some(540), end_min: Some(600), done_date: 20261024, title: "Standup".into() },
        ];
        let raw = format_nusa_todos_str(&items);
        assert_eq!(raw, "0\t720\t-\t0\tRapat jam 12 siang\n1\t540\t600\t20261024\tStandup\n");
        let parsed = parse_nusa_todos_str(&raw);
        assert_eq!(parsed, items);
    }

    /// Network test: `cargo test -- --ignored`
    #[test]
    #[ignore]
    fn web_search_live() {
        let r = run("web_search", json!({"query": "rust programming language", "max_results": 3})).unwrap();
        assert!(r.contains("1.") && r.contains("http"), "{r}");
    }
}
