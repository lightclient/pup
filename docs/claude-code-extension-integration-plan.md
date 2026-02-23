# Claude Code Extension Integration Plan

## 1. System Analysis

### Claude Code Extension Model ("Plugins")

Claude Code extensions are called **plugins**. They are directory-based packages
discovered from `~/.claude/plugins/` and managed via marketplace repos. A plugin
consists of declarative configuration files — no programmatic API, no runtime
process model.

**Plugin anatomy:**

```
plugin-name/
├── .claude-plugin/
│   └── plugin.json          # Manifest (name, version, author)
├── commands/                 # Slash commands — markdown with YAML frontmatter
│   └── review.md            #   /review → prompt template sent to LLM
├── agents/                   # Subagent definitions — markdown
│   └── code-reviewer.md     #   Claude selects based on task context
├── skills/                   # Model-invoked capabilities
│   └── skill-name/
│       └── SKILL.md          #   Claude autonomously uses based on description
├── hooks/
│   └── hooks.json           # Event hooks — bash commands or LLM prompts
├── .mcp.json                # MCP server definitions (stdio or http)
└── scripts/                 # Helper scripts for hooks
```

**Extension primitives:**

| Primitive     | Type          | How it works                                         |
|---------------|---------------|------------------------------------------------------|
| **Commands**  | Markdown      | `/command` → prompt template injected as user message |
| **Agents**    | Markdown      | Subagent definitions; Claude spawns when appropriate  |
| **Skills**    | Markdown      | Context injected into LLM when task matches `description` |
| **Hooks**     | JSON + scripts| Bash commands or LLM prompts executed on lifecycle events |
| **MCP**       | JSON          | Stdio/HTTP MCP servers → additional LLM tools        |
| **LSP**       | JSON          | Language servers for code intelligence                |

**Hook events:**

| Event              | When                    | Can block?  |
|--------------------|-------------------------|-------------|
| `PreToolUse`       | Before tool executes    | Yes (deny)  |
| `PostToolUse`      | After tool completes    | No          |
| `Stop`             | Agent considers stopping| Yes (block) |
| `SubagentStop`     | Subagent stopping       | Yes (block) |
| `UserPromptSubmit` | User submits prompt     | No (context)|
| `SessionStart`     | Session begins          | No (context)|
| `SessionEnd`       | Session ends            | No          |
| `PreCompact`       | Before compaction       | No (context)|
| `Notification`     | Notification sent       | No          |

**Hook I/O:** JSON on stdin, JSON on stdout. Exit code 0 = success, exit code 2 =
blocking error. Hooks run in parallel. No programmatic state — all state is
via temp files or `$CLAUDE_ENV_FILE`.

**Key characteristics:**
- **Declarative, not programmatic** — no runtime API, no event subscriptions in code
- **Process-per-invocation** — hooks spawn a new process each time
- **Stateless** — no shared memory between hook invocations
- **No streaming access** — hooks fire at discrete points, no message deltas
- **Prompt-based hooks** — can use LLM reasoning as a hook (unique to Claude Code)
- **MCP-native** — first-class MCP server integration for tools

---

### Pi Extension Model

Pi extensions are **TypeScript modules** loaded into the pi process. They have a
rich programmatic API via `ExtensionAPI` and `ExtensionContext`.

**Key characteristics:**
- **Programmatic, in-process** — full TypeScript API, event subscriptions, shared state
- **Rich event lifecycle** — 25+ event types covering every stage of agent operation
- **Streaming access** — `message_update`, `thinking_delta`, `tool_execution_update`
- **Bidirectional** — can block events, modify messages, inject context, control flow
- **Stateful** — extensions persist state in memory, reconstruct from session entries
- **Custom tools** — register tools callable by the LLM with custom rendering
- **Custom UI** — full TUI components, dialogs, widgets, custom editors
- **Session control** — fork, compact, navigate tree, send messages as user

---

### Pup Architecture (Current)

Pup bridges pi sessions to chat platforms (currently Telegram) via a three-layer
architecture:

```
┌─────────────────────────────────────────────────────────┐
│  pi process                                             │
│  └── pup extension (index.ts)                           │
│      └── Unix socket server at ~/.pi/pup/<id>.sock      │
│          Protocol: newline-delimited JSON                │
│          Events: hello, history, agent_*, message_*,     │
│                  tool_*, session_*, model_changed, ...   │
│          Commands: send, abort, get_info, get_history    │
└──────────────────────┬──────────────────────────────────┘
                       │ Unix socket (ndjson)
┌──────────────────────┴──────────────────────────────────┐
│  pup-daemon (Rust)                                      │
│  ├── Discovery: watches ~/.pi/pup/ for .sock files      │
│  ├── SessionManager: connects to sockets, fans out      │
│  │   events to backends, routes incoming messages       │
│  └── Backend trait: ChatBackend                         │
│      └── handle_event(SessionEvent) + recv_incoming()   │
└──────────────────────┬──────────────────────────────────┘
                       │ Telegram Bot API
┌──────────────────────┴──────────────────────────────────┐
│  Telegram                                               │
│  ├── DM mode: /ls, /attach, /detach per-user            │
│  └── Topics mode: per-session forum topics              │
└─────────────────────────────────────────────────────────┘
```

**IPC Protocol (sock ↔ daemon):**

| Direction        | Messages                                              |
|------------------|-------------------------------------------------------|
| Server → Client  | `Event { event, data }`, `Response { command, ... }`  |
| Client → Server  | `Send { message, mode }`, `Abort`, `GetInfo`, `GetHistory` |

The protocol is deliberately thin — it carries text events and text commands.
The pi extension translates rich pi events into simple JSON events, and the
daemon translates backend-specific messages into simple `Send`/`Abort` commands.

---

## 2. Compare and Contrast

### Conceptual Model

| Dimension          | Claude Code Plugins              | Pi Extensions                    |
|--------------------|----------------------------------|----------------------------------|
| Language           | Markdown + JSON + shell scripts  | TypeScript                       |
| Runtime model      | Process-per-hook invocation      | In-process, long-lived           |
| State              | Stateless (temp files)           | Stateful (in-memory + session)   |
| Event granularity  | 9 coarse lifecycle events        | 25+ fine-grained events          |
| Streaming          | None                             | Full (deltas for text/thinking/tools) |
| Tools              | Via MCP servers (separate process)| In-process `registerTool()`      |
| Commands           | Markdown templates               | Programmatic handlers            |
| Interception       | PreToolUse can deny              | tool_call can block + modify     |
| Context injection  | Via hook stdout/systemMessage     | Via before_agent_start return    |
| UI integration     | None (headless hooks)            | Full TUI API (dialogs, widgets)  |
| Distribution       | Git marketplace repos            | npm/git packages + local dirs    |

### What Claude Code Plugins Offer That Pi Doesn't

1. **Prompt-based hooks** — Using the LLM itself as a hook evaluator. Novel idea,
   though expensive (extra API call per hook invocation).
2. **MCP server integration** — First-class `.mcp.json` for declaring MCP servers
   that provide additional tools.
3. **LSP server integration** — Declarative `lspServers` config to wire up
   language servers for code intelligence.
4. **Declarative simplicity** — A plugin can be just a `plugin.json` + one markdown
   file. Zero code needed for common patterns.
5. **Subagent definitions** — `agents/` directory for specialized subagent
   personas that Claude can spawn.

### What Pi Extensions Offer That Claude Code Doesn't

1. **Full streaming access** — Every token, every tool update, every thinking delta.
2. **Rich programmatic API** — Event subscriptions, tool registration, session
   control, model management, all in-process.
3. **Stateful extensions** — In-memory state that persists via session entries,
   reconstructed on fork/reload.
4. **Custom TUI** — Dialogs, widgets, status bars, custom editors, overlays.
5. **Bidirectional event modification** — Can modify messages in `context` event,
   replace system prompt, modify tool results.

### What Pup Currently Surfaces

The pup IPC protocol exposes a **subset** of pi's extension capabilities:

| Pi Capability           | Pup IPC Status        | Notes                        |
|-------------------------|-----------------------|------------------------------|
| Agent lifecycle         | ✅ Full               | agent_start/end, turn_start/end |
| Message streaming       | ✅ Full               | message_start/delta/end, thinking_delta |
| Tool lifecycle          | ✅ Full               | tool_start/update/end        |
| Session metadata        | ✅ Full               | hello, session_name_changed, model_changed |
| Send messages           | ✅ Full               | send command (steer/follow_up) |
| Abort                   | ✅ Full               | abort command                |
| History                 | ✅ Full               | get_history command          |
| Slash commands          | ⚠️ Partial            | /compact, /name, /quit, /status work; /new, /fork unsupported |
| Tool interception       | ❌ Not exposed         | No PreToolUse equivalent     |
| Context injection       | ❌ Not exposed         | No before_agent_start equivalent |
| Custom tool registration| ❌ Not exposed         | No registerTool equivalent   |
| Session control         | ❌ Not exposed         | No fork, tree, resume        |

---

## 3. Integration Plan: Claude Code Plugin Support via Pup

### Goal

Allow Claude Code-style plugins to be loaded and executed by the pup daemon,
bringing their capabilities (hooks, MCP servers, commands, skills) to pi sessions
via the existing sock ↔ daemon protocol.

### Design Principles

1. **Daemon-side implementation** — Plugins run in the daemon, not in pi. This
   avoids modifying pi or the pup extension.
2. **Protocol extensions are additive** — New IPC messages extend the existing
   protocol; the extension and daemon remain backward compatible.
3. **Plugins are optional** — Zero-plugin configurations work exactly as today.
4. **Hooks map to IPC events** — The daemon already receives all the events hooks
   need; it just needs to evaluate hooks and send back results.

### Phase 1: Plugin Discovery & Loading

**Crate: `pup-plugins`**

New crate that handles:
- Parsing `.claude-plugin/plugin.json` manifests
- Discovering `commands/`, `agents/`, `skills/`, `hooks/hooks.json`
- Resolving `${CLAUDE_PLUGIN_ROOT}` in hook commands
- Loading and validating hook configurations

```rust
// pup-plugins/src/lib.rs

pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    pub manifest: PluginManifest,
    pub hooks: HookConfig,
    pub commands: Vec<SlashCommand>,
    pub skills: Vec<Skill>,
    pub agents: Vec<AgentDef>,
    pub mcp_servers: Vec<McpServerDef>,
}

pub struct HookConfig {
    pub pre_tool_use: Vec<HookEntry>,
    pub post_tool_use: Vec<HookEntry>,
    pub stop: Vec<HookEntry>,
    pub session_start: Vec<HookEntry>,
    pub session_end: Vec<HookEntry>,
    pub user_prompt_submit: Vec<HookEntry>,
    pub pre_compact: Vec<HookEntry>,
    pub notification: Vec<HookEntry>,
}

pub struct HookEntry {
    pub matcher: Option<String>,  // regex for tool name matching
    pub hook: HookType,
    pub timeout: Duration,
}

pub enum HookType {
    Command { command: String },
    // Prompt-based hooks require LLM access — deferred to Phase 4
}
```

**Config extension:**

```toml
[plugins]
# Directories to scan for plugins (supports glob)
paths = [
    "~/.claude/plugins/cache/*/*",
    "~/.pup/plugins/*",
]

# Per-plugin enable/disable
[plugins.enabled]
"rust-analyzer-lsp@claude-plugins-official" = true
"claude-mem@thedotmack" = false
```

### Phase 2: Hook Execution Engine

**Core idea:** The daemon already receives `SessionEvent` variants that map
directly to Claude Code hook events. Add a `HookEngine` that intercepts events
and runs matching hooks.

**Event mapping:**

| SessionEvent              | Claude Code Hook     | Hook Input                    |
|---------------------------|----------------------|-------------------------------|
| `ToolStart`               | `PreToolUse`         | `{ tool_name, tool_input }`   |
| `ToolEnd`                 | `PostToolUse`        | `{ tool_name, tool_result }`  |
| `AgentEnd`                | `Stop`               | `{ reason }`                  |
| `Connected`               | `SessionStart`       | `{ session_id, cwd }`        |
| `Disconnected`            | `SessionEnd`         | `{ session_id }`             |
| `UserMessage`             | `UserPromptSubmit`   | `{ user_prompt }`            |
| `Notification`            | `Notification`       | `{ text }`                   |

**Problem: PreToolUse blocking requires protocol extension.**

Currently the daemon is a passive observer — it receives events but cannot
influence pi's behavior. `PreToolUse` hooks need to be able to **block** a tool
call. This requires the pi-side pup extension to participate.

**Solution: Add a `tool_call` event handler in the pup extension that consults
the daemon before allowing execution.**

This means adding a new **synchronous** IPC exchange:

```
pi extension                    daemon
     │                              │
     ├── Event: pre_tool_use ──────►│
     │   { tool_call_id,            │  ← daemon runs hooks
     │     tool_name, args }        │
     │                              │
     │◄── Response: hook_result ────┤
     │   { decision: "allow"|       │
     │     "deny"|"modify",         │
     │     reason?, updated_input? }│
     │                              │
```

**New protocol messages:**

```rust
// Server → Client (new)
/// Request hook evaluation from the daemon.
HookRequest {
    hook_type: String,       // "pre_tool_use", "user_prompt_submit", "stop"
    request_id: String,      // for correlation
    data: serde_json::Value, // hook-specific input
}

// Client → Server (new)  
/// Hook evaluation result.
HookResponse {
    request_id: String,
    decision: String,        // "allow", "deny", "modify"
    reason: Option<String>,
    updated_input: Option<serde_json::Value>,
    system_message: Option<String>,
}
```

**pup extension changes:**

```typescript
// In the pi extension, add a tool_call handler:
pi.on("tool_call", async (event, ctx) => {
    // Send pre_tool_use to daemon and await response
    const result = await requestHookEvaluation("pre_tool_use", {
        tool_name: event.toolName,
        tool_input: event.input,
        tool_call_id: event.toolCallId,
    });

    if (result.decision === "deny") {
        return { block: true, reason: result.reason ?? "Blocked by plugin hook" };
    }
    // "modify" case: return updated input (if pi supports it)
});
```

**Daemon hook runner:**

```rust
// pup-plugins/src/hooks.rs

pub struct HookEngine {
    plugins: Vec<Plugin>,
}

impl HookEngine {
    /// Run all matching hooks for an event. Returns aggregated decision.
    pub async fn evaluate(
        &self,
        hook_type: HookType,
        input: serde_json::Value,
        cwd: &Path,
    ) -> HookResult {
        let matching = self.find_matching_hooks(hook_type, &input);
        
        // Run all matching hooks in parallel (Claude Code semantics)
        let results = futures::future::join_all(
            matching.iter().map(|hook| self.run_hook(hook, &input, cwd))
        ).await;
        
        // Aggregate: any "deny" → deny; otherwise allow
        self.aggregate(results)
    }
    
    async fn run_hook(
        &self,
        hook: &HookEntry,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> HookResult {
        match &hook.hook {
            HookType::Command { command } => {
                // Spawn process, pipe input as JSON stdin, read JSON stdout
                let child = tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(command)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .env("CLAUDE_PLUGIN_ROOT", &hook.plugin_root)
                    .env("CLAUDE_PROJECT_DIR", cwd)
                    .current_dir(cwd)
                    .spawn()?;
                
                // Write input, read output, apply timeout
                // Parse JSON output for decision
            }
        }
    }
}
```

### Phase 3: Commands, Skills, and Context Injection

**Commands:** Claude Code commands are markdown files that expand into prompts.
The daemon can inject these as context when a user sends a matching `/command`.

This requires extending the `ClientMessage::Send` to support richer payloads:

```rust
// New variant or extension of Send
ClientMessage::Send {
    message: String,
    mode: Option<SendMode>,
    /// Additional context to inject (from plugin commands/skills)
    context: Option<String>,
    id: Option<String>,
}
```

On the pi extension side, this maps to `before_agent_start` system prompt
injection:

```typescript
pi.on("before_agent_start", async (event, ctx) => {
    // If there's pending plugin context, inject it
    if (pendingContext) {
        return {
            systemPrompt: event.systemPrompt + "\n\n" + pendingContext,
        };
    }
});
```

**Skills:** The daemon reads `SKILL.md` files and can inject them as context
when relevant. Two approaches:

1. **Keyword matching** — Parse skill `description` field, match against user
   prompts, inject matching skills as context.
2. **Always inject** — Send all skill content to pi as system prompt additions
   (simple but uses context).

**Recommendation:** Start with approach 1 (keyword matching) as a daemon-side
filter, with the injection happening via the new `context` field on
`ClientMessage::Send` or via a new `InjectContext` command.

### Phase 4: MCP Server Lifecycle (Future)

MCP servers defined in `.mcp.json` are separate processes that provide additional
tools. Integrating these requires one of:

- **Option A:** The daemon manages MCP server processes and proxies their tool
  definitions/calls through to pi via custom tool registration. This is complex
  but keeps pi unmodified.
- **Option B:** The pup extension registers proxy tools in pi that call back
  to the daemon which calls the MCP server. Requires extension-side changes but
  keeps the architecture clean.

**Recommendation:** Option B, implemented as a protocol extension:

```
daemon → extension: register_tool { name, description, parameters }
extension → daemon: tool_execute { tool_call_id, name, args }
daemon → extension: tool_result { tool_call_id, content, is_error }
```

The daemon manages MCP server lifecycle (start/stop/restart) and translates
between pi's tool protocol and MCP's tool protocol.

### Phase 5: Prompt-Based Hooks (Future)

Claude Code's unique `"type": "prompt"` hooks use the LLM itself to evaluate
conditions. This requires the daemon to have LLM access.

**Options:**
- Use the same pi session's LLM (via a separate `prompt` IPC command)
- Use a dedicated cheap model (e.g. Haiku) configured in pup
- Skip this feature (most hooks work fine as command hooks)

**Recommendation:** Defer. Command hooks cover 90%+ of use cases.

---

## 4. Implementation Roadmap

### Milestone 1: Foundation (Phase 1)
- [ ] Create `pup-plugins` crate
- [ ] Plugin discovery and manifest parsing
- [ ] Hook configuration loading and validation
- [ ] Plugin config section in `config.toml`
- [ ] Unit tests for parsing

### Milestone 2: Hook Execution (Phase 2)
- [ ] `HookEngine` with command hook execution
- [ ] Matcher support (exact, pipe-separated, regex, wildcard)
- [ ] Parallel hook execution with timeout
- [ ] `SessionStart`/`SessionEnd`/`PostToolUse`/`Notification` hooks (passive, no protocol change)
- [ ] Integration tests with real hook scripts

### Milestone 3: Bidirectional Hooks (Phase 2 cont.)
- [ ] New IPC messages: `HookRequest`/`HookResponse`
- [ ] Extend pup extension with `tool_call` handler
- [ ] `PreToolUse` blocking support
- [ ] `UserPromptSubmit` context injection
- [ ] `Stop` hook support (requires new `agent_end` protocol extension)
- [ ] End-to-end tests

### Milestone 4: Commands & Skills (Phase 3)
- [ ] Command markdown parsing (frontmatter + body)
- [ ] Skill SKILL.md parsing (frontmatter + body)
- [ ] Keyword-based skill activation
- [ ] Context injection via IPC
- [ ] Extend pup extension with `before_agent_start` handler

### Milestone 5: MCP & Polish (Phase 4)
- [ ] MCP server lifecycle management in daemon
- [ ] Tool proxy protocol extension
- [ ] Dynamic tool registration in pup extension
- [ ] Plugin hot-reload on config change

---

## 5. Protocol Changes Summary

### New Server → Client Messages

```json
{"type": "hook_request", "hook_type": "pre_tool_use", "request_id": "...", "data": {...}}
{"type": "register_tool", "name": "...", "description": "...", "parameters": {...}}
```

### New Client → Server Messages

```json
{"type": "hook_response", "request_id": "...", "decision": "allow", ...}
{"type": "tool_execute", "tool_call_id": "...", "name": "...", "args": {...}}
{"type": "inject_context", "session_id": "...", "context": "...", "position": "system_prompt"}
```

### Modified Messages

```json
// Send gains optional context field
{"type": "send", "message": "...", "mode": "steer", "context": "additional context"}
```

### Backward Compatibility

All new messages are additive. The existing extension ignores unknown client
messages. The daemon already ignores unknown events. Version negotiation can
be added to the `hello` handshake:

```json
{"type": "event", "event": "hello", "data": {..., "protocol_version": 2, "capabilities": ["hooks", "mcp"]}}
```

---

## 6. Alternative: Direct Claude Code Streaming via `--output-format stream-json`

Claude Code exposes a **full streaming event protocol** when run in print mode:

```bash
claude -p "your prompt" --output-format stream-json --verbose
```

This emits newline-delimited JSON to stdout with the following event types:

| Event Type | Description |
|---|---|
| `system` (subtype `init`) | Session initialization: model, tools, session_id, version |
| `system` (subtype `hook_started`) | Hook execution started |
| `system` (subtype `hook_progress`) | Hook stdout/stderr streaming |
| `system` (subtype `hook_response`) | Hook completed with outcome |
| `system` (subtype `compact_boundary`) | Context compaction occurred |
| `user` | User message (with `isReplay` flag for resumed sessions) |
| `assistant` | Assistant message with full `content` array (text, tool_use, thinking blocks) |
| `progress` | Tool execution progress (partial results) |
| `stream_event` | **Raw Anthropic API stream events** — `message_start`, `content_block_start`, `content_block_delta` (text_delta, input_json_delta, thinking_delta), `message_delta`, `message_stop` |
| `tool_use_summary` | Summarized tool call results |
| `attachment` | Structured output, max_turns_reached, etc. |
| `result` | Final result with `subtype`: `success`, `error_max_turns`, `error_max_budget_usd`, `error_during_execution` |
| `keep_alive` | Heartbeat during long operations |

**This is the key:** `stream_event` wraps the raw Anthropic streaming API events,
giving full token-by-token access to text deltas, thinking deltas, and tool input
JSON deltas. This is functionally equivalent to pi's `message_update` and
`thinking_delta` events.

### Additional CLI options for streaming mode

| Flag | Purpose |
|---|---|
| `--output-format stream-json` | Enable streaming ndjson output |
| `--verbose` | **Required** for stream-json; includes tool details and thinking |
| `--include-partial-messages` | Include partial message chunks as they arrive |
| `--input-format stream-json` | Enable streaming ndjson input (bidirectional) |
| `--resume <session-id>` | Resume an existing session |
| `--continue` | Continue the most recent session |
| `--model <model>` | Specify model |
| `--allowedTools` | Restrict tool set |
| `--max-turns <n>` | Limit agent turns |
| `--system-prompt <prompt>` | Custom system prompt |
| `--append-system-prompt <prompt>` | Append to system prompt |
| `--dangerously-skip-permissions` | Skip permission prompts (for automation) |
| `--sdk-url <url>` | WebSocket endpoint for SDK I/O streaming |

### How pup could use this

Instead of (or in addition to) the pi extension socket approach, pup could
directly manage Claude Code processes:

```
┌─────────────────────────────────────────────────────────┐
│  pup-daemon (Rust)                                      │
│  ├── Discovery: watches for pi sockets (existing)       │
│  ├── ClaudeCodeManager: spawns/manages claude processes  │
│  │   └── claude -p --output-format stream-json --verbose │
│  │       stdin: ndjson commands (prompts)                │
│  │       stdout: ndjson events (streaming)               │
│  ├── SessionManager: unified event fan-out              │
│  └── Backends: Telegram, etc.                           │
└─────────────────────────────────────────────────────────┘
```

**Advantages over pi extension approach:**
- Works with Claude Code directly — no pi dependency required
- Full streaming access including raw API events
- Hooks fire natively (SessionStart, PreToolUse, etc.)
- Plugin ecosystem (commands, skills, MCP servers) works out of the box
- Session persistence and resume built in
- `--input-format stream-json` enables bidirectional streaming control

**Disadvantages:**
- Process-per-session (heavier than socket connection)
- Less control than pi's in-process extension API
- Permission prompts need `--dangerously-skip-permissions` or a permission
  prompt tool for headless use
- No equivalent of pi's `tool_call` blocking from the daemon side (hooks
  fill this role instead)

### Mapping stream-json events to pup SessionEvent

| Claude Code stream-json | Pup SessionEvent |
|---|---|
| `system` (init) | `Connected` |
| `assistant` message_start | `MessageStart` |
| `stream_event` content_block_delta (text_delta) | `MessageDelta` |
| `stream_event` content_block_delta (thinking_delta) | `ThinkingDelta` |
| `assistant` (complete) | `MessageEnd` |
| `stream_event` content_block_start (tool_use) | `ToolStart` |
| `progress` | `ToolUpdate` |
| tool_use_summary or assistant tool_result | `ToolEnd` |
| `result` | `AgentEnd` |
| `user` | `UserMessage` |

This mapping is straightforward. A `ClaudeCodeSession` adapter in the daemon
could parse the ndjson stream and emit the same `SessionEvent` variants that
the pi socket adapter already produces, making both session types transparent
to backends.
