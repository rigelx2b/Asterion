# Claude Code 개발환경 자동 구성 지침
## Rust 기반 다중 미래 시점 주가 예측 프로젝트

> **목적:** 이 문서를 Claude Code에 제공하여, 현재 Windows 개발환경을 점검하고 Rust·딥러닝·금융 시계열 연구에 필요한 Skills, Plugins, MCP 서버 및 개발 도구를 안전하게 설치·설정하도록 한다.
>
> **중요:** 설치 명령을 무조건 일괄 실행하지 않는다. 먼저 현재 Claude Code 버전, 공식 문서, 실제 마켓플레이스 등록 여부, 패키지 유지보수 상태를 확인한다. 확인되지 않는 플러그인이나 MCP는 임의로 설치하지 말고 대안을 제시한다.

---

# 1. 프로젝트 목표와 개발 원칙

이 프로젝트는 Rust를 주 언어로 사용하여 주식의 여러 미래 거래일 가격 및 수익률을 예측하는 딥러닝 시스템을 개발한다.

예측 기간 예시:
- 1거래일 후
- 3거래일 후
- 5거래일 후
- 10거래일 후
- 20거래일 후

초기 기술 후보:
- Rust / Cargo
- Burn 또는 Candle
- Polars
- Serde
- Reqwest
- Chrono
- SQLite 또는 Parquet
- Plotters

핵심 연구 목표는 **미래 수익률 방향성 예측 정확도 80%**다. 이는 달성 보장치가 아니라 실험 목표다. 실제 out-of-sample 성능이 확인되지 않으면 달성했다고 주장하지 않는다.

개발 원칙:
1. Rust를 핵심 구현 언어로 사용한다.
2. Python은 필수 런타임이나 핵심 모델 구현 언어로 사용하지 않는다. 필요한 경우 보조 분석에 한정한다.
3. 데이터 누수, 생존 편향, 룩어헤드 편향을 엄격히 방지한다.
4. 모델 복잡도를 높이기 전에 단순 기준 모델과 비교한다.
5. 모든 라이브러리 API는 설치된 버전의 공식 문서로 확인한다.
6. 모든 설치·설정 후 실제 명령으로 동작 여부를 검증한다.
7. 프로젝트 파일과 기존 설정을 백업하거나 보존한다. 기존 파일을 임의로 덮어쓰지 않는다.

---

# 2. 작업 시작 전 필수 점검

먼저 프로젝트 루트에서 다음을 확인한다.

```powershell
$PSVersionTable
rustc --version
cargo --version
rustup show
claude --version
claude doctor
claude plugin --help
claude mcp --help
git --version
```

추가 확인:
```powershell
rustup component list --installed
Get-Command claude -All
claude plugin marketplace list
claude plugin list
claude mcp list
```

명령이 현재 버전에서 지원되지 않으면 공식 Claude Code 문서를 확인하고 동등한 최신 명령을 사용한다.

다음 사항을 보고한다.
- Windows 및 PowerShell 버전
- Rust toolchain 및 target
- Claude Code 버전과 설치 경로
- Git 설치 여부
- 프로젝트 루트 및 기존 `.claude` 설정
- 현재 설치된 Plugins, MCP, Skills
- NVIDIA GPU 유무와 `nvidia-smi` 실행 가능 여부

GPU 확인:
```powershell
nvidia-smi
```

`nvidia-smi`가 없거나 GPU가 없더라도 CPU 환경 구성을 진행한다. GPU 종류와 드라이버를 확인하기 전 CUDA Toolkit을 임의로 설치하지 않는다.

**점검 결과를 먼저 사용자에게 요약하고, 설치 계획을 제시한다.** 계정 로그인, OAuth 승인, 권한 허용, 토큰 생성 등 사용자 상호작용이 필요한 단계는 사용자가 직접 수행하도록 안내한다.

---

# 3. Rust 개발 도구 설치

Rust가 설치되어 있지 않으면 공식 rustup 설치 절차를 안내한다.

공식 사이트:
- https://rustup.rs/
- https://www.rust-lang.org/tools/install

설치 후 다음을 실행한다.

```powershell
rustup update
rustup component add rust-analyzer
rustup component add clippy
rustup component add rustfmt
```

필요한 Windows C/C++ 빌드 도구가 없는 경우에만 Microsoft 공식 Visual Studio Build Tools 설치를 안내한다.
- https://visualstudio.microsoft.com/visual-cpp-build-tools/

설치할 때 C++ 빌드 도구 및 적절한 Windows SDK를 선택한다. 이미 설치되어 있으면 중복 설치하지 않는다.

품질 및 테스트 도구:
```powershell
cargo install cargo-nextest --locked
cargo install cargo-audit --locked
cargo install cargo-deny --locked
```

설치 실패 시 오류를 확인한다. 무조건 재시도하거나 다른 출처의 바이너리를 설치하지 않는다.

선택 도구:
- `cargo-watch`: 파일 변경 감지 및 재실행
- `criterion`: 벤치마킹 라이브러리(Cargo 의존성으로 프로젝트별 추가)
- `cargo-llvm-cov`: 코드 커버리지

이 도구들은 필수 설치 목록과 구분하여 필요할 때만 설치한다.

검증:
```powershell
rustc --version
cargo --version
rust-analyzer --version
cargo clippy --version
cargo fmt --version
cargo nextest --version
cargo audit --version
```

---

# 4. Claude Code Plugins 설치

## 4.1 공식 마켓플레이스 확인

Anthropic 공식 플러그인 저장소:
https://github.com/anthropics/claude-plugins-official

먼저 현재 등록된 마켓플레이스 목록을 확인한다.

```powershell
claude plugin marketplace list
```

공식 마켓플레이스가 없고 현재 CLI 문서상 아래 명령이 유효한 경우에만 추가한다.

```powershell
claude plugin marketplace add anthropics/claude-plugins-official
```

그 후 실제 플러그인 목록 및 플러그인별 README를 확인한다. 아래 플러그인 ID가 실제 존재하고 현재 버전과 호환되는 경우에만 설치한다.

## 4.2 설치 후보

| 후보 | 목적 | 우선순위 |
|---|---|---|
| `rust-analyzer-lsp` | Rust 코드 진단 및 심볼 탐색 | 높음 |
| `code-review` | 변경 코드 리뷰 | 높음 |
| `feature-dev` | 기능 분석·설계·구현 워크플로 | 높음 |
| `commit-commands` | Git 커밋 관련 작업 | 보통 |

설치 명령 형식(실제 플러그인 ID를 확인한 뒤 사용):
```powershell
claude plugin install rust-analyzer-lsp@claude-plugins-official --scope project
claude plugin install code-review@claude-plugins-official --scope project
claude plugin install feature-dev@claude-plugins-official --scope project
claude plugin install commit-commands@claude-plugins-official --scope project
```

주의:
- 플러그인 ID 또는 마켓플레이스 이름이 현재 등록 정보와 다르면 추측해서 설치하지 않는다.
- Rust Analyzer LSP 플러그인이 제공되지 않더라도 `rust-analyzer` 바이너리와 VS Code 확장 설치는 별도로 가능하다.
- 이미 설치된 플러그인은 중복 설치하지 않는다.
- 프로젝트 범위 설치를 우선한다. 사용자 전체 범위 설치는 사용자 동의를 받은 경우에만 한다.

## 4.3 커뮤니티 Rust Skills 플러그인 후보

다음 저장소는 커뮤니티 후보로만 취급한다.
https://github.com/actionbook/rust-skills

저장소의 실제 설치 지침, 라이선스, 최근 업데이트, 스킬 내용, 설치 스크립트 권한을 검토한다. 저장소가 존재하거나 신뢰할 수 있는지 확인하지 못하면 설치하지 않는다.

설치 방식은 저장소 README와 현재 Claude Code 공식 문서를 따른다. `marketplace add`나 `plugin install` 명령을 임의로 가정하지 않는다.

---

# 5. MCP 서버 설정

MCP는 필요한 서버만 최소 권한으로 구성한다. MCP 서버는 코드나 데이터에 접근할 수 있으므로 저장소, 패키지, 권한을 확인한다.

## 5.1 Context7 — 라이브러리 공식 문서 조회

목적:
- Burn, Polars, Serde, Tokio, Reqwest 등 최신 API 확인
- 버전별 API 차이 확인
- 존재하지 않는 API를 임의로 작성하는 문제 감소

후보 저장소:
https://github.com/upstash/context7

먼저 공식 README에서 현재 권장되는 Claude Code 설치법과 인증 요구사항을 확인한다. 공식 Context7 플러그인 마켓플레이스가 현재 제공된다면 플러그인 방식으로 설치한다. 그렇지 않으면 공식 문서에 명시된 MCP 연결 방식을 사용한다.

설치가 끝나면 `/mcp`에서 연결 상태를 확인한다. 인증이 필요하면 사용자에게 OAuth 또는 API Key 입력을 요청한다. 비밀키를 소스 코드, `.claude` 설정 파일, 로그에 노출하지 않는다.

## 5.2 GitHub MCP — 저장소 및 PR 작업

공식 저장소:
https://github.com/github/github-mcp-server

목적:
- 저장소 및 코드 탐색
- Issue 및 Pull Request 관리
- 변경 사항 검토
- GitHub 작업 흐름 지원

공식 README에서 최신 HTTP 연결 주소, 인증 방식, 권한 범위를 확인한다. 현재 문서가 지원하는 경우 다음 연결 방식을 후보로 사용한다.

```powershell
claude mcp add --transport http github https://api.githubcopilot.com/mcp/
```

명령이 현재 Claude Code 버전에서 유효한지 먼저 확인한다. GitHub 로그인 및 OAuth 권한 승인은 사용자가 직접 수행한다. 가능한 최소 권한을 선택한다. GitHub MCP를 통한 쓰기 작업은 사용자 승인 없이 실행하지 않는다.

## 5.3 Fetch MCP — 웹 문서 가져오기 (선택)

공식 MCP 서버 및 유지보수 상태를 확인한 뒤 설치한다.
- https://github.com/modelcontextprotocol/servers

Fetch 서버의 현재 권장 설치법과 패키지 이름을 README에서 확인한다. 과거 패키지 이름이나 오래된 `uvx` 명령을 검증 없이 사용하지 않는다.

## 5.4 SQLite MCP — 로컬 DB 조회 (선택)

SQLite를 실제 프로젝트 데이터 저장소로 채택한 뒤에만 검토한다.

공식 MCP 서버 저장소의 유지보수 상태, 지원 도구, 파일 접근 범위를 확인한다. 초기에는 가능하면 읽기 전용 DB 또는 테스트 DB에 연결한다. 학습 데이터나 예측 결과를 삭제·수정할 수 있는 쓰기 권한은 별도 승인 없이 부여하지 않는다.

## 5.5 MCP 설치 후 확인

```powershell
claude mcp list
```

Claude Code 내부:
```text
/mcp
```

MCP 서버별로 다음을 기록한다.
- 서버 이름 및 출처
- 연결 방식
- 부여된 권한
- 인증 필요 여부
- 실제 연결 테스트 결과

MCP가 많아져 도구 선택이 혼란스러워지면 사용하지 않는 서버를 제거하거나 비활성화한다.

---

# 6. 프로젝트 전용 Skills 생성

프로젝트 루트에 다음 폴더를 만든다.

```text
.claude/
└── skills/
    ├── time-series-forecasting/
    │   └── SKILL.md
    ├── quantitative-finance/
    │   └── SKILL.md
    ├── deep-learning/
    │   └── SKILL.md
    ├── financial-data-engineering/
    │   └── SKILL.md
    ├── model-evaluation/
    │   └── SKILL.md
    └── burn-framework/
        └── SKILL.md
```

각 `SKILL.md`는 Claude Code의 현재 Skills 공식 형식에 맞게 작성한다. 설명(description)은 스킬이 적용되어야 하는 상황을 명확히 기술한다.

## 6.1 time-series-forecasting/SKILL.md

```markdown
---
name: time-series-forecasting
description: Use when designing, implementing, training, or evaluating multi-horizon financial time-series forecasting models.
---

# Time Series Forecasting

- Treat the task as financial multi-horizon forecasting.
- Preserve chronological ordering.
- Never use future information in historical features.
- Support configurable lookback windows and forecast horizons.
- Compare LSTM, GRU, TCN, and Transformer only when justified.
- Evaluate every forecast horizon independently.
- Use walk-forward validation.
- Analyze concept drift and market regime changes.
- Compare against naive and statistical baselines.
- Never claim 80% accuracy without untouched out-of-sample evidence.
```

## 6.2 quantitative-finance/SKILL.md

```markdown
---
name: quantitative-finance
description: Use for stock market data analysis, return calculations, quantitative research, and backtesting.
---

# Quantitative Finance

- Distinguish price prediction from return prediction.
- Use adjusted prices appropriately and document adjustment assumptions.
- Account for splits, dividends, delistings, and trading calendars.
- Prevent look-ahead bias and survivorship bias.
- Analyze volatility and market regimes.
- Include transaction costs, spread, and slippage in backtests.
- Evaluate CAGR, Sharpe Ratio, and Maximum Drawdown.
- Do not equate directional accuracy with investment profitability.
```

## 6.3 deep-learning/SKILL.md

```markdown
---
name: deep-learning
description: Use when designing, implementing, training, or optimizing neural networks.
---

# Deep Learning

- Explain tensor shapes at every model stage.
- Verify forward passes, gradients, and optimizer updates.
- Compare MSE, MAE, and Huber loss where appropriate.
- Prevent overfitting with regularization and early stopping.
- Track training and validation metrics separately.
- Keep experiments reproducible with recorded seeds and configurations.
- Use ablation studies for architecture comparisons.
- Prefer simple baselines before complex architectures.
```

## 6.4 financial-data-engineering/SKILL.md

```markdown
---
name: financial-data-engineering
description: Use when collecting, processing, storing, or validating financial market data.
---

# Financial Data Engineering

- Use documented and legally accessible data sources.
- Respect API rate limits and terms of service.
- Handle missing, duplicate, stale, and invalid records.
- Normalize timestamps and trading calendars.
- Account for corporate actions.
- Validate OHLCV consistency.
- Cache historical data and record provenance.
- Store collection timestamps and source metadata.
- Prevent future-data leakage in all transformations.
```

## 6.5 model-evaluation/SKILL.md

```markdown
---
name: model-evaluation
description: Use when validating forecasting models, calculating metrics, or performing financial backtests.
---

# Model Evaluation

- Split training, validation, and test periods chronologically.
- Never tune hyperparameters on the final test period.
- Perform walk-forward validation.
- Report MAE, RMSE, and suitable percentage error metrics.
- Report directional accuracy, precision, recall, F1, and balanced accuracy.
- Evaluate every forecast horizon independently.
- Compare with naive baselines such as previous close and historical drift.
- Report sample counts and uncertainty intervals where appropriate.
- Include transaction costs and slippage in strategy backtests.
- Disclose weak, unstable, or statistically insignificant results.
```

## 6.6 burn-framework/SKILL.md

```markdown
---
name: burn-framework
description: Use when implementing neural networks, tensor operations, training, or inference with the Rust Burn framework.
---

# Burn Framework

- Consult current official Burn documentation.
- Verify every API against the exact version in Cargo.lock.
- Explain tensor dimensions and device/backend choices.
- Separate model architecture, data pipeline, training, and inference.
- Test forward passes and tensor shapes.
- Verify gradients and optimizer updates.
- Test checkpoint save/load and inference reproducibility.
- Confirm backend compatibility before enabling GPU acceleration.
- Run cargo check, cargo test, and cargo clippy.
- Do not invent Burn APIs or copy examples from incompatible versions.
```

스킬 생성 후 Claude Code가 인식하는지 확인한다. 프로젝트에 이미 같은 이름의 스킬이 있다면 기존 내용을 읽고 병합 여부를 사용자에게 확인한다.

---

# 7. 프로젝트 전용 Subagents 제안

현재 Claude Code 버전의 Subagent 기능과 프로젝트 설정 형식을 확인한 뒤 아래 역할을 선택적으로 만든다. Subagent는 별도 전문 검토자이며, 서로 다른 에이전트가 같은 파일을 동시에 수정하지 않도록 한다.

## 7.1 data-quality-reviewer
책임:
- 데이터 결측치, 중복, 이상치 검사
- 거래일 정렬과 기업행사 반영 검토
- 데이터 누수 및 룩어헤드 편향 탐지
- 원천 데이터와 전처리 결과 비교

## 7.2 model-researcher
책임:
- LSTM, GRU, TCN, Transformer 후보 조사
- 다중 미래 시점 출력 구조 검토
- 손실 함수와 학습 전략 비교
- 최신 논문 및 공식 문서 근거 제시

## 7.3 quant-evaluator
책임:
- Walk-forward validation 설계
- 방향성 정확도 및 가격 오차 계산 검증
- 기준 모델과 성능 비교
- 백테스트 편향, 거래 비용, 슬리피지 검토
- 80% 목표 달성 여부를 엄격하게 판단

## 7.4 rust-reviewer
책임:
- Rust 타입 및 소유권 검토
- 오류 처리와 비동기 코드 검토
- 성능 및 메모리 사용 검토
- Clippy 및 테스트 결과 분석

Subagent는 기본적으로 읽기·분석 중심으로 운용하고, 코드 변경이 필요하면 주 작업 에이전트가 변경하도록 한다.

---

# 8. Hooks 및 자동 검증

현재 Claude Code Hooks 공식 문서를 확인하고, 프로젝트 범위에서 안전한 검증 Hook을 설정한다.

권장 검증 명령:
```powershell
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

주의:
- 초기에는 자동 실행보다 수동 검증 명령으로 안정성을 확인한다.
- Hook에서 네트워크 접근, 데이터 삭제, 모델 학습 전체 실행을 자동화하지 않는다.
- Hook 실패가 코드 수정이나 파일 삭제로 이어지지 않도록 한다.
- 대규모 학습은 명시적으로 요청한 경우에만 실행한다.
- 기존 Hook 설정을 덮어쓰지 않는다.

---

# 9. 최종 설치 검증

모든 설치 작업이 끝나면 아래를 실행한다.

```powershell
rustc --version
cargo --version
rust-analyzer --version
cargo clippy --version
cargo fmt --version
cargo nextest --version
cargo audit --version

claude --version
claude plugin list
claude plugin marketplace list
claude mcp list
```

Claude Code 내부에서:
```text
/plugin
/mcp
/skills
```

프로젝트 전용 스킬 폴더 확인:
```powershell
Get-ChildItem .claude/skills -Recurse
```

임시 Rust 프로젝트를 만들거나 기존 프로젝트에 영향을 주지 않는 테스트 디렉터리를 사용하여 검증한다.

```powershell
cargo new .tmp_rust_env_check
Set-Location .tmp_rust_env_check
cargo check
cargo test
Set-Location ..
Remove-Item .tmp_rust_env_check -Recurse -Force
```

임시 디렉터리가 이미 존재하면 삭제하지 말고 다른 임시 이름을 사용한다.

---

# 10. 작업 결과 보고 형식

작업을 마친 후 다음 표로 결과를 보고한다.

| 항목 | 상태 | 버전/출처 | 비고 |
|---|---|---|---|
| Rust toolchain | 설치/기존/실패 | 버전 | |
| rust-analyzer | 설치/기존/실패 | 버전 | |
| Clippy / rustfmt | 설치/기존/실패 | 버전 | |
| Cargo 보조 도구 | 설치/기존/실패 | 버전 | |
| Claude Plugins | 설치/기존/실패/미지원 | 실제 ID | |
| Context7 MCP | 연결/실패/미설치 | 공식 출처 | |
| GitHub MCP | 연결/실패/미설치 | 공식 출처 | |
| 기타 MCP | 연결/실패/미설치 | 공식 출처 | |
| 프로젝트 Skills | 생성/기존/실패 | 경로 | |
| Subagents | 생성/기존/미지원 | 경로 | |
| Hooks | 설정/기존/미설정 | 경로 | |
| 검증 | 통과/실패 | 실행 결과 | |

반드시 다음을 별도로 보고한다.
1. 실제 설치된 항목과 설치되지 않은 항목
2. 설치 실패 원인 및 해결 방법
3. 사용자가 직접 완료해야 하는 로그인·인증 단계
4. 추가되거나 수정된 파일 목록
5. 실행한 검증 명령과 결과
6. 불필요하거나 중복되어 설치하지 않은 항목
7. 보안상 주의가 필요한 권한

---

# 11. Claude Code 실행 지시

이 문서를 받은 뒤 다음 순서로 행동한다.

1. 현재 환경과 프로젝트 설정을 조사한다.
2. 공식 문서 및 실제 패키지 등록 여부를 확인한다.
3. 설치 계획과 변경 파일 목록을 사용자에게 먼저 제시한다.
4. 승인된 범위에서 Rust 도구와 프로젝트 Skills를 구성한다.
5. 공식 플러그인과 MCP를 검증 후 설치한다.
6. 로그인이나 OAuth 등 사용자 승인이 필요한 단계에서는 중단하고 안내한다.
7. Subagents 및 Hooks는 현재 버전의 지원 여부를 확인한 후 구성한다.
8. 설치 후 모든 항목을 검증한다.
9. 결과를 10번 형식으로 보고한다.

**절대 하지 말 것**
- 존재 여부를 확인하지 않은 플러그인·MCP를 설치하는 행위
- 출처가 불분명한 PowerShell 스크립트 실행
- API Key, Access Token, 비밀번호를 파일이나 로그에 기록
- 사용자 동의 없는 GitHub 쓰기 작업
- 기존 `.claude` 설정이나 프로젝트 파일을 무단으로 덮어쓰기
- CUDA 호환성을 확인하지 않고 CUDA Toolkit 설치
- 모델 학습이나 대량 데이터 다운로드를 사용자 승인 없이 시작
- 정확도 80%를 근거 없이 달성했다고 주장

최종 목표는 단순히 많은 도구를 설치하는 것이 아니라, **안전하고 재현 가능하며 검증 가능한 Rust 금융 시계열 딥러닝 개발환경**을 만드는 것이다.
