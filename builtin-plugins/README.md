# Built-in Wasmtime plugins

Each immediate subdirectory is one Wasmtime plugin package embedded into the
application at build time. The repository ships `hooks` and `mcp` Components for
those two responsibilities, plus `transcript-renderer`, which shapes messages
and tool results into the transcript's display list.

Required files:

```text
builtin-plugins/<plugin>/
├── plugin.json
└── plugin.wasm
```

A package may also carry a sibling `README.md` for humans; `build.rs` ignores
any plain file that is not `plugin.json` or `plugin.wasm` (it rejects
subdirectories). See [`mcp/README.md`](mcp/README.md) and
[`hooks/README.md`](hooks/README.md).

For the MCP Component, the host binds the instance to the active global or
project configuration root. The Component then requests its configuration
through the generic `read-plugin-file` host import, and edits it through
`write-plugin-file` (which needs `permissions.writePluginFiles` in the
manifest) when the user uninstalls a server from its surface:

```text
.mcp.json
.hooks.json
```

The host does not read, parse, embed, or copy those files during discovery or
installation. They are not files in this package. The plugin itself is always
the Wasmtime Component in `plugin.wasm`; `plugin.json` only describes that
Component's ABI, permissions, and UI surfaces.

To manage built-ins:

- add a directory to add a plugin;
- remove a directory to stop embedding it in new builds;
- edit the package files to change it;
- increase `version` when changing a package that users may already have
  installed, so the new build gets a new cache directory instead of replacing
  a user's existing cached files.

The default namespace is `deluxe-defaults`. The package itself remains a
Wasmtime plugin after first launch; the host does not interpret its MCP
behavior.
