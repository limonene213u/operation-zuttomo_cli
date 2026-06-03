use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Mi25,
    OpenRouter,
    Codex,
}

impl Backend {
    fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "mi25" => Some(Self::Mi25),
            "openrouter" | "or" => Some(Self::OpenRouter),
            "codex" | "codex-cli" => Some(Self::Codex),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Mi25 => "mi25",
            Self::OpenRouter => "openrouter",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Chat,
    Question,
    ToTaiwanMandarin,
    ToJapanese,
    Pobo,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Question => "question",
            Self::ToTaiwanMandarin => "tw",
            Self::ToJapanese => "jp",
            Self::Pobo => "pobo",
        }
    }

    fn max_tokens(self) -> u32 {
        match self {
            Self::Chat => 900,
            Self::Question => 1600,
            Self::ToTaiwanMandarin => 500,
            Self::ToJapanese => 700,
            Self::Pobo => 400,
        }
    }

    fn temperature(self) -> f32 {
        match self {
            Self::Question => 0.25,
            Self::Pobo => 0.2,
            _ => 0.3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Message {
    role: String,
    mode: String,
    content: String,
}

impl Message {
    fn new(role: &str, mode: Mode, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            mode: mode.name().to_string(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct Config {
    default_backend: Backend,
    mi25_enabled: bool,
    mi25_base_url: String,
    mi25_model: String,
    openrouter_base_url: String,
    openrouter_api_key: Option<String>,
    openrouter_model: String,
    codex_bin: String,
    codex_workdir: Option<String>,
    history_path: String,
    pair_host: String,
    pair_port: u16,
    http_timeout_secs: u64,
    oss_url: String,
}

impl Config {
    fn from_env() -> Self {
        let mi25_enabled = env_bool("ZUTTOMO_MI25_ENABLED", false);
        let requested_default = env::var("ZUTTOMO_DEFAULT_BACKEND")
            .ok()
            .and_then(|backend| Backend::from_name(&backend))
            .unwrap_or(Backend::Codex);
        let default_backend = if requested_default == Backend::Mi25 && !mi25_enabled {
            Backend::Codex
        } else {
            requested_default
        };

        Self {
            default_backend,
            mi25_enabled,
            mi25_base_url: env::var("ZUTTOMO_MI25_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8000/v1".to_string()),
            mi25_model: env::var("ZUTTOMO_MI25_MODEL")
                .unwrap_or_else(|_| "local-model".to_string()),
            openrouter_base_url: env::var("OPENROUTER_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string()),
            openrouter_api_key: env_nonempty("OPENROUTER_API_KEY"),
            openrouter_model: env::var("OPENROUTER_MODEL")
                .unwrap_or_else(|_| "openai/gpt-4o-mini".to_string()),
            codex_bin: env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string()),
            codex_workdir: env_nonempty("CODEX_WORKDIR"),
            history_path: env::var("ZUTTOMO_HISTORY")
                .unwrap_or_else(|_| "zuttomo-history.jsonl".to_string()),
            pair_host: env::var("ZUTTOMO_PAIR_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            pair_port: env::var("ZUTTOMO_PAIR_PORT")
                .ok()
                .and_then(|port| port.parse().ok())
                .unwrap_or(8787),
            http_timeout_secs: env::var("ZUTTOMO_HTTP_TIMEOUT_SECS")
                .ok()
                .and_then(|secs| secs.parse().ok())
                .unwrap_or(120),
            oss_url: env::var("ZUTTOMO_OSS_URL")
                .unwrap_or_else(|_| "https://limonene213u.github.io/ai-2026-computex/".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
struct PairServer {
    url: String,
}

fn env_bool(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(value) => match value.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" => true,
            "0" | "false" | "no" | "off" | "disabled" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn system_prompt(mode: Mode) -> &'static str {
    match mode {
        Mode::Chat => {
            r#"
你是「りもこズッ友キラキラポボモフォ2.0」。
正式名稱是「檸檬烯姐總合熱烈爆裂友情智慧板」。
你是日本語與台灣華語之間的技術會話翻譯器。

使用者「りもこ」可以輸入日本語、ローマ字、英語、或混合文。
りもこ英語可以寫，但現場口說久違了；中國語與台灣華語輸入不可假設會用。

任務：
- 幫助日本語與台灣華語的技術會話。
- 讓文字適合在 Computex Taipei 的技術展示攤位直接給對方看。
- 以繁體中文、台灣華語為優先。
- 不要使用簡體字。
- 短、自然、友善，但不要太像業務話術。
- 技術者之間要自然、有禮貌、容易繼續聊天。
- 如果輸入是台灣華語，請翻成自然日本語。
- 如果輸入是日本語、ローマ字、英語，請翻成自然台灣華語。

保持原樣：
程式碼、指令、URL、API名、型號、產品名不要擅自翻譯。
AMD, ROCm, MI25, GPU, Linux, Ubuntu, Python, Rust, C, Go, GitHub, API, CLI, OSS, LLM 保持原樣。

合言葉：
昨日的技術交流、今日的ズッ友。
"#
        }
        Mode::Question => {
            r#"
你是「檸檬烯姐總合熱烈爆裂友情智慧板」的技術質問模式。
あなたは、りもこが技術質問をするための相棒です。

目的：
- C, Rust, Linux, Ubuntu, ROCm, GPU, LLM, CLI, OSS の質問に答える。
- 回答は実装寄りにする。
- なるべく短く、すぐ試せる形にする。
- 必要ならコマンド、コード、設計方針を出す。
- チャット履歴を踏まえて回答する。
- わからないことは推測で断言しない。

口調：
- 日本語中心。
- 必要なら台灣華語も少し添えてよい。
- 楽しく、でも技術的には正確に。
"#
        }
        Mode::ToTaiwanMandarin => {
            r#"
你是台灣華語翻譯模式。
使用者可能輸入日本語、ローマ字、英語、或混合文。

任務：
- 輸入を自然な繁體中文・台灣華語にする。
- 技術展示會でそのまま相手に見せられる短文にする。
- 簡體字は使わない。
- 技術用語、型番、API名、GitHub、AMD、MI25、ROCm、Linux、Ubuntu は保持する。
- 営業っぽくしすぎない。
- りもこが中国語入力できない前提で、入力意図を汲み取る。
- ローマ字の日本語も日本語として解釈する。

出力方針：
- 基本は1から3文。
- 必要な場合だけ「そのまま見せる文」「少し丁寧な文」を分ける。
- ブース担当者・エンジニアに失礼のない自然な文にする。

台灣華語技術語彙：
程式碼、函式、變數、資料、檔案、資料夾、伺服器、網路、資料庫、顯示卡、驅動程式、作業系統、本地模型、開源、儲存、記憶體、推論、訓練、量化。
"#
        }
        Mode::ToJapanese => {
            r#"
你是台灣華語到日本語的翻譯模式。
輸入は台灣華語・繁體中文を想定する。

任務：
- 入力を自然な日本語にする。
- 技術用語は日本のエンジニアに通じる表現にする。
- コード、コマンド、URL、API名、型番、製品名は勝手に翻訳しない。
- 相手の意図、質問、温度感がわかるように訳す。
- 長い説明にせず、会話で使いやすくする。

必要なら、最後に短い補足を1行だけ付けてよい。
"#
        }
        Mode::Pobo => {
            r#"
你是ㄅㄆㄇㄈ輔助模式。
入力された日本語・漢字・英語・台灣華語を、自然な台灣華語表現に直し、必要なら注音符號を少し付ける。

方針：
- 繁體中文・台灣華語を使う。
- 簡體字は使わない。
- 注音符號は飾り程度でよい。
- 読みやすさを優先し、長くしすぎない。
- 技術語彙、型番、API名、GitHub、AMD、MI25、ROCm、Linux は保持する。

出力例：
技術友誼裝置
ㄐㄧˋ ㄕㄨˋ ㄧㄡˇ ㄧˊ ㄓㄨㄤ ㄓˋ
"#
        }
    }
}

fn trim_history(history: &[Message], max_messages: usize) -> &[Message] {
    let start = history.len().saturating_sub(max_messages);
    &history[start..]
}

fn call_openai_compatible(
    client: &Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    history: &[Message],
    mode: Mode,
    input: &str,
) -> Result<String> {
    let mut messages = Vec::new();
    messages.push(json!({
        "role": "system",
        "content": system_prompt(mode),
    }));

    for msg in trim_history(history, 12) {
        messages.push(json!({
            "role": msg.role,
            "content": format!("[mode:{}]\n{}", msg.mode, msg.content),
        }));
    }

    messages.push(json!({
        "role": "user",
        "content": input,
    }));

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut req = client.post(url).json(&json!({
        "model": model,
        "messages": messages,
        "temperature": mode.temperature(),
        "max_tokens": mode.max_tokens(),
    }));

    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }

    let value: Value = req
        .send()
        .context("HTTP request failed")?
        .error_for_status()
        .context("LLM API returned error status")?
        .json()
        .context("failed to parse JSON response")?;

    let content = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .context("response did not contain choices[0].message.content")?;

    Ok(content.trim().to_string())
}

fn call_codex(cfg: &Config, history: &[Message], mode: Mode, input: &str) -> Result<String> {
    let mut prompt = String::new();
    prompt.push_str(system_prompt(mode));
    prompt.push_str("\n\n# 現在のモード\n");
    prompt.push_str(mode.name());
    prompt.push_str("\n\n# これまでの会話\n");

    for msg in trim_history(history, 12) {
        prompt.push_str("\n## ");
        prompt.push_str(&msg.role);
        prompt.push_str(" / ");
        prompt.push_str(&msg.mode);
        prompt.push('\n');
        prompt.push_str(&msg.content);
        prompt.push('\n');
    }

    prompt.push_str("\n# 今回の入力\n");
    prompt.push_str(input);
    prompt.push('\n');

    let mut cmd = Command::new(&cfg.codex_bin);
    cmd.arg("exec")
        .arg("--color")
        .arg("never")
        .arg("--skip-git-repo-check");

    if let Some(workdir) = &cfg.codex_workdir {
        cmd.arg("--cd").arg(workdir);
    }

    cmd.arg("-");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("failed to run codex exec")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open codex exec stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write prompt to codex exec stdin")?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for codex exec")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("codex exec failed:\n{}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_string())
}

fn ask_backend(
    cfg: &Config,
    client: &Client,
    backend: Backend,
    history: &[Message],
    mode: Mode,
    input: &str,
) -> Result<String> {
    match backend {
        Backend::Mi25 => {
            if !cfg.mi25_enabled {
                bail!(
                    "MI25 backend is disabled.\nZUTTOMO_MI25_ENABLED=true にすると有効化できます。"
                );
            }

            call_openai_compatible(
                client,
                &cfg.mi25_base_url,
                None,
                &cfg.mi25_model,
                history,
                mode,
                input,
            )
        }
        Backend::OpenRouter => {
            let key = cfg.openrouter_api_key.as_deref().with_context(|| {
                "OPENROUTER_API_KEY が未設定です。\nexport OPENROUTER_API_KEY=\"...\""
            })?;

            call_openai_compatible(
                client,
                &cfg.openrouter_base_url,
                Some(key),
                &cfg.openrouter_model,
                history,
                mode,
                input,
            )
        }
        Backend::Codex => call_codex(cfg, history, mode, input),
    }
}

fn parse_history_line(line: &str) -> Option<Message> {
    if let Ok(msg) = serde_json::from_str::<Message>(line) {
        return Some(msg);
    }

    let value: Value = serde_json::from_str(line).ok()?;
    let role = value.get("role")?.as_str()?.to_string();
    let mode = value
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let content = value.get("content")?.as_str()?.to_string();

    Some(Message {
        role,
        mode,
        content,
    })
}

fn load_history(path: &str) -> Vec<Message> {
    if !Path::new(path).exists() {
        return Vec::new();
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("history load skipped: {error}");
            return Vec::new();
        }
    };

    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                None
            } else {
                parse_history_line(line)
            }
        })
        .collect()
}

fn append_history(path: &str, msg: &Message) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("failed to open history file")?;

    let line = serde_json::to_string(msg)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn export_markdown(path: &str, history: &[Message]) -> Result<()> {
    let mut file = File::create(path).context("failed to create markdown history")?;
    writeln!(file, "# zuttomo history")?;
    writeln!(file)?;

    for msg in history {
        writeln!(file, "## {} ({})", msg.role, msg.mode)?;
        writeln!(file)?;
        writeln!(file, "{}", msg.content)?;
        writeln!(file)?;
    }

    Ok(())
}

fn local_ip_guess() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn pair_display_url(host: &str, port: u16) -> String {
    let display_host = if host == "0.0.0.0" || host == "::" {
        local_ip_guess().unwrap_or_else(|| "127.0.0.1".to_string())
    } else {
        host.to_string()
    };

    format!("http://{display_host}:{port}")
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn from_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;

    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' if idx + 2 < bytes.len() => {
                if let (Some(high), Some(low)) = (
                    from_hex_digit(bytes[idx + 1]),
                    from_hex_digit(bytes[idx + 2]),
                ) {
                    out.push(high * 16 + low);
                    idx += 3;
                } else {
                    out.push(bytes[idx]);
                    idx += 1;
                }
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).to_string()
}

fn parse_form_text(body: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == "text" {
                let text = percent_decode(value).trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }

    None
}

fn pair_form_page(latest_text: Option<&str>, oss_url: &str) -> String {
    let latest = latest_text
        .map(html_escape)
        .unwrap_or_else(|| "まだ入力はありません。".to_string());
    let oss_url = html_escape(oss_url);

    format!(
        r#"<!doctype html>
<html lang="zh-Hant">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>檸檬烯姐智慧板</title>
<style>
body {{ font-family: system-ui, sans-serif; margin: 24px; line-height: 1.5; }}
textarea {{ box-sizing: border-box; width: 100%; min-height: 160px; font-size: 18px; }}
button {{ width: 100%; margin-top: 12px; padding: 14px; font-size: 18px; }}
.latest {{ white-space: pre-wrap; padding: 12px; border: 1px solid #ccc; }}
.oss {{ margin-top: 24px; padding: 16px; border: 1px solid #bbb; }}
.oss a {{ display: block; margin-top: 8px; font-size: 18px; font-weight: 700; }}
</style>
</head>
<body>
<h1>檸檬烯姐智慧板</h1>
<p>請用你的手機輸入中文。りもこ這邊會翻譯成日文。</p>
<form method="post" action="/submit">
<textarea name="text" autofocus placeholder="請在這裡輸入中文"></textarea>
<button type="submit">送出</button>
</form>
<h2>最新輸入</h2>
<div class="latest">{latest}</div>
<section class="oss">
<h2>りもこのOSSまとめ</h2>
<p>ROCm、本地 LLM、MCP Agent、日本語 LLM 評測、開源基礎設施工具。</p>
<a href="{oss_url}" target="_blank" rel="noopener noreferrer">Open tools for local AI / 為本地 AI 打造開源工具</a>
</section>
</body>
</html>"#
    )
}

fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn handle_pair_client(
    mut stream: TcpStream,
    latest_partner_input: Arc<Mutex<Option<String>>>,
    oss_url: String,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header_trimmed = header.trim_end();
        if header_trimmed.is_empty() {
            break;
        }

        if let Some(value) = header_trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let latest_text = || {
        latest_partner_input
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    };

    if request_line.starts_with("GET / ") || request_line.starts_with("GET / HTTP/") {
        let page = pair_form_page(latest_text().as_deref(), &oss_url);
        write_http_response(&mut stream, "200 OK", "text/html", &page);
        return Ok(());
    }

    if request_line.starts_with("POST /submit ") || request_line.starts_with("POST /submit HTTP/") {
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body)?;
        let body = String::from_utf8_lossy(&body);

        let message = if let Some(text) = parse_form_text(&body) {
            if let Ok(mut latest) = latest_partner_input.lock() {
                *latest = Some(text);
            }
            "收到！請回到りもこ的CLI輸入 /you。"
        } else {
            "沒有收到文字，請再試一次。"
        };

        let page = format!(
            "{}<p><strong>{}</strong></p>",
            pair_form_page(latest_text().as_deref(), &oss_url),
            html_escape(message)
        );
        write_http_response(&mut stream, "200 OK", "text/html", &page);
        return Ok(());
    }

    write_http_response(&mut stream, "404 Not Found", "text/plain", "not found");
    Ok(())
}

fn start_pair_server(
    cfg: &Config,
    latest_partner_input: Arc<Mutex<Option<String>>>,
) -> Result<PairServer> {
    let bind_addr = format!("{}:{}", cfg.pair_host, cfg.pair_port);
    let listener = TcpListener::bind(&bind_addr)
        .with_context(|| format!("failed to bind pair server on {bind_addr}"))?;
    let url = pair_display_url(&cfg.pair_host, cfg.pair_port);
    let oss_url = cfg.oss_url.clone();

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let latest_partner_input = Arc::clone(&latest_partner_input);
                    let oss_url = oss_url.clone();
                    thread::spawn(move || {
                        let _ = handle_pair_client(stream, latest_partner_input, oss_url);
                    });
                }
                Err(error) => eprintln!("pair server accept failed: {error}"),
            }
        }
    });

    Ok(PairServer { url })
}

fn command_arg<'a>(input: &'a str, command: &str) -> Option<&'a str> {
    let rest = input.strip_prefix(command)?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

fn print_rimoko_aa() {
    const AA: &str = include_str!("../assets/rimoko.aa");
    println!("{AA}");
}

fn should_show_aa() -> bool {
    env::var("ZUTTOMO_NO_AA").is_err()
}

fn print_banner(backend: Backend, mode: Mode, history_len: usize) {
    if should_show_aa() {
        print_rimoko_aa();
    }

    println!("# りもこズッ友キラキラポボモフォ2.0");
    println!("## 檸檬烯姐總合熱烈爆裂友情智慧板");
    println!("### 昨日的技術交流、今日的ズッ友");
    println!();
    println!(
        "backend={} mode={} history={} messages",
        backend.name(),
        mode.name(),
        history_len
    );
    println!("type /help for commands");
    println!();
}

fn print_help(backend: Backend, mode: Mode, cfg: &Config) {
    println!("current backend={} mode={}", backend.name(), mode.name());
    println!();
    println!("commands:");
    println!("  /tw TEXT          日本語・ローマ字・英語 -> 台灣華語");
    println!("  /me TEXT          /tw の別名。相手に見せる文");
    println!("  /jp TEXT          台灣華語・繁體中文 -> 日本語");
    println!("  /you TEXT         相手入力を日本語化");
    println!("  /pobo TEXT        台灣華語 + ㄅㄆㄇㄈ 少し");
    println!("  /chat [TEXT]      翻訳・会話モード");
    println!("  /question [TEXT]  技術質問モード");
    println!("  /model [NAME]     mi25 / openrouter / codex");
    println!("  /history          直近履歴を表示");
    println!("  /clear            メモリ上の履歴を消去");
    println!("  /prompt           現在モードのsystem prompt表示");
    println!("  /export md        履歴を zuttomo-history.md に出力");
    println!("  /aa               起動バナーAAを表示");
    println!("  /pair             相手スマホ入力フォームを起動");
    println!("  /exit, /quit      終了");
    println!();
    println!("env:");
    println!("  ZUTTOMO_DEFAULT_BACKEND={}", cfg.default_backend.name());
    println!("  ZUTTOMO_MI25_ENABLED={}", cfg.mi25_enabled);
    println!("  ZUTTOMO_MI25_BASE_URL={}", cfg.mi25_base_url);
    println!("  ZUTTOMO_MI25_MODEL={}", cfg.mi25_model);
    println!("  OPENROUTER_BASE_URL={}", cfg.openrouter_base_url);
    println!("  OPENROUTER_MODEL={}", cfg.openrouter_model);
    println!("  CODEX_BIN={}", cfg.codex_bin);
    println!("  ZUTTOMO_HISTORY={}", cfg.history_path);
    println!("  ZUTTOMO_PAIR_HOST={}", cfg.pair_host);
    println!("  ZUTTOMO_PAIR_PORT={}", cfg.pair_port);
    println!("  ZUTTOMO_HTTP_TIMEOUT_SECS={}", cfg.http_timeout_secs);
    println!("  ZUTTOMO_OSS_URL={}", cfg.oss_url);
}

fn preview(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn print_history(history: &[Message], limit: usize) {
    if history.is_empty() {
        println!("history is empty");
        return;
    }

    let start = history.len().saturating_sub(limit);
    for (idx, msg) in history[start..].iter().enumerate() {
        println!(
            "{:>3}. {:<9} {:<8} {}",
            start + idx + 1,
            msg.role,
            msg.mode,
            preview(&msg.content.replace('\n', " "), 96)
        );
    }
}

fn print_backend_error(backend: Backend, error: &anyhow::Error) {
    println!();
    match backend {
        Backend::Mi25 => {
            println!("ふえぇぇ……MI25 APIに接続できません。");
            println!("ZUTTOMO_MI25_BASE_URL と ZUTTOMO_MI25_MODEL を確認してください。");
        }
        Backend::OpenRouter => {
            println!("ふえぇぇ……OpenRouter API呼び出しに失敗しました。");
            println!("OPENROUTER_API_KEY と OPENROUTER_MODEL を確認してください。");
        }
        Backend::Codex => {
            println!("ふえぇぇ……codex exec が失敗しました。");
            println!("CODEX_BIN または CODEX_WORKDIR を確認してください。");
        }
    }
    println!("{error:#}");
    println!();
}

fn run_turn(
    cfg: &Config,
    client: &Client,
    backend: Backend,
    history: &mut Vec<Message>,
    mode: Mode,
    input: &str,
) -> Result<String> {
    let answer = ask_backend(cfg, client, backend, history, mode, input)?;
    let user_msg = Message::new("user", mode, input);
    let assistant_msg = Message::new("assistant", mode, answer.clone());

    append_history(&cfg.history_path, &user_msg)?;
    append_history(&cfg.history_path, &assistant_msg)?;
    history.push(user_msg);
    history.push(assistant_msg);

    Ok(answer)
}

fn execute_turn(
    cfg: &Config,
    client: &Client,
    backend: Backend,
    history: &mut Vec<Message>,
    mode: Mode,
    input: &str,
) {
    match run_turn(cfg, client, backend, history, mode, input) {
        Ok(answer) => {
            println!();
            println!("{answer}");
            println!();
        }
        Err(error) => print_backend_error(backend, &error),
    }
}

fn print_cli_usage() {
    println!("zuttomo 0.1.0");
    println!();
    println!("usage:");
    println!("  zuttomo --run       start the REPL");
    println!("  zuttomo             start the REPL");
    println!("  zuttomo --help      show this help");
    println!("  zuttomo --version   show version");
    println!();
    println!("inside REPL:");
    println!("  /tw TEXT, /me TEXT, /jp TEXT, /you [TEXT], /pobo TEXT");
    println!("  /question [TEXT], /model [mi25|openrouter|codex], /pair");
}

fn run_repl() -> Result<()> {
    let _ = dotenvy::dotenv();

    let cfg = Config::from_env();
    let client = Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs))
        .build()
        .context("failed to build HTTP client")?;
    let mut backend = cfg.default_backend;
    let mut mode = Mode::Chat;
    let mut history = load_history(&cfg.history_path);
    let latest_partner_input = Arc::new(Mutex::new(None));
    let mut pair_server: Option<PairServer> = None;

    print_banner(backend, mode, history.len());

    loop {
        print!("[{}:{}] ㄅㄆㄇㄈ> ", backend.name(), mode.name());
        io::stdout().flush()?;

        let mut line = String::new();
        let n = io::stdin().read_line(&mut line)?;
        if n == 0 {
            break;
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        if input == "/exit" || input == "/quit" {
            println!("檸檬烯姐、終了します。");
            break;
        }

        if input == "/help" {
            print_help(backend, mode, &cfg);
            continue;
        }

        if input == "/aa" {
            print_rimoko_aa();
            continue;
        }

        if let Some(rest) = command_arg(input, "/model") {
            if rest.is_empty() {
                println!("current backend: {}", backend.name());
                println!("available: mi25, openrouter, codex");
            } else if let Some(new_backend) = Backend::from_name(rest) {
                if new_backend == Backend::Mi25 && !cfg.mi25_enabled {
                    println!("MI25 backend is disabled.");
                    println!("ZUTTOMO_MI25_ENABLED=true にすると有効化できます。");
                } else {
                    backend = new_backend;
                    println!("backend switched: {}", backend.name());
                }
            } else {
                println!("unknown backend: {rest}");
                println!("available: mi25, openrouter, codex");
            }
            continue;
        }

        if input == "/history" {
            print_history(&history, 30);
            continue;
        }

        if input == "/clear" {
            history.clear();
            println!("memory history cleared. history file was not deleted.");
            continue;
        }

        if input == "/prompt" {
            println!("{}", system_prompt(mode).trim());
            continue;
        }

        if let Some(rest) = command_arg(input, "/export") {
            let target = if rest.is_empty() || rest == "md" {
                "zuttomo-history.md"
            } else {
                rest
            };
            match export_markdown(target, &history) {
                Ok(()) => println!("exported: {target}"),
                Err(error) => println!("export failed: {error:#}"),
            }
            continue;
        }

        if input == "/pair" {
            if let Some(server) = &pair_server {
                println!("pair server is already running:");
                println!("{}", server.url);
            } else {
                match start_pair_server(&cfg, Arc::clone(&latest_partner_input)) {
                    Ok(server) => {
                        println!("請用你的手機輸入中文。");
                        println!("我這邊會自動翻譯成日文。");
                        println!("{}", server.url);
                        pair_server = Some(server);
                    }
                    Err(error) => {
                        println!("pair server start failed: {error:#}");
                        println!("今は /you TEXT で相手の台灣華語を直接日本語化できます。");
                    }
                }
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/chat") {
            mode = Mode::Chat;
            if rest.is_empty() {
                println!("mode switched: {}", mode.name());
            } else {
                execute_turn(&cfg, &client, backend, &mut history, mode, rest);
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/question") {
            mode = Mode::Question;
            if rest.is_empty() {
                println!("mode switched: {}", mode.name());
            } else {
                execute_turn(&cfg, &client, backend, &mut history, mode, rest);
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/tw") {
            mode = Mode::ToTaiwanMandarin;
            if rest.is_empty() {
                println!("mode switched: {}", mode.name());
            } else {
                execute_turn(&cfg, &client, backend, &mut history, mode, rest);
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/me") {
            mode = Mode::ToTaiwanMandarin;
            if rest.is_empty() {
                println!("usage: /me TEXT");
            } else {
                execute_turn(&cfg, &client, backend, &mut history, mode, rest);
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/jp") {
            mode = Mode::ToJapanese;
            if rest.is_empty() {
                println!("mode switched: {}", mode.name());
            } else {
                execute_turn(&cfg, &client, backend, &mut history, mode, rest);
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/you") {
            mode = Mode::ToJapanese;
            if rest.is_empty() {
                let partner_input = latest_partner_input
                    .lock()
                    .ok()
                    .and_then(|latest| latest.clone());
                if let Some(partner_input) = partner_input {
                    execute_turn(&cfg, &client, backend, &mut history, mode, &partner_input);
                } else {
                    println!("相手スマホ入力はまだありません。今は /you TEXT を使ってください。");
                }
            } else {
                execute_turn(&cfg, &client, backend, &mut history, mode, rest);
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/pobo") {
            mode = Mode::Pobo;
            if rest.is_empty() {
                println!("mode switched: {}", mode.name());
            } else {
                execute_turn(&cfg, &client, backend, &mut history, mode, rest);
            }
            continue;
        }

        if input.starts_with('/') {
            println!("unknown command: {input}");
            println!("type /help for commands");
            continue;
        }

        execute_turn(&cfg, &client, backend, &mut history, mode, input);
    }

    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.as_slice() {
        [] => run_repl(),
        [arg] if arg == "--run" || arg == "run" => run_repl(),
        [arg] if arg == "--help" || arg == "-h" || arg == "help" => {
            print_cli_usage();
            Ok(())
        }
        [arg] if arg == "--version" || arg == "-V" || arg == "version" => {
            println!("zuttomo 0.1.0");
            Ok(())
        }
        _ => {
            print_cli_usage();
            bail!("unknown zuttomo option: {}", args.join(" "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_aliases_parse() {
        assert_eq!(Backend::from_name("mi25"), Some(Backend::Mi25));
        assert_eq!(Backend::from_name("or"), Some(Backend::OpenRouter));
        assert_eq!(Backend::from_name("codex-cli"), Some(Backend::Codex));
        assert_eq!(Backend::from_name("unknown"), None);
    }

    #[test]
    fn command_arg_requires_boundary() {
        assert_eq!(command_arg("/tw hello", "/tw"), Some("hello"));
        assert_eq!(command_arg("/tw", "/tw"), Some(""));
        assert_eq!(command_arg("/twist", "/tw"), None);
    }

    #[test]
    fn history_line_supports_legacy_without_mode() {
        let msg = parse_history_line(r#"{"role":"user","content":"hello"}"#).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.mode, "unknown");
        assert_eq!(msg.content, "hello");
    }

    #[test]
    fn form_text_is_percent_decoded() {
        assert_eq!(
            parse_form_text(
                "ignored&text=%E5%93%87%EF%BC%8C%E9%80%99%E5%BE%88%E6%9C%89%E8%B6%A3%EF%BC%81"
            )
            .as_deref(),
            Some("哇，這很有趣！")
        );
        assert_eq!(
            parse_form_text("text=hello+world").as_deref(),
            Some("hello world")
        );
    }
}
