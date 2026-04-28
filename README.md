# PR Slack Notifier

GitHub Enterprise 조직의 열린 PR 리뷰어들에게 Slack 알림을 보내는 Rust 프로그램입니다.

## 기능

- 여러 조직의 모든 열린 PR 조회 (draft 제외)
- 리뷰 미완료 리뷰어에게 Slack 리뷰 요청 알림 발송
- Assignee에게 리뷰어 알림 발송 안내 DM 전송
- Bot Token으로 `conversations.open` → `chat.postMessage` API를 통해 개인 DM 전송
- Block Kit 기반 깔끔한 메시지 포맷
- dry-run 모드로 실제 전송 없이 미리보기
- 알림 전송 전 리뷰어별 PR 내용을 확인하고 전송 여부를 선택 (기본 동작)
- `--auto-send` 옵션으로 확인 없이 자동 전송
- `--verbose` 옵션으로 상세 로그 출력

## 사전 요구사항

- Rust toolchain (1.85+)
- GitHub Personal Access Token (repo 읽기 권한)
- Slack Bot Token

## 빌드

```bash
# 릴리스 빌드 (strip + LTO 최적화 적용)
make build

# 린트 + 포맷 + 빌드 검증 (CI용)
make check

# /usr/local/bin에 설치
sudo make install
```

## 설정

```bash
# config-sample.json 을 복사하여 config.json을 생성합니다.
cp config-sample.json config.json
```

`config.json` 필드 설명:

| 필드 | 설명 |
|---|---|
| `GITHUB_API_URL` | GitHub Enterprise API base URL |
| `GITHUB_ORGS` | GitHub 조직명 (문자열 또는 배열) |
| `GITHUB_TOKEN` | GitHub Personal Access Token |
| `SLACK_BOT_TOKEN` | Slack Bot Token |
| `REMINDER_HOURS` | 리마인더 기준 시간 (예: `48` → 48시간 이상 경과된 PR만 알림, 미설정 시 전체) |
| `USER_MAPPING` | GitHub username → Slack user ID 매핑 |

### 환경변수

환경변수가 설정되어 있으면 `config.json`보다 우선합니다.

```bash
export GITHUB_TOKEN="ghp_xxxxx"
export SLACK_BOT_TOKEN="xoxb-xxxxx"
```

### Slack Bot 설정

Slack App에 다음 권한이 필요합니다:

- `chat:write` - 메시지 전송
- `im:write` - DM 전송

### USER_MAPPING 설정

GitHub username과 Slack user ID를 매핑합니다.

Slack user ID 확인 방법:

- Slack에서 사용자 프로필 클릭 → 더보기(⋯) → **멤버 ID 복사** (`U01ABCDEF` 형식)

```json
{
    "USER_MAPPING": {
        "hong-gildong": "U01ABCDEF",
        "kim-cheolsu": "U02GHIJKL"
    }
}
```

## 사용법

```bash
# 기본 실행
make run
# 또는
./target/release/pr-slack-notifier

# dry-run 모드 (실제 전송 없이 미리보기)
make dry-run
# 또는
pr-slack-notifier --dry-run

# 확인 없이 자동 전송
pr-slack-notifier --auto-send

# 설정 파일 지정
pr-slack-notifier --config /path/to/config.json

# 옵션 조합
pr-slack-notifier --dry-run --config ./my-config.json

# 버전 확인
pr-slack-notifier --version
```

## Docker

```bash
# 빌드
make docker

# 실행
docker run --rm \
  -v $(pwd)/config.json:/config.json \
  pr-slack-notifier --config /config.json

# 환경변수로 실행
docker run --rm \
  -e GITHUB_TOKEN="ghp_xxxxx" \
  -v $(pwd)/config.json:/config.json \
  pr-slack-notifier --config /config.json
```

## 메시지 예시

알림 메시지는 Block Kit 형식으로 전송됩니다:

리뷰어에게 보내는 메시지:

```text
📬 PR 리뷰 요청

@hong-gildong님, 리뷰가 필요한 PR이 2건 있습니다.

────────────────
my-api#42: API 응답 캐싱 추가
👀 Reviewer

my-sdk#15: SDK v2 마이그레이션
👀 Reviewer
────────────────
🤖 PR Notifier | 리뷰 요청 알림
```

Assignee에게 보내는 메시지:

```text
📣 PR 리뷰 알림 발송 안내

@kim-cheolsu님, 아래 PR의 리뷰어들에게 리뷰 요청 알림을 보냈습니다.

my-api#42: API 응답 캐싱 추가
📨 알림 대상: @hong-gildong, @lee-younghee
────────────────
🤖 PR Notifier | 리뷰 알림 발송 안내
```

## 프레젠테이션

[presenterm](https://github.com/mfontanini/presenterm)으로 프로젝트 소개 슬라이드를 볼 수 있습니다.

```bash
presenterm -X about_this.md
```

## cron 설정 예시

평일(월~금) 업무 시간(09:00 ~ 20:30)에만 30분마다 자동 전송하고 로그를 파일에 남기려면:

```bash
# crontab -e
0/30 9-20 * * 1-5 /Users/ysoftman/workspace/pr-slack-notifier/target/debug/pr-slack-notifier --auto-send --config /Users/ysoftman/workspace/pr-slack-notifier/config.json 2>> /Users/ysoftman/workspace/pr-slack-notifier/send_log.log
```
