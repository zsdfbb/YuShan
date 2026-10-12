# Execution Plan: /login & /model Command Improvement

## Task 1: Create provider.rs
- type: impl
- files: `apps/coding-agent/src/provider.rs`
- description: |
    Create the new provider module with all provider-related types and logic:
    - `ProviderInfo` struct: name, api_base, default_model, compat
    - `AuthEntry` struct: Serialize/Deserialize for auth.json persistence
    - `ApiModel` struct: Deserialize for OpenAI-compatible /v1/models response
    - `FetchModelsResult` enum: Ok(Vec<ApiModel>), Unauthorized, NetworkError(String)
    - `ProviderRegistry` struct with fields: providers, auth_entries, model_cache, auth_path
    - Built-in providers: deepseek (reasoning_content), minimax (tool_calls_as_text), custom (standard)
    - `ProviderRegistry::new()` creates registry with built-in providers
    - `load_auth()` reads ~/.yushan/auth.json, creates dir with 0o700 if missing, file with 0o600
    - `save_auth()` appends/replaces entry in auth.json, writes atomically
    - `remove_auth()` removes entry by provider name
    - `providers()` returns &Vec<ProviderInfo>
    - `auth_for()` returns Option<&AuthEntry> for a provider name
    - `fetch_models()` constructs URL from api_base + "/v1/models", uses reqwest with 5s timeout, returns FetchModelsResult
    - `available_models()` returns cached models for provider, falling back to static known_models() list
    - `compat_for()` maps provider name to ProviderCompat
    - `model_cache` populated by fetch_models() results
- priority: P0

## Task 2: Tests for provider.rs
- type: test
- files: `apps/coding-agent/src/provider.rs` (in #[cfg(test)] mod tests)
- description: |
    Add unit tests in the same file:
    - `auth_roundtrip`: Create temp dir, build ProviderRegistry, save_auth for deepseek, load_auth from disk, verify all fields match
    - `auth_remove`: Save 2 entries, remove one, verify only the other remains
    - `compat_mapping`: Call compat_for("deepseek") -> reasoning_content=true, compat_for("minimax") -> tool_calls_as_text=true, compat_for("custom") -> both false
    - `fetch_models_url_construction`: Test URL construction logic (unit test the URL builder, not the actual HTTP call)
    - `known_models_fallback`: Create empty cache, call available_models(), verify returns hardcoded list from config module
    Use tempdir crate (or manual temp path with cleanup) for file system tests.
- test_method: `cargo test -p yushan-coding-agent provider::`
- priority: P0

## Task 3: Rewrite config.rs
- type: impl
- files: `apps/coding-agent/src/config.rs`
- description: |
    Simplify Config to use ProviderRegistry:
    - Remove: `Provider` struct, `KnownModel` struct, `known_providers()` fn, `known_models()` fn
    - Add: `pub registry: ProviderRegistry` field to Config
    - Update `Config::from_env()`: create ProviderRegistry::new(), no auth loading here
    - Add `current_compat()`: delegates to self.registry.compat_for(&self.model)
    - Update `build_model()` or keep as-is (factory closure already reads config fields)
    - Keep: `ModelFactory` type alias, `set_model_factory()`, `is_configured()`, `build_model()`
    - Update existing tests: test_config() creates Config with registry, test_config_model_factory updated
- priority: P0

## Task 4: Tests for config.rs
- type: test
- files: `apps/coding-agent/src/config.rs` (in #[cfg(test)] mod tests)
- description: |
    Update and add unit tests:
    - `config_from_env`: Verify env var loading still works (keep existing test)
    - `config_model_factory`: Verify factory works with registry present (update existing test)
    - `config_current_compat_delegates`: Set model to "deepseek-chat", call current_compat(), verify has_reasoning_content=true
    - `config_current_compat_fallback`: Set model to unknown name, verify returns standard compat
- test_method: `cargo test -p yushan-coding-agent config::`
- priority: P0

## Task 5: Update main.rs
- type: impl
- files: `apps/coding-agent/src/main.rs`
- description: |
    Update the startup flow:
    - Add `mod provider;` declaration
    - Create ProviderRegistry early, call load_auth()
    - Startup recovery logic:
      a. If env vars provide api_base + api_key: find matching auth.json entry by api_base, set model factory compat from matched provider
      b. If no env vars but auth.json has entries: restore first entry's api_base/api_key/model into Config fields
      c. If no env vars and no auth.json: start unconfigured (current behavior)
    - Model factory closure uses `config.current_compat()` instead of hardcoded ProviderCompat::standard()
    - Initial model build uses config.current_compat() for ProviderCompat
    - Assign registry to config.registry
- priority: P0

## Task 6: Rewrite builtin.rs commands
- type: impl
- files: `apps/coding-agent/src/commands/builtin.rs`
- description: |
    Update all three auth-related commands:
    
    LoginCommand:
    - Read providers from ctx.config.registry.providers()
    - Mark providers with existing auth in selection (checkmark)
    - After user enters API key: ctx.config.registry.save_auth(provider, api_base, api_key)
    - Set config fields (api_base, api_key, model)
    - Optionally spawn fetch_models() to populate cache (non-blocking or best-effort)
    - Build model via config.build_model() and set on agent
    - Print summary including provider name
    
    LogoutCommand:
    - Get current provider name from config.api_base (match to registry)
    - Call ctx.config.registry.remove_auth(provider_name)
    - Clear config fields: api_base = None, api_key = None
    - Set config.model to empty or default
    - Call ctx.agent.set_model(None)
    - Print confirmation
    
    ModelCommand (no args):
    - Call ctx.config.registry.available_models(&ctx.config.model)
    - Build labels with current model marked "model_name  (current)"
    - inquire::Select with the labels
    - On selection: update config.model, rebuild, set on agent
    
    ModelCommand (with args):
    - Direct set: ctx.config.model = target.to_string()
    - Rebuild and set model (keep current behavior)
    
    Update test_config() helper: creates Config with ProviderRegistry
    Update existing tests to work with new Config structure
    Remove imports of known_models/known_providers
- priority: P0

## Task 7: Tests for builtin.rs
- type: test
- files: `apps/coding-agent/src/commands/builtin.rs` (in #[cfg(test)] mod tests)
- description: |
    Add/update unit tests:
    - `login_sets_model`: Mock a LoginCommand flow with pre-set config fields, verify agent gets model
    - `logout_clears_all`: Set config api_base/api_key/model, run LogoutCommand, verify all cleared
    - `model_switch_with_args`: Set model "old", run ModelCommand("new-model"), verify config.model updated
    - Update `test_logout_clears_model` to also verify config field clearing
    - Update `test_config()` helper to create Config with ProviderRegistry
    - Keep all existing non-auth tests working
- test_method: `cargo test -p yushan-coding-agent commands::builtin::`
- priority: P0

## Task 8: Update Cargo.toml
- type: impl
- files: `apps/coding-agent/Cargo.toml`
- description: |
    Ensure dependencies are present:
    - reqwest with features = ["json", "rustls-tls"]
    - serde with features = ["derive"]
    - Check if tempdir is needed for tests (or use std::env::temp_dir)
    - Verify tokio features include fs (already has rt-multi-thread + macros)
    Remove any unused dependencies if applicable.
- priority: P1

## Task 9: Add mod provider to main.rs
- type: impl
- files: `apps/coding-agent/src/main.rs`
- description: |
    Add `mod provider;` to the module declarations at the top of main.rs.
    This is included in Task 5 but called out separately for clarity.
    Verify the module is accessible from commands/builtin.rs and config.rs via crate::provider.
- priority: P0
