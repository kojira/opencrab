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
- Created session: `eeb24f06-2f66-4816-a3bd-fbd558b50dee`
- Sent: "天気を調べるスキルを作って（executableスキルとして）"
- **Result: FAILED** — kairo responded (tool_calls_made=1) but created `skill_type=experience`
- **Root Cause:** REST API session calls `run_agent_response(..., gateway_actions=None)` → gateway actions unavailable; LLM uses built-in `create_my_skill` action (hardcoded `skill_type=experience`)

#### Step 2: create_skill via Discord kairo-test (#1470698801395273861)
- **Observation:** Even via Discord, LLM initially chose `create_my_skill` (built-in) over `create_skill` (gateway)
- **Resolution:** Explicit instruction: "create_my_skillではなくcreate_skillというゲートウェイアクションを使って"
- **Result: SUCCESS**
  - Skill created: ID=`fdb8d880`, name=`東京天気executable`, skill_type=`executable`
  - Code: `curl -s https://wttr.in/Tokyo?format=%25l:+%25c+%25t`
  - source_type: `acquired` (confirms gateway action was used, not built-in)
- Discord: かいろ「東京の天気を取得するスキル「東京天気executable」を作成しました」

#### Step 3: execute_skill — "東京の天気を教えて"
- Sent: "execute_skillを使ってskill_name=東京天気executableを実行して東京の天気を教えて"
- **Result: SUCCESS**
  - tool_calls_made=2 (add_allowed_command試行→失敗, execute_skill→成功)
  - `execute_skill` ran: `sh -c "curl -s https://wttr.in/Tokyo?format=%25l:+%25c+%25t"`
  - **Output: 東京: ☀️ +14°C**
  - Discord: かいろ「東京: ☀️ +14°C」

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
