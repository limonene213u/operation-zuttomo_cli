use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use rustyline::error::ReadlineError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    mpsc::{self, Sender},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<sha2::Sha256>;

const DEFAULT_PAIR_AUTH_CODE_TTL_SECS: u64 = 90;
const MAX_PAIR_SESSION_TTL_SECS: u64 = 5 * 60 * 60;

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
    pair_public_url: String,
    pair_token: Option<String>,
    pair_reverse_tunnel_enabled: bool,
    pair_reverse_tunnel_ssh: String,
    pair_reverse_tunnel_key: Option<String>,
    pair_reverse_tunnel_host_key_alias: Option<String>,
    pair_autostart: bool,
    pair_onetime_ttl_secs: u64,
    pair_session_ttl_secs: u64,
    pair_auth_max_attempts: u32,
    pair_auth_lockout_secs: u64,
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
                .unwrap_or(8080),
            pair_public_url: env::var("ZUTTOMO_PAIR_PUBLIC_URL")
                .unwrap_or_else(|_| "https://zuttomo-test.aets-hiroshima.org".to_string()),
            pair_token: env_nonempty("ZUTTOMO_PAIR_TOKEN"),
            pair_reverse_tunnel_enabled: env_bool("ZUTTOMO_PAIR_REVERSE_TUNNEL", true),
            pair_reverse_tunnel_ssh: env::var("ZUTTOMO_PAIR_REVERSE_TUNNEL_SSH")
                .unwrap_or_else(|_| "ubuntu@100.119.206.15".to_string()),
            pair_reverse_tunnel_key: env_nonempty("ZUTTOMO_PAIR_REVERSE_TUNNEL_KEY")
                .or_else(|| env_nonempty("ZUTTOMO_PAIR_SSH_KEY"))
                .or_else(|| Some("~/.ssh/id_rsa-ansible".to_string())),
            pair_reverse_tunnel_host_key_alias: env_nonempty(
                "ZUTTOMO_PAIR_REVERSE_TUNNEL_HOST_KEY_ALIAS",
            )
            .or_else(|| Some("153.127.64.95".to_string())),
            pair_autostart: env_bool("ZUTTOMO_PAIR_AUTOSTART", true),
            pair_onetime_ttl_secs: env_u64(
                "ZUTTOMO_PAIR_ONETIME_TTL_SECS",
                DEFAULT_PAIR_AUTH_CODE_TTL_SECS,
            ),
            pair_session_ttl_secs: env_u64(
                "ZUTTOMO_PAIR_SESSION_TTL_SECS",
                MAX_PAIR_SESSION_TTL_SECS,
            )
            .min(MAX_PAIR_SESSION_TTL_SECS),
            pair_auth_max_attempts: env_u32("ZUTTOMO_PAIR_AUTH_MAX_ATTEMPTS", 8),
            pair_auth_lockout_secs: env_u64("ZUTTOMO_PAIR_AUTH_LOCKOUT_SECS", 60),
            http_timeout_secs: env::var("ZUTTOMO_HTTP_TIMEOUT_SECS")
                .ok()
                .and_then(|secs| secs.parse().ok())
                .unwrap_or(120),
            oss_url: env::var("ZUTTOMO_OSS_URL")
                .unwrap_or_else(|_| "https://limonene213u.github.io/ai-2026-computex/".to_string()),
        }
    }
}

#[derive(Debug)]
struct PairServer {
    urls: Vec<PairUrl>,
    warnings: Vec<String>,
    _reverse_tunnel: Option<PairReverseTunnel>,
}

#[derive(Debug)]
struct PairUrl {
    label: &'static str,
    url: String,
}

#[derive(Debug)]
struct PairReverseTunnel {
    child: Child,
}

impl Drop for PairReverseTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthCode {
    value: String,
    expires_in_secs: u64,
}

#[derive(Debug, Clone)]
struct PairBubble {
    speaker: String,
    kind: String,
    owner_session: Option<String>,
    body: String,
}

#[derive(Debug, Clone)]
struct PairInput {
    speaker: String,
    text: String,
}

#[derive(Debug, Clone)]
struct AuthAttempt {
    failures: u32,
    locked_until: u64,
}

#[derive(Debug)]
struct OneTimeAuth {
    secret: Vec<u8>,
    sessions: HashMap<String, u64>,
    attempts: HashMap<String, AuthAttempt>,
    code_ttl_secs: u64,
    session_ttl_secs: u64,
    max_attempts: u32,
    lockout_secs: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum OneTimeAuthResult {
    Success {
        session_id: String,
        max_age_secs: u64,
    },
    Invalid,
    Locked {
        retry_after_secs: u64,
    },
}

impl OneTimeAuth {
    fn new(
        code_ttl_secs: u64,
        session_ttl_secs: u64,
        max_attempts: u32,
        lockout_secs: u64,
    ) -> Self {
        Self::with_secret(
            generate_auth_secret().to_vec(),
            code_ttl_secs,
            session_ttl_secs,
            max_attempts,
            lockout_secs,
        )
    }

    fn with_secret(
        secret: Vec<u8>,
        code_ttl_secs: u64,
        session_ttl_secs: u64,
        max_attempts: u32,
        lockout_secs: u64,
    ) -> Self {
        Self {
            secret,
            sessions: HashMap::new(),
            attempts: HashMap::new(),
            code_ttl_secs: code_ttl_secs.max(1),
            session_ttl_secs: session_ttl_secs.min(MAX_PAIR_SESSION_TTL_SECS),
            max_attempts: max_attempts.max(1),
            lockout_secs,
        }
    }

    fn current_code(&self) -> AuthCode {
        self.current_code_at(now_secs())
    }

    fn current_code_at(&self, now: u64) -> AuthCode {
        let window = now / self.code_ttl_secs;
        let expires_in_secs = self.code_ttl_secs - (now % self.code_ttl_secs);
        AuthCode {
            value: auth_code_for_window(&self.secret, window),
            expires_in_secs,
        }
    }

    fn verify_code(&mut self, input: &str, client_id: &str) -> OneTimeAuthResult {
        self.verify_code_at(input, client_id, now_secs())
    }

    fn verify_code_at(&mut self, input: &str, client_id: &str, now: u64) -> OneTimeAuthResult {
        self.prune(now);

        if let Some(attempt) = self.attempts.get(client_id) {
            if attempt.locked_until > now {
                return OneTimeAuthResult::Locked {
                    retry_after_secs: attempt.locked_until.saturating_sub(now),
                };
            }
        }

        if !is_onetime_code(input) || input != self.current_code_at(now).value {
            self.record_failure(client_id, now);
            return OneTimeAuthResult::Invalid;
        }

        let session_id = generate_pair_token();
        self.sessions.insert(
            session_id.clone(),
            now.saturating_add(self.session_ttl_secs),
        );
        self.attempts.remove(client_id);

        OneTimeAuthResult::Success {
            session_id,
            max_age_secs: self.session_ttl_secs,
        }
    }

    fn is_session_authenticated(&mut self, cookie_header: Option<&str>) -> bool {
        self.is_session_authenticated_at(cookie_header, now_secs())
    }

    fn is_session_authenticated_at(&mut self, cookie_header: Option<&str>, now: u64) -> bool {
        self.prune(now);
        let Some(session_id) = cookie_value(cookie_header, PAIR_SESSION_COOKIE) else {
            return false;
        };
        self.sessions
            .get(&session_id)
            .is_some_and(|expires_at| *expires_at > now)
    }

    fn record_failure(&mut self, client_id: &str, now: u64) {
        let attempt = self
            .attempts
            .entry(client_id.to_string())
            .or_insert(AuthAttempt {
                failures: 0,
                locked_until: 0,
            });
        attempt.failures = attempt.failures.saturating_add(1);

        if attempt.failures >= self.max_attempts {
            attempt.failures = 0;
            attempt.locked_until = now.saturating_add(self.lockout_secs);
        }
    }

    fn prune(&mut self, now: u64) {
        self.sessions.retain(|_, expires_at| *expires_at > now);
        self.attempts
            .retain(|_, attempt| attempt.locked_until == 0 || attempt.locked_until > now);
    }
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

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
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

fn is_wildcard_host(host: &str) -> bool {
    host == "0.0.0.0" || host == "::" || host == "[::]"
}

fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "localhost" || host == "::1" || host == "[::1]"
}

fn url_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn pair_base_url(host: &str, port: u16) -> String {
    format!("http://{}:{port}", url_host(host))
}

fn pair_lan_base_url(host: &str, port: u16) -> Option<String> {
    if is_wildcard_host(host) {
        let ip = local_ip_guess()?;
        if is_loopback_host(&ip) {
            None
        } else {
            Some(pair_base_url(&ip, port))
        }
    } else if is_loopback_host(host) {
        None
    } else {
        Some(pair_base_url(host, port))
    }
}

fn pair_url(base_url: &str, token: &str) -> String {
    if base_url.contains("{token}") {
        base_url.replace("{token}", token)
    } else {
        base_url.trim_end_matches('/').to_string()
    }
}

fn pair_page_path(token: &str) -> String {
    format!("/t/{token}")
}

fn pair_submit_path(token: &str) -> String {
    format!("/t/{token}/submit")
}

fn pair_auth_path(token: &str) -> String {
    format!("/t/{token}/auth")
}

fn pair_state_path(token: &str) -> String {
    format!("/t/{token}/state")
}

const PAIR_SESSION_COOKIE: &str = "zuttomo_pair_session";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }

    out
}

fn generate_pair_token() -> String {
    let mut bytes = [0u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return hex_encode(&bytes);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}{:x}", now, std::process::id())
}

fn generate_auth_secret() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_ne_bytes();
        for (idx, byte) in now.iter().enumerate() {
            bytes[idx] ^= *byte;
        }

        let pid = std::process::id().to_ne_bytes();
        for (idx, byte) in pid.iter().enumerate() {
            bytes[16 + idx] ^= *byte;
        }
    }

    bytes
}

fn auth_code_for_window(secret: &[u8], window: u64) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any size");
    mac.update(&window.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let value = ((u32::from(digest[offset]) & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    format!("{:08}", value % 100_000_000)
}

fn is_onetime_code(input: &str) -> bool {
    input.len() == 8 && input.bytes().all(|byte| byte.is_ascii_digit())
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }

    path.to_string()
}

fn start_pair_reverse_tunnel(cfg: &Config) -> Result<Option<PairReverseTunnel>> {
    if !cfg.pair_reverse_tunnel_enabled {
        return Ok(None);
    }

    let remote_forward = format!("127.0.0.1:{port}:127.0.0.1:{port}", port = cfg.pair_port);
    let mut cmd = Command::new("ssh");
    cmd.arg("-N")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new");

    if let Some(alias) = &cfg.pair_reverse_tunnel_host_key_alias {
        cmd.arg("-o").arg(format!("HostKeyAlias={alias}"));
    }

    if let Some(key) = &cfg.pair_reverse_tunnel_key {
        cmd.arg("-i").arg(expand_tilde(key));
    }

    cmd.arg("-R")
        .arg(remote_forward)
        .arg(&cfg.pair_reverse_tunnel_ssh)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("failed to start pair reverse SSH tunnel")?;

    thread::sleep(Duration::from_millis(600));
    if let Some(status) = child
        .try_wait()
        .context("failed to check pair reverse SSH tunnel")?
    {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        bail!(
            "pair reverse SSH tunnel exited with {status}: {}",
            stderr.trim()
        );
    }

    Ok(Some(PairReverseTunnel { child }))
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

fn parse_form_field(body: &str, name: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == name {
                let value = percent_decode(value).trim().to_string();
                return Some(value);
            }
        }
    }

    None
}

fn parse_form_text(body: &str) -> Option<String> {
    parse_form_field(body, "text").filter(|text| !text.is_empty())
}

fn parse_form_onetime_code(body: &str) -> Option<String> {
    parse_form_field(body, "code").filter(|code| is_onetime_code(code))
}

fn pair_speaker_label(session_id: Option<&str>, client_id: &str) -> String {
    if let Some(session_id) = session_id {
        let short = session_id.chars().take(6).collect::<String>();
        return format!("相手 {short}");
    }

    format!("相手 {client_id}")
}

fn cookie_value(cookie_header: Option<&str>, name: &str) -> Option<String> {
    let cookie_header = cookie_header?;

    for cookie in cookie_header.split(';') {
        if let Some((key, value)) = cookie.trim().split_once('=') {
            if key == name && !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    None
}

fn push_pair_bubble(
    pair_bubbles: &Arc<Mutex<Vec<PairBubble>>>,
    speaker: &str,
    kind: &str,
    owner_session: Option<String>,
    body: impl Into<String>,
) {
    if let Ok(mut bubbles) = pair_bubbles.lock() {
        bubbles.push(PairBubble {
            speaker: speaker.to_string(),
            kind: kind.to_string(),
            owner_session,
            body: body.into(),
        });

        let start = bubbles.len().saturating_sub(80);
        if start > 0 {
            bubbles.drain(..start);
        }
    }
}

fn render_pair_bubbles(bubbles: &[PairBubble], current_session: Option<&str>) -> String {
    if bubbles.is_empty() {
        return r#"<p class="empty">まだ会話はありません。</p>"#.to_string();
    }

    let mut html = String::new();
    for bubble in bubbles {
        let (kind, speaker) = if bubble.kind == "web" {
            if bubble
                .owner_session
                .as_deref()
                .zip(current_session)
                .is_some_and(|(owner, current)| owner == current)
            {
                ("self", "自分")
            } else {
                ("other", "他人")
            }
        } else {
            (bubble.kind.as_str(), bubble.speaker.as_str())
        };
        html.push_str(&format!(
            r#"<article class="bubble {}"><div class="speaker">{}</div><div class="text">{}</div></article>"#,
            html_escape(kind),
            html_escape(speaker),
            html_escape(&bubble.body)
        ));
    }
    html
}

fn pair_form_page(
    latest_text: Option<&str>,
    pair_bubbles: &[PairBubble],
    current_session: Option<&str>,
    oss_url: &str,
    _token: &str,
) -> String {
    let latest = latest_text
        .map(html_escape)
        .unwrap_or_else(|| "まだ入力はありません。".to_string());
    let oss_url = html_escape(oss_url);
    let submit_path = html_escape("/submit");
    let state_path = html_escape("/state");
    let bubbles = render_pair_bubbles(pair_bubbles, current_session);

    format!(
        r#"<!doctype html>
<html lang="zh-Hant">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>檸檬烯姐智慧板</title>
<style>
body {{ font-family: system-ui, sans-serif; margin: 0; line-height: 1.5; background: #f6f7f8; color: #17202a; }}
main {{ max-width: 760px; margin: 0 auto; padding: 16px; }}
textarea {{ box-sizing: border-box; width: 100%; min-height: 120px; font-size: 18px; border: 1px solid #b8c0cc; border-radius: 8px; padding: 12px; }}
button {{ width: 100%; margin-top: 10px; padding: 14px; font-size: 18px; border: 0; border-radius: 8px; background: #0b67d1; color: #fff; font-weight: 700; }}
.timeline {{ display: flex; flex-direction: column; gap: 10px; margin: 18px 0; }}
.bubble {{ max-width: 86%; padding: 10px 12px; border-radius: 8px; white-space: pre-wrap; box-shadow: 0 1px 2px rgba(0,0,0,.08); }}
.bubble.cli {{ align-self: flex-start; background: #dcecff; border: 1px solid #a9cdf8; }}
.bubble.self {{ align-self: flex-end; background: #dff5e4; border: 1px solid #a8d9b2; }}
.bubble.other {{ align-self: flex-start; background: #eeeeee; border: 1px solid #d0d0d0; }}
.speaker {{ font-size: 13px; font-weight: 700; margin-bottom: 4px; color: #34495e; }}
.text {{ font-size: 17px; }}
.latest, .empty {{ white-space: pre-wrap; padding: 12px; border: 1px solid #d0d0d0; border-radius: 8px; background: #fff; }}
.oss {{ margin-top: 20px; padding: 14px; border: 1px solid #d0d0d0; border-radius: 8px; background: #fff; }}
.oss a {{ display: block; margin-top: 8px; font-size: 18px; font-weight: 700; }}
</style>
</head>
<body>
<main>
<h1>檸檬烯姐智慧板</h1>
<p>請輸入中文。送出後會自動翻譯，下面可以追蹤對話。</p>
<form method="post" action="{submit_path}">
<textarea name="text" autofocus placeholder="請在這裡輸入中文"></textarea>
<button type="submit">送出</button>
</form>
<h2>對話</h2>
<section id="timeline" class="timeline">{bubbles}</section>
<h2>最新輸入</h2>
<div id="latest" class="latest">{latest}</div>
<section class="oss">
<h2>りもこのOSSまとめ</h2>
<p>ROCm、本地 LLM、MCP Agent、日本語 LLM 評測、開源基礎設施工具。</p>
<a href="{oss_url}" target="_blank" rel="noopener noreferrer">Open tools for local AI / 為本地 AI 打造開源工具</a>
</section>
</main>
<script>
const stateUrl = "{state_path}";
async function refreshState() {{
  try {{
    const res = await fetch(stateUrl, {{ cache: "no-store" }});
    if (!res.ok) return;
    const state = await res.json();
    const timeline = document.getElementById("timeline");
    const latest = document.getElementById("latest");
    if (timeline && state.timeline_html !== undefined) timeline.innerHTML = state.timeline_html;
    if (latest && state.latest_html !== undefined) latest.innerHTML = state.latest_html;
  }} catch (_) {{}}
}}
setInterval(refreshState, 700);
refreshState();
document.addEventListener("visibilitychange", () => {{
  if (!document.hidden) refreshState();
}});
</script>
</body>
</html>"#
    )
}

fn pair_state_json(
    latest_text: Option<&str>,
    pair_bubbles: &[PairBubble],
    current_session: Option<&str>,
) -> String {
    json!({
        "timeline_html": render_pair_bubbles(pair_bubbles, current_session),
        "latest_html": latest_text
            .map(html_escape)
            .unwrap_or_else(|| "まだ入力はありません。".to_string()),
    })
    .to_string()
}

fn pair_auth_page(_token: &str, message: Option<&str>) -> String {
    let auth_path = html_escape("/auth");
    let message = message
        .map(|text| format!(r#"<p class="message">{}</p>"#, html_escape(text)))
        .unwrap_or_default();

    format!(
        r#"<!doctype html>
<html lang="zh-Hant">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>檸檬烯姐智慧板</title>
<style>
body {{ font-family: system-ui, sans-serif; margin: 24px; line-height: 1.5; }}
input {{ box-sizing: border-box; width: 100%; padding: 14px; font-size: 24px; letter-spacing: 0; }}
button {{ width: 100%; margin-top: 12px; padding: 14px; font-size: 18px; }}
.message {{ padding: 12px; border: 1px solid #ccc; }}
</style>
</head>
<body>
<h1>檸檬烯姐智慧板</h1>
<p>請輸入りもこ CLI 用 /auth 顯示的8位數字。認證後可以直接送出中文，CLI會自動翻譯。</p>
{message}
<form method="post" action="{auth_path}">
<input name="code" inputmode="numeric" autocomplete="one-time-code" maxlength="8" pattern="[0-9]{{8}}" autofocus>
<button type="submit">認證</button>
</form>
</body>
</html>"#
    )
}

fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    write_http_response_with_headers(stream, status, content_type, &[], body);
}

fn write_http_response_with_headers(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    headers: &[String],
    body: &str,
) {
    let extra_headers = if headers.is_empty() {
        String::new()
    } else {
        format!("{}\r\n", headers.join("\r\n"))
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn request_method_path(request_line: &str) -> Option<(&str, &str)> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?.split('?').next().unwrap_or("");
    Some((method, path))
}

fn handle_pair_client(
    mut stream: TcpStream,
    latest_partner_input: Arc<Mutex<Option<String>>>,
    pair_auth: Arc<Mutex<OneTimeAuth>>,
    pair_bubbles: Arc<Mutex<Vec<PairBubble>>>,
    oss_url: String,
    token: String,
    input_tx: Sender<PairInput>,
) -> io::Result<()> {
    let client_id = stream
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut content_length = 0usize;
    let mut cookie_header = None;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header_trimmed = header.trim_end();
        if header_trimmed.is_empty() {
            break;
        }

        if let Some((name, value)) = header_trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("Cookie") {
                cookie_header = Some(value.trim().to_string());
            }
        }
    }

    let latest_text = || {
        latest_partner_input
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    };
    let bubbles = || {
        pair_bubbles
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    };

    let (method, path) = match request_method_path(&request_line) {
        Some(parts) => parts,
        None => {
            write_http_response(&mut stream, "400 Bad Request", "text/plain", "bad request");
            return Ok(());
        }
    };

    let page_path = pair_page_path(&token);
    let page_path_slash = format!("{page_path}/");
    let submit_path = pair_submit_path(&token);
    let auth_path = pair_auth_path(&token);
    let auth_path_slash = format!("{auth_path}/");
    let state_path = pair_state_path(&token);
    let state_path_slash = format!("{state_path}/");
    let authenticated = pair_auth
        .lock()
        .map(|mut auth| auth.is_session_authenticated(cookie_header.as_deref()))
        .unwrap_or(false);
    let current_session = cookie_value(cookie_header.as_deref(), PAIR_SESSION_COOKIE);

    if method == "GET"
        && (path == "/"
            || path == "/auth"
            || path == page_path
            || path == page_path_slash
            || path == auth_path
            || path == auth_path_slash)
    {
        if !authenticated {
            let page = pair_auth_page(&token, None);
            write_http_response(&mut stream, "200 OK", "text/html", &page);
            return Ok(());
        }

        let page = pair_form_page(
            latest_text().as_deref(),
            &bubbles(),
            current_session.as_deref(),
            &oss_url,
            &token,
        );
        write_http_response(&mut stream, "200 OK", "text/html", &page);
        return Ok(());
    }

    if method == "GET" && (path == "/state" || path == state_path || path == state_path_slash) {
        if !authenticated {
            write_http_response(
                &mut stream,
                "401 Unauthorized",
                "application/json",
                r#"{"error":"unauthorized"}"#,
            );
            return Ok(());
        }

        let body = pair_state_json(
            latest_text().as_deref(),
            &bubbles(),
            current_session.as_deref(),
        );
        write_http_response(&mut stream, "200 OK", "application/json", &body);
        return Ok(());
    }

    if method == "POST" && (path == "/auth" || path == auth_path || path == auth_path_slash) {
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body)?;
        let body = String::from_utf8_lossy(&body);
        let code = parse_form_onetime_code(&body).unwrap_or_default();

        let result = pair_auth
            .lock()
            .map(|mut auth| auth.verify_code(&code, &client_id))
            .unwrap_or(OneTimeAuthResult::Invalid);

        match result {
            OneTimeAuthResult::Success {
                session_id,
                max_age_secs,
            } => {
                let page = pair_form_page(
                    latest_text().as_deref(),
                    &bubbles(),
                    Some(&session_id),
                    &oss_url,
                    &token,
                );
                let cookie = format!(
                    "Set-Cookie: {PAIR_SESSION_COOKIE}={session_id}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age_secs}"
                );
                write_http_response_with_headers(
                    &mut stream,
                    "200 OK",
                    "text/html",
                    &[cookie],
                    &page,
                );
            }
            OneTimeAuthResult::Locked { retry_after_secs } => {
                let message =
                    format!("失敗が多すぎます。{retry_after_secs}秒後に再試行してください。");
                let page = pair_auth_page(&token, Some(&message));
                write_http_response(&mut stream, "429 Too Many Requests", "text/html", &page);
            }
            OneTimeAuthResult::Invalid => {
                let page = pair_auth_page(
                    &token,
                    Some(
                        "8桁の数字が違うか、期限切れです。CLIで /auth を実行して確認してください。",
                    ),
                );
                write_http_response(&mut stream, "401 Unauthorized", "text/html", &page);
            }
        }

        return Ok(());
    }

    if method == "POST" && (path == "/submit" || path == submit_path) {
        if !authenticated {
            let page = pair_auth_page(&token, Some("先に8桁の数字で認証してください。"));
            write_http_response(&mut stream, "401 Unauthorized", "text/html", &page);
            return Ok(());
        }

        let mut body = vec![0; content_length];
        reader.read_exact(&mut body)?;
        let body = String::from_utf8_lossy(&body);

        let message = if let Some(text) = parse_form_text(&body) {
            let speaker = pair_speaker_label(current_session.as_deref(), &client_id);
            if let Ok(mut latest) = latest_partner_input.lock() {
                *latest = Some(text.clone());
            }
            push_pair_bubble(
                &pair_bubbles,
                "自分",
                "web",
                current_session.clone(),
                text.clone(),
            );
            let _ = input_tx.send(PairInput {
                speaker: speaker.clone(),
                text: text.clone(),
            });
            println!();
            println!("[pair:{speaker}] WebUI input received:");
            println!("{text}");
            println!("[pair] 自動で日本語翻訳します。/you は不要です。");
            let _ = io::stdout().flush();
            "收到！會自動翻譯。"
        } else {
            "沒有收到文字，請再試一次。"
        };

        let page = format!(
            "{}<p><strong>{}</strong></p>",
            pair_form_page(
                latest_text().as_deref(),
                &bubbles(),
                current_session.as_deref(),
                &oss_url,
                &token,
            ),
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
    pair_auth: Arc<Mutex<OneTimeAuth>>,
    pair_bubbles: Arc<Mutex<Vec<PairBubble>>>,
    input_tx: Sender<PairInput>,
) -> Result<PairServer> {
    let bind_addr = format!("{}:{}", cfg.pair_host, cfg.pair_port);
    let listener = TcpListener::bind(&bind_addr)
        .with_context(|| format!("failed to bind pair server on {bind_addr}"))?;
    let token = cfg.pair_token.clone().unwrap_or_else(generate_pair_token);
    let oss_url = cfg.oss_url.clone();
    let mut urls = Vec::new();
    let mut warnings = Vec::new();

    urls.push(PairUrl {
        label: "public",
        url: pair_url(&cfg.pair_public_url, &token),
    });

    if is_wildcard_host(&cfg.pair_host) || is_loopback_host(&cfg.pair_host) {
        urls.push(PairUrl {
            label: "local",
            url: pair_url(&pair_base_url("127.0.0.1", cfg.pair_port), &token),
        });
    }

    if let Some(base_url) = pair_lan_base_url(&cfg.pair_host, cfg.pair_port) {
        urls.push(PairUrl {
            label: "LAN/Tailscale",
            url: pair_url(&base_url, &token),
        });
    }

    let reverse_tunnel = match start_pair_reverse_tunnel(cfg) {
        Ok(tunnel) => tunnel,
        Err(error) => {
            warnings.push(format!("pair reverse SSH tunnel failed: {error:#}"));
            None
        }
    };

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let latest_partner_input = Arc::clone(&latest_partner_input);
                    let pair_auth = Arc::clone(&pair_auth);
                    let pair_bubbles = Arc::clone(&pair_bubbles);
                    let oss_url = oss_url.clone();
                    let token = token.clone();
                    let input_tx = input_tx.clone();
                    thread::spawn(move || {
                        let _ = handle_pair_client(
                            stream,
                            latest_partner_input,
                            pair_auth,
                            pair_bubbles,
                            oss_url,
                            token,
                            input_tx,
                        );
                    });
                }
                Err(error) => eprintln!("pair server accept failed: {error}"),
            }
        }
    });

    Ok(PairServer {
        urls,
        warnings,
        _reverse_tunnel: reverse_tunnel,
    })
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
    println!("  /auth             WebUI認証用の8桁コードを表示");
    println!("  /onetime          /auth の旧名");
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
    println!("  ZUTTOMO_PAIR_PUBLIC_URL={}", cfg.pair_public_url);
    println!(
        "  ZUTTOMO_PAIR_ONETIME_TTL_SECS={}",
        cfg.pair_onetime_ttl_secs
    );
    println!(
        "  ZUTTOMO_PAIR_SESSION_TTL_SECS={}",
        cfg.pair_session_ttl_secs
    );
    println!(
        "  ZUTTOMO_PAIR_AUTH_MAX_ATTEMPTS={}",
        cfg.pair_auth_max_attempts
    );
    println!(
        "  ZUTTOMO_PAIR_AUTH_LOCKOUT_SECS={}",
        cfg.pair_auth_lockout_secs
    );
    println!(
        "  ZUTTOMO_PAIR_TOKEN={}",
        if cfg.pair_token.is_some() {
            "<set>"
        } else {
            "<generated>"
        }
    );
    println!(
        "  ZUTTOMO_PAIR_REVERSE_TUNNEL={}",
        cfg.pair_reverse_tunnel_enabled
    );
    println!(
        "  ZUTTOMO_PAIR_REVERSE_TUNNEL_SSH={}",
        cfg.pair_reverse_tunnel_ssh
    );
    println!("  ZUTTOMO_PAIR_AUTOSTART={}", cfg.pair_autostart);
    println!("  ZUTTOMO_HTTP_TIMEOUT_SECS={}", cfg.http_timeout_secs);
    println!("  ZUTTOMO_OSS_URL={}", cfg.oss_url);
}

fn print_pair_server(server: &PairServer) {
    for pair_url in &server.urls {
        println!("{}: {}", pair_url.label, pair_url.url);
    }

    for warning in &server.warnings {
        println!("warning: {warning}");
    }
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

fn run_turn_shared(
    cfg: &Config,
    client: &Client,
    backend: Backend,
    history: &Arc<Mutex<Vec<Message>>>,
    mode: Mode,
    input: &str,
) -> Result<String> {
    let history_snapshot = {
        history
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    };
    let answer = ask_backend(cfg, client, backend, &history_snapshot, mode, input)?;
    let user_msg = Message::new("user", mode, input);
    let assistant_msg = Message::new("assistant", mode, answer.clone());

    append_history(&cfg.history_path, &user_msg)?;
    append_history(&cfg.history_path, &assistant_msg)?;
    if let Ok(mut guard) = history.lock() {
        guard.push(user_msg);
        guard.push(assistant_msg);
    }

    Ok(answer)
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
    println!("  /question [TEXT], /model [mi25|openrouter|codex], /pair, /auth");
}

fn print_auth_code(pair_auth: &Arc<Mutex<OneTimeAuth>>, cfg: &Config) {
    match pair_auth.lock() {
        Ok(auth) => {
            let code = auth.current_code();
            println!("auth code: {}", code.value);
            println!(
                "valid for {} seconds. WebUI sessions last up to {} hours.",
                code.expires_in_secs,
                cfg.pair_session_ttl_secs / 3600
            );
        }
        Err(_) => println!("auth state is unavailable"),
    }
}

fn repl_prompt(backend: Backend, mode: Mode) -> String {
    format!("[{}:{}] ㄅㄆㄇㄈ> ", backend.name(), mode.name())
}

fn print_repl_prompt(backend: Backend, mode: Mode) -> io::Result<()> {
    print!("{}", repl_prompt(backend, mode));
    io::stdout().flush()
}

fn should_publish_to_pair(mode: Mode) -> bool {
    matches!(mode, Mode::Chat | Mode::ToTaiwanMandarin)
}

fn run_repl() -> Result<()> {
    let _ = dotenvy::dotenv();

    let cfg = Config::from_env();
    let client = Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_secs))
        .build()
        .context("failed to build HTTP client")?;
    let backend = Arc::new(Mutex::new(cfg.default_backend));
    let mode = Arc::new(Mutex::new(Mode::Chat));
    let history = Arc::new(Mutex::new(load_history(&cfg.history_path)));
    let latest_partner_input = Arc::new(Mutex::new(None));
    let pair_bubbles = Arc::new(Mutex::new(Vec::<PairBubble>::new()));
    let pair_auth = Arc::new(Mutex::new(OneTimeAuth::new(
        cfg.pair_onetime_ttl_secs,
        cfg.pair_session_ttl_secs,
        cfg.pair_auth_max_attempts,
        cfg.pair_auth_lockout_secs,
    )));
    let (pair_tx, pair_rx) = mpsc::channel::<PairInput>();
    let mut pair_server: Option<PairServer> = None;
    let mut line_editor =
        rustyline::DefaultEditor::new().context("failed to initialize line editor")?;

    spawn_pair_translation_worker(
        cfg.clone(),
        client.clone(),
        Arc::clone(&backend),
        Arc::clone(&mode),
        Arc::clone(&history),
        pair_rx,
    );

    let initial_backend = backend
        .lock()
        .map(|guard| *guard)
        .unwrap_or(cfg.default_backend);
    let initial_mode = mode.lock().map(|guard| *guard).unwrap_or(Mode::Chat);
    let initial_history_len = history.lock().map(|guard| guard.len()).unwrap_or(0);
    print_banner(initial_backend, initial_mode, initial_history_len);

    if cfg.pair_autostart {
        match start_pair_server(
            &cfg,
            Arc::clone(&latest_partner_input),
            Arc::clone(&pair_auth),
            Arc::clone(&pair_bubbles),
            pair_tx.clone(),
        ) {
            Ok(server) => {
                println!("pair server auto-started:");
                print_pair_server(&server);
                pair_server = Some(server);
                println!();
            }
            Err(error) => {
                println!("pair server auto-start failed: {error:#}");
                println!("CLIは継続します。必要なら /pair を再実行してください。");
                println!();
            }
        }
    }

    loop {
        let current_backend = backend
            .lock()
            .map(|guard| *guard)
            .unwrap_or(cfg.default_backend);
        let current_mode = mode.lock().map(|guard| *guard).unwrap_or(Mode::Chat);
        let line = match line_editor.readline(&repl_prompt(current_backend, current_mode)) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(error) => bail!("failed to read CLI input: {error}"),
        };

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let _ = line_editor.add_history_entry(input);

        if input == "/exit" || input == "/quit" {
            println!("檸檬烯姐、終了します。");
            break;
        }

        if input == "/help" {
            print_help(current_backend, current_mode, &cfg);
            continue;
        }

        if input == "/aa" {
            print_rimoko_aa();
            continue;
        }

        if input == "/auth" || input == "/onetime" {
            print_auth_code(&pair_auth, &cfg);
            continue;
        }

        if let Some(rest) = command_arg(input, "/model") {
            if rest.is_empty() {
                println!(
                    "current backend: {}",
                    backend.lock().map(|guard| guard.name()).unwrap_or("codex")
                );
                println!("available: mi25, openrouter, codex");
            } else if let Some(new_backend) = Backend::from_name(rest) {
                if new_backend == Backend::Mi25 && !cfg.mi25_enabled {
                    println!("MI25 backend is disabled.");
                    println!("ZUTTOMO_MI25_ENABLED=true にすると有効化できます。");
                } else {
                    if let Ok(mut guard) = backend.lock() {
                        *guard = new_backend;
                    }
                    println!("backend switched: {}", new_backend.name());
                }
            } else {
                println!("unknown backend: {rest}");
                println!("available: mi25, openrouter, codex");
            }
            continue;
        }

        if input == "/history" {
            if let Ok(guard) = history.lock() {
                print_history(&guard, 30);
            }
            continue;
        }

        if input == "/clear" {
            if let Ok(mut guard) = history.lock() {
                guard.clear();
            }
            println!("memory history cleared. history file was not deleted.");
            continue;
        }

        if input == "/prompt" {
            println!("{}", system_prompt(current_mode).trim());
            continue;
        }

        if let Some(rest) = command_arg(input, "/export") {
            let target = if rest.is_empty() || rest == "md" {
                "zuttomo-history.md"
            } else {
                rest
            };
            match history.lock() {
                Ok(guard) => match export_markdown(target, &guard) {
                    Ok(()) => println!("exported: {target}"),
                    Err(error) => println!("export failed: {error:#}"),
                },
                Err(_) => println!("export failed: history lock poisoned"),
            }
            continue;
        }

        if input == "/pair" {
            if let Some(server) = &pair_server {
                println!("pair server is already running:");
                print_pair_server(server);
            } else {
                match start_pair_server(
                    &cfg,
                    Arc::clone(&latest_partner_input),
                    Arc::clone(&pair_auth),
                    Arc::clone(&pair_bubbles),
                    pair_tx.clone(),
                ) {
                    Ok(server) => {
                        println!("請用你的手機輸入中文。");
                        println!("我這邊會自動翻譯成日文。");
                        print_pair_server(&server);
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
            if let Ok(mut guard) = mode.lock() {
                *guard = Mode::Chat;
            }
            if rest.is_empty() {
                println!("mode switched: {}", Mode::Chat.name());
            } else {
                if let Some(answer) =
                    execute_turn_shared(&cfg, &client, current_backend, &history, Mode::Chat, rest)
                {
                    push_pair_bubble(&pair_bubbles, "CLI話者", "cli", None, answer);
                }
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/question") {
            if let Ok(mut guard) = mode.lock() {
                *guard = Mode::Question;
            }
            if rest.is_empty() {
                println!("mode switched: {}", Mode::Question.name());
            } else {
                execute_turn_shared(
                    &cfg,
                    &client,
                    current_backend,
                    &history,
                    Mode::Question,
                    rest,
                );
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/tw") {
            if let Ok(mut guard) = mode.lock() {
                *guard = Mode::ToTaiwanMandarin;
            }
            if rest.is_empty() {
                println!("mode switched: {}", Mode::ToTaiwanMandarin.name());
            } else {
                if let Some(answer) = execute_turn_shared(
                    &cfg,
                    &client,
                    current_backend,
                    &history,
                    Mode::ToTaiwanMandarin,
                    rest,
                ) {
                    push_pair_bubble(&pair_bubbles, "CLI話者", "cli", None, answer);
                }
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/me") {
            if let Ok(mut guard) = mode.lock() {
                *guard = Mode::ToTaiwanMandarin;
            }
            if rest.is_empty() {
                println!("usage: /me TEXT");
            } else {
                if let Some(answer) = execute_turn_shared(
                    &cfg,
                    &client,
                    current_backend,
                    &history,
                    Mode::ToTaiwanMandarin,
                    rest,
                ) {
                    push_pair_bubble(&pair_bubbles, "CLI話者", "cli", None, answer);
                }
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/jp") {
            if let Ok(mut guard) = mode.lock() {
                *guard = Mode::ToJapanese;
            }
            if rest.is_empty() {
                println!("mode switched: {}", Mode::ToJapanese.name());
            } else {
                execute_turn_shared(
                    &cfg,
                    &client,
                    current_backend,
                    &history,
                    Mode::ToJapanese,
                    rest,
                );
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/you") {
            if let Ok(mut guard) = mode.lock() {
                *guard = Mode::ToJapanese;
            }
            if rest.is_empty() {
                let partner_input = latest_partner_input
                    .lock()
                    .ok()
                    .and_then(|latest| latest.clone());
                if let Some(partner_input) = partner_input {
                    execute_turn_shared(
                        &cfg,
                        &client,
                        current_backend,
                        &history,
                        Mode::ToJapanese,
                        &partner_input,
                    );
                } else {
                    println!("相手スマホ入力はまだありません。今は /you TEXT を使ってください。");
                }
            } else {
                execute_turn_shared(
                    &cfg,
                    &client,
                    current_backend,
                    &history,
                    Mode::ToJapanese,
                    rest,
                );
            }
            continue;
        }

        if let Some(rest) = command_arg(input, "/pobo") {
            if let Ok(mut guard) = mode.lock() {
                *guard = Mode::Pobo;
            }
            if rest.is_empty() {
                println!("mode switched: {}", Mode::Pobo.name());
            } else {
                execute_turn_shared(&cfg, &client, current_backend, &history, Mode::Pobo, rest);
            }
            continue;
        }

        if input.starts_with('/') {
            println!("unknown command: {input}");
            println!("type /help for commands");
            continue;
        }

        if let Some(answer) = execute_turn_shared(
            &cfg,
            &client,
            current_backend,
            &history,
            current_mode,
            input,
        ) {
            if should_publish_to_pair(current_mode) {
                push_pair_bubble(&pair_bubbles, "CLI話者", "cli", None, answer);
            }
        }
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

fn execute_turn_shared(
    cfg: &Config,
    client: &Client,
    backend: Backend,
    history: &Arc<Mutex<Vec<Message>>>,
    mode: Mode,
    input: &str,
) -> Option<String> {
    match run_turn_shared(cfg, client, backend, history, mode, input) {
        Ok(answer) => {
            println!();
            println!("{answer}");
            println!();
            Some(answer)
        }
        Err(error) => {
            print_backend_error(backend, &error);
            None
        }
    }
}

fn spawn_pair_translation_worker(
    cfg: Config,
    client: Client,
    backend: Arc<Mutex<Backend>>,
    mode: Arc<Mutex<Mode>>,
    history: Arc<Mutex<Vec<Message>>>,
    rx: mpsc::Receiver<PairInput>,
) {
    thread::spawn(move || {
        for input in rx {
            let backend_now = backend
                .lock()
                .map(|guard| *guard)
                .unwrap_or(cfg.default_backend);
            println!();
            println!("[pair:{}] 日本語訳:", input.speaker);
            execute_turn_shared(
                &cfg,
                &client,
                backend_now,
                &history,
                Mode::ToJapanese,
                &input.text,
            );
            let mode_now = mode.lock().map(|guard| *guard).unwrap_or(Mode::Chat);
            let _ = print_repl_prompt(backend_now, mode_now);
        }
    });
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

    #[test]
    fn form_onetime_code_requires_eight_digits() {
        assert_eq!(
            parse_form_onetime_code("code=01234567").as_deref(),
            Some("01234567")
        );
        assert_eq!(parse_form_onetime_code("code=1234567"), None);
        assert_eq!(parse_form_onetime_code("code=1234567x"), None);
    }

    #[test]
    fn auth_code_is_time_window_based_and_reusable() {
        let mut auth = OneTimeAuth::with_secret(vec![7; 32], 90, 3600, 8, 60);
        let code = auth.current_code_at(100);

        assert_eq!(
            auth.verify_code_at("00000000", "192.0.2.10", 101),
            OneTimeAuthResult::Invalid
        );

        let result = auth.verify_code_at(&code.value, "192.0.2.10", 102);
        assert!(matches!(result, OneTimeAuthResult::Success { .. }));
        let result = auth.verify_code_at(&code.value, "192.0.2.11", 103);
        assert!(matches!(result, OneTimeAuthResult::Success { .. }));

        assert_eq!(
            auth.verify_code_at(&code.value, "192.0.2.12", 180),
            OneTimeAuthResult::Invalid
        );
    }

    #[test]
    fn onetime_auth_sets_cookie_session() {
        let mut auth = OneTimeAuth::with_secret(vec![8; 32], 90, 3600, 8, 60);
        let code = auth.current_code_at(100);
        let result = auth.verify_code_at(&code.value, "192.0.2.10", 101);
        let OneTimeAuthResult::Success { session_id, .. } = result else {
            panic!("expected success");
        };

        let cookie = format!("theme=dark; {PAIR_SESSION_COOKIE}={session_id}");
        assert!(auth.is_session_authenticated_at(Some(&cookie), 102));
        assert!(!auth.is_session_authenticated_at(Some(&cookie), 3702));
    }

    #[test]
    fn onetime_auth_locks_after_repeated_failures() {
        let mut auth = OneTimeAuth::with_secret(vec![9; 32], 90, 3600, 2, 60);
        let code = auth.current_code_at(100);

        assert_eq!(
            auth.verify_code_at("11111111", "192.0.2.10", 101),
            OneTimeAuthResult::Invalid
        );
        assert_eq!(
            auth.verify_code_at("22222222", "192.0.2.10", 102),
            OneTimeAuthResult::Invalid
        );
        assert_eq!(
            auth.verify_code_at(&code.value, "192.0.2.10", 103),
            OneTimeAuthResult::Locked {
                retry_after_secs: 59
            }
        );
        assert!(matches!(
            auth.verify_code_at(&code.value, "192.0.2.10", 163),
            OneTimeAuthResult::Success { .. }
        ));
    }

    #[test]
    fn pair_bubbles_render_self_other_and_cli() {
        let bubbles = vec![
            PairBubble {
                speaker: "自分".to_string(),
                kind: "web".to_string(),
                owner_session: Some("session-a".to_string()),
                body: "我的問題".to_string(),
            },
            PairBubble {
                speaker: "自分".to_string(),
                kind: "web".to_string(),
                owner_session: Some("session-b".to_string()),
                body: "別人的問題".to_string(),
            },
            PairBubble {
                speaker: "CLI話者".to_string(),
                kind: "cli".to_string(),
                owner_session: None,
                body: "翻譯結果".to_string(),
            },
        ];

        let html = render_pair_bubbles(&bubbles, Some("session-a"));
        assert!(html.contains(r#"class="bubble self""#));
        assert!(html.contains(r#"class="bubble other""#));
        assert!(html.contains(r#"class="bubble cli""#));
    }

    #[test]
    fn pair_state_json_contains_rendered_fragments() {
        let bubbles = vec![PairBubble {
            speaker: "CLI話者".to_string(),
            kind: "cli".to_string(),
            owner_session: None,
            body: "翻譯結果".to_string(),
        }];

        let value: Value = serde_json::from_str(&pair_state_json(
            Some("最新輸入"),
            &bubbles,
            Some("session-a"),
        ))
        .unwrap();
        assert!(value["timeline_html"]
            .as_str()
            .unwrap()
            .contains(r#"class="bubble cli""#));
        assert_eq!(value["latest_html"], "最新輸入");
    }

    #[test]
    fn pair_speaker_label_uses_session_short_id() {
        assert_eq!(
            pair_speaker_label(Some("abcdef123456"), "192.0.2.10"),
            "相手 abcdef"
        );
        assert_eq!(pair_speaker_label(None, "192.0.2.10"), "相手 192.0.2.10");
    }

    #[test]
    fn pair_publish_modes_are_partner_visible_only() {
        assert!(should_publish_to_pair(Mode::Chat));
        assert!(should_publish_to_pair(Mode::ToTaiwanMandarin));
        assert!(!should_publish_to_pair(Mode::Question));
        assert!(!should_publish_to_pair(Mode::ToJapanese));
        assert!(!should_publish_to_pair(Mode::Pobo));
    }

    #[test]
    fn pair_url_adds_token_path() {
        assert_eq!(
            pair_url("https://pair.example.com/", "abc123"),
            "https://pair.example.com"
        );
        assert_eq!(
            pair_url("https://pair.example.com/{token}", "abc123"),
            "https://pair.example.com/abc123"
        );
    }

    #[test]
    fn request_method_path_ignores_query() {
        assert_eq!(
            request_method_path("GET /t/abc123?x=1 HTTP/1.1"),
            Some(("GET", "/t/abc123"))
        );
    }

    #[test]
    fn pair_pages_use_stable_tokenless_form_paths() {
        let page = pair_form_page(
            None,
            &[],
            Some("session-a"),
            "https://example.com",
            "abc123",
        );
        assert!(page.contains(r#"action="/submit""#));
        assert!(page.contains(r#"const stateUrl = "/state";"#));

        let auth_page = pair_auth_page("abc123", None);
        assert!(auth_page.contains(r#"action="/auth""#));
    }
}
