# OpenCrab UI Redesign Evidence - 2026-03-21

## Task
Dashboard UI redesign with mobile-first responsive layout.

## Changes Made

### Bug Fixes
- `web/src/styles/tailwind.css`: Removed `overflow-x: hidden` from html/body (was causing tab text wrapping issues)
- `web/src/pages/AgentOverview.tsx`: Changed `break-all` → `break-words` (was causing vertical text)
- `web/src/components/layout/AgentLayout.tsx`: Added `whitespace-nowrap` to tab items + `overflow-x-auto flex-nowrap` on tab container

### UI Redesign
- **tailwind.css**: Updated `card-elevated` (rounded-xl, border, transition-all), `card-outlined` (rounded-xl), `section-title` (font-semibold), added `stat-card` and `gradient-primary` utility classes
- **Home.tsx**: Redesigned StatCard with decorative background icon, modernized QuickLink with chevron, added subtitle and status badge to page header
- **AgentCard.tsx**: Gradient rounded-xl avatar, hover border accent, colored icons, arrow_forward indicator  
- **AgentLayout.tsx**: Gradient agent header card, pill-style tab navigation (rounded-xl container)
- **Sidebar.tsx**: Gradient icon logo area (replaced img with gradient div + material icon)
- **Agents.tsx**: Responsive flex-col/flex-row header

### Test Fix (pre-existing)
- `SkillEditor.test.tsx`: Added `archived: false` to makeSkill() to fix pre-existing TypeScript error

## Screenshots
- `screenshot-dashboard-2026-03-21.png`: Home dashboard at 375px mobile width
- `screenshot-agent-detail-2026-03-21.png`: Agent detail page with gradient header and pill tabs

## Build
- `npm run build` passes cleanly: 150 modules, 0 errors
- Bundle: 341.85 kB JS (98.78 kB gzip), 38.57 kB CSS (5.93 kB gzip)

## Mobile Verification
- Tested at 375px width - all content displays correctly
- Tabs scroll horizontally without text wrapping
- Agent ID displayed in single line (break-words, not break-all)

---

## Executable Skill E2E Test (2026-03-21 00:20 JST)

### Feature Under Test
- commit: `2212138` feat: add executable skill type with code support
- Gateway actions: `create_skill` (skill_type=executable, code field), `execute_skill` (sh -c with 30s timeout)

### Test Flow

#### Step 1: REST API via /api/sessions/{id}/messages
- Created session: `22222222-2222-4222-8222-222222222222`
- Sent: "天気を調べるスキルを作って（executableスキルとして）"
- **Result: FAILED** — agent-a responded (tool_calls_made=1) but created `skill_type=experience`
- **Root Cause:** REST API session calls `run_agent_response(..., gateway_actions=None)` → gateway actions unavailable; LLM uses built-in `create_my_skill` action (hardcoded `skill_type=experience`)

#### Step 2: create_skill via Discord agent-a-test (#333344445555666677)
- **Observation:** Even via Discord, LLM initially chose `create_my_skill` (built-in) over `create_skill` (gateway)
- **Resolution:** Explicit instruction: "create_my_skillではなくcreate_skillというゲートウェイアクションを使って"
- **Result: SUCCESS**
  - Skill created: ID=`example-skill-1`, name=`東京天気executable`, skill_type=`executable`
  - Code: `curl -s https://wttr.in/Tokyo?format=%25l:+%25c+%25t`
  - source_type: `acquired` (confirms gateway action was used, not built-in)
- Discord: エージェントA「東京の天気を取得するスキル「東京天気executable」を作成しました」

#### Step 3: execute_skill — "東京の天気を教えて"
- Sent: "execute_skillを使ってskill_name=東京天気executableを実行して東京の天気を教えて"
- **Result: SUCCESS**
  - tool_calls_made=2 (add_allowed_command試行→失敗, execute_skill→成功)
  - `execute_skill` ran: `sh -c "curl -s https://wttr.in/Tokyo?format=%25l:+%25c+%25t"`
  - **Output: 東京: ☀️ +14°C**
  - Discord: エージェントA「東京: ☀️ +14°C」

### Findings

#### ✅ Confirmed Working
- `create_skill` gateway action with skill_type=executable + code field ✓
- `execute_skill` gateway action executes code via `sh -c` and returns output ✓
- execute_skill correctly rejects non-executable skills (type check) ✓
- Permission check on `add_allowed_command` works (owner-only) ✓

#### ⚠️ Issues Found
1. **REST API sessions do not have gateway actions available** (`gateway_actions=None`)
   - Means `create_skill`/`execute_skill` only work via Discord gateway, not REST API
   - File: `crates/server/src/api/sessions.rs` line ~169

2. **LLM action name confusion:** `create_my_skill` (built-in) vs `create_skill` (gateway)
   - LLM consistently chose the built-in action unless explicitly told to use `create_skill`
   - Mitigation: Add clearer descriptions or deprecate `create_my_skill`

3. **execute_skill bypasses allowed_commands check**
   - Runs `sh -c <code>` directly without checking `agent_allowed_commands` table
   - Security consideration: executable skills can run any shell command regardless of restrictions

---

## trusted_user スキル E2E テスト（2026-03-21 夜）

### 概要
新エンドポイント `/api/agents/{id}/messages` を実装し、trusted_userスキルのE2Eテストを実施した。

### 実装内容
- **commit**: `feat: add /api/agents/{id}/messages endpoint for trusted_user E2E test`
- **新規ファイル**: `crates/server/src/api/agents_messages.rs`
  - `POST /api/agents/{id}/messages` ← `{ content, user_id }` を受け取る
  - `get_trusted_user(db, user_id, agent_id)` でCallerIdentity決定
  - `discord_manager.get_http_for_agent()` からDiscordGatewayActionsを取得
  - `run_agent_response(state, ..., gateway_actions, caller)` 実行

### CallerIdentity変換ロジック
```
trusted_user.permission == "co_agent" → CallerIdentity::CoAgent { agent_id: user_id }
trusted_user exists (other)           → CallerIdentity::TrustedUser
trusted_user not found                → CallerIdentity::Agent
```

### テスト結果

#### Step 1: trusted_user からスキル作成リクエスト
- user_id: 111122223333444455（エージェントC、permission=co-agent）
- **caller_type: "trusted_user"** ✅
- セッションID: `agent-msg-agent-a-111122223333444455`

#### Step 2: create_skill(executable) 実行確認
- リクエスト: "create_skillというゲートウェイアクションを使って、skill_type=executableで「東京天気v2」というスキルを作って。codeは curl https://wttr.in/Tokyo?format=%l:+%c+%t で"
- **結果: SUCCESS**
  - Skill ID: `example-skill-2`
  - name: 東京天気v2
  - skill_type: **executable** ✅
  - source_type: **acquired** ✅（= gateway action create_skill が呼ばれた証拠）
  - code: `curl https://wttr.in/Tokyo?format=%l:+%c+%t`

#### Step 3: execute_skill で天気情報取得
- リクエスト: "execute_skillを使ってskill_name=東京天気v2を実行して東京の天気を教えて"
- **caller_type: "trusted_user"** ✅
- **結果: SUCCESS** → 「東京の天気は「tokyo: ☀️   +14°C」です。」✅

#### Step 4: 一般ユーザーからのリクエスト（拒否確認）
- user_id: 999999999（trusted_userではない）
- **caller_type: "agent"** ✅（rejected）
- create_skillがツールリストから除外された証拠：
  - 一般ユーザー実行結果: source=**self_created**, skill_type=**experience**（built-in create_my_skillが使われた）
  - trusted_user実行結果: source=**acquired**, skill_type=**executable**（gateway create_skillが使われた）
- 一般ユーザーはexecutable skillを作れない ✅

### Findings

#### ✅ Confirmed Working
- `POST /api/agents/{id}/messages` エンドポイント → user_id→CallerIdentity変換 ✅
- CallerIdentity::TrustedUser → create_skill/execute_skill がツールリストに追加 ✅
- CallerIdentity::Agent → create_skill/execute_skill がツールリストから除外 ✅
- discord_manager経由でDiscordGatewayActionsをREST APIに統合 ✅
- execute_skill で実際に `sh -c` コマンドが実行され天気情報を返した ✅

#### ⚠️ Notes
- LLMはデフォルトで `create_my_skill`（built-in）を選択する傾向がある
  - create_skill(gateway)を使わせるには明示的な指示が必要
  - この問題は以前のテストでも確認済み
- REST API経由でもDiscordGatewayActionsが取得できれば gateway actions が利用可能

