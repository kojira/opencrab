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
