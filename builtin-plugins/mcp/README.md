# `mcp` — Model Context Protocol client

The Wasmtime Component behind the `mcp@deluxe-defaults` plugin. It connects to
Model Context Protocol servers, publishes their tools to the model as
`mcp__<server>__<tool>`, and forwards the calls. The Component owns the JSON-RPC
framing and the handshake; the host only supplies bounded process/HTTP
transports and the plugin-file capability. See [`../README.md`](../README.md)
for how packages are built and embedded, and
[`../../docs/wasmtime-plugin-guide.md`](../../docs/wasmtime-plugin-guide.md)
for the Component ABI.

## Enable it

Off until the config turns it on. In `config.toml` (Windows:
`%APPDATA%\deluxe-agent\config\config.toml`):

```toml
[plugins."mcp@deluxe-defaults"]
enabled = true
```

This row applies in every project.

## Configure servers

The Component reads exactly one file through the host's `read-plugin-file`
import, bound to the configuration root of its scope:

| Scope | File |
| --- | --- |
| global (the bundled plugin) | `~/.deluxe-agents/.mcp.json` |
| project (a plugin pinned to one repo) | `<project>/.mcp.json` |

Most installs only ever have the global one. Either way the file is not part of
this package — the host does not read, parse, or embed it.

```json
{
  "mcpServers": {
    "chrome-devtools": {
      "type": "stdio",
      "command": "npx.cmd",
      "args": ["-y", "chrome-devtools-mcp@latest"]
    },
    "local-http": {
      "type": "http",
      "url": "http://127.0.0.1:3001/mcp"
    }
  }
}
```

Per-server fields:

| Field | Transport | Meaning |
| --- | --- | --- |
| `url` | HTTP | JSON-RPC endpoint. Its presence selects HTTP. |
| `command` | stdio | Executable to launch. Selected when there is no `url`. |
| `args` | stdio | Arguments, a JSON array. Defaults to `[]`. |
| `env` | stdio | Extra environment variables, a JSON object. Defaults to `{}`. |
| `cwd` | stdio | Working directory. Empty uses the configuration root. |
| `disabled` | both | `true` keeps the declaration but stops the server. |

`type` (`"stdio"` / `"http"`) is accepted for readability but not read: the
transport is whichever of `url` or `command` is present. A server with neither
is skipped. `args` must be an array and `env` an object, or configuration
fails.

Two limits are worth knowing up front:

- **Windows needs an extension.** The host launches stdio servers with
  `tokio::process::Command`, which goes through `CreateProcess` and appends only
  `.exe` to the program name. `npx`, `npm`, `yarn`, and `pnpm` are `.cmd` shims,
  so `"command": "npx"` fails with "program not found" — write `npx.cmd`
  (`npm.cmd`, …). On Unix the bare name resolves through `PATH`.
- **HTTP headers are not configurable.** The Component sends `content-type`,
  `accept`, `mcp-protocol-version`, and, once the server issues one,
  `mcp-session-id`. There is no way to add an `Authorization` header, so
  token-authenticated HTTP servers are not supported yet; use stdio instead.

## Tools the model sees

On configure, every active server is handshaken once and its `tools/list` is
read; those tools are this plugin's contribution to the registry, named:

```text
mcp__<server>__<tool>
```

The server name and the remote tool name must be ASCII letters, digits, `_`, or
`-`, at most 128 characters; a tool with any other name is dropped. Published
tools are marked `mutating`, and `hostValidatesArguments` is `false` because the
remote schema is the only authority. A JSON-RPC `error`, a non-2xx HTTP status,
or a transport failure is reported back to the model as the tool error.

## The MCP surface

The plugin registers an `mcp` UI surface listing every configured server, its
transport, and its state, with two actions per row:

- **停止 (Stop)** writes `"disabled": true` into `.mcp.json` and closes the
  connection. The row stays and can be started again; the state is persisted,
  so it survives a reload and an application restart.
- **卸载 (Uninstall)** removes the server from `.mcp.json` and drops the
  connection.

Both rewrite the whole file through the host's bounded write capability (hence
the manifest's `writePluginFiles`), preserving every other key and server. Hand
editing is equally fine.

## Applying changes

`.mcp.json` is read once, in `configure()`. After editing it by hand, the new
servers do not exist until the instance is reconfigured — **restart the
application (or reload the plugin)** to pick them up. Stop and uninstall from
the surface update the live set immediately.

## Permissions

`plugin.json` declares `processCommands: ["*"]` and `networkHosts: ["*"]`,
because a generic client cannot know which servers the user will declare. That
makes `.mcp.json` a trust boundary: a stdio server is an arbitrary command, and
an HTTP server can be reached over unrestricted network access. Only declare
servers you trust.
