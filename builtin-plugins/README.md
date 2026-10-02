# Built-in Wasmtime plugins

Each immediate subdirectory is one Wasmtime plugin package embedded into the
application at build time. The repository currently ships separate `hooks`
and `mcp` Components for those two responsibilities.

Required files:

```text
builtin-plugins/<plugin>/
├── plugin.json
└── plugin.wasm
```

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

The default plugin marketplace is `deluxe-defaults`. The package itself remains
a Wasmtime plugin after first launch; the host does not interpret its MCP
behavior.
