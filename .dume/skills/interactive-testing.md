---
name: interactive-testing
description: Test and debug DUM-E's native interactive mode in an isolated tmux terminal.
---

# Testing DUM-E Interactive Mode with tmux

Build the native executable from the repository root:

```sh
cargo build -p dume-cli --locked --offline
```

Use `target/debug/dume` (an absolute path when the session starts outside the repository). Do not use Node, Bun, or the nonexistent `dume-test.sh`.

## Isolation

- Allocate a unique tmux session name and temporary HOME and working directory.
- Pass a clean environment with only PATH, HOME, TERM, and explicitly required test settings. Do not inherit provider API keys or production credentials.
- Seed only fake credentials/catalog data in the temporary HOME. Use local provider fixtures in unit/integration tests; never send fake test prompts to live services.
- Keep the session database, artifacts, worktrees, MCP configuration, and trust records inside the test directories.
- Write any ad-hoc fixture scripts to temporary files before executing them.
- Do not kill unrelated tmux sessions or delete another session's work.

## Terminal scenario

```sh
tmux new-session -d -s <unique-name> -x 80 -y 24 -c <temporary-work-directory>
tmux send-keys -t <unique-name> 'env -i PATH="<path>" HOME="<temporary-home>" TERM=xterm-256color <absolute-native-binary> interactive --model <test-model>' Enter
tmux capture-pane -t <unique-name> -p
```

Submit a prompt and wait for its terminal result. Startup alone is not a passing interactive test. Without a local model fixture, use a deliberately invalid model with fake credentials to exercise admission and the failed-run path; report this as a failure-path check, not successful model inference. Successful inference/tool/queue behavior must also pass the scripted local-provider tests.

Check these behaviors:

1. Input is cleared only after durable admission; failures preserve the draft.
2. Busy Enter submits a steer; the configured follow-up binding submits a separate queued request.
3. Steer is consumed at a safe boundary. Follow-up begins only after success; failure/cancel leaves it paused.
4. Completed, failed, cancelled, and needs-attention outcomes are visibly distinct. Completion duration remains visible; session totals survive reopening.
5. Cancellation terminates the active executor before another run starts. Unknown side effects require explicit resolution rather than automatic replay.
6. Deferred MCP activation changes the next model request and survives reopening; untrusted/disabled/hidden tools do not execute.
7. Repeat at narrow terminal widths. Confirm normal editing, multiline input, and Korean text remain usable.
8. Capture OSC bytes separately where needed. Fixed status payloads must not contain prompt/tool/credential text, and redirected/non-TTY output must contain no OSC.

Capture the pane after the result and inspect the temporary SQLite state as evidence. Exit DUM-E normally, terminate only the uniquely named test session, and remove only this test's temporary files. OAuth/retry/crash fixtures are verified through the locked/offline Rust tests, not live accounts.
