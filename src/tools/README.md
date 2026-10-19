# Assistant tool UI modules

Add a module implementing `ToolUiDefinition`, then export it in `available-tools.ts`:

```ts
export { default as my_tool } from "./my-tool";
```

The module owns its display metadata, settings controls and optional `onEnable`
preparation hook. Shared menu code discovers the exports and sorts by `order`;
there are no tool-name branches to update there. Register settings controls with
`ToolSettingsContext` for busy state and use `onRefresh` for saved values.

Use `timeoutSetting` with the tool name, default and range. New tools write the
`timeouts[name]` map; `legacyKey` is only for the four pre-existing flat fields.
Rust validates the range. Provider-specific commands and credential controls stay
in the tool module. Preparation hooks should download only that tool's optional
runtime and preserve existing selections on failure.

Add the corresponding Rust module/entry in `src-tauri/src/assistant_tools/available.rs`
and tests for its handler, settings and selection behavior. The two registration
lists target different runtimes; these are compiled-in tools. Runtime plugins and
schema-rendered plugin settings are planned separately in
`docs/assistant/tool-plugins-plan.md`.
