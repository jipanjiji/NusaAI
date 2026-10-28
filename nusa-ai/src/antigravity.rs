use crate::agent::{self, trim_working_set, Emit};
use crate::tools;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub fn client_id() -> String {
    const B: &[u8] = &[107, 106, 109, 107, 106, 106, 108, 106, 108, 106, 111, 99, 107, 119, 46, 55, 50, 41, 41, 51, 52, 104, 50, 104, 107, 54, 57, 40, 63, 104, 105, 111, 44, 46, 53, 54, 53, 48, 50, 110, 61, 110, 106, 105, 63, 42, 116, 59, 42, 42, 41, 116, 61, 53, 53, 61, 54, 63, 47, 41, 63, 40, 57, 53, 52, 46, 63, 52, 46, 116, 57, 53, 55];
    B.iter().map(|b| (b ^ 0x5A) as char).collect()
}

pub fn client_secret() -> String {
    const B: &[u8] = &[29, 21, 25, 9, 10, 2, 119, 17, 111, 98, 28, 13, 8, 110, 98, 108, 22, 62, 22, 16, 107, 55, 22, 24, 98, 41, 2, 25, 110, 32, 108, 43, 30, 27, 60];
    B.iter().map(|b| (b ^ 0x5A) as char).collect()
}
pub const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
pub const LOAD_CODE_ASSIST_URL: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
pub const STREAM_GENERATE_URL: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse";

pub const DEFAULT_MODEL: &str = "gemini-3.8-flash-tiered";
pub const FALLBACK_MODEL: &str = "gemini-3-flash";

pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AntigravityToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub email: String,
    pub name: String,
    pub project_id: String,
    #[serde(default = "default_model")]
    pub model: String,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

pub fn token_path() -> PathBuf {
    agent::data_dir().join("antigravity_token.json")
}

pub fn load_token() -> Option<AntigravityToken> {
    let p = token_path();
    let text = std::fs::read_to_string(&p).ok()?;
    let mut token: AntigravityToken = serde_json::from_str(&text).ok()?;
    if token.model.trim().is_empty() || token.model == "gemini-3.8-flash" {
        token.model = DEFAULT_MODEL.to_string();
    }
    Some(token)
}

pub fn save_token(token: &AntigravityToken) -> Result<(), String> {
    let p = token_path();
    let text = serde_json::to_string_pretty(token).map_err(|e| e.to_string())?;
    std::fs::write(&p, text).map_err(|e| format!("Failed to write token file: {e}"))?;
    Ok(())
}

pub fn clear_token() {
    let _ = std::fs::remove_file(token_path());
}

pub fn is_authenticated() -> bool {
    load_token()
        .map(|t| !t.refresh_token.trim().is_empty() || !t.access_token.trim().is_empty())
        .unwrap_or(false)
}

pub fn status_json() -> Value {
    if let Some(t) = load_token() {
        if !t.refresh_token.trim().is_empty() || !t.access_token.trim().is_empty() {
            return json!({
                "connected": true,
                "email": t.email,
                "name": t.name,
                "project_id": t.project_id,
                "model": t.model,
            });
        }
    }
    json!({
        "connected": false,
        "email": "",
        "name": "",
        "project_id": "",
        "model": DEFAULT_MODEL,
    })
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 48];
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    for (i, b) in bytes.iter_mut().enumerate() {
        let x = (t.wrapping_add(i as u128 * 0x9e3779b97f4a7c15)).wrapping_mul(pid as u128 | 1);
        *b = (x ^ (x >> 56) ^ (x >> 32)) as u8;
    }
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hash);
    (verifier, challenge)
}

fn new_http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .ip_family(ureq::config::IpFamily::Ipv4Only)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(180)))
        .build()
        .new_agent()
}

pub fn refresh_token(token: &mut AntigravityToken) -> Result<(), String> {
    if token.refresh_token.trim().is_empty() {
        return Err("No refresh token available. Please sign in again.".to_string());
    }
    let body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        url_encode(&client_id()),
        url_encode(&client_secret()),
        url_encode(&token.refresh_token)
    );
    let agent = new_http_agent();
    let mut resp = agent
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .map_err(|e| format!("Token refresh network error: {e}"))?;
    let status = resp.status().as_u16();
    let resp_text = resp.body_mut().read_to_string().unwrap_or_default();
    if status >= 400 {
        return Err(format!("Token refresh failed ({status}): {resp_text}"));
    }
    let v: Value = serde_json::from_str(&resp_text).map_err(|e| format!("Invalid refresh JSON: {e}"))?;
    let access = v["access_token"].as_str().ok_or("Missing access_token in refresh response")?;
    let expires_in = v["expires_in"].as_i64().unwrap_or(3600);
    token.access_token = access.to_string();
    token.expires_at = chrono::Local::now().timestamp() + expires_in;
    if let Some(r) = v["refresh_token"].as_str() {
        if !r.trim().is_empty() {
            token.refresh_token = r.to_string();
        }
    }
    save_token(token)?;
    Ok(())
}

pub fn ensure_valid_token() -> Result<AntigravityToken, String> {
    let mut token = load_token().ok_or("Belum login Google Antigravity. Silakan login terlebih dahulu.")?;
    let now = chrono::Local::now().timestamp();
    if token.expires_at <= now + 300 {
        refresh_token(&mut token)?;
    }
    Ok(token)
}

fn extract_project_id(val: &Value) -> Option<String> {
    if let Some(p) = val.get("cloudaicompanionProject") {
        if let Some(s) = p.as_str() {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
        if let Some(id) = p.get("id").and_then(|x| x.as_str()) {
            if !id.trim().is_empty() {
                return Some(id.to_string());
            }
        }
    }
    if let Some(p) = val.get("managedProjectId").and_then(|x| x.as_str()) {
        if !p.trim().is_empty() {
            return Some(p.to_string());
        }
    }
    None
}

fn fetch_user_info(access_token: &str) -> (String, String) {
    let agent = new_http_agent();
    let mut resp = match agent
        .get(USERINFO_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .call()
    {
        Ok(r) => r,
        Err(_) => return (String::new(), String::new()),
    };
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (String::new(), String::new());
    };
    let email = v["email"].as_str().unwrap_or("").to_string();
    let name = v["name"].as_str().unwrap_or("").to_string();
    (email, name)
}

fn discover_project(access_token: &str) -> Result<String, String> {
    let agent = new_http_agent();
    let body = json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });
    let body_s = body.to_string();
    let mut resp = agent
        .post(LOAD_CODE_ASSIST_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", "antigravity/1.15.8 windows/amd64")
        .header("X-Goog-Api-Client", "google-cloud-sdk vscode_cloudshelleditor/0.1")
        .header("Client-Metadata", "{\"ideType\":\"ANTIGRAVITY\",\"platform\":\"WINDOWS\",\"pluginType\":\"GEMINI\"}")
        .send(body_s.as_bytes())
        .map_err(|e| format!("loadCodeAssist network error: {e}"))?;

    let status = resp.status().as_u16();
    let resp_text = resp.body_mut().read_to_string().unwrap_or_default();
    if status >= 400 {
        return Err(format!("loadCodeAssist error ({status}): {resp_text}"));
    }
    let v: Value = serde_json::from_str(&resp_text).map_err(|e| format!("loadCodeAssist invalid JSON: {e}"))?;
    if let Some(proj) = extract_project_id(&v) {
        return Ok(proj);
    }
    // Fallback to default if loadCodeAssist succeeded without explicit project
    Ok("default".to_string())
}

pub fn start_oauth_flow() -> Result<AntigravityToken, String> {
    let listener = TcpListener::bind("127.0.0.1:51121")
        .map_err(|e| format!("Gagal membuka port login 51121: {e}. Pastikan tidak ada aplikasi lain yang memakai port ini."))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set nonblocking listener: {e}"))?;

    let (verifier, challenge) = generate_pkce();
    let state = format!("{:x}", SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis());

    let auth_url = format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        AUTH_URL,
        url_encode(&client_id()),
        url_encode(REDIRECT_URI),
        url_encode(&SCOPES.join(" ")),
        url_encode(&challenge),
        url_encode(&state)
    );

    // Open browser on Windows
    let _ = std::process::Command::new("rundll32")
        .args(&["url.dll,FileProtocolHandler", &auth_url])
        .spawn();

    let start_wait = Instant::now();
    let timeout = Duration::from_secs(180);
    let mut code: Option<String> = None;

    while start_wait.elapsed() < timeout {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);

                // Extract query from GET line: e.g. "GET /oauth-callback?code=... HTTP/1.1"
                let first_line = req_str.lines().next().unwrap_or("");
                if let Some(query_part) = first_line.split_whitespace().nth(1) {
                    if let Some((_, qs)) = query_part.split_once('?') {
                        for pair in qs.split('&') {
                            if let Some((k, v)) = pair.split_once('=') {
                                if k == "code" {
                                    // URL decode
                                    let decoded = urlencoding_decode(v);
                                    code = Some(decoded);
                                }
                            }
                        }
                    }
                }

                let html = if code.is_some() {
                    r#"<!DOCTYPE html>
<html lang="id">
<head>
  <meta charset="utf-8">
  <title>Nusa AI - Antigravity</title>
  <style>
    body { background: #0c0d12; color: #f1f5f9; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; display: flex; flex-direction: column; align-items: center; justify-content: center; height: 100vh; margin: 0; text-align: center; }
    .box { background: #181920; border: 1px solid #2d2f39; border-radius: 16px; padding: 40px; max-width: 420px; box-shadow: 0 20px 40px rgba(0,0,0,0.5); }
    h1 { font-size: 22px; color: #4ade80; margin: 0 0 12px; }
    p { font-size: 14px; color: #94a3b8; line-height: 1.6; margin: 0 0 24px; }
    .btn { background: #2563eb; color: #fff; padding: 10px 20px; border-radius: 8px; text-decoration: none; font-size: 13px; font-weight: 500; }
  </style>
</head>
<body>
  <div class="box">
    <h1>✓ Login Berhasil!</h1>
    <p>Akun Google Antigravity kamu telah terhubung ke <b>Nusa AI</b>.<br>Silakan tutup tab browser ini dan kembali ke aplikasi.</p>
    <a href="javascript:window.close()" class="btn">Tutup Tab Ini</a>
  </div>
  <script>setTimeout(function(){ window.close(); }, 3500);</script>
</body>
</html>"#
                } else {
                    r#"<!DOCTYPE html>
<html lang="id">
<head>
  <meta charset="utf-8">
  <title>Nusa AI - Login Gagal</title>
  <style>
    body { background: #0c0d12; color: #f1f5f9; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; display: flex; flex-direction: column; align-items: center; justify-content: center; height: 100vh; margin: 0; text-align: center; }
    .box { background: #181920; border: 1px solid #3b2828; border-radius: 16px; padding: 40px; max-width: 420px; }
    h1 { font-size: 22px; color: #f87171; margin: 0 0 12px; }
    p { font-size: 14px; color: #94a3b8; line-height: 1.6; margin: 0 0 24px; }
  </style>
</head>
<body>
  <div class="box">
    <h1>✕ Login Dibatalkan</h1>
    <p>Kode otorisasi Google tidak ditemukan atau izin dibatalkan.</p>
  </div>
</body>
</html>"#
                };

                let resp_header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                let _ = stream.write_all(resp_header.as_bytes());
                let _ = stream.flush();
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(_) => break,
        }
    }

    let code = code.ok_or("Proses login dibatalkan atau waktu habis (timeout).")?;

    // Exchange code for tokens
    let body = format!(
        "code={}&client_id={}&client_secret={}&redirect_uri={}&grant_type=authorization_code&code_verifier={}",
        url_encode(&code),
        url_encode(&client_id()),
        url_encode(&client_secret()),
        url_encode(REDIRECT_URI),
        url_encode(&verifier)
    );
    let agent = new_http_agent();
    let mut resp = agent
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body.as_bytes())
        .map_err(|e| format!("Gagal menghubungi Google Token Endpoint: {e}"))?;

    let status = resp.status().as_u16();
    let resp_text = resp.body_mut().read_to_string().unwrap_or_default();
    if status >= 400 {
        return Err(format!("Google OAuth token exchange gagal ({status}): {resp_text}"));
    }

    let v: Value = serde_json::from_str(&resp_text).map_err(|e| format!("Invalid JSON token: {e}"))?;
    let access = v["access_token"].as_str().ok_or("Missing access_token in Google response")?;
    let refresh = v["refresh_token"].as_str().unwrap_or("");
    let expires_in = v["expires_in"].as_i64().unwrap_or(3600);

    let (email, name) = fetch_user_info(access);
    let project_id = discover_project(access).unwrap_or_else(|_| "default".to_string());

    let token = AntigravityToken {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        expires_at: chrono::Local::now().timestamp() + expires_in,
        email,
        name,
        project_id,
        model: DEFAULT_MODEL.to_string(),
    };
    save_token(&token)?;
    Ok(token)
}

fn urlencoding_decode(s: &str) -> String {
    let mut out = Vec::new();
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next().unwrap_or(b'0');
            let h2 = chars.next().unwrap_or(b'0');
            let hex_str = [h1, h2];
            if let Ok(num) = u8::from_str_radix(std::str::from_utf8(&hex_str).unwrap_or("00"), 16) {
                out.push(num);
            }
        } else if b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convert standard messages into Antigravity/Gemini contents format
pub fn build_antigravity_payload(
    project_id: &str,
    model: &str,
    messages: &[Value],
    specs: &[Value],
) -> Value {
    let mut sys_text = String::new();
    let mut contents: Vec<Value> = Vec::new();

    // Map of tool_call_id -> function name
    let mut tool_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut legacy_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for m in messages {
        let role = m["role"].as_str().unwrap_or("");
        if role == "system" {
            if let Some(txt) = m["content"].as_str() {
                if !sys_text.is_empty() {
                    sys_text.push_str("\n\n");
                }
                sys_text.push_str(txt);
            }
            continue;
        }

        match role {
            "user" => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(txt) = m["content"].as_str() {
                    if !txt.is_empty() {
                        parts.push(json!({"text": txt}));
                    }
                } else if let Some(arr) = m["content"].as_array() {
                    for item in arr {
                        let t = item["type"].as_str().unwrap_or("");
                        if t == "text" {
                            if let Some(txt) = item["text"].as_str() {
                                parts.push(json!({"text": txt}));
                            }
                        } else if t == "image_url" {
                            if let Some(url) = item["image_url"]["url"].as_str() {
                                if let Some((mime, b64)) = parse_data_url(url) {
                                    parts.push(json!({
                                        "inlineData": {
                                            "mimeType": mime,
                                            "data": b64
                                        }
                                    }));
                                }
                            }
                        }
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "user",
                        "parts": parts
                    }));
                }
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(txt) = m["content"].as_str() {
                    if !txt.is_empty() {
                        parts.push(json!({"text": txt}));
                    }
                }
                if let Some(calls) = m["tool_calls"].as_array() {
                    for c in calls {
                        let id = c["id"].as_str().unwrap_or("");
                        let name = c["function"]["name"].as_str().unwrap_or("");
                        let args_str = c["function"]["arguments"].as_str().unwrap_or("{}");
                        let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                        if !id.is_empty() && !name.is_empty() {
                            tool_names.insert(id.to_string(), name.to_string());
                        }

                        let sig = c.get("thought_signature")
                            .or_else(|| c.get("thoughtSignature"))
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.trim().is_empty());

                        if let Some(s) = sig {
                            parts.push(json!({
                                "thoughtSignature": s,
                                "functionCall": {
                                    "name": name,
                                    "args": args
                                }
                            }));
                        } else {
                            // If missing thoughtSignature (e.g. legacy/cross-provider tool call),
                            // serialize as text so Gemini doesn't reject with 400!
                            if !id.is_empty() {
                                legacy_tool_ids.insert(id.to_string());
                            }
                            parts.push(json!({
                                "text": format!("[Called tool '{}' with arguments: {}]", name, args_str)
                            }));
                        }
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "model",
                        "parts": parts
                    }));
                }
            }
            "tool" => {
                let id = m["tool_call_id"].as_str().unwrap_or("");
                let name = tool_names.get(id).map(|s| s.as_str()).unwrap_or("tool");
                let content_str = m["content"].as_str().unwrap_or("");

                if legacy_tool_ids.contains(id) {
                    contents.push(json!({
                        "role": "user",
                        "parts": [{
                            "text": format!("[Tool result for '{}': {}]", name, content_str)
                        }]
                    }));
                } else {
                    let resp_obj = json!({ "output": content_str });
                    contents.push(json!({
                        "role": "user",
                        "parts": [{
                            "functionResponse": {
                                "name": name,
                                "response": resp_obj
                            }
                        }]
                    }));
                }
            }
            _ => {}
        }
    }

    // Merge consecutive messages with the same role to satisfy Gemini's turn requirements
    let mut normalized: Vec<Value> = Vec::new();
    for msg in contents {
        if let Some(last) = normalized.last_mut() {
            if last["role"] == msg["role"] {
                if let (Some(last_parts), Some(msg_parts)) = (
                    last["parts"].as_array_mut(),
                    msg["parts"].as_array(),
                ) {
                    last_parts.extend(msg_parts.iter().cloned());
                    continue;
                }
            }
        }
        normalized.push(msg);
    }

    // If conversation starts with model, prepend a friendly user turn
    if normalized.first().map(|m| m["role"] == "model").unwrap_or(false) {
        normalized.insert(0, json!({
            "role": "user",
            "parts": [{"text": "Hello"}]
        }));
    }

    // Convert specs to functionDeclarations
    let mut funcs = Vec::new();
    for s in specs {
        if let Some(f) = s.get("function") {
            funcs.push(json!({
                "name": f["name"],
                "description": f["description"],
                "parameters": f["parameters"],
            }));
        }
    }

    let mut request_obj = json!({
        "contents": normalized,
        "generationConfig": {
            "temperature": 0.7,
            "maxOutputTokens": 8192
        }
    });

    if !sys_text.is_empty() {
        request_obj["systemInstruction"] = json!({
            "parts": [{ "text": sys_text }]
        });
    }

    if !funcs.is_empty() {
        request_obj["tools"] = json!([{
            "functionDeclarations": funcs
        }]);
    }

    json!({
        "project": project_id,
        "model": model,
        "request": request_obj
    })
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    if !url.starts_with("data:") {
        return None;
    }
    let rest = &url["data:".len()..];
    let (meta, b64) = rest.split_once(";base64,")?;
    let mime = if meta.is_empty() { "image/png" } else { meta };
    Some((mime.to_string(), b64.to_string()))
}

/// Stream completions from Antigravity
pub fn stream(
    emit: &Emit,
    messages: &[Value],
    specs: &[Value],
    cancel: &Arc<AtomicBool>,
) -> Result<(String, Vec<Value>, String, u64), String> {
    let t0 = Instant::now();
    let token = ensure_valid_token()?;

    let model_name = if token.model.trim().is_empty() {
        DEFAULT_MODEL
    } else {
        token.model.as_str()
    };

    let payload = build_antigravity_payload(&token.project_id, model_name, messages, specs);
    let body_s = payload.to_string();

    let agent = new_http_agent();
    let mut resp = agent
        .post(STREAM_GENERATE_URL)
        .header("Accept", "text/event-stream")
        .header("Authorization", &format!("Bearer {}", token.access_token))
        .header("Content-Type", "application/json")
        .header("User-Agent", "antigravity/1.15.8 windows/amd64")
        .header("X-Goog-Api-Client", "google-cloud-sdk vscode_cloudshelleditor/0.1")
        .header("Client-Metadata", "{\"ideType\":\"ANTIGRAVITY\",\"platform\":\"WINDOWS\",\"pluginType\":\"GEMINI\"}")
        .send(body_s.as_bytes())
        .map_err(|e| format!("Antigravity connection failed: {e}"))?;

    let mut status = resp.status().as_u16();
    if status >= 400 {
        // Automatic fallback to FALLBACK_MODEL (gemini-3-flash)
        if model_name != FALLBACK_MODEL {
            let fb_payload = build_antigravity_payload(&token.project_id, FALLBACK_MODEL, messages, specs);
            let fb_body_s = fb_payload.to_string();
            if let Ok(r) = agent
                .post(STREAM_GENERATE_URL)
                .header("Accept", "text/event-stream")
                .header("Authorization", &format!("Bearer {}", token.access_token))
                .header("Content-Type", "application/json")
                .header("User-Agent", "antigravity/1.15.8 windows/amd64")
                .header("X-Goog-Api-Client", "google-cloud-sdk vscode_cloudshelleditor/0.1")
                .header("Client-Metadata", "{\"ideType\":\"ANTIGRAVITY\",\"platform\":\"WINDOWS\",\"pluginType\":\"GEMINI\"}")
                .send(fb_body_s.as_bytes())
            {
                if r.status().as_u16() < 400 {
                    resp = r;
                    status = resp.status().as_u16();
                }
            }
        }
    }

    drop(body_s);
    trim_working_set();

    if status >= 400 {
        let mut resp = resp;
        let t = resp.body_mut().read_to_string().unwrap_or_default();
        let m = serde_json::from_str::<Value>(&t)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(|x| x.to_string()))
            .unwrap_or(t);
        return Err(format!("Antigravity API error {status}: {}", tools::truncate(&m, 400)));
    }

    let reader = BufReader::new(resp.into_body().into_reader());
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut thinking_off = false;
    let mut think_ms: Option<u64> = None;
    let mut latest_thought_sig: Option<String> = None;

    for line in reader.lines() {
        if cancel.load(Ordering::SeqCst) {
            return Ok((content, vec![], reasoning, t0.elapsed().as_millis() as u64));
        }
        let line = line.map_err(|e| format!("Antigravity stream read error: {e}"))?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }

        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(err) = v.get("error") {
            return Err(format!("Antigravity error: {}", err["message"].as_str().unwrap_or(&err.to_string())));
        }

        // Candidates response check
        // Antigravity returns either `{"response": {"candidates": [...]}}` or `{"candidates": [...]}`
        let cand_container = v.get("response").unwrap_or(&v);
        if let Some(candidates) = cand_container.get("candidates").and_then(|c| c.as_array()) {
            for cand in candidates {
                if let Some(parts) = cand.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array()) {
                    for p in parts {
                        let is_thought = p.get("thought").and_then(|t| t.as_bool()).unwrap_or(false);
                        if is_thought {
                            if let Some(r) = p.get("text").and_then(|t| t.as_str()) {
                                if !r.is_empty() {
                                    reasoning.push_str(r);
                                    emit(json!({"t":"reason","text":r}));
                                }
                            }
                        } else if let Some(txt) = p.get("text").and_then(|t| t.as_str()) {
                            if !txt.is_empty() {
                                if !thinking_off {
                                    thinking_off = true;
                                    let ms = t0.elapsed().as_millis() as u64;
                                    think_ms = Some(ms);
                                    emit(json!({"t":"thinking","on":false,"ms":ms}));
                                }
                                content.push_str(txt);
                                emit(json!({"t":"delta","text":txt}));
                            }
                        }

                        if let Some(sig) = p.get("thoughtSignature")
                            .or_else(|| p.get("thought_signature"))
                            .and_then(|s| s.as_str())
                        {
                            if !sig.is_empty() {
                                latest_thought_sig = Some(sig.to_string());
                            }
                        }

                        if let Some(fc) = p.get("functionCall") {
                            let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                            let args = fc.get("args").cloned().unwrap_or(json!({}));
                            let args_s = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                            let call_id = fc.get("id").and_then(|s| s.as_str()).unwrap_or("");
                            let idx = calls.len();
                            let id = if !call_id.is_empty() {
                                call_id.to_string()
                            } else {
                                format!("call_{}_{}", chrono::Local::now().timestamp_millis(), idx)
                            };

                            let sig = p.get("thoughtSignature")
                                .or_else(|| p.get("thought_signature"))
                                .or_else(|| fc.get("thoughtSignature"))
                                .or_else(|| fc.get("thought_signature"))
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string())
                                .or_else(|| latest_thought_sig.clone());

                            let mut call_val = json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": args_s
                                }
                            });
                            if let Some(s) = sig {
                                call_val["thought_signature"] = json!(s);
                            }
                            calls.push(call_val);
                        }
                    }
                }
            }
        }
    }

    let final_ms = think_ms.unwrap_or_else(|| t0.elapsed().as_millis() as u64);
    if !thinking_off {
        emit(json!({"t":"thinking","on":false,"ms":final_ms}));
    }

    Ok((content, calls, reasoning, final_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payload_with_thought_signature() {
        let msgs = vec![
            json!({"role": "user", "content": "halo"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_123",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"command\":\"dir\"}"},
                        "thought_signature": "valid_sig_base64_abc"
                    }
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_123",
                "content": "output from dir"
            })
        ];
        let payload = build_antigravity_payload("test-proj", DEFAULT_MODEL, &msgs, &[]);
        let contents = payload["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        
        let model_part = &contents[1]["parts"][0];
        assert_eq!(model_part["thoughtSignature"], "valid_sig_base64_abc");
        assert_eq!(model_part["functionCall"]["name"], "shell");

        let tool_part = &contents[2]["parts"][0];
        assert_eq!(tool_part["functionResponse"]["name"], "shell");
        assert_eq!(tool_part["functionResponse"]["response"]["output"], "output from dir");
    }

    #[test]
    fn test_payload_legacy_without_thought_signature_falls_back_to_text() {
        let msgs = vec![
            json!({"role": "user", "content": "halo"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_legacy_456",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{\"command\":\"echo test\"}"}
                    }
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_legacy_456",
                "content": "test"
            })
        ];
        let payload = build_antigravity_payload("test-proj", DEFAULT_MODEL, &msgs, &[]);
        let contents = payload["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        
        let model_part = &contents[1]["parts"][0];
        // Must NOT have functionCall because that would cause 400!
        assert!(model_part.get("functionCall").is_none());
        assert!(model_part["text"].as_str().unwrap().contains("[Called tool 'shell'"));

        let tool_part = &contents[2]["parts"][0];
        assert!(tool_part.get("functionResponse").is_none());
        assert!(tool_part["text"].as_str().unwrap().contains("[Tool result for 'shell'"));
    }
}
