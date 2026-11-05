# rex-picker

`rex-picker` is an early-stage, reusable terminal picker for [Rex](https://www.superlogical.com/rex/) workflows. It provides one fast interaction engine that can be shared by model, URL, file, password, and history pickers while keeping their data sources and actions explicit.

The project is intentionally a **one-shot process**: Rex starts it inside a floating terminal layer, the user selects an item, the picker writes a structured result, and it exits. There is no resident picker daemon.

## Current capabilities

- Parallel fuzzy matching powered by [`nucleo`](https://github.com/helix-editor/nucleo)
- Responsive Ratatui interface with Unicode-aware input editing
- Single and multi-selection
- Structured alternate actions such as copy or background-open
- Optional static preview pane
- Versioned JSON input and JSON result protocols
- Newline- and NUL-delimited input adapter
- Atomic result-file replacement
- `/dev/tty` rendering, allowing picker data and results to use standard input/output safely

This repository is a foundation, not yet a drop-in replacement for the existing dotfiles pickers. Rex and Pi adapters will be added after the protocol and interaction model have proven stable.

## Build

```sh
cargo build --release
```

The optimized binary is written to `target/release/rex-picker`.

## Quick start

Pick from shell-generated lines:

```sh
printf '%s\n' alpha beta gamma |
  rex-picker lines --title 'Choose an item'
```

The picker renders through `/dev/tty`, leaving standard output available for the result:

```json
{"action":"accept","selected":[{"id":"1","label":"beta","metadata":{"value":"beta"}}]}
```

Use NUL-delimited records for paths or other values that may contain newlines:

```sh
fd --type f --print0 |
  rex-picker lines --read0 --multi --title Files
```

Add adapter-defined actions with `KEY:ID:LABEL`:

```sh
rex-picker lines \
  --input urls.txt \
  --action 'ctrl-y:copy:copy URL' \
  --action 'ctrl-b:background:open in background'
```

## JSON protocol

```sh
rex-picker json --input examples/model-picker.json --output result.json
```

A version 1 document has picker options and stable candidate IDs:

```json
{
  "version": 1,
  "options": {
    "title": "Pi Model Picker",
    "prompt": "› ",
    "placeholder": "Filter models",
    "footer": "tab toggles scope",
    "initial_query": "",
    "multi": false,
    "max_visible": 12,
    "actions": [
      { "id": "refresh", "key": "ctrl-r", "label": "refresh" }
    ]
  },
  "items": [
    {
      "id": "github-copilot/gpt-5.6-sol:high",
      "label": "Sol 5.6",
      "description": "github-copilot · high",
      "search_text": "github-copilot/gpt-5.6-sol high",
      "preview": "Provider: github-copilot\nModel: gpt-5.6-sol",
      "metadata": {
        "provider": "github-copilot",
        "modelId": "gpt-5.6-sol",
        "thinkingLevel": "high"
      }
    }
  ]
}
```

Accepting an item produces:

```json
{
  "action": "accept",
  "selected": [
    {
      "id": "github-copilot/gpt-5.6-sol:high",
      "label": "Sol 5.6",
      "metadata": {
        "provider": "github-copilot",
        "modelId": "gpt-5.6-sol",
        "thinkingLevel": "high"
      }
    }
  ]
}
```

Validate a producer without opening the TUI:

```sh
rex-picker validate --input picker.json
```

### Compatibility rules

- `version` must currently be `1`.
- Item IDs must be nonempty and unique.
- Action IDs and keys must be nonempty and unique.
- Unknown JSON fields are ignored so producers can add adapter-local data.
- `metadata` is returned unchanged and must still be validated by the authoritative adapter. A Pi extension, for example, must verify that a requested model remains available before selecting it.

## Architecture

```text
Rex Lua action
  └─ creates a floating terminal layer
       └─ rex-picker <adapter>
            ├─ adapter loads candidates
            ├─ picker core filters and renders
            ├─ adapter receives PickerResult
            └─ process exits
```

The reusable boundary is deliberately narrow:

- **Core:** matching, selection, input, rendering, previews, action dispatch
- **Adapters:** discovery, refresh, validation, and side effects
- **Rex:** shortcut routing, float placement, and focus
- **Pi:** authoritative model and thinking-level state

This avoids building a large configuration language that merely recreates `fzf` bindings. Complex pickers remain normal typed Rust code while sharing the performance-sensitive interaction loop.

## Performance design

- Candidate text is converted to Nucleo's UTF-32 representation once during injection.
- Matching runs on a bounded worker pool and never blocks the terminal event loop for more than a small tick budget.
- The UI renders only the visible result window.
- Query extension is marked as an append operation, allowing Nucleo to reuse previous matching work.
- The release profile uses thin LTO, one codegen unit, and symbol stripping.
- No shell or `nc` subprocess is required by the generic picker.

Run the matching benchmark with:

```sh
cargo bench --bench matching
```

The benchmark covers construction, indexing, and matching at 100, 1,000, and 5,000 candidates. Adapter-specific I/O should be benchmarked separately because filesystem walking, scrollback capture, SQLite queries, previews, and external actions have different bottlenecks.

## Planned adapters

1. Pi model snapshot and selection socket
2. Rex scrollback URL extraction and URL actions
3. Password-store entry names with `pass --clip` kept outside the renderer
4. File discovery, multi-select, and dynamic preview
5. Nushell history scopes and edit/insert/run actions

The file adapter is intentionally later: hidden/ignored toggles, streaming discovery, preview cancellation, multi-selection, and shell-safe result injection need explicit designs rather than a premature generic command DSL.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

The codebase forbids unsafe code in project crates. Unsafe optimizations remain encapsulated inside audited dependencies such as Nucleo.
