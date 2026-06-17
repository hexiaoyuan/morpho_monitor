## 🤖 项目开发提示词 (Prompt): morpho_monitor

---

## 1. 项目概述与全局架构

* **定位**：多链 Morpho 自动化风控与实时告警系统（Rust 后端 + 极简前端）。
* **痛点**：用户无法 24/7 监控仓位清算风险，危机发生时冷/硬件钱包提款响应太慢。
* **方案**：用户在前端签署 EIP-712 授权并配置飞书告警；后端实时监控多链状态。触发阈值时由热钱包原子化打包交易、垫付 Gas 执行清退，并通过飞书秒级通知。

### 📡 架构关系图

```
                      +---------------------------------------+
                      |             极简前端页面               |
                      |    (SIWE 登录 / 预签名 / 飞书配置)     |
                      +-------------------+-------------------+
                                          |
                                    HTTP  | (JWT Auth)
                                          v
+-----------------------------------------+-------------------------------------------+
|                                  Rust 后端服务                                        |
|                                                                                      |
|  +--------------------------+  +--------------------------+  +---------------------+  |
|  |   M3: HTTP-RPC API       |  |   M1: 状态监控引擎       |  |  M5: AlertManager   |  |
|  | (Axum 路由 / JWT 权限守卫)|  | (Alloy-rs / 内存状态机)  |  | (飞书 Token 自动刷新)|  |
|  +------------+-------------+  +------------+-------------+  +----------+----------+  |
|               |                           |                           |              |
|               v                           v                           v              |
|        本地 JSON 存储               多链 RPC 节点              用户飞书客户端        |
|  (orders / whitelist / alerts)  (Eth / Base / OP / Arb /     (接收预警与提款通知)       |
|                                  Unichain / HyperEVM)                                  |
|                                           |                                         |
|                                           v                                         |
|                              +------------+-------------+                           |
|                              |   M4: 执行机器人 (Bot)  |                           |
|                              | (热钱包原子化 Multicall) |                           |
|                              +------------+-------------+                           |
|                                           |                                         |
|                              +------------+-------------+                           |
|                              |  M6: GQL 监控 (GqlMonitor)|                          |
|                              | (Morpho GraphQL 零配置)  |                           |
|                              +--------------------------+                           |
+-------------------------------------------+-----------------------------------------+
                                            |
                                            v (私密广播 / Flashbots)
                                         [区块链]
```

---

## 2. 技术栈选型

* **后端 (Rust)**：Tokio 异步架构，Alloy-rs 链上交互与事件监听，Axum 提供 HTTP API。
* **安全验证**：`siwe`（Sign-In with Ethereum 标准），`jsonwebtoken`（JWT 状态保持）。
* **前端 (Single HTML)**：原生 HTML5 + TailwindCSS + Vanilla JS + Viem，LocalStorage 存储自选列表与服务器地址，支持 GitHub Pages 托管。
* **数据存储**：服务器本地轻量化 JSON 文件存储（`orders.json`、`whitelist.json`、`alerts.json`），通过 `Arc<RwLock<…>>` 控制并发读写。
* **支持的链**：

| 链 | 说明 |
|---|---|
| **Ethereum** 主网 | Morpho Blue 主要部署链，优先级最高 |
| **Base** | L2，低 Gas 成本 |
| **Optimism** | OP Stack L2 |
| **Arbitrum** | Arbitrum One L2 |
| **Unichain** | Uniswap L2 |
| **HyperEVM** | 高性能 EVM 兼容链 |

> 前端通过 GitHub Pages 托管于 `hexiaoyuan.github.io/morpho_monitor`，与后端服务分离部署。页面内置服务器地址配置（默认 `http://localhost:16800`），用户可自定义并保存到浏览器 LocalStorage。

---

## 3. 核心机制与冷热钱包交互流程

### 3.1 授权核心：Morpho Blue `setAuthorizationWithSignature`

后端不持有用户私钥，必须通过 Morpho 原生支持的 **EIP-712 结构化签名**获得操作权限。

#### Domain Separator

```solidity
bytes32 constant DOMAIN_TYPEHASH = keccak256(
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
);
```

#### Authorization 类型

```solidity
struct Authorization {
    address authorizer; // 用户冷钱包
    address authorized; // 服务器热钱包
    bool isAuthorized;  // 设定为 true 表示授权
    uint256 nonce;      // 用户在 Morpho 的当前 Nonce（通过 publicNonces 查询）
    uint256 deadline;   // 签名截止时间（如：1 年）
}

bytes32 constant AUTHORIZATION_TYPEHASH = keccak256(
    "Authorization(address authorizer,address authorized,bool isAuthorized,uint256 nonce,uint256 deadline)"
);
```

#### ⚠️ 关键注意事项

* **单次有效**：一旦成功触发 `setAuthorizationWithSignature`，用户在 Morpho 的 Nonce 会自增（`++publicNonces[authorizer]`），当前签名即刻失效。后续保护需用户重新签名提交新订单。
* **Nonce 监控**：监控模块需持续监听 `NonceIncremented` 和 `AuthorizationSet` 事件。若用户在外部自行操作（如直接与 Morpho 合约交互）导致 Nonce 改变，后端必须立即：
  1. 作废该用户所有关联的条件单
  2. 通过飞书通知用户「授权已失效，请重新签名」
* **deadline 设计**：建议默认 1 年（`block.timestamp + 365 days`），兼顾安全性与用户体验。

### 3.2 冷热钱包交互时序图

```
[ 用户冷钱包 ]          [ 前端页面 ]              [ 后端服务 ]              [ 飞书服务器 ]
      |                      |                         |                         |
      |-- 1. 配置阈值 ------>|                         |                         |
      |   与飞书通知目标      |                         |                         |
      |                      |                         |                         |
      |<-- 2. 返回待签名 ----|                         |                         |
      |   EIP-712 结构体     |                         |                         |
      |   (前端查链 Nonce)   |                         |                         |
      |                      |                         |                         |
      |-- 3. 钱包签名 ------>|                         |                         |
      |   (零 Gas 消耗)      |                         |                         |
      |                      |-- 4. POST /api/orders ->|                         |
      |                      |   (条件 + 签名 + 飞书)   | (SIWE 验签 / 白名单校验)   |
      |                      |                         | (持久化至 orders.json)   |
      |                      |                         |                         |
      |                      |                         | ~~~ 轮询多链 RPC ~~~    |
      |                      |                         |                         |
      |                      |     [情况 A: 触发阈值]    |                         |
      |                      |                         |-- 5a. 原子化打包交易 -->|
      |                      |                         |   (Multicall 执行提款)  |
      |                      |                         |-- 5b. 飞书通知 --------->|
      |                      |                         |   "提款成功/失败"        |
      |                      |                         |                         |
      |                      |     [情况 B: 未触发阈值]  |                         |
      |                      |                         |  (继续轮询，无动作)       |
      |                      |                         |                         |
      |                      |     [情况 C: Nonce 变更] |                         |
      |                      |                         |-- 6. 飞书通知 ---------->|
      |                      |                         |   "授权失效，请重签"      |
```

#### ⚠️ 原子性执行 (Atomic Execution)

触发提款时，热钱包必须在**单笔交易**中（通过 Multicall 或 Morpho 原生 Batch）同时完成以下操作：

1. `setAuthorizationWithSignature(authorization, signature)` — 激活授权
2. `withdrawCollateral(marketParams, amount, onBehalf, receiver)` — 提取抵押品

**严禁分两笔交易发送**，原因：
* 防止权限在交易间隔被恶意利用（MEV 夹击 / 抢跑）
* 防止第一笔成功后第二笔失败导致资产留存在合约中

---

## 4. 权限与安全认证设计 (SIWE + JWT)

后端采用无状态（Stateless）鉴权架构，确保接口访问安全性。

* **Admin (管理员)**：由环境变量 `ADMIN_ADDRESS` 唯一指定，可访问所有接口，并拥有编辑 `whitelist.json`（增删钱包地址与 Nickname）的专属权限。
* **User (普通用户)**：钱包地址必须存在于 `whitelist.json` 中方可登录并操作自己的条件单。

### 🔐 认证流转步骤

1. 前端请求 `GET /api/auth/nonce?address=0x…` 获取一次性随机数（服务端生成 `uuid v4`，TTL 5 分钟）。
2. 前端组装标准 SIWE (Sign-In with Ethereum) 文本消息，用户冷钱包签名。
3. 前端 `POST /api/auth/login` 提交 `{ message, signature }`。后端通过 `siwe` 库校验签名有效性（含 domain、nonce、expiration、chain-id 校验）后，签发 JWT Token。
   * JWT Payload 包含：小写标准格式的 `address`（EIP-55 校验前先 toLower）、`role`（`admin` 或 `user`）、`exp`（过期时间，建议 24h）。
4. 后续所有敏感请求在 Header 携带 `Authorization: Bearer <JWT>`，Axum 通过 `FromRequestParts<AppState>` 提取器直接从 State 读取 JWT secret 验证 token，提取 `AuthUser { address, role }` 注入 handler。
5. Token 过期后前端自动引导用户重新签名登录。

---

## 5. 告警通知模块 (AlertManager)

支持用户级与系统级飞书自建应用 API 客户端通知，内置防噪防抖算法。

### 5.1 配置规则

飞书通知采用全用户级配置，每个用户在前端页面填写自己的飞书应用凭证和 OpenID，持久化至 `alerts.json`：

* **用户级**：存放于 `alerts.json`（key = user_address）。用户在页面配置自己的飞书 `app_id`、`app_secret`、`user_openid`。若未配置，则不发送通知。
* **Admin 通知**：Admin 同样通过前端页面配置自己的飞书参数。建议 Admin 配置通知以便接收系统级异常告警（如：热钱包 Gas 不足、多链 RPC 断连、JSON 文件读写失败）。

### 5.2 告警防噪、防抖逻辑

为防止阈值边缘临界点时频繁轰炸用户，AlertManager 使用**双计数器状态机**：

* **异常计数器 `backoff_level`**：控制告警间隔的指数退避级别（0-6）。
* **正常计数器 `normal_streak`**：控制恢复确认所需的连续正常轮数。

```
                        [监控指标发生变化]
                               |
                    是否触发风险阈值 (LLTV / 借贷因子)?
                               |
                  +------------+------------+
                  |                         |
                 (是)                      (否)
                  |                         |
        [进入异常状态分支]          [进入正常状态分支]
                  |                         |
          backoff_level == 0?        normal_streak >= 3?
                  |                         |
         +--------+--------+       +--------+--------+
         |                 |       |                 |
        (是)              (否)    (是)              (否)
         |                 |       |                 |
    [立即执行提款]    [检查指数退避] [确认风险解除]    [normal_streak++]
    [发送飞书告警]    [间隔是否已过?] [重置 backoff=0]  [不发送通知]
    [backoff_level=1]        |       [重置 streak=0]
                       +-----+-----+ [发送"恢复"通知]
                       |           |
                      (是)        (否)
                       |           |
                  [发送预警]    [静默忽略]
                  [更新间隔]    [等待下一轮]
                  [backoff++]
```

#### 指数退避间隔表

| `backoff_level` | 告警间隔 |
|:---:|:---:|
| 0 | 即时（首次触发） |
| 1 | 1 分钟 |
| 2 | 2 分钟 |
| 3 | 4 分钟 |
| 4 | 8 分钟 |
| 5 | 16 分钟 |
| 6 | 32 分钟 |
| 7+ | 64 分钟（封顶） |

* **持续告警指数退避 (Exponential Backoff)**：当指标持续保持在阈值之上，每次发送告警后 `backoff_level` 递增，间隔翻倍。
* **恢复确认机制 (Anti-Flapping)**：指标回到正常区间时，必须**连续维持 3 轮**正常记录（`normal_streak >= 3`）才算真正恢复，此时发送"危机解除"通知并重置所有计数器。中间任何一轮重新触发阈值，`streak` 立即归零并切回异常分支。

---

## 6. 功能模块详细设计

### 🧱 M1: 状态监控引擎 (Monitor)

* **多链监听**：通过 WebSocket (推荐) 或 HTTP Polling 同步监听 Ethereum、Base、Optimism、Arbitrum、Unichain、HyperEVM 上 Morpho Blue 合约的链上事件。另有 GQL 零配置回退（Morpho GraphQL API，~60s 延迟）始终在线。
* **内存状态机**：解析 Log 实时维护全量监控对象的 `(MarketParams, positionHealth, lltv, borrowShares, collateralAmount)` 元组，基于 Morpho 的 `lastUpdate` 和 `_accrueFee` 计算当前健康因子。
* **废单清理**：实时捕捉 `NonceIncremented(id, authorizer, newNonce)` 和 `AuthorizationSet` 事件。若 Nonce 增加超出订单中的 nonce 值，说明签名已过期，自动清理该用户所有关联订单并触发通知。
* **可配置性**：每链独立配置 RPC URL、Polling 间隔（默认 12s，即一个以太坊区块时间）、重试策略。

### 🌐 M2: 极简前端页面 (Frontend)

* **部署方式**：单 HTML 文件，通过 GitHub Pages 托管（仓库 Settings → Pages → `/static` 目录），与后端分离部署。
* **服务器地址配置**：页面左上角可自定义后端 API 地址（默认 `http://localhost:16800`），修改后保存到浏览器 LocalStorage。
* **自选看板**：支持用户添加/编辑关注的 Market 列表（存入 LocalStorage）。
* **状态交互**：集成钱包连接（SIWE 登录）、条件单 CRUD、EIP-712 签名触发、白名单管理（Admin 专属视图）。
* **飞书配置**：用户可在页面填写飞书通知目标（`user_openid` 或 Webhook URL）。

### 📡 M3: 后端通讯服务 (HTTP-RPC/API)

采用 Axum 路由守卫机制，通过 `FromRequestParts<AppState>` 提取器直接从 State 读取 JWT secret 验证 token。启用 CORS 允许跨域前端访问，`/api/*` 之外的请求 fallback 到静态文件服务（`static/` 目录）。

| 路由前缀 | 权限 | 说明 |
|---|---|---|
| `/api/auth/*` | 公开 | 获取 Nonce、SIWE 登录验证 |
| `/api/orders` | User / Admin | 条件单 CRUD，持久化至 `orders.json` |
| `/api/alerts` | User / Admin | 飞书通知配置，持久化至 `alerts.json` |
| `/api/admin/whitelist` | Admin | 白名单增删查改（`address` + `nickname`），持久化至 `whitelist.json` |
| `/api/health` | 公开 | 健康检查 |
| `/*` | 公开 | 静态文件服务（前端页面） |

### ⚡ M4: 执行机器人 (Bot Executor)

* **私密广播**：触发时优先通过 Flashbots Protect / MEV-Blocker 等私密 RPC 端点广播，避免在公网 Mempool 被抢跑清算（Front-running）。
* **Multicall 原子化**：使用 Morpho Blue 的 `multicall` 或通用 Multicall3 合约，在单笔交易中捆绑 `setAuthorizationWithSignature` + `withdrawCollateral`。
* **余额预警**：内置热钱包原生代币（ETH）余额监控。余额低于 `GAS_MIN_BALANCE` 阈值时，向 Admin 发送飞书告警"热钱包余额不足，清退可能失败"。

---

## 7. 配置管理与运行环境

### 7.1 配置文件 (`config.toml`)

```toml
[server]
host = "0.0.0.0"
port = 16800

[admin]
address = "0xYourAdminAddress"      # EIP-55 校验格式

[hot_wallet]
private_key = "0x..."                # 热钱包私钥（勿提交至版本控制）
# 可通过环境变量 MORPHO_HOT_WALLET_KEY 覆盖
gas_min_balance = "0.1"              # ETH 最低余额阈值

[chains.ethereum]
rpc_http = "https://eth-mainnet.g.alchemy.com/v2/..."
# rpc_ws = "wss://..."               # WebSocket（可选）
polling_interval_secs = 12

[chains.base]
rpc_http = "https://base-mainnet.g.alchemy.com/v2/..."
polling_interval_secs = 12

# [chains.optimism]                   # 可选链
# rpc_http = "https://opt-mainnet.g.alchemy.com/v2/..."
# polling_interval_secs = 12

# [chains.arbitrum]                   # 可选链
# rpc_http = "https://arb-mainnet.g.alchemy.com/v2/..."
# polling_interval_secs = 12

# [chains.unichain]                   # 可选链
# rpc_http = "https://mainnet.unichain.org"
# polling_interval_secs = 12

# [chains.hyperevm]                   # 可选链
# rpc_http = "https://rpc.hyperevm.xyz"
# polling_interval_secs = 6

# [flashbots]                         # 私密广播（可选）
# rpc_url = "https://rpc.flashbots.net"
```

### 7.2 环境变量覆盖（敏感信息优先使用环境变量）

| 变量名 | 说明 |
|---|---|
| `MORPHO_HOT_WALLET_KEY` | 热钱包私钥 |
| `MORPHO_ADMIN_ADDRESS` | 管理员地址 |
| `MORPHO_JWT_SECRET` | JWT 签名密钥（不设则自动生成） |
| `MORPHO_GQL_URL` | Morpho GraphQL API 地址 |
| `MORPHO_SERVER_PORT` | 服务端口（默认 16800） |
| `RPC_ETH_WS` / `RPC_ETH_HTTP` | 以太坊 RPC |
| `RPC_BASE_WS` / `RPC_BASE_HTTP` | Base RPC |
| `RPC_OPTIMISM_WS` / `RPC_OPTIMISM_HTTP` | Optimism RPC |
| `RPC_ARBITRUM_WS` / `RPC_ARBITRUM_HTTP` | Arbitrum RPC |
| `RPC_UNICHAIN_WS` / `RPC_UNICHAIN_HTTP` | Unichain RPC |
| `RPC_HYPEREVM_HTTP` | HyperEVM RPC |

---

## 8. 错误处理与容错策略

### 8.1 RPC 断连与重试

* WebSocket 断连时自动执行指数退避重连（1s → 2s → 4s → … → 最大 60s）。
* HTTP Polling 失败时记录错误日志，连续失败 ≥ 5 次后通过飞书通知 Admin。
* 多链独立重试，一条链的故障不影响其他链的监控。

### 8.2 交易执行失败处理

* 交易 revert / 超时：记录错误原因，向用户发送飞书通知"清退交易失败：{原因}"。
* Gas 不足：立即停止执行队列，通知 Admin。不重复提交交易避免 Nonce 混乱。
* Nonce 已被使用（用户签名过期）：自动清理订单，通知用户重新签名。

### 8.3 签名校验失败

* SIWE 签名不匹配 → 返回 401 Unauthorized。
* EIP-712 签名在链上验证失败（`setAuthorizationWithSignature` revert）→ 订单标记为 `invalid`，不再重试，通知用户。

### 8.4 JSON 文件读写

* 使用 `Arc<RwLock<HashMap<…>>>` 做内存缓存 + 定期 flush 到文件。
* 写文件使用「先写临时文件 → `fs::rename` 原子替换」策略，防止写一半崩溃导致数据损坏。
* 启动时若 JSON 文件损坏，记录错误日志并从空状态启动（或恢复备份）。

---

## 9. 开发实施步骤 (Implementation Guide)

请 AI 按照以下顺序逐步提供完整可运行的代码，每步完成后应可通过 `cargo build` 编译。

> **依赖关系速览**：
> Step 1（数据结构）→ Step 2（认证）→ Step 3（告警）+ Step 4（API CRUD）
> → Step 5（监控）→ Step 6（执行器）→ Step 7（前端）
>
> Step 3 与 Step 4 可并行开发；Step 5 依赖 Step 1 的数据结构；Step 6 依赖 Step 1 + Step 4。

### Step 1: 核心数据结构与配置加载

* **内容**：定义所有 Rust 结构体与枚举。
  * `Authorization`（EIP-712 授权结构体）
  * `Order`（订单 ID、用户地址、市场标识、触发条件、阈值、签名、状态）
  * `WhitelistEntry`（地址 + Nickname）
  * `AlertConfig`（用户级飞书通知目标）
  * `FeishuTomlConfig`（系统级飞书 app_id / app_secret）
  * `MonitorState`（链上资产状态 + 健康因子）
  * `AlertState`（告警退避计数器 + 恢复确认计数器，在 alert 模块中定义）
  * `AppConfig`（映射 `config.toml` 所有节）
* **依赖**：无
* **验收**：`cargo build` 通过，结构体实现 `Serialize` / `Deserialize`。

### Step 2: Axum 安全与认证模块

* **内容**：
  * `GET /api/auth/nonce` — 生成并缓存 Nonce
  * `POST /api/auth/login` — SIWE 验证 + JWT 签发
  * `AuthExtractor` — 实现 `FromRequestParts`，解析 Bearer Token 并提取 `(address, role)`
  * 二级路由守卫 — Admin 路由额外校验 `role == "admin"`
* **依赖**：Step 1（需要 `AppConfig` 中的 `ADMIN_ADDRESS`）
* **验收**：`curl` 测试登录流程，JWT 后可访问受保护路由，错误 Token 返回 401。

### Step 3: AlertManager 模块

* **内容**：
  * 飞书 Token 获取与自动刷新（缓存 Token，提前 5 分钟续期）
  * 消息发送函数（支持 text / interactive card 格式）
  * 防抖状态机实现（`backoff_level` + `normal_streak` 逻辑）
* **依赖**：Step 1（需要 `FeishuConfig` 结构体）
* **验收**：单元测试覆盖退避序列和恢复确认逻辑；集成测试向飞书测试群发送消息成功。

### Step 4: 条件单、白名单、告警配置的业务 API

* **内容**：
  * `orders.json` — CRUD（用户只能操作自己的订单，Admin 可全局查看）
  * `whitelist.json` — Admin 专属增删查
  * `alerts.json` — 用户配置自己的飞书通知目标
  * JSON 文件读写工具函数（`Arc<RwLock<…>>` + 原子写策略）
* **依赖**：Step 2（需要认证提取器校验权限）
* **验收**：`curl` 测试各端点 CRUD，重启服务后数据持久化不丢失。

### Step 5: Alloy-rs 监控与告警判定逻辑

* **5a: 链上事件监听**
  * 通过 Alloy-rs 的 WebSocket Provider 订阅 Morpho Blue 事件：
    * `Events::SupplyCollateral`, `Events::WithdrawCollateral`, `Events::Borrow`, `Events::Repay`
    * `Events::AuthorizationSet`, `Events::NonceIncremented`
  * 或使用 HTTP Polling（通过 `eth_getLogs` 定期拉取新区块的事件日志）
  * 解析后更新内存中的 `HashMap<MarketId, MonitorState>`
* **5b: GQL 零配置回退（gql_monitor）**
  * 始终在后台轮询 Morpho GraphQL API，无需任何链上 RPC 配置即可工作
  * 查询市场总供应/借贷数据 + 用户仓位，计算近似健康因子
  * 约 60s 延迟，适合作为 RPC 监控的兜底
* **5c: 告警判定**
  * 遍历所有活跃 `Order`，从 `MonitorState` 查询对应 Market 的健康因子
  * 与订单阈值比较，调用 `AlertManager::evaluate_risk()` 结合 `AlertState` 的退避状态决定是否触发提款或发送通知
  * Nonce 变更检测 → 废单 + 通知
* **依赖**：Step 1（数据结构）+ Step 3（AlertManager）+ Step 4（需读取 `orders.json`）
* **验收**：接入测试网（如 Base Sepolia），手动操作仓位触发阈值，确认日志输出正确。

### Step 6: Bot 执行器

* **内容**：
  * 热钱包交易构建（Alloy-rs `TransactionRequest` + Multicall 编码）
  * `setAuthorizationWithSignature` + `withdrawCollateral` 的 calldata 拼装
  * 通过 Flashbots `eth_sendPrivateTransaction` 广播
  * 交易状态跟踪（Pending → Confirmed → 通知 / Reverted → 错误处理）
  * Gas 预估 + 余额检查
* **依赖**：Step 1（`Authorization` 类型）+ Step 4（读取 `Order` 中的签名数据）
* **验收**：在测试网上成功执行一次完整的清退交易（用测试币）。

### Step 7: 单页面前端

* **内容**：
  * 服务器地址配置（默认 `http://localhost:16800`，用户可自定义，保存到 LocalStorage）
  * 钱包连接按钮（通过 `window.ethereum` 兼容 MetaMask 等）
  * SIWE 登录流程（获取 Nonce → 个人签名 → 提交 login → 存储 JWT）
  * 条件单管理面板（列表 + 新建表单 + 取消）
  * EIP-712 签名触发（`eth_signTypedData_v4`）
  * 飞书通知配置表单（GET/PUT `/api/alerts`）
  * Admin 白名单管理（条件渲染，仅 admin 角色可见）
  * Market 自选看板（LocalStorage 持久化）
* **部署**：GitHub Pages（`/static` 目录作为根），与后端分离。
* **依赖**：Step 2（认证 API）+ Step 4（业务 API）
* **验收**：浏览器打开 `https://hexiaoyuan.github.io/morpho_monitor/`，配置服务器地址 → 连接钱包 → 登录 → 创建条件单 → 配置飞书 → 白名单管理全流程。

---

## 附录 A: 术语表

| 术语 | 全称 / 说明 |
|---|---|
| **Morpho Blue** | Morpho 的无预言机借贷协议核心合约 |
| **LLTV** | Liquidation Loan-To-Value，清算借贷比阈值 |
| **EIP-712** | 以太坊结构化数据签名标准，用户在钱包中看到人类可读的签名内容 |
| **SIWE** | Sign-In with Ethereum，基于以太坊钱包的标准登录协议 |
| **JWT** | JSON Web Token，无状态身份凭证 |
| **Multicall** | 单笔交易中批量执行多个合约调用的模式 |
| **Flashbots** | MEV 缓解服务，支持私密交易广播，避免被抢跑 |
| **Nonce** | Morpho Blue 中每个 authorizer 的授权计数器，每次 `setAuthorizationWithSignature` 成功后自增 |
| **热钱包** | 服务器持有的在线私钥钱包，用于垫付 Gas 和执行清退交易 |
| **冷钱包** | 用户离线保存的私钥钱包（如硬件钱包），用于签署授权签名 |
| **Atomic Execution** | 原子化执行 — 多步操作要么全部成功，要么全部回滚 |

## 附录 B: 关键合约地址

| 合约 | Ethereum | Base | Optimism | Arbitrum | Unichain | HyperEVM |
|---|---|---|---|---|---|---|
| Morpho Blue | `0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb` | `0xBBBBBbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb` | `0xce95AfbB8EA029495c66020883F87aaE8864AF92` | `0x6c247b1F6182318877311737BaC0844bAa518F5e` | `0x8f5ae9CddB9f68de460C77730b018Ae7E04a140A` | `0x68e37dE8d93d3496ae143F2E900490f6280C57cD` |
| Multicall3 | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` | `0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9` |

## 附录 C: 部署参考

详细部署步骤见 [`deploy.md`](./deploy.md)，包括：
- 环境准备 & 构建
- 配置文件填写
- systemd 后台运行
- GitHub Pages 前端托管
- 飞书通知配置
- 热钱包充值 & 验证
