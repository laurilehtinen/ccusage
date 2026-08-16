# Cursor CLI Data Source

ccusage can read locally recorded Cursor Agent CLI and Cursor SDK usage as a supported data source. Cursor uses the same unified and focused report model as other agents.

## Focused Views

```bash
# Daily Cursor CLI usage
ccusage cursor daily

# Monthly Cursor CLI usage
ccusage cursor monthly

# Cursor CLI sessions
ccusage cursor session
```

Most users can start with unified reports such as `ccusage daily`. Add the `cursor` namespace only when you want to focus the same report shape on Cursor CLI usage.

## Data Source

The CLI reads recorded token usage from SQLite stores and optional JSONL under the Cursor Agent home. Conversation transcripts are ignored: those files are chat text, not a usage ledger.

Root resolution (highest first):

1. A non-empty `CURSOR_AGENT_HOME` (one directory, or comma-separated roots)
2. `~/.cursor`

```bash
CURSOR_AGENT_HOME="$HOME/.cursor" ccusage cursor daily
```

```text
$CURSOR_AGENT_HOME/   # or ~/.cursor
├── chats/<workspace>/<session>/store.db          # interactive CLI
├── acp-sessions/<session>/store.db               # ACP / `agent acp`
└── projects/<slug>/sdk-agent-store/<hash>/
    ├── index.db                                  # SDK + CLI catalog
    └── runs.ndjson                               # optional JSONL store
```

Only rows that already carry token counts are counted. All-zero usage blobs, including placeholder `tokenCount: {0,0}` values, are skipped. The Cursor Admin API is not used.

## Report Views

| Focused view             | Description                    | See also                                |
| ------------------------ | ------------------------------ | --------------------------------------- |
| `ccusage cursor daily`   | Aggregate usage by date        | [Daily Usage](/guide/daily-reports)     |
| `ccusage cursor monthly` | Aggregate usage by month       | [Monthly Usage](/guide/monthly-reports) |
| `ccusage cursor session` | Group usage by Cursor session  | [Session Usage](/guide/session-reports) |

These views support `--json`, `--compact`, `--mode`, and `--offline`.

Focused terminal tables omit the `Cache Create` column when the selected Cursor
rows report no cache-creation tokens. The column reappears when selected rows
contain a non-zero value; [JSON output](/guide/json-output) keeps
`cacheCreationTokens` in the stable report schema either way.

## What Gets Calculated

- **Token usage** - Cursor hook payloads use inclusive input (cache sits inside `input_tokens`). When `totalTokens` matches an exclusive sum (`input + output + cache`), input is left uncached. Cache read and cache write are reported separately.
- **Reasoning tokens** - Reasoning is a subset of output and is **not** added again.
- **Precomputed cost** - These local files do not persist an invoice amount. `display` is `$0` unless a record includes `costUSD` / `chargedCents`.
- **Pricing** - `calculate`, and the default `auto` when no recorded cost exists, use LiteLLM / models.dev tables, plus estimated public rates for Cursor models LiteLLM does not list (Composer 2.5). Pricing tries `model_id` first (for example `grok-4.6`), then the display id, then a `cursor-` prefix strip, then Grok Build family candidates (`grok-build` → `grok-build-0.1`). Composer Fast uses the `composer-2.5-fast` estimate when the recorded id ends in `-fast` or `:fast`. Override the estimates with [`pricingOverrides`](/guide/config-files#pricing-overrides) if Cursor changes the published rates.
- **Model labels** - Display form is the hook `model` when present (for example `cursor-grok-4.6-high-fast`). The Agent column identifies the Cursor CLI source in unified reports.

## Environment Variables

| Variable             | Description                                                                 |
| -------------------- | --------------------------------------------------------------------------- |
| `CURSOR_AGENT_HOME`  | Cursor Agent data home (one root, or comma-separated roots)                 |
| `LOG_LEVEL`          | Adjust verbosity (0 silent ... 5 trace)                                     |

## Configuration

```json
{
	"cursor": {
		"defaults": {
			"offline": true
		},
		"commands": {
			"session": {
				"json": true
			}
		}
	}
}
```

The `cursor` namespace supports the same shared report options as other focused
sources. Use `cursor.defaults` for all Cursor reports and a matching
`cursor.commands.daily`, `cursor.commands.monthly`, or `cursor.commands.session`
object for report-specific overrides. The data root is discovered from
`CURSOR_AGENT_HOME` or `~/.cursor`, not from ccusage configuration.

## Troubleshooting

::: details No Cursor CLI usage data found
Ensure recorded usage exists under `~/.cursor/chats/**/store.db`, `~/.cursor/acp-sessions/**/store.db`, or `~/.cursor/projects/*/sdk-agent-store/*/index.db`. Transcript JSONL under `agent-transcripts/` is not a usage source. Set `CURSOR_AGENT_HOME` if your data lives elsewhere.
:::

::: details Costs showing as $0.00
Cursor does not persist invoice USD in these local files, so `display` is zero. Use `--mode calculate` (or the default `auto`) to price from the tables. If a model is missing from pricing, the cost stays at zero and a missing-pricing warning may appear.
:::

::: details Totals lower than a Cursor UI usage panel
ccusage only counts turns that already recorded token counts on disk. In-progress turns, transcripts, and cloud Admin API history are out of scope.
:::
