use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use colored::Colorize;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

// ── CLI ──

#[derive(Parser)]
#[command(
    version,
    about = "GitHub Enterprise의 열린 PR 담당자들에게 Slack 알림을 보냅니다."
)]
struct Cli {
    /// 실제 전송 없이 미리보기
    #[arg(long)]
    dry_run: bool,

    /// 확인 없이 모든 알림을 자동 전송
    #[arg(long)]
    auto_send: bool,

    /// 설정 파일 경로 (기본: config.json)
    #[arg(long, default_value = "config.json")]
    config: PathBuf,
}

// ── Types ──

#[derive(Clone, Copy)]
enum Role {
    Assignee,
    Reviewer,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::Assignee => "👤 Assignee",
            Self::Reviewer => "👀 Reviewer",
        }
    }
}

#[derive(Clone)]
struct PrInfo {
    number: u64,
    title: String,
    url: String,
    repo: String,
    role: Role,
    created_at: Option<DateTime<Utc>>,
}

// ── Config ──

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
struct FileConfig {
    github_api_url: String,
    github_org: String,
    github_token: Option<String>,
    slack_bot_token: Option<String>,
    reminder_hours: Option<u64>,
    user_mapping: Option<HashMap<String, String>>,
}

struct AppConfig {
    github_api_url: String,
    github_org: String,
    github_token: String,
    slack_bot_token: String,
    reminder_hours: Option<u64>,
    user_mapping: HashMap<String, String>,
}

impl AppConfig {
    fn load(cli: &Cli) -> Result<Self> {
        let content = std::fs::read_to_string(&cli.config).with_context(|| {
            format!(
                "설정 파일을 찾을 수 없습니다: {}\nconfig.json.example을 참고하여 config.json을 생성하세요.",
                cli.config.display()
            )
        })?;

        let fc: FileConfig = serde_json::from_str(&content).with_context(|| {
            format!(
                "설정 파일이 유효한 JSON이 아닙니다: {}",
                cli.config.display()
            )
        })?;

        let github_token = env_or("GITHUB_TOKEN", fc.github_token);
        let slack_bot_token = env_or("SLACK_BOT_TOKEN", fc.slack_bot_token);

        if fc.github_api_url.is_empty() {
            bail!("GITHUB_API_URL이 설정되지 않았습니다.");
        }
        if fc.github_org.is_empty() {
            bail!("GITHUB_ORG가 설정되지 않았습니다.");
        }
        if github_token.is_empty() {
            bail!("GITHUB_TOKEN이 설정되지 않았습니다. 환경변수 또는 config.json에 설정하세요.");
        }
        if slack_bot_token.is_empty() {
            bail!("SLACK_BOT_TOKEN이 설정되지 않았습니다. 환경변수 또는 config.json에 설정하세요.");
        }

        Ok(AppConfig {
            github_api_url: fc.github_api_url.trim_end_matches('/').to_string(),
            github_org: fc.github_org,
            github_token,
            slack_bot_token,
            reminder_hours: fc.reminder_hours,
            user_mapping: fc.user_mapping.unwrap_or_default(),
        })
    }
}

fn format_elapsed(now: DateTime<Utc>, created: DateTime<Utc>) -> String {
    let hours = (now - created).num_hours();
    if hours < 24 {
        format!(" | ⏰ {hours}시간 전")
    } else {
        format!(" | ⏰ {}일 전", hours / 24)
    }
}

fn env_or(key: &str, fallback: Option<String>) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .or(fallback)
        .unwrap_or_default()
}

// ── Logging ──

fn log_info(msg: &str) {
    eprintln!("{} {msg}", "[INFO]".green());
}

fn log_warn(msg: &str) {
    eprintln!("{} {msg}", "[WARN]".yellow());
}

fn log_error(msg: &str) {
    eprintln!("{} {msg}", "[ERROR]".red());
}

fn log_debug(msg: &str) {
    eprintln!("{} {msg}", "[DEBUG]".blue());
}

// ── App ──

const SLACK_API_BASE: &str = "https://slack.com/api";

struct App {
    cfg: AppConfig,
    dry_run: bool,
    auto_send: bool,
    client: reqwest::blocking::Client,
}

impl App {
    fn new(cfg: AppConfig, dry_run: bool, auto_send: bool) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("pr-slack-notifier")
            .build()
            .context("HTTP 클라이언트 생성 실패")?;

        Ok(App {
            cfg,
            dry_run,
            auto_send,
            client,
        })
    }

    fn run(&self) -> Result<()> {
        let prs = self.fetch_open_prs()?;

        if prs.is_empty() {
            log_info("열린 PR이 없습니다. 종료합니다.");
            return Ok(());
        }

        if let Some(hours) = self.cfg.reminder_hours {
            log_info(&format!("리마인더 모드: {hours}시간 이상 경과된 PR만 알림"));
        }

        log_info("PR별 담당자 정보를 수집합니다...");
        let user_map = self.build_user_pr_map(&prs);

        if user_map.is_empty() {
            log_info("담당자가 지정된 PR이 없습니다. 종료합니다.");
            return Ok(());
        }

        log_info(&format!("알림 대상: {}명", user_map.len()));
        self.send_notifications(&user_map)
    }

    // ── GitHub API ──

    fn github_api(&self, endpoint: &str) -> Result<serde_json::Value> {
        let url = format!("{}{endpoint}", self.cfg.github_api_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.cfg.github_token)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .with_context(|| format!("GitHub API 요청 실패: {url}"))?;

        let status = resp.status();
        let body = resp.text().context("GitHub API 응답 읽기 실패")?;

        if !status.is_success() {
            bail!("GitHub API 요청 실패 (HTTP {status}): {url}\n{body}");
        }

        serde_json::from_str(&body).context("GitHub API 응답 JSON 파싱 실패")
    }

    fn fetch_open_prs(&self) -> Result<Vec<serde_json::Value>> {
        log_info("조직의 열린 PR을 조회합니다...");

        let mut all_prs: Vec<serde_json::Value> = Vec::new();
        let max_pages = 10; // GitHub search API는 최대 1000건 (100 x 10)
        let query = format!("org:{}+type:pr+state:open+draft:false", self.cfg.github_org);

        for page in 1..=max_pages {
            let response = self.github_api(&format!(
                "/search/issues?q={query}&per_page=100&page={page}"
            ))?;

            let items = response["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                break;
            }

            all_prs.extend(items);

            let total = response["total_count"].as_u64().unwrap_or(0);
            if all_prs.len() as u64 >= total {
                break;
            }
        }

        log_info(&format!("열린 PR {}개 조회 완료", all_prs.len()));
        Ok(all_prs)
    }

    // ── User-PR mapping ──

    fn build_user_pr_map(&self, prs: &[serde_json::Value]) -> HashMap<String, Vec<PrInfo>> {
        let mut user_map: HashMap<String, Vec<PrInfo>> = HashMap::new();
        let now = Utc::now();

        for pr in prs {
            let number = pr["number"].as_u64().unwrap_or(0);
            let title = pr["title"].as_str().unwrap_or("").to_string();
            let html_url = pr["html_url"].as_str().unwrap_or("").to_string();
            let repo = html_url.rsplit('/').nth(2).unwrap_or("").to_string();

            let pr_detail = pr["pull_request"]["url"].as_str().and_then(|api_url| {
                let path = api_url.strip_prefix(&self.cfg.github_api_url)?;
                match self.github_api(path) {
                    Ok(detail) => Some(detail),
                    Err(e) => {
                        log_warn(&format!("PR #{number} 상세 조회 실패: {e:#}"));
                        None
                    }
                }
            });

            let created_at = pr["created_at"]
                .as_str()
                .and_then(|s| s.parse::<DateTime<Utc>>().ok());

            // reminder_hours 설정 시, 해당 시간 미만인 PR은 건너뜀
            if let (Some(hours), Some(created)) = (self.cfg.reminder_hours, created_at) {
                let elapsed_hours = (now - created).num_hours();
                if elapsed_hours < hours as i64 {
                    continue;
                }
            }

            let base = PrInfo {
                number,
                title,
                url: html_url,
                repo,
                role: Role::Assignee, // placeholder, overwritten below
                created_at,
            };

            let mut add_users = |arr: &[serde_json::Value], role: Role| {
                for item in arr {
                    if let Some(login) = item["login"].as_str() {
                        let mut info = base.clone();
                        info.role = role;
                        user_map.entry(login.to_string()).or_default().push(info);
                    }
                }
            };

            // Assignees (prefer PR detail, fallback to search result)
            if let Some(arr) = pr_detail
                .as_ref()
                .and_then(|d| d["assignees"].as_array())
                .or_else(|| pr["assignees"].as_array())
            {
                add_users(arr, Role::Assignee);
            }

            // Requested reviewers (PR detail only)
            if let Some(arr) = pr_detail
                .as_ref()
                .and_then(|d| d["requested_reviewers"].as_array())
            {
                add_users(arr, Role::Reviewer);
            }
        }

        user_map
    }

    // ── Slack ──

    fn build_slack_blocks(&self, github_user: &str, prs: &[PrInfo]) -> serde_json::Value {
        let mention = match self.cfg.user_mapping.get(github_user) {
            Some(id) => format!("<@{id}>"),
            None => format!("*{github_user}*"),
        };

        let mut blocks = vec![
            serde_json::json!({
                "type": "header",
                "text": {"type": "plain_text", "text": "📬 PR 리뷰/처리 요청", "emoji": true}
            }),
            serde_json::json!({
                "type": "section",
                "text": {"type": "mrkdwn", "text": format!("{mention}님, 확인이 필요한 PR이 *{}건* 있습니다.", prs.len())}
            }),
            serde_json::json!({"type": "divider"}),
        ];

        let now = Utc::now();
        for pr in prs {
            let elapsed = pr
                .created_at
                .map(|c| format_elapsed(now, c))
                .unwrap_or_default();
            blocks.push(serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("*<{}|{}#{}: {}>*\n{}{}", pr.url, pr.repo, pr.number, pr.title, pr.role.label(), elapsed)
                }
            }));
        }

        blocks.push(serde_json::json!({"type": "divider"}));
        blocks.push(serde_json::json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": "🤖 PR Notifier | 열린 PR 알림"}]
        }));

        serde_json::Value::Array(blocks)
    }

    fn send_bot_dm(
        &self,
        slack_user_id: &str,
        blocks: &serde_json::Value,
        text: &str,
    ) -> Result<()> {
        if self.dry_run {
            log_debug(&format!("[DRY-RUN] Bot DM 전송 대상: {slack_user_id}"));
            let payload = serde_json::json!({"text": text, "blocks": blocks});
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(());
        }

        // Open DM channel
        let channel_resp: serde_json::Value = self
            .client
            .post(format!("{SLACK_API_BASE}/conversations.open"))
            .bearer_auth(&self.cfg.slack_bot_token)
            .json(&serde_json::json!({"users": slack_user_id}))
            .send()
            .context("Slack DM 채널 열기 실패")?
            .json()
            .context("Slack 응답 파싱 실패")?;

        if channel_resp["ok"].as_bool() != Some(true) {
            bail!(
                "Slack DM 채널 열기 실패: {}",
                channel_resp["error"].as_str().unwrap_or("unknown")
            );
        }

        let channel_id = channel_resp["channel"]["id"]
            .as_str()
            .context("Slack 채널 ID를 찾을 수 없습니다")?;

        // Send message
        let send_resp: serde_json::Value = self
            .client
            .post(format!("{SLACK_API_BASE}/chat.postMessage"))
            .bearer_auth(&self.cfg.slack_bot_token)
            .json(&serde_json::json!({
                "channel": channel_id,
                "text": text,
                "blocks": blocks
            }))
            .send()
            .context("Slack DM 전송 실패")?
            .json()
            .context("Slack 응답 파싱 실패")?;

        if send_resp["ok"].as_bool() != Some(true) {
            bail!(
                "Slack DM 전송 실패: {}",
                send_resp["error"].as_str().unwrap_or("unknown")
            );
        }

        log_info(&format!("Slack DM 전송 성공: {slack_user_id}"));
        Ok(())
    }

    fn print_pr_summary(&self, github_user: &str, prs: &[PrInfo]) {
        let now = Utc::now();
        eprintln!(
            "\n{}",
            format!("── {github_user} ({} PRs) ──", prs.len()).cyan()
        );
        for pr in prs {
            let elapsed = pr
                .created_at
                .map(|c| format_elapsed(now, c))
                .unwrap_or_default();
            eprintln!(
                "  {} {}#{}: {}{}",
                pr.role.label(),
                pr.repo,
                pr.number,
                pr.title,
                elapsed
            );
            eprintln!("    {}", pr.url.dimmed());
        }
    }

    fn ask_confirm(prompt: &str) -> bool {
        eprint!("{prompt} (yes/no): ");
        io::stderr().flush().ok();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return false;
        }
        matches!(input.trim().to_ascii_lowercase().as_str(), "yes" | "y")
    }

    fn send_notifications(&self, user_map: &HashMap<String, Vec<PrInfo>>) -> Result<()> {
        let mut sent = 0u32;
        let mut skipped = 0u32;
        let mut failed = 0u32;

        let mut users: Vec<&String> = user_map.keys().collect();
        users.sort();

        for github_user in users {
            let prs = &user_map[github_user];
            let slack_id = self.cfg.user_mapping.get(github_user.as_str());

            self.print_pr_summary(github_user, prs);

            if slack_id.is_none() {
                log_warn(&format!(
                    "GitHub 사용자 '{github_user}'의 Slack ID 매핑이 없습니다. 건너뜁니다."
                ));
                failed += 1;
                continue;
            }

            if !self.auto_send
                && !Self::ask_confirm(&format!("  → {github_user}에게 알림을 보내시겠습니까?"))
            {
                log_info(&format!("{github_user} 알림 건너뜀"));
                skipped += 1;
                continue;
            }

            let blocks = self.build_slack_blocks(github_user, prs);
            let text = format!("확인이 필요한 PR이 {}건 있습니다.", prs.len());

            match self.send_bot_dm(slack_id.unwrap(), &blocks, &text) {
                Ok(()) => sent += 1,
                Err(e) => {
                    log_error(&format!("{e:#}"));
                    failed += 1;
                }
            }
        }

        eprintln!();
        log_info("=== 알림 전송 완료 ===");
        log_info(&format!(
            "성공: {sent}건, 건너뜀: {skipped}건, 실패: {failed}건"
        ));

        if sent == 0 && failed > 0 {
            bail!("모든 알림 전송에 실패했습니다.");
        }
        Ok(())
    }
}

// ── Main ──

fn main() {
    if let Err(e) = run() {
        log_error(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.dry_run {
        log_warn("DRY-RUN 모드: 실제 Slack 메시지를 전송하지 않습니다.");
    }

    let cfg = AppConfig::load(&cli)?;
    log_info("설정 로드 완료");

    let app = App::new(cfg, cli.dry_run, cli.auto_send)?;
    app.run()
}
