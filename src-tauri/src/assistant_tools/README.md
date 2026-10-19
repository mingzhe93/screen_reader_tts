# Built-in Assistant tools

Add a module beside `get_weather.rs`, then add one line to `available.rs`:

```rust
my_tool => "my_tool.rs",
```

The module's `SPEC` owns its name, description, single string argument, argument byte
limit, timeout defaults/range, and async handler. The registration macro declares the
module and adds the ordered entry used for schemas, validation, dispatch, selection,
and settings metadata. There are no shared dispatch or validation branches to update.

For a new tool, set `legacy_timeout: None`. Its timeout uses the module default until
`ToolLimits.timeouts["my_tool"]` overrides it; the registry validates the name and range.
The four existing flat timeout fields and Brave settings commands are compatibility
adapters. Provider/key validation and use belong to `web_search.rs`.

Handlers use `ToolServices` for URL grants, source numbering, the sandbox and timeouts,
and return `ToolResponse`. Shared code creates the untrusted-data envelope. Pass the
cancellation flag into service calls and preserve their limits. Put handler-specific
tests in the module; existing qualified schemas and the policy prompt stay stable.

These modules are compiled-in tools. Runtime-installed tools are a separate future
feature; no code in this directory loads or executes a plugin from disk.
