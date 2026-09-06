# Design Plan: /login & /model Command Improvement

## 1. Problem Statement

The current `/login` and `/model` commands have 5 gaps compared to the Pi reference implementation:

1. **No credential persistence** -- `LoginCommand` stores credentials only in-memory (`Config` fields). On process exit, all login state is lost.
2. **No startup recovery** -- When env vars `YUSHAN_API_BASE` / `YUSHAN_API_KEY` are set, there is no mechanism to match them to a known provider or restore credentials from disk.
3. **Model list is fully static** -- `known_models()` returns hardcoded entries. No dynamic model fetching from provider APIs.
4. **Logout is incomplete** -- `LogoutCommand` only calls `agent.set_model(None)`. It does not clear `config.api_base` / `config.api_key`, so `is_configured()` still returns `true`.
5. **No provider-level auth tracking** -- Multiple providers cannot be tracked simultaneously. Each login overwrites the previous provider's credentials entirely.

## 2. Solution Overview

Introduce a `ProviderRegistry` struct that lives as a field of `Config`. The registry owns:

- Built-in provider definitions (deepseek, minimax, custom)
- Auth persistence via `~/.yushan/auth.json` (file mode 0o600)
- A cached model list populated by `fetch_models()` per provider
- Compat mapping per provider (ProviderCompat)

The registry is created in `main.rs`, assigned to `config.registry`, and accessed by commands via `ctx.config.registry.xxx()`. `CommandContext` does NOT change.

Config's `api_base`, `api_key`, `model` fields become the "runtime mirror" of whichever provider is currently active -- they are set by `/login` and read by the model factory.

## 3. Module Changes

### 3.1 New: `apps/coding-agent/src/provider.rs`

Contains all provider-related types and auth persistence logic. This is a pure data/logic module with no command or TUI dependencies.

**Removed from config.rs:** `Provider`, `KnownModel`, `known_providers()`, `known_models()`.

### 3.2 Modified: `apps/coding-agent/src/config.rs`

- Remove: `Provider`, `KnownModel`, `known_providers()`, `known_models()`.
- Add: `pub registry: ProviderRegistry` field on `Config`.
- `Config::from_env()` simplified -- no auth loading, just env vars.
- `current_compat()` delegates to `registry.compat_for(model)`.

### 3.3 Modified: `apps/coding-agent/src/commands/builtin.rs`

- `LoginCommand`: reads providers from `ctx.config.registry.providers()`, saves auth, fetches models, sets config fields, builds model.
- `LogoutCommand`: calls `registry.remove_auth()`, clears config fields, calls `agent.set_model(None)`.
- `ModelCommand` (no args): uses `registry.available_models()` with current model highlighted.
- `ModelCommand` (with args): direct set, no validation.

### 3.4 Modified: `apps/coding-agent/src/main.rs`

- Create `ProviderRegistry`, call `load_auth()`.
- Startup recovery: match env api_base to auth.json entry, or restore first entry.
- Model factory uses `config.current_compat()` for compat selection.

### 3.5 Modified: `apps/coding-agent/Cargo.toml`

- Ensure `reqwest` (with json + rustls-tls features) and `serde` (with derive) are present.

## 4. Key Data Structures

```rust
// provider.rs

/// Built-in provider definition with static metadata.
pub struct ProviderInfo {
    pub name: &'static str,
    pub api_base: &'static str,
    pub default_model: &'static str,
    pub compat: ProviderCompat,
}

/// Persisted auth entry in ~/.yushan/auth.json.
#[derive(Serialize, Deserialize)]
pub struct AuthEntry {
    pub provider: String,
    pub api_base: String,
    pub api_key: String,
    pub default_model: String,
    pub saved_at: String, // ISO 8601
}

/// Result of fetching models from a provider API.
pub enum FetchModelsResult {
    Ok(Vec<ApiModel>),
    Unauthorized,   // 401 -- invalid credentials
    NetworkError(String), // timeout, DNS, connection refused
}

/// Model info returned by provider API (OpenAI-compatible /v1/models).
#[derive(Deserialize)]
pub struct ApiModel {
    pub id: String,
    pub owned_by: Option<String>,
}

/// Central registry: providers + auth persistence + model cache.
pub struct ProviderRegistry {
    providers: Vec<ProviderInfo>,
    auth_entries: Vec<AuthEntry>,
    model_cache: HashMap<String, Vec<ApiModel>>, // provider_name -> models
    auth_path: PathBuf,
}
```

## 5. Data Flows

### 5.1 `/login` (interactive)

```
1. ctx.config.registry.providers() -> list of ProviderInfo
2. Mark providers with existing auth in selection list (checkmark)
3. User selects provider, enters API key (and API base for custom)
4. registry.save_auth(provider, api_base, api_key) -> writes ~/.yushan/auth.json
5. Set config fields: api_base, api_key, model = provider.default_model
6. Spawn fetch_models() in background (non-blocking); on success, cache result
7. ctx.config.build_model() -> agent.set_model(Some(model))
8. Print summary
```

### 5.2 `/model` (no args)

```
1. ctx.config.registry.available_models(current_model) ->
   - Return cached models for current provider
   - Fallback to static known_models() if cache empty
2. Show models in inquire::Select with "model_name  (current)" highlighting
3. On selection: update config.model, rebuild model, set on agent
```

### 5.3 `/logout`

```
1. registry.remove_auth(current_provider) -> update auth.json
2. Clear config fields: api_base = None, api_key = None
3. Set model to sensible default or empty string
4. agent.set_model(None)
5. Print confirmation
```

### 5.4 Startup recovery (main.rs)

```
1. Create ProviderRegistry (loads ~/.yushan/auth.json if exists)
2. If env vars provide api_base + api_key:
   a. Find matching auth.json entry by api_base
   b. If found: use its provider name for compat
   c. If not found: create ad-hoc entry
3. If no env vars but auth.json has entries:
   a. Restore first entry -> set config fields
4. Register model factory with current_compat()
5. Build initial model
```

## 6. Test Plan

### Unit Tests (9)

| # | File | Test | What it verifies |
|---|------|------|------------------|
| 1 | provider.rs | `auth_roundtrip` | save_auth -> load_auth -> data matches |
| 2 | provider.rs | `auth_remove` | remove_auth deletes entry, others untouched |
| 3 | provider.rs | `compat_mapping` | deepseek -> reasoning_content, minimax -> tool_calls_as_text, custom -> standard |
| 4 | provider.rs | `fetch_models_url_construction` | URL built correctly from api_base |
| 5 | provider.rs | `known_models_fallback` | available_models() returns static list when cache empty |
| 6 | config.rs | `config_from_env` | env var loading still works |
| 7 | config.rs | `config_model_factory` | factory build_model still works with registry |
| 8 | config.rs | `config_current_compat_delegates` | current_compat() calls registry.compat_for() |
| 9 | builtin.rs | `login_sets_model` | LoginCommand flow sets config + agent model |

### Manual Tests (2)

| # | What | Steps |
|---|------|-------|
| 1 | Full login flow | `yushan-coding-agent`, /login deepseek, enter key, verify auth.json written |
| 2 | Startup recovery | Set YUSHAN_API_BASE=YUSHAN_API_KEY, restart, verify provider detected |
