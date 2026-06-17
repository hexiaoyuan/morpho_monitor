# CLAUDE.md — morpho_monitor

> Multi-chain Morpho automated risk hedging & real-time alerting system.
> Rust backend + single-page HTML frontend.

## Quick commands

```bash
cargo build                  # debug build
cargo build --release        # release build
cargo test --lib                     # works in parallel
cargo test -p morpho_monitor
cargo check                  # fast compile check, no output binary

# Run (requires data/ dir)
mkdir -p data
export MORPHO_ADMIN_ADDRESS=0x...      # mandatory
export MORPHO_HOT_WALLET_KEY=0x...     # needed for executor
export MORPHO_JWT_SECRET=...           # optional, auto-generated
./target/debug/morpho_monitor          # or release
```

## Architecture

```
main.rs  →  init_app_state()  →  build_router()
   │              │                    │
   ├─ GqlMonitor.spawn         Axum routes (see api/mod.rs)
   ├─ monitor::start_monitors  CORS + ServeDir("static")
   └─ axum::serve(listener)

Request flow:
  Browser → CORS → api_router → FromRequestParts<AppState> → AuthUser → handler
                ↘ fallback: ServeDir("static")
```

### Module map

| File | Role |
|---|---|
| `main.rs` | Entry: tokio runtime, CORS, static files, spawns background monitors |
| `lib.rs` | `init_app_state()` — loads config + 3 JSON files into `AppState` |
| `config.rs` | `AppConfig::load()` — TOML file → env var overrides, `AppConfig::default()` fallback |
| `models.rs` | All data types: `Order`, `Authorization`, `WhitelistEntry`, `AlertConfig`, `AppState`, `MonitorState`, API request/response types |
| `error.rs` | `AppError` enum (10 variants) → HTTP status codes via `IntoResponse` |
| `auth.rs` | JWT create/verify, `AuthUser` extractor (`FromRequestParts`), SIWE verification |
| `alert.rs` | `AlertState` debounce state machine, `AlertManager` (per-user feishu, token cache) |
| `monitor.rs` | `ChainMonitor` — per-chain RPC polling, nonce invalidation, position health eval |
| `gql_monitor.rs` | `GqlMonitor` — zero-config Morpho GraphQL polling (~60s), health factor approximation |
| `executor.rs` | `BotExecutor` — atomic Multicall3 tx: `setAuthorizationWithSignature` + `withdrawCollateral` |
| `api/mod.rs` | Router tree: `/api/auth`, `/api/orders`, `/api/alerts`, `/api/admin`, `/api/health` |
| `api/auth.rs` | `GET /nonce`, `POST /login` |
| `api/orders.rs` | CRUD: `GET/POST /`, `GET/PUT/DELETE /:id` |
| `api/alerts.rs` | `GET/PUT /`, `POST /test` |
| `api/admin.rs` | `GET/POST /whitelist`, `DELETE /whitelist/:address` |
| `static/index.html` | Single-page frontend: Viem + SIWE + EIP-712 signing + feishu config + watchlist |

## State & persistence

`AppState` is the central shared state — cloned into every handler and background task:

```
AppState
├── orders:        Arc<RwLock<HashMap<String, Order>>>       → data/orders.json
├── whitelist:     Arc<RwLock<HashMap<String, WhitelistEntry>>> → data/whitelist.json
├── alert_configs: Arc<RwLock<HashMap<String, AlertConfig>>> → data/alerts.json
├── monitor_states:Arc<RwLock<HashMap<String, MonitorState>>> (in-memory only)
├── nonce_store:   Arc<RwLock<HashMap<String, (String, i64)>>> (in-memory, SIWE nonces)
├── config:        Arc<AppConfig>                              (immutable after load)
└── jwt_secret:    String
```

- **Direct writes**: JSON files use `fs::write` directly to the target path (no tmp-rename).
- **GQL monitor** and **RPC monitors** write into `monitor_states` in-memory; not persisted across restarts.
- **RWLocks** are `tokio::sync::RwLock` (async), not `std::sync::RwLock`.

## Config resolution order

1. Read `config.toml` (path from `MORPHO_CONFIG` env var, default `./config.toml`)
2. If file missing, use `AppConfig::default()` (all empty/defaults)
3. Env var overrides (highest priority): `MORPHO_ADMIN_ADDRESS`, `MORPHO_HOT_WALLET_KEY`, `MORPHO_GQL_URL`, `MORPHO_SERVER_PORT`, `RPC_*_HTTP`, `RPC_*_WS`
4. **Hard requirement**: `config.admin.address` must be non-empty → set `MORPHO_ADMIN_ADDRESS` or `[admin]` in TOML

JWT secret resolution (special):
1. `MORPHO_JWT_SECRET` env var
2. `data/jwt_secret` file
3. Auto-generate UUID v4 + write to `data/jwt_secret`

## Auth flow

```
GET /api/auth/nonce?address=0x…  →  uuid v4, 5min TTL, stored in nonce_store
POST /api/auth/login {message, signature}
  → SIWE parse + verify (nonce match, expiry check)
  → role: "admin" if address == config.admin.address
          "user"  if address ∈ whitelist
          reject  otherwise
  → JWT issued (168h TTL), claims: {address, role, exp, iat, jti}
```

- `AuthUser` implements `FromRequestParts<AppState>` — reads `Authorization: Bearer <token>`, validates JWT, returns `{address, role}`.
- Admin-only routes call `require_admin(&user)` which returns `AppError::Forbidden` if role != "admin".

## Alert debounce state machine

Per position (key = `chain:market:user`), `AlertState` has:

```
in_alert: bool          — true = currently in risky state
backoff_level: u32      — 0–7, controls alert interval (0=instant, 1=1m, 2=2m, …, 7=64m cap)
last_alert_at: i64      — unix timestamp of last alert
normal_streak: u32      — consecutive normal rounds, need ≥3 for recovery
```

Decision matrix:
- First risk trigger → instant alert + execute, `backoff_level=1`
- Ongoing risk + backoff elapsed → re-alert, `backoff_level++`
- Ongoing risk within backoff → suppress
- Normal detected while in_alert → `normal_streak++`, suppress until ≥3 → "recovered" notification
- Risk returns during recovery streak → `normal_streak` resets to 0, back to alert branch

## Chains & contract addresses

| Chain | Morpho Blue | Multicall3 |
|---|---|---|
| Ethereum | `0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` |
| Base | `0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` |
| Optimism | `0xce95AfbB8EA029495c66020883F87aaE8864AF92` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` |
| Arbitrum | `0x6c247b1F6182318877311737BaC0844bAa518F5e` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` |
| Unichain | `0x8f5ae9CddB9f68de460C77730b018Ae7E04a140A` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` |
| HyperEVM | `0x68e37dE8d93d3496ae143F2E900490f6280C57cD` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` |

Source: `monitor.rs:morpho_address()` and `executor.rs:BotExecutor::MULTICALL3`.

## Key invariants

- **RPC monitors only start if `rpc_http` is configured** for that chain. Without RPC, the GQL monitor is the sole data source.
- **GQL monitor is always-on** — launched unconditionally from `main.rs`, 60s polling.
- **Feishu is per-user** — each user configures their own app credentials via `PUT /api/alerts`, stored in `alerts.json`. There is no global `[feishu]` config section.
- **Orders are validated but NOT verified on-chain at creation time** — the authorization signature is only validated when a tx is actually executed.
- **Nonce invalidation** — the RPC monitor watches `NonceIncremented` events; if a user's nonce advances beyond the order's nonce, the order is marked `Invalid`.
- **User addresses are always lowercase** — normalized at login and whitelist entry points.
- **Alloy-rs v2** is used (not v1 or v3) — ensure dependency features match (`alloy = { version = "2", features = [...] }`).

## Testing notes

- Tests use `tempfile` for isolated config files — no real `config.toml` needed.
- HTTP tests use `tower::ServiceExt::oneshot` against router directly — no real TCP listener.
- Tests write to `data/*.json` directly (no tmp-rename), so parallel execution is safe.
- Mock external services (RPC, feishu, GQL) are NOT mocked — tests only cover pure logic and API routing.

## Adding a new chain

1. `config.rs` — add `pub newchain: Option<ChainConfig>` to `ChainsConfig`
2. `config.rs` — add env var override in `AppConfig::load()` (`env_override!` macro)
3. `monitor.rs` — add entry to `morpho_address()` match
4. `monitor.rs` — add `("newchain", state.config.chains.newchain.as_ref())` to `start_monitors()` vec
5. `static/index.html` — add to `CHAIN_IDS` and `MORPHO_ADDRS` objects
6. Update docs: `config.example.toml`, `.env.example`, `prompt.md` Appendix B
