<p align="center">
  <img src="assets/dume_logo.jpg" alt="DUM-E Mascot" width="160" height="160" />
</p>

<h1 align="center">D U M - E</h1>

<p align="center">
  <strong>Clean-Engine Autonomous Multi-Agent Harness (Native Rust)</strong>
  <br/>
  <sub>100% standalone native Rust executable with zero Node.js, Bun, or Python runtime dependencies.</sub>
</p>

<p align="center">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-2024%20Edition-red?style=flat-square">
  <img alt="Ratatui" src="https://img.shields.io/badge/TUI-Ratatui-blue?style=flat-square">
  <img alt="SQLite WAL" src="https://img.shields.io/badge/SQLite-WAL-orange?style=flat-square">
  <img alt="License" src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-green?style=flat-square">
</p>

---

## DUM-E Mission

**DUM-E** is an autonomous multi-agent coding harness built from scratch in pure Rust for long-running, fault-tolerant missions:

1. **Zero Legacy Overhead**: Clean Rust modular crates (`dume-core`, `dume-store`, `dume-git`, `dume-worker`, `dume-mcp`, `dume-provider`, `dume-tui`, `dume-cli`).
2. **Native Distribution**: Single standalone binary without Node or Bun runtime dependencies.
3. **Crash & Restart Recovery**: Preserves goals, attempts, and execution logs in an SQLite WAL store (`~/.dume/rust/harness.db`).
4. **Epoch Fencing Guard**: Strictly invalidates stale or zombie worker submissions, preventing race conditions or stale overwrites.
5. **DAG Task Orchestration**: Dynamically schedules multi-agent tasks respecting strict dependency graphs.
6. **Independent Verification**: Worker submissions are never auto-accepted. Independent acceptance criteria and modified path whitelists are verified in detached git worktrees before serialized integration.
7. **Hash-Addressed Blob Storage**: Offloads massive logs and changed blobs to content-addressable storage, avoiding prompt token exhaustion.

---

## Installation & Setup

### Install a Prebuilt Release

Download and run the native installer script:

```bash
curl -fsSL https://raw.githubusercontent.com/jeongminsang/dum-e/main/scripts/install.sh | sh
```

To install a specific version:

```bash
curl -fsSL https://raw.githubusercontent.com/jeongminsang/dum-e/main/scripts/install.sh | sh -s -- --ref v0.1.0
```

The installer verifies `SHA256SUMS` and places the standalone native binary in `~/.local/bin/dume`.

### Build from Source

Requirements: Rust toolchain (`cargo`), Git.

```bash
# Clone repository
git clone https://github.com/jeongminsang/dum-e.git
cd dum-e

# Build release binary (native Rust)
cargo build --release --locked --offline -p dume-cli

# Install binary to PATH
cargo install --locked --offline --path crates/dume-cli
```

Local development build and installation can also be run with:

```bash
sh scripts/install.sh --dev
```

---

## Authentication & Model Configuration

DUM-E supports OAuth (Anthropic, OpenAI Codex) with automatic token refresh, as well as API key authentication:

```bash
# OAuth login
dume login anthropic
dume login openai-codex
dume login openai-codex --device

# API Key login
dume login openai --api-key
dume login google --api-key

# Logout and credential management
dume logout openai-codex
```

API keys can also be supplied via environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`).

Specify the model provider when launching interactive sessions or checking catalogs:

```bash
# Start interactive TUI session with a specific model
dume interactive --model anthropic/claude-sonnet-4-5
dume interactive --model openai-codex/gpt-5.4

# Query catalog across 39 supported providers
dume models --provider openai-codex
dume models --provider deepseek
```

In the interactive TUI, `/subagents` shows the subagent model setting. Use
`/subagents model <provider/model>` to select a model for newly spawned
subagents, or `/subagents model inherit` to use the parent model. This setting
is stored in `~/.dume/agent/subagents.json`.

---

## CLI Usage

```bash
# 1. Start interactive coding session (DUM-E Ratatui TUI)
dume

# 2. Check DUM-E harness status
dume status

# 3. Start autonomous coordinator with state recovery
dume coordinator --repo-path .

# 4. Run workspace test suite offline
cargo test --workspace --locked --offline
```

## Native Runtime Behavior

### 선택적 Jev 모델 라우팅

라우팅은 기본적으로 꺼져 있습니다. `/routing` 또는 기본 단축키 Ctrl+R로
설정을 열고 분류 서버와 표준 계획·복잡한 계획·구현 모델을 지정합니다.
단축키는 `~/.dume/agent/keybindings.json`의 `OpenRoutingSettings` 동작으로
재지정할 수 있습니다. TUI와 자율 워커는 같은 Rust 정책을 사용합니다.
설정은 전역 `~/.dume/agent/routing.json`에만 저장하며 프로젝트 설정은
읽지 않습니다. 다음은 모든 역할이 현재 선택 모델을 상속하는 설정입니다.

```json
{
  "version": 1,
  "enabled": false,
  "classifier": null,
  "models": {
    "standard": null,
    "complex": null,
    "implementation": null
  }
}
```

분류 설정에는 `backend` (`opencode` 또는 `local_system_one`), `base_url`,
`model`을 사용합니다. API 기준주소 뒤에 `systemone` 경로를 붙입니다.
예를 들어 로컬 기준주소 `http://127.0.0.1:8080/v1`은
`/v1/systemone`으로 요청합니다. 서버와 모델 ID는 직접 지정해야 합니다.
로컬 서버는 네이티브 System One 결정 모델을 제공해야 합니다.
일반 채팅 모델의 다음 토큰 확률을 이용하는 분류는 지원하지 않습니다.
llama.cpp에서 결정 모델을 인식하는 기준은 0.6.0 이상과
`architecture.output_modalities`의 `decisions`입니다. 이는 배포 중인
서버와의 실제 호환성을 검증했다는 뜻은 아닙니다.

꺼져 있거나 미설정이거나 세 역할이 모두 현재 모델을 상속하면 분류 요청은
없습니다. 활성화한 경우 대화·공개 설정·기준 모델로 정한 epoch에서 한 번
분류하고, 복잡도 확률이 0.5 이상이면 복잡한 계획 모델을 사용합니다.
분류 입력은 최신 사용자 요청을 UTF-8 손상 없이 최대 16,000문자로 자릅니다.
분류는 5초 제한의 단일 시도이며 자동 재시도하지 않습니다.
성공한 `write_file` 또는 `replace_file_content`의 영속 기록 뒤 **다음**
생성 요청부터 구현 모델을 사용합니다. 실패·읽기·불확실한 도구 결과는
단계를 바꾸지 않습니다. 압축과 공급자 캐시 ID 변경도 대화 단계를
초기화하지 않습니다.

분류 실패나 미완료 분류는 해당 epoch에서 원래 선택 모델을 유지합니다.
나중에 키를 고치거나 파일 쓰기에 성공해도 같은 epoch의 실패를 다시
분류하지 않습니다. 공개 설정이나 기준 모델 변경은 다음 안전하게 수락한
사용자 입력부터 적용하고, 진행 중인 요청·도구 재생의 설정은 유지합니다.
생성 오류는 기존 재시도·오류 경로를 사용하며 다른 모델로 재실행하지
않습니다. 취소 후에는 대체 생성 요청을 보내지 않습니다. 명시적 하위
에이전트 모델은 우선하고, 상속은 부모의 실제 요청 모델을 사용합니다.
하위 에이전트는 부모의 단계를 변경하거나 자동으로 다시 분류하지 않습니다.

분류 키는 공개 설정에 넣지 않고 기존 `~/.dume/agent/auth.json`에 따로
저장합니다. OpenCode Zen은 기존 `opencode` 로그인 키를 공유하며
`OPENCODE_API_KEY`가 저장 키보다 우선합니다. 로컬 분류 키는
`jev-local-system-one`으로 분리합니다. 저장 키 삭제는 환경변수를
제거하지 않습니다. 공개 설정과 키 저장·삭제는 독립 작업으로, 두 파일의
동시 원자적 저장을 보장하지 않습니다. 인증 JSON은 권한으로 보호되지만
암호화된 비밀 저장소는 아닙니다.

설정 저장은 분류 서버에 연결하지 않습니다. 연결 확인은 외부 전송·비용
경고를 확인한 뒤 명시적으로 실행하며 민감하지 않은 고정 문구만 보냅니다.
활성 분류는 사용자 요청을 설정한 서버로 전송하고 비용이 발생할 수
있습니다. 분류 토큰과 생성 사용량은 구분하며, 분류 가격이 없으면
미확인으로 표시합니다. 비용 절감·품질·속도 향상은 측정하지 않았습니다.

### MCP servers

MCP server configuration is JSON in `~/.dume/agent/mcp.json`; project-local
servers go in `<repo>/.dume/mcp.json`. Both files use a top-level `servers`
object. Project entries override same-named global entries unless that global
server is disabled. Project-defined servers require explicit trust before use.
For example:

```json
{
  "servers": {
    "docs": {
      "command": "${HOME}/bin/docs-mcp",
      "args": ["--stdio"],
      "env": {"DOCS_TOKEN": "${DOCS_TOKEN}"},
      "enabled": true,
      "exposure": "deferred",
      "tool_overrides": {"delete_document": "hidden"}
    }
  }
}
```

`exposure` is `direct` (default), `deferred`, or `hidden`; exact tool-name
entries in `tool_overrides` can set the same values for individual tools.
Hidden-only servers are not started. A direct override is declared immediately;
deferred tools become available through `tool_search`. Activation is saved only
after storage succeeds and restored only when configuration and schema match.
`${NAME}` expands from the process environment in command, args, env values,
and cwd; unset variables are errors. Expansion is substitution only, not shell
evaluation. Inspect servers with `dume mcp list` or
`dume mcp list --project <repo>` and trust the current project configuration
with `dume mcp trust --repo <repo>`. Trust records bind the canonical repository
path and that server's SHA-256 digest of its expanded configuration. Editing
the project server configuration changes the digest and requires trust again;
trust is per server and does not grant sandboxing or make a server safe.

### Queued input and durable tool execution

During a busy run, Enter queues a steer for the current run; Ctrl+Q queues a
follow-up for later. Keybindings can be overridden in
`~/.dume/agent/keybindings.json`, for example
`{"bindings":{"Ctrl+q":"SubmitFollowUp"}}`. `/follow-up <text>` also queues
a follow-up. `/queue` or `/queue list` lists pending and paused inputs;
`/queue withdraw <input-id>` withdraws a pending/paused input, and
`/queue resume <input-id>` reactivates a paused one. A failed or restarted run
does not automatically execute paused work. The CLI also exposes
`dume runtime queue list conversation:<id>`, `dume runtime queue withdraw conversation:<id> <input-id>`,
and `dume runtime queue resume conversation:<id> <input-id>`.
Opening a conversation does not resume execution. `/run resume [run-id]`
explicitly resumes an interrupted run; `/queue resume` only reactivates input.

Tool executions and results are durably journaled and saved success or known-failure results
are reused on recovery. A crash while a non-replay-safe tool is running leaves
its outcome as `outcome_unknown`; DUM-E does not blindly dispatch it again.
Inspect a run using `dume runtime inspect <run-id>`. After checking what the
tool actually did, resolve an unknown execution with its current revision and
operator evidence, for example:

```bash
dume runtime resolve <execution-id> --run-id <run-id> --revision <revision> \
  --operator "operator-name" --decision observed_succeeded \
  --evidence "verified external change <reference>"
```

Allowed decisions are `observed_succeeded`, `observed_not_run`, and
`retry_authorized`. Supply `--result <text>` when recording an observed result.
`observed_not_run` requires evidence beginning with `confirmed_not_run:` and a
reliable non-execution reference; a timeout or missing file is not confirmation.
`retry_authorized` may repeat external effects; explicitly acknowledge that
risk with `--acknowledge-duplicate-effect`. Revision checks reject stale
resolutions, and a live executor owning the run prevents resolution. These
controls do not provide exactly-once execution: operators must establish the
external outcome using reliable evidence.

`/session` displays persisted run, model-attempt, and tool timing. Recorded
session totals can be lower bounds after interruption or recovery; incomplete
historical timing is marked lower bound or unknown, not reconstructed duration.
Runs can end in `succeeded`, `failed`, `cancelled`, `interrupted`, or
`needs_attention`; exhausting the worker turn limit is a failure, not a
successful completion.

### Provider retries and program status

Transient provider HTTP/SSE failures retry up to four total attempts with
500 ms, 1 s, then 2 s backoff, subject to a 30 s maximum individual delay and
120 s overall retry window. A provider retry delay may replace the backoff,
within those limits. Failed/provisional attempt usage can remain recorded in
cost totals. Retry clears the failed attempt's provisional text and tool calls;
retry does not
undo external effects. Authentication, invalid-request, and unknown failures
are not automatically retried. OAuth refresh has an independent owner that
keeps the credential lock through token validation and atomic storage. Cancelling
the caller does not cancel an in-progress refresh. Shutdown drains refresh owners
for up to 95 seconds; forced termination cannot guarantee token preservation.

The TUI can emit terminal OSC 7501 program-status messages. `DUME_PROGRAM_STATUS`
accepts `auto` (default), `1` (enable), or `0` (disable); unknown values disable
it. It is always disabled when output is not a TTY, including with `1`. The
status payload uses fixed built-in messages and does not include user prompt or
tool content. The standalone Rust executable does not require Node.js or
Python at runtime.

---

## Architecture

<p align="center">
  <img src="assets/dume_banner.jpg" alt="DUM-E Autonomous Multi-Agent Harness Architecture" width="100%" />
</p>

```mermaid
flowchart TD
    UI[CLI / User Interface] --> COORD[DUM-E Coordinator]
    COORD <--> STORE[(SQLite WAL & Blob Store)]
    COORD --> WORKER[Isolated Worker Hosts]
    WORKER --> WT[Per-Attempt Worktree]
    WORKER --> RESULT[Result Manifest & Artifacts]
    VERIFY[Independent Acceptance Verifier]
    RESULT --> VERIFY
    VERIFY --> INTEGRATE[Serialized Integration]
    INTEGRATE --> STORE
```

---

## Project Status & Contributions

This repository is developed and maintained for personal and dedicated internal use. External contributions, issues, and pull requests are not currently accepted.

---

## Acknowledgements

DUM-E's core concepts, protocol design, and architectural foundation originated from and were inspired by [pi](https://github.com/badlogic/pi) by [Mario Zechner](https://github.com/badlogic). DUM-E has since been fully re-architected and rewritten from the ground up as a 100% standalone native Rust engine.

---

## License

DUM-E is licensed under either of:
- Apache License, Version 2.0 ([LICENSE-APACHE](http://www.apache.org/licenses/LICENSE-2.0))
- MIT license ([LICENSE](LICENSE))

at your option.
