# ccusage-adapter-cursor

The Cursor CLI adapter: it turns locally recorded Cursor Agent CLI and Cursor
SDK usage into the entries the reports render.

## Owns

- `loader.rs` — reading sessions, progress, global dedupe, `has_data`
- `parser.rs` — token split, model pricing candidates, JSON/SQLite mapping
- `paths.rs` — root resolution and `store.db` / SDK `index.db` / JSONL discovery
- `report.rs` — daily / monthly / session summary shapes

Anything that is not specific to this source belongs in `ccusage-core` or
`ccusage-adapter-common` instead.

## Data source

Only records that already carry token counts. Transcript JSONL under
`agent-transcripts/` is ignored: those files are conversation text, not a
usage ledger.

```text
$CURSOR_AGENT_HOME/   # or ~/.cursor
├── chats/<workspace>/<session>/store.db          # interactive CLI
├── acp-sessions/<session>/store.db               # ACP / `agent acp`
└── projects/<slug>/sdk-agent-store/<hash>/
    ├── index.db                                  # SDK + CLI catalog (`runs`, `run_events`)
    ├── runs.ndjson                               # optional JSONL store
    ├── run_events.ndjson                         # optional JSONL event log
    └── agents/<sha256>/store.db                  # per-agent blobs when `index.db` is absent
```

Runtime root priority: a non-empty `CURSOR_AGENT_HOME` (one directory, or
comma-separated roots) → `~/.cursor`.

## Token mapping

Cursor's `stop` / `afterAgentResponse` hook payload and the SDK `TokenUsage`
object use inclusive input (cache sits inside `input_tokens` /
`inputTokens`). When `totalTokens` is present and matches an exclusive sum,
input is left uncached. Reasoning tokens are a subset of output and are not
added again.

| Cursor field                         | ccusage field                 | Rule                                      |
| ------------------------------------ | ----------------------------- | ----------------------------------------- |
| uncached input                       | `input_tokens`                | inclusive input minus cache, clamped      |
| `cacheReadTokens` / `cache_read_*`   | `cache_read_input_tokens`     |                                           |
| `cacheWriteTokens` / `cache_write_*` | `cache_creation_input_tokens` |                                           |
| `outputTokens` / `output_tokens`     | `output_tokens`               | as recorded                               |
| `reasoningTokens`                    | dropped                       | already inside output                     |

Costs are calculated from LiteLLM / models.dev. Cursor does not persist an
invoice amount in these local files.

## Model display and pricing

- Display label: hook `model` (e.g. `cursor-grok-4.6-high-fast`) when present,
  otherwise `model_id` (e.g. `grok-4.6`)
- Pricing tries `model_id` first, then the display id, then a `cursor-` strip,
  then Grok Build family candidates (`grok-build` → `grok-build-0.1`)

## Public surface

- `loader::load_entries`
- `loader::has_data`
- `report::summarize_entries`
- `run`

## Testing

```powershell
# Requires CCUSAGE_PRICING_JSON_PATH (or Nix) for the embedded LiteLLM snapshot.
cargo test -p ccusage-adapter-cursor
```
