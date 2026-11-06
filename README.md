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
- Direct Pi model-picker Unix-socket adapter
- In-place scope switching and provider-catalog refresh
- Pi model-picker protocol 1 and protocol 2 compatibility
- Atomic result-file replacement
- `/dev/tty` rendering, allowing picker data and results to use standard input/output safely

The Pi adapter is functional and tested against a live Rex-hosted Pi session, but this repository has not yet been wired into the dotfiles Rex action. The existing picker remains active until that separate integration is performed.

## Build

```sh
cargo build --release
```

The optimized binary is written to `target/release/rex-picker`.

A pinned Nix build is also available:

```sh
nix build
nix run . -- --help
nix develop
```

The flake package runs the complete test and Clippy suite during `nix flake check`.

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

## Pi model adapter

When Pi is running in a Rex terminal block with the dotfiles model-picker extension loaded:

```sh
rex-picker pi-model "$REX_BLOCK"
```

The adapter derives the extension socket from `XDG_STATE_HOME` and the originating Rex block ID. It communicates with Pi directly—there is no Bash wrapper or `nc` subprocess.

Controls supplied by the extension's capabilities:

- `Tab`: toggle between all and scoped models
- `Ctrl+R`: refresh provider model catalogs
- `Enter`: ask Pi to select the highlighted model and pinned thinking level
- `Escape`: cancel

Scope and refresh replace the candidate document in place while preserving the fuzzy query. Pi remains authoritative: the extension validates availability, authentication, and scoped thinking-level pins before applying a selection.

Useful options:

```sh
rex-picker pi-model "$REX_BLOCK" --scope scoped
rex-picker pi-model "$REX_BLOCK" --refresh
rex-picker pi-model "$REX_BLOCK" --socket /explicit/model-picker.sock
```

By default, the process uses `REX_SESSION` to restore focus to the originating block when it exits. Pass `--restore-zoom` when the Rex action temporarily unzooms the source block before showing the float. Use `--no-restore-focus` when another Rex action owns focus restoration.

A Rex terminal layer should invoke the binary directly:

```lua
command = { "/absolute/path/to/rex-picker", "pi-model", ctx.block_id },
shell = "none",
```

The adapter accepts both wire formats used by the dotfiles extension:

- Protocol 1: `SELECT_JSON<TAB>VISIBLE_ROW`
- Protocol 2: `STABLE_ID<TAB>SELECT_JSON<TAB>VISIBLE_ROW`

Protocol 2 capability metadata controls whether scope and refresh actions are shown. ANSI styling is sanitized, while concealed search aliases remain searchable without being rendered.

## URL adapter

Search the newest 20,000 lines of a Rex terminal's scrollback:

```sh
rex-picker url "$REX_SESSION" "$REX_BLOCK"
```

The picker renders a loading screen immediately while a worker captures and parses scrollback. It prefers Rex's bounded blob API so large terminals read only the recent suffix, and falls back to `rex capture --format=html` when that API is unavailable.

Controls:

- `Enter`: open the URL
- `Ctrl+B`: open without bringing the browser forward
- `Ctrl+Y`: copy the URL
- `Escape`: cancel

Both visible URLs and OSC 8/HTML hyperlinks are supported. Trailing sentence punctuation and unmatched closing brackets are removed, duplicate destinations are collapsed newest-first, and hyperlink labels remain searchable.

For a Rex floating layer:

```lua
command = {
  "/absolute/path/to/rex-picker",
  "url",
  ctx.session_id,
  ctx.block_id,
},
shell = "none",
```

## Password adapter

Choose an entry name from a pass-compatible password store and copy it without exposing the secret to the picker:

```sh
rex-picker password --origin-block "$REX_BLOCK"
```

The store defaults to `PASSWORD_STORE_DIR`, then `~/.password-store`. Only relative `.gpg` entry names are indexed; encrypted contents are never opened, decrypted, logged, placed in metadata, or rendered. Symlinks and non-UTF-8 entry paths are skipped.

After selection, the adapter executes:

```sh
pass show --clip ENTRY
```

The child process's standard output and error are suppressed so a misbehaving pass-compatible implementation cannot render secret material in the floating terminal. Use `--pass-bin` or `--store` for alternate implementations and locations.

## File adapter

Discover files natively and inject a shell-safe Neovim command into the originating Rex block:

```sh
rex-picker file "$REX_SESSION" "$REX_BLOCK" "$PWD" nu
rex-picker file "$REX_SESSION" "$REX_BLOCK" "$PWD" zsh
```

The initial filesystem walk runs off the render thread. File matching uses the same Nucleo index as every other adapter, while previews are loaded lazily by a dedicated worker only for the highlighted item. Text previews include line numbers and bounded reads; binary files show metadata without dumping bytes.

Controls:

- `Tab`: select multiple files
- `Enter`: open with `nvim`, using `-O2` for multiple selections
- `Alt+.`: toggle hidden files
- `Alt+I`: toggle ignored files
- `Alt+V`: toggle preview
- `Alt+Y`: copy selected relative paths
- `Alt+O`: reveal selected files

The adapter never follows symlinks during discovery, always excludes `.git`, caps each result set at 5,000 files, validates returned paths as relative, and quotes commands separately for Nushell and Zsh.

## Nushell history adapter

Search Nushell's SQLite history using session, workspace, and global scopes:

```sh
rex-picker history \
  "$REX_SESSION" "$REX_BLOCK" \
  "$history_database" "$history_session_id" \
  "$PWD" "$workspace" "$result_file" "$initial_query"
```

Controls:

- `Enter`: run the selected command
- `Tab` or `Ctrl+Y`: return it for insertion without running
- `Ctrl+O`: edit it with `EDITOR`, then return it for insertion
- `Ctrl+R`: cycle session, workspace, and global scopes

Recent rows are queried with bounded scan limits, normalized and deduplicated in Rust, and displayed with exit status, duration, and relative age. The original command is fetched by history ID only after selection, preserving tabs and newlines. When a result file is provided, the adapter replaces it atomically and sends `F13` to the originating Rex block; an empty result-file argument emits JSON on stdout instead.

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
    "header": "MODEL  PROVIDER  CTX  INPUT/M  OUTPUT/M",
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

- **Core:** matching, selection, input, rendering, previews, action dispatch, in-place document replacement
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

## Adapter status

The Pi model, URL, password, file, and Nushell history adapters are implemented. Dotfiles migration remains intentionally separate so each existing picker can retain a tested fallback during rollout.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

The codebase forbids unsafe code in project crates. Unsafe optimizations remain encapsulated inside audited dependencies such as Nucleo.
