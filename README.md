# 🛡️ morpho_monitor

> Morpho监控,实时告警,机器人条件单平仓!

[![Rust](https://img.shields.io/badge/rust-1.75+-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-58%20passed-green.svg)](.)

用户签署一次 EIP-712 授权，后端 24/7 监控多链 Morpho Blue 仓位健康度。触发清算阈值时，热钱包原子化执行清退并通过飞书秒级通知 — 解决冷/硬件钱包无法及时响应链上危机的痛点。

---

## 🏗️ 架构

```
前端 (GitHub Pages)          Rust 后端 (自部署)           区块链
┌──────────────────┐       ┌─────────────────────┐    ┌──────────┐
│  index.html      │─JWT──▶│  Axum HTTP API       │    │ Ethereum │
│  TailwindCSS     │       │  /api/auth/*         │    │ Base     │
│  Viem (EIP-712)  │       │  /api/orders         │◀───│ Optimism │
│  LocalStorage    │       │  /api/admin/*        │    │ Arbitrum │
└──────────────────┘       │                      │    │ Unichain │
                           │  AlertManager 🚨      │───▶│ HyperEVM │
                           │  Monitor (Alloy-rs)   │    └──────────┘
                           │  GqlMonitor (fallback)│    ┌──────────┐
                           │  Executor (Multicall) │───▶│ 飞书通知  │
                           └─────────────────────┘    └──────────┘
```

## ✨ 核心功能

- **🔐 SIWE 登录** — 钱包签名即登录，JWT 无状态鉴权，Admin/User 二级权限
- **📝 条件单** — 设置健康因子/LLTV 阈值，触发时自动执行清退
- **✍️ EIP-712 授权** — 用户在前端签署 Morpho Blue `setAuthorizationWithSignature`，私钥不离开钱包
- **⚡ 原子化执行** — Multicall3 单笔交易捆绑授权+提款，防止 MEV 夹击
- **🔔 飞书告警** — 指数退避防抖 + 3 轮恢复确认，避免告警轰炸
- **🌐 多链监控** — Ethereum / Base / Optimism / Arbitrum / Unichain / HyperEVM 独立 RPC 轮询 + GQL 零配置回退
- **📊 自选看板** — 关注市场列表，浏览器 LocalStorage 持久化

## 🚀 快速开始

### 前端（用户）

浏览器打开：

```
https://hexiaoyuan.github.io/morpho_monitor/
```

页面默认连接 `http://localhost:16800`，可在左上角修改为你的服务器地址。

### 后端（部署者）

```bash
# 1. 克隆
git clone https://github.com/hexiaoyuan/morpho_monitor.git
cd morpho_monitor

# 2. 配置
cp config.example.toml config.toml
# 编辑 config.toml — 填入 RPC URL、热钱包私钥、Admin 地址

# 3. 构建 & 测试
cargo build --release
cargo test --lib

# 4. 启动
export MORPHO_HOT_WALLET_KEY="0x..."     # 热钱包私钥
export MORPHO_JWT_SECRET="$(uuidgen)"    # JWT 密钥
mkdir -p data
./target/release/morpho_monitor
```

详细部署指南 → [`deploy.md`](./deploy.md)

## 📁 项目结构

```
├── Cargo.toml
├── config.example.toml        # 配置模板
├── deploy.md                  # 部署指南
├── prompt.md                  # 设计文档
├── static/
│   └── index.html             # 前端页面（GitHub Pages）
├── data/                      # 运行时数据
│   ├── orders.json
│   ├── whitelist.json
│   └── alerts.json
└── src/
    ├── main.rs                # 入口（CORS + 静态文件）
    ├── lib.rs                 # 模块注册
    ├── config.rs              # TOML 配置 + 环境变量覆盖
    ├── models.rs              # 数据结构 + AppState
    ├── error.rs               # 统一错误类型
    ├── auth.rs                # JWT + SIWE + FromRequestParts
    ├── alert.rs               # 飞书通知 + 防抖状态机
    ├── monitor.rs             # 多链 RPC 监控引擎
    ├── gql_monitor.rs         # GraphQL 零配置回退
    ├── executor.rs            # Multicall 交易执行
    └── api/
        ├── mod.rs             # 路由树
        ├── auth.rs            # /api/auth/*
        ├── orders.rs          # /api/orders CRUD
        ├── alerts.rs          # /api/alerts 配置
        └── admin.rs           # /api/admin/whitelist
```

## 🧪 测试

```bash
cargo test --lib
```

```
test result: ok. 58 passed; 0 failed; 0 ignored
```

## 🛠️ 技术栈

| 层 | 技术 |
|---|---|
| 后端语言 | Rust (edition 2021) |
| 异步运行时 | Tokio |
| HTTP 框架 | Axum 0.8 |
| 链上交互 | Alloy-rs v2 (Provider, Solidity ABI, Signer) |
| EIP-712 | alloy-sol-types + Viem (前端) |
| SIWE | siwe-rs 0.6 |
| JWT | jsonwebtoken 9 |
| 前端 | HTML5 + TailwindCSS + Viem + Vanilla JS |
| 通知 | 飞书开放平台 API |
| 数据源 | Morpho GraphQL API (零配置回退) |
| 数据存储 | 本地 JSON |

## 📖 文档

| 文档 | 说明 |
|---|---|
| [`prompt.md`](./prompt.md) | 完整项目设计文档（中文） |
| [`deploy.md`](./deploy.md) | 部署指南（systemd、GitHub Pages、飞书配置） |
| [`config.example.toml`](./config.example.toml) | 配置文件模板 |

## ⚠️ 免责声明

本软件按"原样"提供，不提供任何形式的明示或暗示担保。使用本软件进行链上操作的风险由用户自行承担。作者不对因使用本软件而导致的任何资产损失负责。

在生产环境使用前，请在测试网充分验证所有功能。
