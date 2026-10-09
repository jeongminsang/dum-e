# DUM-E Pi 기능 이식 구현 계획

## 1. 기준과 범위

- 기준: DUM-E `dev`, `d320a09`; Pi `v1.1.0` 및 최근 `v0.99.0`/`v1.0.0` 기능. 작성일: 2026-10-08 KST.
- 구현 기준 계획이다. 현재 구현과 통합 검증을 진행 중이며, 완료 판정은 아래 검증 기준을 따른다. 별도 요청 없는 커밋·푸시는 하지 않는다.
- 2026-10-09 KST 검증: `cargo check --workspace --locked --offline`, 전체 테스트 273개, `cargo build -p dume-cli --locked --offline` 통과. 80/40열 tmux에서 한글 입력·삭제, 실패 상태와 `/session` 표시 및 SQLite 저장을 확인했다. 이 터미널 시나리오는 invalid-model 실패 경로이며 성공 모델 호출 검증은 Rust 로컬 HTTP fixture에 한정된다. 실제 계정 OAuth와 물리 IME 조합 입력은 검증하지 않았다.
- 사용자 지시에 따라 확정 7개와 이전 추가 권고 2개를 모두 포함한다.

| 적용 기능 | 최종 산출물 |
| --- | --- |
| 도구 검색·지연 로딩 | MCP 실행 연결, 필요한 도구만 검색·선언, 활성화 저장·재개 |
| 실행 중 지시 변경·후속 큐 | steer/follow-up admission, FIFO·저장·취소·재개 |
| 도구 실행 단위 복구 | 의도/결과 journal, 안전한 재실행, 결과 불명 정지·확인 |
| 모델·도구별 시간 | 시도·호출 단위 duration과 결과 저장 |
| 작업·세션 시간 | 진행 시간, 완료 시간, 재개 후 누적 작업 시간 |
| 종료 상태 구분 | 성공·실패·취소·프로세스 중단을 별도 기록·표시 |
| OSC 7501 | interactive 터미널에 committed 상태 보고 |
| 취소에 안전한 OAuth 갱신 | 자동 refresh가 caller 취소 후에도 저장까지 완료 |
| 공급자 오류 재시도 | HTTP와 스트림의 명확한 일시 오류에 제한적 재시도 |

제외: `+tool/-tool` 문법·패턴 선택, codemode/JavaScript 실행기, 자동 모델 라우팅, 세션 분기, 백그라운드 압축, Pi Chord 런타임, 이미지 생성, MCP 리소스 브라우저, streamable HTTP 신규 전송 계층, MCP 서버 OAuth 신규 로그인.

이번 MCP 완료 기준은 stdio 서버의 실제 연결·검색·실행·복구이다. 새 HTTP 전송과 MCP 서버 OAuth는 명시적으로 선택되지 않은 확장이므로 범위에 넣지 않는다. 기존 공급자 OAuth 갱신은 포함한다. Rust 단일 바이너리와 기존 SQLite·artifact·worktree·DAG·독립 검증·epoch fencing을 유지한다.

## 2. 이식 전 기준과 변경 접점

| 영역 | 현재 | 변경 접점 |
| --- | --- | --- |
| 저장 | `store.rs` append/replace transaction | 실행·큐·결과 원자적 저장 API |
| TUI 저장 | `app.rs`의 비동기 PersistEvent, 일부 오류 무시 | 중요한 입력·결과는 저장 확인 후 승인·표시 |
| 실행 | autonomous AgentLoop와 TUI run_conversation | 공통 요청/도구/settle 경계로 계약 단일화 |
| 도구 | ToolDispatcher, wait_with_output | journal·복구 정책·상태·duration |
| 입력 | SubmitInput은 !is_busy일 때만 제출 | busy admission, 안전한 경계 소비 |
| 시간 | busy_started_at, 종료 후 제거 | 완료 표시·저장·세션 누적 |
| MCP | stdio client/bridge prototype | 수명 관리·신뢰·dispatch·deferred registry |
| 재시도 | HTTP 429/5xx·연결 오류 | stream transient 분류와 단일 총 budget |
| OAuth | caller future가 refresh await와 저장 수행 | owner task가 lock부터 commit까지 소유 |
| 키 | keybinding.rs의 직접 key match | 설정 가능한 binding과 기본값 |

`request_usage`/`session_usage`를 토큰 집계 기준으로 유지한다. attempt 전용 `external_operations`를 일반 도구 journal로 억지 재사용하지 않는다. 현재 압축·원문 artifact 보존은 유지한다.

## 3. 반드시 먼저 고정할 동작 계약

### 3.1 작업과 상태

- conversation은 사용자 세션, run은 요청 하나의 실행이다. 공급자 캐시 session_id와 conversation_id를 혼동하지 않는다.
- steer는 현재 run에 편입한다. follow-up은 현재 run 이후 별도 run으로 실행한다.
- 실제 모델 시도마다 새 request_id를 사용하고 논리 turn_id는 유지한다.
- 모든 runtime event에 scope, run_id, owner_generation을 넣는다. 이전 run의 늦은 Finished 이벤트가 새 실행 상태를 덮지 못한다.
- Run 상태: queued/running/cancelling/succeeded/failed/cancelled/interrupted/needs_attention.
- Tool 상태: prepared/running/succeeded/failed/cancelled/outcome_unknown.
- 실패 reason은 provider_error, tool_error, turn_limit, storage_error, owner_lost 등으로 분리한다. turn limit를 성공으로 표시하지 않는다.
- coordinator epoch는 기존 attempt fencing에 그대로 사용한다. interactive는 conversation lock과 조건부 상태 전이로 소유권을 검증한다.
- 취소 요청과 종료 확인은 다르다. executor 정지 확인까지 cancelling으로 남기고 같은 scope에서 새 실행을 시작하지 않는다.

### 3.2 저장 순서와 실행 복구

정상 순서: 입력 저장 -> 모델 요청 -> assistant 및 전체 tool 계획 저장 -> tool 시작 기록 -> 외부 실행 -> 결과와 tool message 동시 저장 -> UI/다음 모델 요청.

- DB commit 실패를 숨기지 않는다. 입력 receipt는 저장 후 반환하며 결과 저장 실패 시 다음 모델 요청을 보내지 않는다.
- HTTP/프로세스 실행 동안 SQLite transaction이나 mutex를 잡지 않는다.
- `(run_id, tool_call_id)` 중복이면 name/arguments hash를 검사한다. 같으면 저장 결과 사용, 다르면 무결성 오류.
- 저장된 결과는 다시 실행하지 않는다. prepared는 실행 가능. running에서 죽은 도구는 기본 결과 불명.
- read/read_artifact/검색처럼 부작용 없는 도구만 명시적으로 replay-safe 등록한다. 반복 read가 같은 파일 내용을 반환한다는 보장은 하지 않는다.
- bash/edit/write/MCP/subagent 생성·통합은 기본 자동 재실행 금지. MCP hint만으로 안전성을 확정하지 않는다.
- 외부 효과 발생 후 result commit 전 crash는 outcome_unknown이다. worktree/artifact/arguments를 보존하고 사용자 확인 결과 또는 명시적 재실행 승인 전에는 run을 정지한다.
- 프로세스를 죽였어도 이미 발생한 외부 효과가 되돌려졌다고 주장하지 않는다.
- 기존 history repair보다 journal 증거를 우선한다. 증거 없는 과거 기록은 interrupted로 표시하고 성공/미실행으로 추정하지 않는다.
- child run/worktree 연계도 저장한다. 사라진 subagent handle을 성공으로 위조하거나 자동 재생성하지 않는다. worker 복구는 기존 lease/epoch 선점 이후만 허용한다.
- unknown resolve는 현재 owner/실행 revision을 검사하는 transaction이다. `observed_succeeded`, `observed_not_run`, `retry_authorized`를 구분하고 operator·원 실행 attempt·증거 artifact·경고 확인을 수정 불가 감사 기록에 남긴다.
- observed_succeeded는 운영자 확인 결과를 tool message와 함께 저장하며 source=operator_observed로 표시한다. 자동으로 검증된 원 실행 결과라고 표현하지 않는다. observed_not_run은 미실행 판단 근거를 기록하고 새 attempt를 준비한다.
- retry_authorized는 중복 부작용 가능성을 명시적으로 경고한 뒤 새 execution attempt ID를 발급한다. 원 executor 종료 또는 기존 owner의 유효성 상실이 확인되기 전에는 승인된 재실행도 시작하지 않는다.
- late original completion은 원 owner/attempt 토큰으로 canonical 상태를 수정할 수 없다. 별도 감사 증거로만 보관하며 관찰 결과와 모순되면 needs_attention으로 알린다. 이미 기록된 operator 결과나 transcript를 조용히 덮지 않는다.

resolve의 transaction 전제는 logical execution.state=outcome_unknown, expected revision 일치, 현재 owner 일치이다. 원 attempt는 outcome_unknown인 종료 기록으로 보존하고 다시 실행 가능한 상태로 바꾸지 않는다. resolution 감사 행·logical execution 갱신·revision 증가 및 아래 부수 효과가 한 번에 commit되며 일부만 성공하는 것은 금지한다.

| resolution | 증거 조건 | 같은 transaction의 상태 변화 |
| --- | --- | --- |
| observed_succeeded | operator, 원 execution/attempt 참조, 관찰 근거, bounded result 필수 | logical execution -> succeeded; operator_observed result와 tool message 저장; 새 attempt 없음; run은 명시적 재개 가능한 상태로 전환 |
| observed_not_run | 서버의 확정 미접수 receipt 또는 도구별 신뢰 가능한 미실행 확인 필수; 단순 timeout/파일 없음/증거 공란은 거절 | logical execution -> prepared; 새 고유 attempt를 prepared로 생성하고 active_attempt_id 연결; 원 unknown attempt와 감사 증거 유지 |
| retry_authorized | operator, 근거 메모, 중복 부작용 경고 확인 필수 | logical execution -> prepared; 새 고유 attempt를 prepared로 생성하고 active_attempt_id 연결; 승인 범위는 이 attempt 한 번이며 자동 반복 승인 아님 |

증거 부족·state/revision/owner 불일치는 무변경 오류다. prepared 새 attempt를 실행할 때 현재 owner와 승인/증거를 다시 검사한다. 원 local executor가 아직 살아 있으면 종료 확인까지 실행하지 않는다. 원격 효과는 확인 불가능할 수 있으며 retry_authorized의 중복 위험 경고에서 이를 별도로 표시한다. 원 결과가 늦게 와도 새 active_attempt_id로 finalize할 수 없다.

### 3.3 입력 큐

- idle Enter는 새 run, busy Enter는 steer. 별도 follow-up action과 `/follow-up`으로 후속 요청 제출.
- 기본 follow-up key는 Ctrl+Q로 제안하며 사용자 설정으로 변경한다. 새 직접 key 검사는 금지한다.
- steer는 현재 tool batch 직후 다음 모델 요청 전에 FIFO 편입한다. 도구를 자동 중단하지 않는다.
- assistant 최종 응답 직전에 pending steer가 있으면 settled 확정 전 소비해 계속한다. terminal 뒤 도착한 steer는 새 run admission임을 표시한다.
- follow-up은 성공 terminal 뒤 하나씩 실행한다. 실패·취소·중단 후 pending 입력은 보존하고 자동 시작하지 않는다.
- 사용자 취소는 현재 run에 편입된 입력만 종료한다. 미소비 steer/follow-up은 남기고 명시적 재개 때 처리한다.
- admission과 큐 소비/transcript append는 각각 transaction. 같은 client request ID와 payload는 중복 삽입하지 않고 payload mismatch는 오류.
- 큐 count/bytes limit를 둔다. 저장 실패·limit 초과 시 입력 초안을 편집기에 남긴다.
- admission과 settlement의 기준 시점은 scope별 SQLite transaction commit이다. admit_input은 transaction 안에서 현재 run 상태를 읽고 아래 disposition을 receipt에 영속 저장한다. receipt는 실제 소비 완료와 별개이며 current_run이면 input ID/target_run_id를 함께 반환한다.

| admission 시 scope 상태 | 입력 | disposition / 영속 변경 |
| --- | --- | --- |
| 활성 primary run 없음 | 일반 제출 또는 steer 또는 follow-up | new_run; queued run 생성 + 최초 user transcript append + input consumed를 한 transaction; 과거 paused 입력은 건드리지 않음 |
| primary run queued/running | steer | current_run; input pending, target_run_id=현재 run; 경계에서 소비 전까지 transcript에 넣지 않음 |
| primary run queued/running | follow-up | pending; input pending, 현재 run에 편입하지 않음; 성공 뒤 별도 run 대기 |
| cancelling/needs_attention인 run 존재 | steer 또는 follow-up | pending; input paused; 자동 시작·현재 run 편입 금지 |

- inbox state는 pending/paused/consumed/withdrawn이다. consumed는 consumed_run_id와 transcript 위치를 함께 갖는다. 같은 input ID를 두 run에서 소비하지 않는다.
- `settle_or_consume_steer`의 성공 경계 transaction: 현재 owner 검사 -> pending steer(kind=steer,target_run_id=현재 run)를 FIFO 소비·transcript append하고 계속하기, 또는 steer가 없으면 succeeded+elapsed commit 중 하나를 선택한다. follow-up은 이 검사에서 제외한다.
- 성공을 commit하는 동일 transaction 안에서 가장 오래된 pending follow-up이 있으면 새 queued run 생성·최초 transcript append·input consumed를 수행한다. 새 executor/provider 호출은 commit 후 시작한다. 중간에 다른 primary run을 만들 수 없다.
- 실패/취소 terminal transaction은 남은 pending 입력을 paused로 바꾸며 새 run을 만들지 않는다. already consumed 입력은 원 transcript/run에 남긴다. paused 입력은 이후 다른 run이 성공해도 자동 소비 대상이 아니며 명시적 재개/삭제가 필요하다.
- terminal 전 admission된 steer만 그 run의 소비 대상이다. follow-up은 전후 시점과 무관하게 현재 run에 편입하지 않는다. terminal 뒤 admission은 그 시점의 scope 상태를 다시 읽는다: 이미 다음 run이 queued면 그 run에 steer, 활성 run이 없으면 new_run이다.
- paused 입력의 명시적 재개는 동일 input ID를 owner-fenced transaction으로 pending/new run에 한 번만 재배치한다. 메모리 pending 확인 후 별도 terminal write하는 경로는 금지한다.

### 3.4 시간

- 모델 duration: 실제 요청 시작부터 해당 시도 완료/실패/취소까지. OAuth와 retry backoff는 별도이며 run 시간에 포함.
- 도구 duration: 실제 executor 호출부터 종료까지. 큐·정책·저장 시간과 구분.
- run active 시간: 요청 처리 시작부터 terminal까지. 도구·압축·retry·foreground child 대기를 포함하고 사용자 응답을 기다리는 blocked 구간은 제외.
- 세션 누적 작업 시간: primary top-level run의 active 시간 합계. child 시간을 다시 더하지 않는다. `/btw`는 별도 보조 run 통계로 표시하고 primary 누적에 중복 합산하지 않는다.
- Instant로 측정하고 정수 milliseconds로 저장한다. UTC timestamps는 추적/정렬용이며 wall-clock 차이로 duration을 계산하지 않는다.
- retry 실패·사용자 취소도 시간을 기록한다. usage 미보고는 확정 0으로 표시하지 않는다.
- 최대 1초 간격으로 active elapsed checkpoint. abrupt crash 뒤 마지막 checkpoint까지만 알려진 값이며 timing_complete=false, `최소 N초`로 표시한다.
- resume에서는 새 monotonic 구간을 더하고 세션을 닫은 downtime은 더하지 않는다. 과거 미기록 시간은 `기록 없음`이지 소급 추정값이 아니다.
- 모델/도구 duration 합은 병렬·중첩 실행 때문에 run elapsed와 같지 않아도 정상이다.
- persisted elapsed_ms는 누적 확정 하한이다. owner가 실행을 시작할 때 이를 segment_base로 읽고 새 Instant를 만든다. checkpoint는 delta를 반복 더하지 않고 `segment_base + 해당 owner의 active monotonic elapsed`를 조건부 저장한다.
- resume는 소유권 확보와 새 generation 전이를 한 번만 commit한다. 중복 reopen/checkpoint/finalize는 elapsed를 늘리지 않으며 stale owner 쓰기는 거절한다. crash에서 생긴 timing_complete=false는 나중에 정상 종료해도 정확한 값으로 바꾸지 않는다.

## 4. 데이터와 공통 API

| 저장 대상 | 핵심 필드·제약 |
| --- | --- |
| agent_runs 신규 | run_id, conversation_id 또는 attempt_id, parent_run_id, agent_id, owner_generation, status/reason, started/ended_at, elapsed_ms, timing_complete; 활성 primary run 중복 방지 |
| tool_executions 신규 | execution_id, run_id, turn/tool_call_id, name, arguments/hash, replay_policy, state, revision, active_attempt_id, result/error/artifact refs; UNIQUE(run_id, tool_call_id) |
| tool_execution_attempts 신규 | execution_id, attempt_no, state, started/ended_at, duration_ms/completeness; 안전한 복구 실행도 이력 유지 |
| tool_resolutions 신규 | execution/원 attempt ID, expected revision, operator, resolution_kind, evidence digest, duplicate-effect warning 확인; append-only 감사 기록 |
| agent_inbox 신규 | scope, client_request_id, 증가 sequence, kind, payload, disposition, target_run_id, pending/paused/consumed/withdrawn, consumed_run_id/transcript 위치; 중복 admission 방지 |
| request_usage 확장 | run/turn_id, attempt_index, state/reason, started/ended_at, duration/completeness; 기존 request_id·usage 유지 |
| conversation runtime state 신규 | 활성 canonical tool IDs, 서버 config/schema fingerprints; client/credential 저장 금지 |

기존 initialize_schema 방식을 사용해 데이터를 보존한다. 구 API 별칭·별도 호환 런타임을 늘리지 않는다. 기존 데이터에 시간 없음은 자연스럽게 null/unknown 처리한다.

큰 tool 결과는 기존 artifact에 먼저 저장하고 journal에는 bounded text/digest를 기록한다. result+transcript, input-consumption+transcript, run-terminal+elapsed를 각각 한 transaction으로 저장한다.

제안 API 계약:

- `admit_input(scope, client_request_id, kind, payload) -> InputReceipt`
- `consume_inputs(run_id, owner_generation, boundary) -> messages`: 소비와 transcript commit을 함께 처리.
- `start_run`, `checkpoint_run_elapsed`, `settle_run(expected_owner, outcome, elapsed)`
- `prepare_assistant_turn_and_tools`, `begin_tool`, `commit_tool_result_and_message`, `recover_run`
- `resolve_unknown(expected_owner, expected_revision, resolution, evidence)`; `settle_or_consume_steer`는 owner-fenced 원자적 분기.
- `start_model_attempt`, `finish_model_attempt(outcome, duration, known_usage)`
- `activate_tools(conversation, ids, fingerprints)`, `run_metrics`, `conversation_metrics`

usage의 단일 원장은 request_id별 실제 모델 시도다. retry 실패·취소·성공을 각각 1회 기록한다. request_usage terminal finalize에서 같은 provider session의 session_usage에 알려진 usage를 1회 합산한다. 중복 finalize는 집계를 바꾸지 않는다.

사용자 conversation의 primary 총량은 primary run 및 그 child tree에 귀속된 request_id를 한 번씩 조회해 계산한다. provider session별 aggregate나 부모 AgentOutcome의 descendant 합계를 다시 더하지 않는다. `/btw`는 보조 subtotal로 분리한다. 논리 turn 총량은 모든 시도의 알려진 usage 합계와 unknown-attempt 수를 함께 표시한다. 미보고 usage를 0인 완전한 기록으로 바꾸지 않는다.

실제 시도 원장/aggregate finalize는 공통 worker 요청 경계 한 곳이 소유한다. TUI의 Completed 사용량 합산과 autonomous 별도 합산은 표시용 projection으로 전환하며 원장을 갱신하지 않는다. 직접 호출도 같은 기록 경계를 사용한다. 공통 record/event는 dume-core, 저장은 dume-store, 실행 경계·registry는 dume-worker가 소유한다. TUI는 명령을 제출하고 committed 이벤트를 구독한다.

## 5. 단계별 구현과 완료 기준

### A. 상태·저장·공통 실행 기반

대상: `crates/dume-core/src/types.rs`, `crates/dume-store/src/{schema,store,lib}.rs`, `crates/dume-worker/src/{agent_loop,ipc,host}.rs`, `crates/dume-tui/src/app.rs`.

1. RunOutcome/ToolOutcome/InputKind/ReplayPolicy/RuntimeEvent와 identity를 정의한다.
2. 테이블·index·조건부 전이·원자적 API를 추가한다.
3. worker 공통 runtime 모듈로 요청/도구/settle 경계를 추출해 autonomous와 TUI가 같은 계약을 호출하게 한다. coordinator 전체 재작성은 하지 않는다.
4. UI의 fire-and-forget만으로 저장하던 중요 결과를 runtime commit으로 전환하고 이중 저장 callsite를 정리한다.
5. IPC의 완료/실패에 명시적 outcome을 넣고 기존 manifest·검증을 유지한다.
6. cancelling 동안 중복 실행과 늦은 이벤트 덮어쓰기를 차단한다.

완료 기준: 기존 interactive/autonomous 요청이 새 계약으로 정상 동작. 중복 입력, stale owner, DB 실패, terminal 두 번, assistant/tool-plan rollback, 기존 DB reopen, usage 중복 집계 테스트 통과.

### B. 모델·도구·작업 시간과 종료 UI, OSC

대상: `crates/dume-provider/src/{types,runtime}.rs`, `crates/dume-worker/src/{agent_loop,tools}.rs`, `crates/dume-tui/src/{app,component,lib}.rs`; 신규 `program_status.rs`.

1. 실제 요청·executor timer와 run checkpoint/finalize를 연결한다. 직접 모델 호출도 동일 측정 계약을 사용한다.
2. primary/child/보조 run을 구분하고 기존 descendant usage의 이중 집계를 제거한다.
3. 진행 상태 바에 경과 시간을 우선 표시하며 종료 후 transcript에 `완료 · 1분 23초`, `취소 · 12초` 등을 남긴다.
4. `/session`에 누적 시간·모델/도구별 통계·retry attempt·불완전 기록 수를 표시한다. 80/40-column에서도 상태/시간을 볼 수 있게 한다.
5. OSC는 committed 상태와 매핑한다: 작업=working, 로그인=blocked/auth, 확인=blocked/question, 성공=done, 최종 실패=error, 초기/취소=idle.
6. `DUME_PROGRAM_STATUS=auto|1|0`. auto는 interactive TTY에서 보고한다. OSC 규격은 탐색 없는 보고를 허용하므로 첫 구현은 query를 보내지 않으며 지원을 실제 감지했다고 표시하지 않는다.
7. `OSC 7501;state=...:app=dume:msg=<base64 UTF-8> ST`; control 제거, 크기 제한, secret 제거. 상태 변경 때만 동일 TUI writer에서 출력한다. JSON/IPC/nonTTY에는 출력하지 않는다.
8. 정상 완료 직후 exit에서 done을 무조건 idle로 덮지 않는다. retry 중간 오류는 최종 error로 보고하지 않는다.
9. OSC msg는 enum에서 고정 allowlist 문구로만 만든다: 작업 중/인증 대기/확인 대기/완료/실패/취소. 사용자 요청·모델 응답·tool output·raw error·credential·URL·파일 경로를 넣지 않는다. app은 dume 고정, title은 생략한다. base64는 암호화가 아니므로 민감한 문자열을 encode해 보내는 방식은 금지한다.

완료 기준: 완료 후 표시·reopen 보존, 병렬 child 중복 없음, blocked/downtime 제외, crash 최소 시간 표기, 중복 reopen/checkpoint/finalize, 정확한 OSC bytes/한글/base64/한계, credential-shaped text/ESC/BEL 주입이 OSC에 안 들어감, forced-off/nonTTY, narrow layout 테스트 통과.

### C. 도구 journal과 복구

대상: `crates/dume-worker/src/{agent_loop,tools,executor}.rs`, `crates/dume-store/src/store.rs`, `crates/dume-tui/src/app.rs`, `crates/dume-cli/src/main.rs`.

1. 완료 assistant와 batch 전체 tool intent를 저장한 뒤 개별 tool을 실행한다.
2. 로컬·MCP·subagent가 ToolDispatcher의 동일 journal 경계를 통과하게 한다.
3. 저장된 결과로 history를 복원하고 synthetic missing reply보다 우선한다.
4. replay policy를 명시 등록한다. safe read-only만 자동 재실행한다.
5. effect 후 result 전 crash는 outcome_unknown으로 정지한다. `/resume`에 execution ID·도구·args·worktree/artifact 증거를 표시한다.
6. UI/CLI resolve 동작에 3.2의 세 resolution을 구현한다. operator evidence·원 attempt·revision·duplicate 경고 확인을 원자적으로 기록하고 새 attempt ID와 late result 격리를 적용한다.
7. child/worktree ID 및 attempt fencing과 연계해 이전 소유자가 기록을 덮지 못한다.
8. subprocess stdout/stderr는 bounded admission/artifact 경로에 연결한다. 큰 결과를 journal이나 무제한 메모리 buffer에 쌓지 않는다.

완료 기준: prepared/실행 중/effect 후/result 후 crash fixtures, 저장된 결과 무재실행, read-only replay, bash/MCP unknown 무재실행, arguments mismatch, 취소 후 partial effect, artifact 실패, compaction 뒤 journal 복구, stale epoch, resolve별 상태/감사/새 attempt 원자성, 증거 부족 거절, resolve 중복/stale revision/late completion 충돌 테스트 통과. marker 파일 fixture로 승인 없는 중복 부작용이 없음을 검증한다.

### D. steer/follow-up와 키바인딩

대상: `crates/dume-worker/src/{agent_loop,ipc,host}.rs`, `crates/dume-tui/src/{app,keybinding,component}.rs`, `crates/dume-store/src/store.rs`.

1. idle/busy 입력을 durable admission에 넣고 receipt 뒤 입력창을 비운다.
2. tool batch 및 settled 경계에서 steer를 소비하고 성공 terminal 뒤 follow-up을 별도 run으로 시작한다.
3. worker IPC에 input kind/request ID/scope/epoch를 전달한다. Cancel과 구분한다.
4. binding을 설정 가능한 map으로 만들고 follow-up 기본값을 추가한다. `/follow-up`, `/queue` 조회·미소비 입력 제거를 제공한다.
5. 큐 종류·대기 수·메시지를 표시한다. `/btw`는 기존 독립 대화로 유지한다.
6. 취소/실패/reopen 후 pending은 보존하며 자동 시작하지 않는다. old Finished와 새 입력 경합을 차단한다.

완료 기준: admission 표의 모든 조합, busy Enter, tool-round/final-response 경계, admission 대 terminal commit의 양쪽 순서 경합, FIFO, follow-up 별도 run, 성공+follow-up 원자적 handoff, 중복 admission, queue limit, DB 실패 초안 유지, 취소 도중 입력, consumed 입력 무재접수/paused reopen rebind, stale epoch, binding 변경, 한글 IME 테스트 통과.

### E. MCP stdio와 검색·지연 로딩

대상: `crates/dume-mcp/src/{client,bridge,types,lib}.rs`, 신규 `config.rs`, `crates/dume-worker/src/agent_loop.rs`, `crates/dume-tui/src/app.rs`, `crates/dume-cli/src/main.rs`, 관련 provider types.

1. JSON 설정: `~/.dume/agent/mcp.json`, `.dume/mcp.json`. executable/argv/env/cwd, enabled, direct/deferred/hidden, exact tool override, timeout/output limits를 정의한다. +/-·wildcard는 넣지 않는다.
2. 프로젝트 설정은 canonical repo path+실행 설정 digest의 사용자 신뢰 후 실행한다. command/env/endpoint가 바뀌면 재확인. 전역 명시 disable은 프로젝트가 되살릴 수 없다.
3. env는 `${VAR}`만 확장하고 shell command 문자열은 실행하지 않는다. secret은 로그/UI에서 제거한다.
4. client가 reader/writer/child/pending을 소유한다. initialize/list/call timeout, EOF/send failure cleanup, bounded stderr/frame/output, grace/kill/reap를 구현한다.
5. inputSchema/isError/structuredContent 등 wire 필드와 response/notification을 올바르게 모델링하고 pagination/tools-list 변경을 처리한다.
6. bridge 하나가 노출 정책과 canonical name route를 검사한다. provider 이름 제한·정규화 충돌을 검증하고 stable hash를 사용한다.
7. `/mcp`, `dume mcp list`에서 disabled/connecting/ready/failed·도구 수·stderr tail을 표시한다. 서버 연결이 TUI 시작을 막지 않는다.
8. built-in tool_search는 결정적인 lexical ranking으로 name/description/server 설명을 검색한다. 새 벡터 서비스는 만들지 않는다.
9. activation IDs와 config/schema fingerprint를 저장하고 다음 provider payload에 스키마를 선언한다. resume에서 요청 생성 전에 복원한다.
10. direct 서버만 첫 요청 전에 bounded 연결 대기. deferred는 검색/호출 시 bounded 대기. 실패/disabled/hidden은 검색·선언·실행에서 제외한다.
11. config/서버 변경 때 active IDs/schema를 재검증하고 UI에 알린다. 현재 tool-change session ID 정책을 임의 제거하지 않는다.
12. 호출은 journal/cancel/output admission을 통과한다. call timeout은 자동 재시도하지 않는다. JSON/schema 오류는 정상 실패 result로 전달하고 JSON schema 전체 validator를 자체 재구현하지 않는다.

완료 기준: fake stdio initialize/list/call, camelCase, pagination, 이름 충돌, malformed/oversize, delayed startup, pending EOF cleanup, bounded stderr, process-tree reap, trust/config 변경, disable 우선권, no shell expansion, hidden dispatch 차단, 검색 직후 payload 변경, resume activation 복원, schema 변경, effect 후 response loss 무재호출 테스트 통과.

### F. 취소에 안전한 공급자 OAuth 갱신

대상: `crates/dume-provider/src/{auth,oauth,runtime}.rs`, provider tests, TUI/CLI shutdown callsite.

1. 자동 refresh owner가 lock 획득·최신 credential 재조회·만료 확인·refresh·검증·atomic write까지 소유한다.
2. caller 취소는 대기만 취소한다. 이미 시작한 refresh는 commit까지 계속한다.
3. lock 뒤 재조회해 concurrent refresh를 방지한다. 기존 0600/atomic replace/fsync를 유지한다.
4. 실패 refresh로 기존 credential을 덮지 않는다. 취소 뒤 owner 실패도 redacted log/event로 관측한다.
5. 정상 shutdown은 유한 시간 내 refresh 완료를 기다린다. timeout/I/O failure/강제 프로세스 종료까지 보장한다고 주장하지 않는다.
6. browser callback/code exchange/device polling 로그인은 취소 가능하게 유지한다. 자동 refresh만 uncancellable owner 경계로 분리한다.

완료 기준: fake server rotation 응답 지연 중 caller 취소 후 새 token 저장, 동시 resolve 1회, invalid token/reject/persist failure, lock timeout, interactive cancel 후 port 재사용, shutdown timeout 테스트 통과. 실계정 없이 검증한다.

### G. 일시적 공급자 오류 재시도

대상: `crates/dume-provider/src/{types,client,runtime,anthropic,openai,codex,gemini}.rs`, `crates/dume-worker/src/agent_loop.rs`, `crates/dume-tui/src/app.rs`, `crates/dume-provider/tests/mock_http_fixtures.rs`.

1. typed ProviderError로 transient-overload/transport/auth/invalid-request/context-overflow/protocol/unknown/cancelled를 구분한다.
2. HTTP와 stream retry가 하나의 budget을 공유한다. 기본 최대 4 network attempts, delay 0.5/1/2초, backoff/Retry-After 최대 30초, 전체 retry 경과 한도 120초를 제안한다. 중첩 retry 예산을 금지한다.
3. 명시 overload/server_busy 및 알려진 일시 transport 오류만 retry. 인증/권한/잘못된 JSON/unknown은 즉시 실패, context overflow는 기존 문맥 처리에 전달한다.
4. 같은 turn/model/provider session에서 실패 모델 요청만 재시도한다. attempt request ID는 새로 생성하고 저장된 tool은 반복하지 않는다.
5. UI text/tool delta는 provisional attempt로 즉시 stream한다. retry 때 실패 시도의 임시 출력 reset과 상태 표시. 성공 terminal 전 canonical assistant/tool intent를 commit하지 않는다.
6. 모든 이벤트를 끝까지 buffer해 live TUI를 없애지 않는다. 실패 시도의 알려진 usage/time도 저장하며 미보고 usage는 incomplete로 남긴다.
7. user cancel/storage_error는 retry하지 않는다. backoff 취소는 즉시 멈추되 이미 시작한 OAuth refresh는 단계 F 계약을 따른다.
8. length/content-filter 등 기존 terminal semantics를 유지한다. arbitrary 문자열 매칭으로 retry 대상을 확대하지 않는다.

완료 기준: 429->503->success, HTTP+SSE 총 4회 상한, busy SSE, partial tool/text reset, tool 단일 실행, terminal 뒤 무retry, auth/unknown/protocol 무retry, Retry-After seconds/date/invalid/cap, backoff cancel, history 유지, failed-attempt usage 합산·incomplete 표기. 모든 provider/callsite가 같은 계약을 사용한다.

### H. 통합·문서·검증

대상: `crates/dume-cli/tests/harness_integration.rs`, `crates/dume-provider/tests/mock_http_fixtures.rs`, 수정 module unit tests, `README.md`, `.dume/skills/interactive-testing.md`.

1. 임시 HOME/DB/credentials/worktree와 fake provider/MCP만 사용하는 tests 작성.
2. 긴 tool -> steer -> follow-up -> 성공 -> 시간 기록 -> exit/resume 시나리오 검증.
3. crash 안전 복구/unknown 정지, retry 도중 tool 중복 없음, 큐·시간·usage 보존 확인.
4. bench request count/usage completeness/cost accounting callsite와 tests도 갱신한다. 실패 비용을 숨겨 개선을 과장하지 않는다.
5. README에 MCP 설정·신뢰, 큐/binding, 복구 한계, 시간 정의, 상태/OSC override, OAuth/retry 정책을 추가한다.
6. interactive-testing의 tmux 절차는 유지하되 존재하지 않는 dume-test.sh/Node/Bun 안내를 실제 native target/debug/dume로 갱신한다. mock TUI fixture로 실제 답변/작업 완료까지 확인한다.
7. dev branch에서는 CHANGELOG를 작성하지 않는다. commit/push는 별도 사용자 요청 없이는 하지 않는다.

## 6. 실행 순서와 병렬 경계

- 순차: A -> B -> C -> D -> E -> H.
- F/G는 A의 attempt/event contract가 고정된 뒤 병렬 가능. types.rs/runtime.rs는 한 담당자가 소유하고 순차 통합하며 H 전에 완료한다.
- MCP config/lifecycle와 UI formatting은 공통 계약에 독립적인 부분만 병렬화한다. 실제 dispatch/activation은 C/D 완료 계약 뒤 연결한다.
- agent_loop.rs/app.rs/schema.rs/store.rs를 여러 구현자가 동시에 수정하지 않는다.
- delegation은 작업당 명시 파일 3~5개로 나누고 subagent는 gate/formatter를 실행하지 않는다. 계약 생성 작업이 끝나기 전에 소비자 구현을 시작하지 않는다.
- 각 단계는 실제 작동하는 vertical slice로 종료한다. stub/no-op/미완성 기능을 지원한다고 표시하지 않는다.

## 7. 검증과 완료 판정

구현 중 focused tests를 실행·수정 반복하고 통합 후 상위 실행자가 실행한다:

```sh
cargo fmt --all
cargo check --workspace --locked --offline
cargo test --workspace --locked --offline
cargo build -p dume-cli --locked --offline
```

- formatter는 최종 수정 파일 합집합에 한 번. 모든 gate는 locked/offline이며 dependency가 없으면 online/unlock로 우회하지 않는다.
- tmux 검증은 별도 세션·임시 HOME·native 바이너리로 수행한다. 다른 사용자의 tmux/session/worktree를 정리하지 않는다.
- 검증 못 한 항목은 구체적으로 미완료라 명시한다. 테스트/경고를 숨기지 않는다.
- 최종 기준: 9개 범위 연결, unknown side effect 자동 반복 없음, input/result commit 확인, crash/reopen queue/time 보존, failed attempt 비용 포함, 상태 정확성, nonTTY OSC 없음, MCP trust/hidden enforcement, 기존 독립 검증과 fencing 통과.

## 8. 위험과 제한

- 외부 효과와 SQLite는 단일 transaction이 아니다. 정확히 한 번 실행 대신 결과 불명 식별과 명시적 복구를 제공한다.
- provider retry는 이미 청구된 생성 비용을 취소하지 못하고 OAuth rotation 후 강제 종료도 완전 복구하지 못한다.
- WAL synchronous=NORMAL의 전원 장애 내구성을 과장하지 않는다. 복구는 저장된 상태와 프로세스 crash 중심이다.
- project MCP trust는 코드 실행 승인이고 exposure는 sandbox/파일 접근 권한이 아니다.
- OSC auto는 TTY 보고 정책이지 지원 감지 완료가 아니다.
- 새 schema/실행 경계가 가장 큰 변경이다. 여기의 계약·crash fixtures를 고정한 뒤 UI/MCP를 넓힌다.

## 9. 근거

- Pi release: https://github.com/earendil-works/pi/releases/tag/v1.1.0
- MCP/tool exposure: https://github.com/earendil-works/pi/blob/v1.1.0/packages/coding-agent/docs/mcp.md
- Durable queue/tools: https://github.com/earendil-works/pi/blob/v1.1.0/packages/durable/README.md
- Provider changes: https://github.com/earendil-works/pi/blob/v1.1.0/packages/ai/CHANGELOG.md
- OSC 7501: https://www.superlogical.com/rex/docs/build/program-status

Pi 코드를 그대로 복사하지 않고 동작 계약을 DUM-E native Rust와 기존 안전성 규칙에 맞춰 이식한다.
