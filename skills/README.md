# Workspace Skills

This directory is the workspace-local skill store.

There are two supported layouts in this repository:

- `*.skill.md`: legacy/self-created skills used by the runtime skill action.
- `<skill-name>/SKILL.md`: portable OpenClaw import layout, suitable for skills copied from external `skill.md` sources.

For external skills, prefer:

```text
skills/
  <skill-name>/
    SKILL.md
```

Keep the original source URL in the skill body when importing from the web.
