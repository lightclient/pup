# Display Mode Refactor Plan

Refactor the Telegram turn rendering in `crates/pup-telegram/src/turn_tracker.rs` to support two display modes, controlled by a new config option `display.mode` (values: `"temporal"` or `"compact"`, default `"compact"`).

## Current problem

`render_parts()` renders in a fixed structural order: all tool calls, then thinking, then streaming text. This doesn't match the actual temporal flow. If the model thinks → calls tool A → thinks again → calls tool B → responds, the thinking appears below all tools rather than interleaved where it occurred.

## Mode 1: `"compact"`

A single live message that shows a concise progress indicator while the agent works, then is replaced with the final assistant text when the turn ends. No page freezing, no multi-message splitting.

During the turn, the single message shows (all in one message, edited in-place):
```
⏳ Thinking…

▸ Bash
  echo hello

✓ Bash (3 lines)
▸ Read src/main.rs
```

Rules:
- One line per tool call: status icon + tool name + short arg summary + output line count (if done)
- No tool output shown inline (just the line count like `(3 lines)` or `(error)`)
- No streaming text shown during the turn — just the progress indicator
- Thinking shows as `⏳ Thinking…` or `⏳ <first 100 chars of thinking>…` at the top
- Cancel button on this message as usual
- When the turn ends (`end_turn`): the progress message is edited to a final summary (tools list with status icons, no keyboard), and the full assistant text is sent as a **separate message below** with no keyboard
- If the turn produces no text (tools-only), the progress message becomes the final message

This mode optimizes for minimal Telegram API calls and a clean reading experience on mobile. The full tool output is deliberately hidden — the user sees *what* tools ran and whether they succeeded, not the raw output.

## Mode 2: `"temporal"`

Events are rendered in the order they actually occur, matching the real flow. Thinking blocks are interleaved with tool calls based on when they happened.

The `TurnState` tracks an ordered list of **segments** instead of separate `tools` + `thinking_text` + `streaming_text` fields:

```rust
enum Segment {
    Thinking(String),      // accumulated thinking text
    Tool(TrackedTool),     // a tool call (existing struct)
    Text(String),          // streaming assistant text
}
```

Events append to or create segments:
- `thinking_delta` → if the last segment is `Thinking`, append to it; otherwise push a new `Thinking` segment
- `tool_start` → push a new `Tool` segment
- `tool_update` / `tool_end` → update the last `Tool` segment matching the `tool_call_id`
- `message_delta` → if the last segment is `Text`, append to it; otherwise push a new `Text` segment

`render_parts()` iterates segments in order, rendering each one:
- `Thinking(text)` → `<i>text</i>` (truncated to last 2000 chars)
- `Tool(t)` → same tool rendering as today (status icon + name + args + output)
- `Text(text)` → `to_telegram_html(text)` with the same sentence-boundary snapping

Page freezing works the same way but counts completed `Tool` segments rather than indexing into a flat `tools` vec. The `page_start` becomes an index into the segments list.

At `end_turn`: same as today — the live message gets the verbose summary (segments minus the final text), and the final text goes as a separate message. The difference is that thinking is now interleaved with tools in the summary rather than dumped at the end.

## Implementation plan

1. Add `DisplayMode` enum (`Compact`, `Temporal`) to `turn_tracker.rs`
2. Add `display_mode: DisplayMode` to `TurnTracker` and `TurnState`
3. Add `mode = "compact"` to `[display]` in config, parse in `config.rs`, thread through to `TelegramConfig`
4. For `Temporal` mode:
   - Add `Segment` enum
   - Replace `tools: Vec<TrackedTool>` + `thinking_text: String` + `streaming_text: String` with `segments: Vec<Segment>` in `TurnState`
   - Update `tool_start`, `tool_update`, `tool_end`, `thinking_delta`, `message_delta` to operate on segments
   - Update `render_parts`, `render_frozen_page`, `render_tools` to iterate segments in order
   - Page freezing counts completed tool segments
5. For `Compact` mode:
   - Keep the flat `tools` vec (no segments needed — ordering doesn't matter since output is hidden)
   - New `render_compact()` method that produces the one-line-per-tool summary
   - `flush()` and `end_turn` use `render_compact()` instead of `render_parts()`
   - `end_turn` sends final text as a separate message (same as verbose mode today)
6. Update all existing unit tests to specify the mode they're testing
7. Add new unit tests for both modes
8. Run the E2E test suite (`bash tests/e2e/run_e2e.sh`) and fix any failures

## Constraints

- The `show_thinking` and `show_tools` per-session toggles (`/thinking`, `/tools`, `/verbose`) must continue to work in both modes
- Cancel button behavior must be identical (the CB01-CB05 E2E tests must pass)
- Page freezing in temporal mode must not produce duplicate cancel buttons (the `update_pending_send` fix must still work)
- The non-verbose path (both toggles off) must be unchanged — no progress message, just the final text
- Existing config files without `mode` must default to `"compact"`
