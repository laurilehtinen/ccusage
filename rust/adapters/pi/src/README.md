# pi-agent Source

Data source:

```text
${PI_AGENT_DIR:-~/.pi/agent/sessions/}
```

Commands:

```sh
ccusage pi daily
ccusage pi monthly
ccusage pi session
ccusage pi daily --json
ccusage pi daily --pi-path /path/to/sessions
```

Model labels are `[store] <id>` (default store `pi`). A log that already stored
a `[name] …` label is not wrapped again. Pricing strips that prefix after
trying the recorded spelling, so `[pi] composer-2.5` uses the Composer 2.5
estimate when LiteLLM has no entry.
