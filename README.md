# Nusa AI ⚡

> **The ultra-lightweight, native AI assistant plugin engineered specifically for [Nusa](https://rama2555.github.io/nusa/).**  
> Docks seamlessly beneath the Nusa notch, powered by an advanced autonomous agent with a full suite of system tools — running at an astonishing **< 2 MB RAM footprint**.

---

![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011%20(64--bit)-0078D6?style=flat-square&logo=windows)
![Memory Footprint](https://img.shields.io/badge/RAM%20Footprint-%3C%202%20MB-success?style=flat-square)
![Language](https://img.shields.io/badge/Engine-Rust%20🦀-orange?style=flat-square)
![Architecture](https://img.shields.io/badge/Safety-Human--in--the--Loop-blue?style=flat-square)
![License](https://img.shields.io/badge/License-MIT-green?style=flat-square)

---

## ✨ Overview

**Nusa AI** is a companion plugin for **Nusa** (the Windows desktop notch / Dynamic Island). Built in high-performance Rust, it sits directly underneath the notch window, tracking its geometry, DPI scaling, and animations in real-time.

Unlike heavy, resource-draining desktop AI apps, Nusa AI was designed with extreme performance and minimalism in mind. The core engine maintains a private working set of **under 2 MB of RAM**, making it completely unnoticeable in the background while remaining instantly available whenever you need it.

---

## 🚀 Key Highlights

- **⚡ Unbelievably Lightweight (< 2 MB RAM)**  
  Engineered from the ground up in Rust with custom memory virtualization, stream-based JSON parsing, and zero background bloat. Stays featherlight even during long conversations with heavy media.

- **🏝️ Seamless Nusa Notch Integration**  
  Hooks directly into the Nusa notch window (`NotchWindow`). It snaps beneath the notch header, animates smoothly with it, and shares live state with Nusa's built-in widgets.

- **📝 Native Todo & Notch Sync**  
  Nusa AI can directly read, add, update, complete, and organize reminders and tasks inside Nusa's built-in notch Todo widget in real-time.

- **👁️ Multimodal Vision & Native Windows OCR**  
  Attach screenshots, diagrams, and photos with instant previews. Equipped with built-in Windows OCR and document parsers capable of reading `.pdf`, `.docx`, `.xlsx`, `.pptx`, and image files offline.

- **🛡️ Human-in-the-Loop Safety**  
  File edits, deletions, shell commands, and Python scripts require explicit user approval. Interactive visual diff cards display exact changes before anything touches your system.

- **🌐 Bring Your Own Model (BYOM)**  
  Works with any OpenAI-compatible endpoint — OpenAI, OpenRouter, Groq, Cohere, Anthropic proxies, or local offline LLMs running via **Ollama** or **LM Studio**.

- **📦 Standalone 1-Click Installer**  
  Ships with `NusaAI-Setup.exe` that automatically configures Start Menu shortcuts, Desktop icons, Windows auto-start, and clean uninstallation through Windows Settings.

---

## 🛠️ Built-in Agent Tools

Nusa AI gives the LLM direct, controlled superpowers over your local workspace:

### 📁 File & Workspace Management
- `read_file` — Reads source code, markdown, documents (`.pdf`, `.docx`, `.xlsx`, `.pptx`, `.odt`, `.rtf`), or images via OCR with line-offset pagination.
- `write_file` — Safely writes or creates new files with approval diffs.
- `edit_file` — Performs surgical string replacements with visual before/after diff previews.
- `list_dir` — Explores directory trees with configurable depth.
- `glob` — Fast file searching using glob patterns (e.g. `**/*.rs`).
- `grep` — Regex-based text search across projects.
- `delete_path` — Safely moves files/folders to the Windows Recycle Bin (non-destructive).
- `move_path` — Moves and renames files or directories.

### 🔍 Vision & OCR
- `ocr_image` — Extracts text locally from any `.png`, `.jpg`, `.webp`, or `.bmp` using Windows built-in OCR.

### 💻 System Execution & Shell
- `shell` — Executes PowerShell commands with configurable working directories and timeouts.
- `run_python` — Executes standalone Python 3 scripts with stdout/stderr capture.
- `bg_start` / `bg_output` / `bg_stop` — Manages long-running background processes (dev servers, builds, test runners).

### 🌐 Web Intelligence
- `web_search` — Real-time web searching (DuckDuckGo) with curated titles, links, and snippets.
- `web_fetch` — Fetches web pages and converts them to clean, readable markdown for the LLM.

### 📋 Nusa Notch Synchronization
- `nusa_todo` — Direct integration with the Nusa Notch Todo widget (add, complete, delete, list tasks, set 24h start/end times).
- `todo_write` — Renders an interactive live checklist directly inside the chat interface.

### 🛡️ Safety & Interaction
- `ask_user` — Asks clarifying questions with optional multi-choice buttons.
- `request_permission` — Solicits explicit user consent before performing sensitive actions.

---

## 📥 Installation

### Option 1: Standalone Installer (Recommended)
1. Download **`NusaAI-Setup.exe`** from the [Releases](https://github.com/jipanjiji/NusaAI/releases) page.
2. Run the installer. It will install Nusa AI to:
   ```
   %LOCALAPPDATA%\Programs\NusaAI\
   ```
3. Nusa AI will launch automatically and can be toggled directly from your system.

### Option 2: Build from Source
Ensure you have the Rust toolchain installed:
```powershell
# Clone the repository
git clone https://github.com/jipanjiji/NusaAI.git
cd NusaAI/nusa-ai

# Build release binary
cargo build --release --bin nusa-ai

# Or build the standalone setup package
cargo build --release --bin NusaAI-Setup
```

---

## ⚙️ Configuration

Nusa AI stores your configuration safely in your user profile:
```
%APPDATA%\NusaAI\config.json
```

### Easy Setup
1. Launch Nusa AI.
2. If unconfigured, a **Setup Config** card will greet you. Click **Setup Config** (or click the **⚙️ Gear** button in the header) to open `config.json` directly in Notepad.
3. Fill in your provider details and save:

```json
{
  "api_key": "your-api-key-here",
  "base_url": "https://api.openai.com/v1",
  "model": "gpt-4o"
}
```

### Popular Providers

#### OpenRouter
```json
{
  "api_key": "sk-or-v1-...",
  "base_url": "https://openrouter.ai/api/v1",
  "model": "anthropic/claude-3.5-sonnet"
}
```

#### Groq (Ultra-Fast)
```json
{
  "api_key": "gsk_...",
  "base_url": "https://api.groq.com/openai/v1",
  "model": "llama-3.3-70b-versatile"
}
```

#### Local LLM (Ollama)
```json
{
  "api_key": "ollama",
  "base_url": "http://localhost:11434/v1",
  "model": "llama3.1"
}
```

---

## 🔒 Privacy & Safety

- **Zero Hardcoded Secrets**: Your API keys and endpoints stay on your device in `%APPDATA%\NusaAI\config.json`.
- **Zero Telemetry**: No tracking, no data sent to third-party servers. Requests only travel to the AI endpoint you configure.
- **Approval Gate**: Any potentially harmful operation (shell execution, code running, file writing/deletion) prompts a confirmation modal with clear visual inspection.

---

## 🤝 Companion Projects

- **[Nusa](https://rama2555.github.io/nusa/)** — The core Dynamic Island and notch desktop interface for Windows.

---

## 📄 License

Distributed under the **MIT License**. See `LICENSE` for details.
