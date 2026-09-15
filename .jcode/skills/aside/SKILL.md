---
name: aside
description: Drive a real Chrome browser via the Aside CLI (AI browser agent) for genuinely browser-only work, including visual site checks, web-only UI actions, and multi-step tasks where no suitable native MCP, CLI, or API integration is available. For connected SaaS services, use the native integration first.
---

# Aside — AI browser agent

Aside is a browser agent with its own model loop: give it the whole task in one prompt, and it navigates, reads, and acts on its own.

## Native integrations come first

Before using a browser for a connected service such as Google Workspace, Slack, Granola,
Jira, a CRM, or an analytics platform, check for and use its suitable MCP, CLI, or API
integration. Do not use Aside merely because the service is authenticated in Chrome.

Use Aside only when the task genuinely requires a visual or web-only interface, no suitable
native integration exists, or the native route was attempted and is blocked. State the
fallback reason before browsing. Never use a browser to work around native authentication,
configuration, or permission failures.

## The one rule

**Hand it a complete task, not a sequence of steps.** Describe what to find, what to click/change,
and what to report back — in one prompt.

```
aside exec "Log into LinkedIn Campaign Manager, open the Attribution Black Hole campaign,
and report total spend, clicks, and CTR per ad set for the last 7 days." --effort high
```

Not this (driving it step-by-step burns tokens and time for no benefit — it has its own model,
let it use it):

```
aside repl "await openTab('linkedin.com')"
aside repl "click the campaigns tab"
aside repl "type 'Attribution' into search"
```

## Commands

- `aside exec "<prompt>" [--effort high|xhigh|ultrabrowse] [--session <id>]` — the default. Runs
  a full browser-agent session against the prompt and returns its final report as text.
  - `--effort high` for most tasks; `--effort ultrabrowse` for open-ended or judgment-heavy tasks
    (research, multi-page synthesis) where you want its highest thinking level.
  - `--session <id>` continues a prior session (keeps the same tab/login state) instead of
    starting fresh — use this for a multi-turn follow-up on the same page.
- `aside repl "<js>"` — raw JS evaluation in the live page. Only reach for this when `exec` has
  genuinely failed twice on the same UI path, or you need one specific DOM value via
  `page.evaluate()`. If you do use it, batch the whole sequence (navigate, click, type, read)
  into one call — don't screenshot after every micro-step, only at start and end.
- `aside <url>` — just open a page (no agent task).

## Troubleshooting

- `ECONNREFUSED` on any `aside` call means the Aside desktop app isn't running:
  `open -a "/Applications/Aside.app"`, wait ~8s, retry.
- Rate-limited (e.g. LinkedIn 429s)? Back off with a real wait (`ScheduleWakeup`, not chained
  sleeps). This is rare with `exec` since it's one session, not N separate navigations.

## Browser routing

When the native-first check permits browser use, use `/aside` and `/Applications/Aside.app`.
Give Aside one complete, high-level prompt and let it navigate, read, and act through its
authenticated Chrome session.
