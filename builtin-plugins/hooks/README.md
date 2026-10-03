# `hooks` — PostToolUse command hooks

The Wasmtime Component behind the `hooks@deluxe-defaults` plugin. After each
tool call the agent loop emits a `tool.finished` event carrying the tool name
and its output; this Component runs a shell command for every configured
`PostToolUse` matcher that matches, and appends the command's output to the tool
result the model reads. See [`../README.md`](../README.md) for packaging and
[`../../docs/wasmtime-plugin-guide.md`](../../docs/wasmtime-plugin-guide.md)
for the Component ABI.

## Enable it

```toml
[plugins."hooks@deluxe-defaults"]
enabled = true
```

This row applies in every project.

## Configure hooks

The Component reads one file through the host's `read-plugin-file` import,
bound to the configuration root of its scope:

| Scope | File |
| --- | --- |
| global | `~/.deluxe-agents/.hooks.json` |
| project | `<project>/.hooks.json` |

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "apply_patch|exec",
        "hooks": [
          { "type": "command", "command": "cargo fmt --check" }
        ]
      }
    ]
  }
}
```

What the Component actually does with that file:

- only `hooks.PostToolUse` is read; every other key is ignored;
- each group's `matcher` is a Rust `regex`, tested unanchored (a substring
  match). An empty or absent matcher matches every tool; a matcher that fails to
  compile makes the whole group be skipped silently;
- a group without a `hooks` array is skipped, as are entries whose `type` is not
  `"command"` and entries with an empty or whitespace-only `command`;
- the file must be valid UTF-8 JSON. If it is not, `configure()` fails and no
  hooks load at all.

### What the matcher runs against

The event carries the model-facing tool name; the matcher is tested against that
name and, for built-in tools, a set of Claude-Code-style aliases:

| Tool | Aliases also tested |
| --- | --- |
| `apply_patch` | `Write`, `Edit`, `MultiEdit`, `NotebookEdit` |
| `read_file` | `Read`, `NotebookRead` |
| `exec` | `Bash`, `Shell` |
| `list_dir` | `LS`, `Glob`, `Grep` |

So `"matcher": "Edit|Write"` fires on `apply_patch`, and `"matcher": ".*"`
fires on everything.

## How a hook runs

The command runs through the host's `exec` tool, with the agent's working
directory as the cwd, under that tool's normal timeout and output cap. Its
output is appended to the tool result as:

```text
Plugin event handler `<command>` (plugin hooks@deluxe-defaults):
<command output>
```

A nonzero exit inserts the word `failed` before the colon, but the text is still
appended. A call the host *refused* (the destructive-command denylist stopped
it) never emits the event, so a hook never reacts to something that did not
happen.

## Applying changes

`.hooks.json` is read once, in `configure()`. After editing it by hand, the new
hooks do not exist until the instance is reconfigured — **restart the
application (or reload the plugin)** to pick them up.

## Security

A hook is an unrestricted shell command. The manifest's `invokeTools: ["exec"]`
only means the Component may call `exec`; the command itself is **not** screened
by the agent loop's destructive-command denylist, and its working directory is
project-controlled. Treat `.hooks.json`, global or project, as arbitrary code
execution, and only use hooks from sources you trust.
