<div align="center">

# opencode-rs

**以 Rust 原生重寫的 [opencode](https://github.com/anomalyco/opencode)：終端介面、桌面應用、CLI 與 HTTP 伺服器。**

[English](README.md) · 繁體中文

</div>

> [!NOTE]
> 非官方社群專案，與 opencode 團隊無隸屬或背書關係。

## 簡介

opencode-rs 以原生執行檔重新實作 TypeScript 版 opencode（對應標籤 **v1.19.5**），不需要 Node 或 Bun 執行環境。
它可以和已安裝的官方 opencode（包含 2.2.x）並存，共用登入與設定，但對話分開存放，不會動到原版的資料。

| 介面 | 指令 | 技術 |
|---|---|---|
| 終端介面 (TUI) | `opencode` | ratatui |
| 桌面應用 | `opencode-desktop` | GPUI |
| CLI | `opencode run`、`models`、`providers`、`mcp`、`export`、`import`、`stats`… | clap |
| 伺服器／網頁 | `opencode serve`、`opencode web` | axum，提供原版 HTTP API 並內嵌原版網頁介面，手機也能用 |

## 特色

- **與官方版並存**：共用 `auth.json`、`opencode.json[c]`、技能、代理與 MCP 伺服器；對話存在自己的
  `opencode-rs.db`，從不開啟原版的 `opencode.db`。
- **相容 opencode 2.x**：
  - 讀取 2.x 的登入（唯讀）
  - 匯入 2.x 的對話
  - 接受 v2 的 `permissions` 規則與 `cli.json` 的 TUI 設定
- **相容你正在用的其他 AI 代理**：
  - `opencode import` 可從 Claude Code、Codex、Cursor、Windsurf、Grok Build、DeepSeek Harness、`~/.mcp.json`
    與 opencode 匯入 MCP 伺服器、代理與技能
  - 也能匯入 Claude Code、Codex、Grok Build、DeepSeek Harness 與 opencode 的對話紀錄
  - 讀取 `AGENTS.md`、`CLAUDE.md` 指示，以及家目錄和專案裡 `.claude/skills`、`.agents/skills` 的技能
  - 其他代理的指令名稱也能用，例如 `/rewind` → `/undo`、`/resume` → 對話列表
  - `opencode acp` 支援 Agent Client Protocol，可接上支援 ACP 的編輯器
- **用手機派任務**：`opencode web` 在區域網路提供網頁介面，用手機瀏覽器就能下任務、看進度，實際工作在電腦上執行
  （[作法](#用手機操作)）
- **移植自 v2 的功能**：
  - 任務執行中可插話改變方向
  - 改良的上下文壓縮
  - 背景 shell 工作
  - 自動修正格式有誤的工具參數
  - `/btw`、`/goal`、`/stats`、`/reload`
- **Claude 風格外觀**：
  - 暖色 *clay* 主題
  - `● Tool(args)` 工具列，結果以 `⎿` 接在下方
  - 桌面版動畫

  一個設定就能切回原版排版。
- **以 Windows 為優先**：
  - shell 依序選用 Git Bash → PowerShell 7 → 可攜式 bash → Windows PowerShell 5.1 → cmd
  - 正確解碼 Big5／GBK 的主控台輸出
  - Windows 傳統主控台（conhost）在中日韓字碼頁下 TUI 也能對齊
- **細節**：
  - TUI 與桌面版都有提示音效
  - 桌面版介面語言跟隨系統地區

## 安裝

### 預先編譯版（Windows x64）

從 [Releases](../../releases) 下載：

| 檔案 | 內容 |
|---|---|
| `opencode-desktop-win-x64.exe` | 安裝程式：桌面應用 + `opencode.exe` |
| `opencode-windows-x64.zip` | 只有 `opencode.exe`（TUI、CLI、伺服器） |
| `SHA256SUMS` | zip 檔的校驗碼 |

**安裝程式**：
- 只安裝給目前使用者，不需要系統管理員權限。
- 安裝到 `%LOCALAPPDATA%\Programs\opencode-desktop-rs`，並在開始功能表建立 *OpenCode* 資料夾。
- 註冊 `opencode://` 連結；若原本已有其他程式註冊，解除安裝時會還原。
- 從「設定 → 應用程式」或開始功能表解除安裝，對話與設定會保留。

執行檔沒有程式碼簽章；若 SmartScreen 跳出警告，請選「其他資訊 → 仍要執行」。驗證下載檔：

```powershell
Get-FileHash .\opencode-windows-x64.zip -Algorithm SHA256   # 與 SHA256SUMS 比對
```

**在終端機使用 TUI**：安裝程式不會修改 `PATH`。執行檔和官方 CLI 同名（`opencode.exe`），建議另取一個指令名稱，
不要放進 `PATH` 而蓋過原版：

```powershell
# 加到 $PROFILE
function ocr { & "$env:LOCALAPPDATA\Programs\opencode-desktop-rs\opencode.exe" @args }
```

### 從原始碼編譯

需求：
- [rustup](https://rustup.rs)，會依 `rust-toolchain.toml` 自動安裝指定的工具鏈（1.99.1）
- Git
- Windows：Visual Studio Build Tools，勾選「使用 C++ 的桌面開發」

編譯時會把原版儲存庫的提示詞、主題等文字一起編進去，所以要把原版切到 **v1.19.5**，資料夾命名為 `opencode-ts`，
放在本儲存庫旁邊：

```
workspace/
├─ opencode-ts/    github.com/anomalyco/opencode @ v1.19.5
└─ opencode-rs/    本儲存庫
```

```powershell
mkdir workspace; cd workspace
git clone --depth 1 --branch v1.19.5 https://github.com/anomalyco/opencode.git opencode-ts
git clone https://github.com/EustaceSop/opencode-rs.git
cd opencode-rs
cargo build --release --locked -p opencode-cli -p opencode-desktop
```

編譯結果在 `target\release\`：`opencode.exe` 與 `opencode-desktop.exe`。

| 選用項目 | 作法 |
|---|---|
| `opencode web` 的網頁介面 | 先用 Bun 編譯 `opencode-ts/packages/app`（`bun install` 後 `bun run build`）再 `cargo build`；或把 `OPENCODE_WEB_UI_DIR` 指向已編譯好的 `dist`。沒有的話伺服器不帶介面；預先編譯版已內含 |
| 發佈用 zip | `scripts\package.ps1`，連同 `SHA256SUMS` 輸出到 `dist\` |
| 安裝程式 | `scripts\package-desktop-installer.ps1`；找不到 NSIS 時會下載固定版本到 `target\tools` 並驗證 SHA-256 |
| Linux／macOS | `scripts/package.sh`（未測試） |
| 測試 | `cargo test --workspace` |

## 快速開始

```powershell
opencode                          # 在目前資料夾開啟 TUI
opencode run "explain this repo"  # 單次提問
opencode serve --port 4096        # HTTP API
opencode providers login          # 登入模型供應商（別名 auth）
opencode import                   # 從其他 AI 代理匯入 MCP 伺服器、代理、技能與對話
```

桌面版可以從開始功能表（*OpenCode*）開啟，或直接執行 `opencode-desktop.exe`。已經在官方 opencode 登入過的話，
登入是共用的，不需要再設定。

### 用手機操作

在電腦上設定密碼，並讓網頁伺服器在區域網路上監聽：

```powershell
$env:OPENCODE_SERVER_PASSWORD = "自訂密碼"
opencode web --hostname 0.0.0.0 --port 4096
```

1. 手機連同一個 Wi-Fi，開啟 `http://<電腦 IP>:4096`。電腦的 IPv4 位址可用 `ipconfig` 查。
2. 用帳號 `opencode` 和剛設定的密碼登入。
3. Windows 防火牆若跳出詢問，允許「私人網路」存取。

沒設密碼時伺服器會警告「未受保護」，請一定要設定。要從外面連回家，請用 VPN，不要把連接埠直接開到網際網路上。

### TUI 設定

TUI 設定在 `~/.local/state/opencode/kv.json`，原版會忽略這些鍵：

| 鍵 | 作用 |
|---|---|
| `"rs.style": "opencode"` | 改用原版排版（不用 Claude 風格） |
| `"rs.theme": "<name>"` | TUI 主題（預設 `clay`） |
| `"rs.sounds": false` | TUI 靜音（也可用 `/sounds`） |

## 與原版並存

| 與原版共用 | opencode-rs 獨有 |
|---|---|
| `auth.json`、`mcp-auth.json` | `opencode-rs.db`（對話） |
| `opencode.json[c]`、`.opencode/`、技能、代理、指令 | `snapshot-rs/`（檔案快照） |
| 外掛、`models.json` 快取、日誌 | `desktop-rs.json`（桌面版狀態）、`~/.config/opencode-rs/`（匯入內容） |

`opencode uninstall` 只會刪除 `opencode-rs.db`、`snapshot-rs/` 和 `desktop-rs.json`。詳見
[docs/REFERENCE.md](docs/REFERENCE.md)（英文）。

## 狀態與待辦

目前在 Windows 10 x64 上測試過；Linux 與 macOS 的編譯流程已存在但尚未測試。執行檔沒有簽章，也沒有自動更新。

- [ ] 桌面版專屬圖示（目前沿用原版）
- [ ] 載入 v2 `plugin add` 安裝的外掛
- [ ] 使用 2.x 以網址為鍵的 well-known 登入
- [ ] 查看與編輯排隊中的訊息；遠端 TUI 也能選擇排隊或插話
- [ ] `/goal` 重啟後保留，並在畫面上顯示目前目標
- [ ] 連線遠端（`attach`）時支援 `/btw` 與 `/stats`
- [ ] `/usage`、`/cd`
- [ ] CLI：`upgrade`、`github install|run`、`debug v2`、`debug agent --tool`
- [ ] 網頁版專案選擇器：接受直接輸入的完整路徑
- [ ] 首次啟動的匯入對話框
- [ ] `opencode console login` 後驗證 OpenCode Go
- [ ] 程式碼簽章；驗證 Linux 與 macOS 版本

完整清單：[docs/TODO.md](docs/TODO.md)（英文）。

## 專案結構

```
crates/
  opencode-core       設定、路徑、登入、代理、權限、技能
  opencode-storage    SQLite 儲存
  opencode-llm        模型供應商、串流用戶端、模型目錄
  opencode-tools      bash、read、write、edit、grep、webfetch…
  opencode-session    代理迴圈、上下文壓縮、子代理
  opencode-server     HTTP API
  opencode-tui        終端介面
  opencode-desktop    桌面應用
  opencode-cli        `opencode` 執行檔
  …                   acp、client、import、lsp、mcp、plugin
docs/spec/            依 v1.19.5 撰寫的行為規格
docs/REFERENCE.md     crate 說明、相容性約定、指令狀態
scripts/              打包腳本
```

## 致謝

- **[opencode](https://github.com/anomalyco/opencode)**：本專案依循的原版，感謝原作者們的設計。
- **[Claude Code](https://claude.com/claude-code)**（Anthropic）：本專案以 Claude Code 開發；由 Claude 撰寫的 commit
  附有 `Co-Authored-By` 標記。

## 授權

[MIT](LICENSE)。本專案建立在 [opencode](https://github.com/anomalyco/opencode)（MIT，© 2025 opencode）之上，
包含移植的行為與文字、桌面版音效以及目前的圖示；提示詞與主題在編譯時從原版儲存庫編入。
