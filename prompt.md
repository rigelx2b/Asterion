# Asterion 개발환경 재현 프롬프트

> **사용법 (사람이 읽는 부분)**
> 1. 새 컴퓨터에서 저장소를 클론한다.
>    ```
>    git clone https://github.com/rigelx2b/Asterion.git
>    cd Asterion
>    ```
> 2. 그 폴더에서 Claude Code를 실행하고 폴더 신뢰(trust)를 승인한다.
> 3. 아래 **「프롬프트」** 구분선 이하 전체를 Claude Code에 붙여넣거나, `@prompt.md 파일을 읽고 지침을 수행해 줘`라고 입력한다.
>
> 저장소에는 이미 프로젝트 Skills, Subagents, Hook, 플러그인 활성화 목록(`.claude/`)이 들어 있다. 이 프롬프트는 그 설정을 **덮어쓰지 않고**, 컴퓨터마다 따로 설치해야 하는 도구(Rust, Cargo 보조 도구, 플러그인 캐시, MCP 인증, Git 작성자 정보)만 맞춘다.

---

## 프롬프트

이 저장소(Asterion)는 Rust로 주식의 여러 미래 거래일(1·3·5·10·20일) 수익률을 예측하는 딥러닝 연구 프로젝트다. 이 컴퓨터에서 원래 개발환경과 같은 상태를 재현해 줘. 진행 순서는 다음과 같다.

1. 현재 환경을 점검한다.
2. 점검 결과와 설치 계획을 먼저 요약한다.
3. 안전성을 확인하면서 설치한다.
4. 모든 항목을 실제 명령으로 검증한다.
5. 마지막에 결과표로 보고한다.

로그인, OAuth, 토큰 생성처럼 사용자 상호작용이 필요한 단계에서는 멈추고 안내한다.

### 0. 기준 환경 (원본 컴퓨터, 2026-11-10 검증)

| 항목 | 원본 버전 | 비고 |
|---|---|---|
| OS | Windows 11, PowerShell 5.1 | macOS/Linux도 가능 (아래 OS별 메모 참고) |
| Rust | rustc / cargo 1.99.1, stable 툴체인 | Windows는 `x86_64-pc-windows-msvc` |
| rustup components | rust-analyzer, clippy, rustfmt, rust-docs | |
| Cargo 보조 도구 | cargo-nextest 0.9.151, cargo-audit 0.22.2, cargo-deny 0.20.2 | 참고 버전이며 고정 버전 아님 |
| Claude Code | 2.1.291 | |
| Node.js | 24.x | Hook 스크립트 실행에 필요 (18 이상) |
| Git | 2.55 | |
| GPU | NVIDIA 없음 (CPU 기준 구성) | |

버전이 달라도 괜찮다. 다만 차이가 있으면 보고한다.

### 1. 사전 점검

다음을 실행하고 결과를 요약한다. 명령이 현재 버전에서 지원되지 않으면 공식 문서에서 같은 기능의 최신 명령을 찾아 사용한다.

```
rustc --version
cargo --version
rustup show
rustup component list --installed
cargo install --list
claude --version
claude plugin marketplace list
claude plugin list
claude mcp list
git --version
node --version
nvidia-smi
```

추가로 확인할 것:
- **Windows**: C++ 빌드 도구(Visual Studio 또는 Build Tools의 "C++를 사용한 데스크톱 개발")와 Windows SDK. `vswhere.exe`로 확인한다. 이미 있으면 중복 설치하지 않는다.
- **macOS**: `xcode-select -p`
- **Linux**: `cc --version`, `pkg-config --version`, OpenSSL 개발 헤더
- `git status`와 `git remote -v`. origin이 `https://github.com/rigelx2b/Asterion.git`인지 확인한다.
- `.claude/` 내용. `settings.json`, `skills/` 6개, `agents/` 4개, `hooks/rust-fmt-check.mjs`가 있어야 한다.

### 2. Rust 개발 도구

- Rust가 없으면 공식 rustup 설치 절차(https://rustup.rs/)를 **안내만** 한다. 설치 스크립트는 사용자가 직접 실행한다.
- C++ 빌드 도구가 없을 때만 공식 설치 경로를 안내한다.
  - Windows: https://visualstudio.microsoft.com/visual-cpp-build-tools/
  - macOS: `xcode-select --install`
  - Linux: 배포판 패키지 관리자
- Rust가 있으면 다음을 실행한다.
  ```
  rustup update
  rustup component add rust-analyzer clippy rustfmt
  ```
- 이미 설치되지 않은 Cargo 보조 도구만 설치한다. 빌드가 오래 걸리므로 백그라운드로 실행한다.
  ```
  cargo install cargo-nextest --locked
  cargo install cargo-audit --locked
  cargo install cargo-deny --locked
  ```
  설치가 실패하면 오류를 분석해 보고한다. 무작정 재시도하거나 다른 출처의 바이너리를 받지 않는다.
- 선택 도구(cargo-watch, cargo-llvm-cov)는 요청이 있을 때만 설치한다.

### 3. Claude Code 플러그인 (프로젝트 범위)

`.claude/settings.json`의 `enabledPlugins`에는 다음 5개가 이미 활성화로 기록돼 있다. 하지만 새 컴퓨터에는 플러그인 캐시가 없으므로 설치가 필요할 수 있다.

| 플러그인 ID | 목적 |
|---|---|
| `rust-analyzer-lsp@claude-plugins-official` | Rust 진단과 심볼 탐색 (`rust-analyzer` 바이너리 필요) |
| `code-review@claude-plugins-official` | 변경 코드 리뷰 |
| `feature-dev@claude-plugins-official` | 기능 분석·설계·구현 워크플로 |
| `commit-commands@claude-plugins-official` | Git 커밋 작업 |
| `context7@claude-plugins-official` | Context7 문서 조회 MCP (HTTP 호스팅) |

절차:
1. `claude plugin marketplace list`에 `claude-plugins-official`(GitHub `anthropics/claude-plugins-official`)이 있는지 확인한다. 없을 때만 `claude plugin marketplace add anthropics/claude-plugins-official`을 실행한다.
2. 마켓플레이스 목록에서 위 5개 ID가 실제로 있는지 확인한다. 없거나 이름이 다르면 추측해서 설치하지 않고 보고한다.
3. `claude plugin list`에서 이 프로젝트에 이미 설치·활성화된 것은 건너뛴다. 나머지만 다음 형식으로 설치한다.
   ```
   claude plugin install <ID> --scope project
   ```
4. 설치 후 `.claude/settings.json`을 다시 읽는다. 기존 `hooks` 블록이 그대로 남아 있는지 확인한다. 사라졌거나 바뀌었으면 `git diff`로 원래 내용을 복원한다.
5. 사용자 전역(user) 범위 설치는 사용자 동의 없이 하지 않는다.

**설치하지 말 것: `rust-skills@rust-skills` (actionbook/rust-skills)**
원본 환경 검토에서 제외한 커뮤니티 플러그인이다. 이유는 다음과 같다.
- 버전이 고정되지 않은 `npx -y @actionbookdev/mcp@latest`를 자동 실행한다.
- 흔한 단어에도 발동하는 `UserPromptSubmit` Hook이 강제 출력 형식을 주입한다.
- 스킬이 38개라 컨텍스트 부담이 크다.

이 컴퓨터에 해당 마켓플레이스가 등록돼 있어도 이 프로젝트에서는 켜지 않는다.

### 4. MCP 서버

- **Context7**: 플러그인으로 들어온다. `claude mcp list`에 `plugin:context7:context7`이 보여야 한다. "Needs authentication"이면 사용자에게 `/mcp`에서 직접 인증하라고 안내한다. Context7에 조회 내용이 외부로 전송된다는 점을 알린다.
- **GitHub MCP (선택)**: 사용자가 원할 때만 설치한다.
  1. 사용자가 `rigelx2b/Asterion` 저장소에만 접근하는 fine-grained PAT를 직접 만든다. 처음에는 읽기 권한만 준다.
  2. 사용자가 OS 사용자 환경 변수 `GITHUB_PERSONAL_ACCESS_TOKEN`에 그 토큰을 저장한다.
  3. 그다음 `claude plugin install github@claude-plugins-official --scope project`를 실행한다.

  토큰 값은 절대 파일, 로그, 대화에 기록하지 않는다. GitHub 쓰기 작업은 사용자 승인 없이 실행하지 않는다.
- **Fetch MCP**: 설치하지 않는다. Claude Code 내장 WebFetch와 기능이 겹친다.
- **SQLite MCP**: 프로젝트가 SQLite를 실제 저장소로 채택한 뒤에만 검토한다. 처음에는 읽기 전용 DB에만 연결한다.

### 5. 저장소에 포함된 프로젝트 설정 (검증만, 재생성 금지)

다음 파일은 Git으로 관리된다. **새로 만들거나 덮어쓰지 말고** 존재 여부와 인식 여부만 확인한다. 내용을 바꿔야 한다고 판단되면 먼저 사용자에게 제안한다.

- Skills (`.claude/skills/<name>/SKILL.md`): time-series-forecasting, quantitative-finance, deep-learning, financial-data-engineering, model-evaluation, burn-framework
- Subagents (`.claude/agents/`): data-quality-reviewer, model-researcher, quant-evaluator, rust-reviewer. 모두 Edit/Write 권한이 없는 읽기·분석 전용이다.
- Hook (`.claude/settings.json`의 `PostToolUse`): Edit/Write/MultiEdit 다음에 `node "$CLAUDE_PROJECT_DIR/.claude/hooks/rust-fmt-check.mjs"`를 실행한다.
  - `.rs` 파일이고 프로젝트 안에 `Cargo.toml`이 있을 때만 `cargo fmt --all -- --check`를 실행한다.
  - 차이가 있으면 1을 반환해 사용자에게 경고만 표시한다.
  - 파일 수정, 네트워크 접근, 작업 차단은 하지 않는다.

확인 명령:
```
claude plugin validate .claude
node -e "JSON.parse(require('fs').readFileSync('.claude/settings.json','utf8')); console.log('settings.json OK')"
```

### 6. Git 작성자 정보 (저장소 로컬 설정)

이 저장소에서만 다음 값을 쓴다. `--global`은 사용하지 않는다.
```
git config user.name "rigelx2b"
git config user.email "295735711+rigelx2b@users.noreply.github.com"
```
push는 Git Credential Manager 등으로 사용자가 직접 인증한다. 커밋과 push는 사용자가 요청할 때만 한다.

### 7. 최종 검증

```
rustc --version
cargo --version
rust-analyzer --version
cargo clippy --version
cargo fmt --version
cargo nextest --version
cargo audit --version
cargo deny --version
claude --version
claude plugin list
claude mcp list
```

**임시 Rust 프로젝트로 빌드를 검증한다.**
- 이름이 `.`으로 시작하는 디렉터리는 Cargo 패키지 이름으로 쓸 수 없다. 반드시 `--name`을 지정한다.
- 같은 이름의 디렉터리가 이미 있으면 삭제하지 말고 다른 이름을 쓴다.

```
cargo new .tmp_rust_env_check --name tmp_rust_env_check --vcs none
cd .tmp_rust_env_check
cargo check
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

**같은 임시 프로젝트로 Hook을 검증한다.**
- Hook 입력 JSON은 반드시 Node로 생성한다. 셸 echo나 heredoc을 거치면 Windows 경로의 `\\`가 망가진다. 그러면 Hook이 파싱 실패로 조용히 0을 반환해서 테스트가 무의미해진다.
- 기대 결과는 다음과 같다.

| 경우 | 기대 결과 |
|---|---|
| ① `.rs`가 아닌 파일 | 0 반환 |
| ② 포맷이 틀린 `src/main.rs` (예: `fn main(){println!("x");}`) | 1 반환, 경고 메시지 출력, 파일 내용은 변하지 않음 |
| ③ 포맷이 맞는 `src/main.rs` | 0 반환 |

검증이 끝나면 직접 만든 임시 디렉터리만 삭제한다.

Claude Code 안에서 `/plugin`, `/mcp`, `/skills`, `/agents`로 인식 상태를 확인하도록 사용자에게 안내한다.

### 8. 결과 보고 형식

| 항목 | 상태 | 버전/출처 | 비고 |
|---|---|---|---|
| Rust toolchain | 설치/기존/실패 | | |
| rust-analyzer | 설치/기존/실패 | | |
| Clippy / rustfmt | 설치/기존/실패 | | |
| Cargo 보조 도구 | 설치/기존/실패 | | |
| Claude Plugins | 설치/기존/실패/미지원 | 실제 ID | |
| Context7 MCP | 연결/인증 필요/실패 | 공식 플러그인 | |
| GitHub MCP | 연결/미설치 | 공식 플러그인 | |
| 프로젝트 Skills / Subagents / Hook | 확인/누락 | `.claude/` | |
| Git 작성자 | 설정/기존 | 로컬 config | |
| 검증 | 통과/실패 | 실행 결과 | |

결과표와 별도로 다음을 보고한다.
1. 실제 설치된 항목과 설치하지 않은 항목
2. 실패 원인과 해결 방법
3. 사용자가 직접 해야 하는 인증 단계
4. 추가되거나 수정된 파일 목록 (`git status` 기준)
5. 실행한 검증 명령과 결과
6. 중복이라 건너뛴 항목
7. 보안상 주의가 필요한 권한

### 절대 하지 말 것

- 존재를 확인하지 않은 플러그인이나 MCP 설치
- `rust-skills` 플러그인 활성화
- 출처가 불분명한 스크립트 실행 (rustup 설치 스크립트도 사용자가 직접 실행)
- API Key, Access Token, 비밀번호를 파일·로그·대화에 기록
- 기존 `.claude/` 설정이나 프로젝트 파일 덮어쓰기
- 사용자 동의 없는 Git 커밋·push 및 GitHub 쓰기 작업
- GPU와 드라이버 호환성을 확인하지 않은 CUDA Toolkit 설치
- 사용자 승인 없는 모델 학습이나 대량 데이터 다운로드
- 근거 없이 방향성 정확도 80%를 달성했다고 주장
