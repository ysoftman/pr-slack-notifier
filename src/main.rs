use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use colored::Colorize;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static VERBOSE: AtomicBool = AtomicBool::new(false);

// ── CLI ──

#[derive(Parser)]
#[command(
    version,
    about = "GitHub Enterprise의 열린 PR 리뷰어들에게 Slack 알림을 보냅니다."
)]
struct Cli {
    /// 실제 전송 없이 미리보기
    #[arg(long)]
    dry_run: bool,

    /// 확인 없이 모든 알림을 자동 전송
    #[arg(long)]
    auto_send: bool,

    /// 상세 로그 출력
    #[arg(long, short)]
    verbose: bool,

    /// 설정 파일 경로 (기본: config.json)
    #[arg(long, default_value = "config.json")]
    config: PathBuf,
}

// ── Types ──

#[derive(Clone)]
struct PrInfo {
    number: u64,
    title: String,
    url: String,
    repo: String,
    created_at: Option<DateTime<Utc>>,
    reviewers: Vec<String>,
    assignees: Vec<String>,
}

// ── Config ──

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
struct FileConfig {
    github_api_url: String,
    github_orgs: OneOrMany,
    github_token: Option<String>,
    slack_bot_token: Option<String>,
    reminder_hours: Option<u64>,
    user_mapping: Option<HashMap<String, String>>,
}

struct AppConfig {
    github_api_url: String,
    github_orgs: Vec<String>,
    github_token: String,
    slack_bot_token: String,
    reminder_hours: Option<u64>,
    user_mapping: HashMap<String, String>,
}

impl AppConfig {
    fn load(cli: &Cli) -> Result<Self> {
        let content = std::fs::read_to_string(&cli.config).with_context(|| {
            format!(
                "설정 파일을 찾을 수 없습니다: {}\nconfig-sample.json을 참고하여 config.json을 생성하세요.",
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

        let github_orgs: Vec<String> = fc
            .github_orgs
            .into_vec()
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();

        if fc.github_api_url.is_empty() {
            bail!("GITHUB_API_URL이 설정되지 않았습니다.");
        }
        if github_orgs.is_empty() {
            bail!("GITHUB_ORGS가 설정되지 않았습니다.");
        }
        if github_token.is_empty() {
            bail!("GITHUB_TOKEN이 설정되지 않았습니다. 환경변수 또는 config.json에 설정하세요.");
        }
        if slack_bot_token.is_empty() {
            bail!("SLACK_BOT_TOKEN이 설정되지 않았습니다. 환경변수 또는 config.json에 설정하세요.");
        }

        Ok(AppConfig {
            github_api_url: fc.github_api_url.trim_end_matches('/').to_string(),
            github_orgs,
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
    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("{} {msg}", "[DEBUG]".blue());
    }
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
        let raw_prs = self.fetch_open_prs()?;

        if raw_prs.is_empty() {
            log_info("열린 PR이 없습니다. 종료합니다.");
            return Ok(());
        }

        if let Some(hours) = self.cfg.reminder_hours {
            log_info(&format!("리마인더 모드: {hours}시간 이상 경과된 PR만 알림"));
        }

        log_info("PR별 리뷰어 정보를 수집합니다...");
        let pr_infos = self.build_pr_infos(&raw_prs);

        if pr_infos.is_empty() {
            log_info("리뷰 대기 중인 리뷰어가 없습니다. 종료합니다.");
            return Ok(());
        }

        self.send_notifications(&pr_infos)
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

        log_debug(&format!("GitHub API: {endpoint} → HTTP {status}"));

        if !status.is_success() {
            bail!("GitHub API 요청 실패 (HTTP {status}): {url}\n{body}");
        }

        serde_json::from_str(&body).context("GitHub API 응답 JSON 파싱 실패")
    }

    fn fetch_org_repos(&self, org: &str) -> Result<Vec<String>> {
        let mut repos = Vec::new();
        for page in 1..=10 {
            let endpoint = format!("/orgs/{org}/repos?per_page=100&page={page}");
            let response = self.github_api(&endpoint)?;
            let items = response.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                break;
            }
            for repo in &items {
                if let Some(name) = repo["name"].as_str() {
                    repos.push(name.to_string());
                }
            }
            if items.len() < 100 {
                break;
            }
        }
        Ok(repos)
    }

    fn fetch_open_prs(&self) -> Result<Vec<serde_json::Value>> {
        let mut all_prs: Vec<serde_json::Value> = Vec::new();

        for org in &self.cfg.github_orgs {
            log_info(&format!("[{org}] 레포지토리 목록을 조회합니다..."));
            let repos = self.fetch_org_repos(org)?;
            log_info(&format!("[{org}] 레포지토리 {}개 발견", repos.len()));

            for repo_name in &repos {
                for page in 1..=10 {
                    let endpoint = format!(
                        "/repos/{org}/{repo_name}/pulls?state=open&per_page=100&page={page}"
                    );
                    let response = match self.github_api(&endpoint) {
                        Ok(r) => r,
                        Err(e) => {
                            log_warn(&format!("{org}/{repo_name} PR 조회 실패: {e:#}"));
                            break;
                        }
                    };

                    let items = response.as_array().cloned().unwrap_or_default();
                    let count = items.len();
                    if count == 0 {
                        break;
                    }

                    let non_draft: Vec<_> = items
                        .into_iter()
                        .filter(|pr| pr["draft"].as_bool() != Some(true))
                        .collect();
                    all_prs.extend(non_draft);

                    if count < 100 {
                        break;
                    }
                }
            }
        }

        log_info(&format!("열린 PR {}개 조회 완료", all_prs.len()));
        Ok(all_prs)
    }

    // ── PR info collection ──

    fn build_pr_infos(&self, prs: &[serde_json::Value]) -> Vec<PrInfo> {
        let now = Utc::now();
        let mut result = Vec::new();

        for pr in prs {
            let number = pr["number"].as_u64().unwrap_or(0);
            let title = pr["title"].as_str().unwrap_or("").to_string();
            let html_url = pr["html_url"].as_str().unwrap_or("").to_string();
            let repo = html_url.rsplit('/').nth(2).unwrap_or("").to_string();

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

            let reviewers: Vec<String> = pr["requested_reviewers"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| r["login"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            if reviewers.is_empty() {
                continue;
            }

            let assignees: Vec<String> = pr["assignees"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a["login"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            result.push(PrInfo {
                number,
                title,
                url: html_url,
                repo,
                created_at,
                reviewers,
                assignees,
            });
        }

        result
    }

    // ── Slack ──

    fn mention_for(&self, github_user: &str) -> String {
        match self.cfg.user_mapping.get(github_user) {
            Some(id) => format!("<@{id}>"),
            None => format!("*{github_user}*"),
        }
    }

    fn build_reviewer_blocks(&self, github_user: &str, prs: &[&PrInfo]) -> serde_json::Value {
        let mention = self.mention_for(github_user);

        let mut blocks = vec![
            serde_json::json!({
                "type": "header",
                "text": {"type": "plain_text", "text": "📬 PR 리뷰 요청", "emoji": true}
            }),
            serde_json::json!({
                "type": "section",
                "text": {"type": "mrkdwn", "text": format!("{mention}님, 리뷰가 필요한 PR이 *{}건* 있습니다.", prs.len())}
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
                    "text": format!("*<{}|{}#{}: {}>*\n👀 Reviewer{}", pr.url, pr.repo, pr.number, pr.title, elapsed)
                }
            }));
        }

        blocks.push(serde_json::json!({"type": "divider"}));
        blocks.push(serde_json::json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": "🤖 PR Notifier | 리뷰 요청 알림"}]
        }));

        serde_json::Value::Array(blocks)
    }

    fn build_assignee_blocks(
        &self,
        github_user: &str,
        pr: &PrInfo,
        notified_reviewers: &[&str],
    ) -> serde_json::Value {
        let mention = self.mention_for(github_user);
        let reviewer_mentions: Vec<String> = notified_reviewers
            .iter()
            .map(|r| self.mention_for(r))
            .collect();

        let now = Utc::now();
        let elapsed = pr
            .created_at
            .map(|c| format_elapsed(now, c))
            .unwrap_or_default();

        let blocks = vec![
            serde_json::json!({
                "type": "header",
                "text": {"type": "plain_text", "text": "📣 PR 리뷰 알림 발송 안내", "emoji": true}
            }),
            serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        "{mention}님, 아래 PR의 리뷰어들에게 리뷰 요청 알림을 보냈습니다.\n\n*<{}|{}#{}: {}>*{}",
                        pr.url, pr.repo, pr.number, pr.title, elapsed
                    )
                }
            }),
            serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("📨 알림 대상: {}", reviewer_mentions.join(", "))
                }
            }),
            serde_json::json!({"type": "divider"}),
            serde_json::json!({
                "type": "context",
                "elements": [{"type": "mrkdwn", "text": "🤖 PR Notifier | 리뷰 알림 발송 안내"}]
            }),
        ];

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

    fn ask_confirm(prompt: &str) -> bool {
        eprint!("{prompt} (yes/no): ");
        io::stderr().flush().ok();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return false;
        }
        matches!(input.trim().to_ascii_lowercase().as_str(), "yes" | "y")
    }

    fn send_notifications(&self, pr_infos: &[PrInfo]) -> Result<()> {
        // 리뷰어별 PR 그룹핑
        let mut reviewer_prs: HashMap<String, Vec<&PrInfo>> = HashMap::new();
        for pr in pr_infos {
            for reviewer in &pr.reviewers {
                reviewer_prs.entry(reviewer.clone()).or_default().push(pr);
            }
        }

        let mut sent = 0u32;
        let mut skipped = 0u32;
        let mut failed = 0u32;

        // 1) 리뷰어에게 리뷰 요청 알림
        log_info(&format!("리뷰어 알림 대상: {}명", reviewer_prs.len()));
        let mut reviewers: Vec<&String> = reviewer_prs.keys().collect();
        reviewers.sort();

        // PR별 실제 알림 보낸 리뷰어 추적
        let mut notified_reviewers_per_pr: HashMap<u64, Vec<String>> = HashMap::new();

        for reviewer in &reviewers {
            let prs = &reviewer_prs[*reviewer];
            let slack_id = self.cfg.user_mapping.get(reviewer.as_str());

            let now = Utc::now();
            eprintln!(
                "\n{}",
                format!("── {reviewer} ({} PRs) ──", prs.len()).cyan()
            );
            for pr in prs {
                let elapsed = pr
                    .created_at
                    .map(|c| format_elapsed(now, c))
                    .unwrap_or_default();
                eprintln!(
                    "  👀 Reviewer {}#{}: {}{}",
                    pr.repo, pr.number, pr.title, elapsed
                );
                eprintln!("    {}", pr.url.dimmed());
            }

            if slack_id.is_none() {
                log_warn(&format!(
                    "GitHub 사용자 '{reviewer}'의 Slack ID 매핑이 없습니다. 건너뜁니다."
                ));
                failed += 1;
                continue;
            }

            if !self.auto_send
                && !Self::ask_confirm(&format!("  → {reviewer}에게 알림을 보내시겠습니까?"))
            {
                log_info(&format!("{reviewer} 알림 건너뜀"));
                skipped += 1;
                continue;
            }

            let blocks = self.build_reviewer_blocks(reviewer, prs);
            let text = format!("리뷰가 필요한 PR이 {}건 있습니다.", prs.len());

            match self.send_bot_dm(slack_id.unwrap(), &blocks, &text) {
                Ok(()) => {
                    sent += 1;
                    for pr in prs {
                        notified_reviewers_per_pr
                            .entry(pr.number)
                            .or_default()
                            .push(reviewer.to_string());
                    }
                }
                Err(e) => {
                    log_error(&format!("{e:#}"));
                    failed += 1;
                }
            }
        }

        // 2) Assignee에게 리뷰어 알림 발송 안내
        let mut assignee_sent = 0u32;
        for pr in pr_infos {
            let notified: Vec<&str> = notified_reviewers_per_pr
                .get(&pr.number)
                .map(|v| v.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default();

            if notified.is_empty() {
                continue;
            }

            for assignee in &pr.assignees {
                let slack_id = match self.cfg.user_mapping.get(assignee.as_str()) {
                    Some(id) => id,
                    None => continue,
                };

                let blocks = self.build_assignee_blocks(assignee, pr, &notified);
                let text = format!(
                    "{}#{} PR의 리뷰어 {}명에게 알림을 보냈습니다.",
                    pr.repo,
                    pr.number,
                    notified.len()
                );

                match self.send_bot_dm(slack_id, &blocks, &text) {
                    Ok(()) => assignee_sent += 1,
                    Err(e) => log_error(&format!("Assignee {assignee} 알림 실패: {e:#}")),
                }
            }
        }

        eprintln!();
        log_info("=== 알림 전송 완료 ===");
        log_info(&format!(
            "리뷰어: 성공 {sent}건, 건너뜀 {skipped}건, 실패 {failed}건"
        ));
        log_info(&format!("Assignee 안내: {assignee_sent}건"));

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

    VERBOSE.store(cli.verbose, Ordering::Relaxed);

    if cli.dry_run {
        log_warn("DRY-RUN 모드: 실제 Slack 메시지를 전송하지 않습니다.");
    }

    let cfg = AppConfig::load(&cli)?;
    log_info("설정 로드 완료");

    let app = App::new(cfg, cli.dry_run, cli.auto_send)?;
    app.run()
}
