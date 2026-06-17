# morpho_monitor 部署指南

## 1. 环境要求

| 依赖 | 版本 |
|---|---|
| Rust | ≥ 1.75 (需要原生 async-trait 支持) |
| Git | ≥ 2.x |
| 操作系统 | Linux / macOS (Windows 通过 WSL2) |
| 钱包 | MetaMask 或其他 EIP-712 兼容浏览器钱包 |

---

## 2. 获取代码

```bash
git clone git@github.com:hexiaoyuan/morpho_monitor.git
cd morpho_monitor
```

---

## 3. 构建后端

```bash
# Debug 构建（开发用）
cargo build

# Release 构建（部署用）
cargo build --release
```

验证测试通过：

```bash
cargo test --lib -- --test-threads=1
```

---

## 4. 配置文件

从模板复制并编辑：

```bash
cp config.example.toml config.toml
```

编辑 `config.toml`，填入真实值：

```toml
[server]
host = "0.0.0.0"      # 监听地址
port = 16800           # 监听端口

[admin]
address = "0xYourAdminWalletAddress"   # 管理员钱包地址 (EIP-55 格式)

[hot_wallet]
private_key = "0x..."                  # 热钱包私钥（或通过环境变量 MORPHO_HOT_WALLET_KEY 设置）
gas_min_balance = "0.1"                # ETH 最低余额告警阈值

[chains.ethereum]
rpc_http = "https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"
polling_interval_secs = 12

[chains.base]
rpc_http = "https://base-mainnet.g.alchemy.com/v2/YOUR_KEY"
polling_interval_secs = 12

# [chains.optimism]                         # 可选
# rpc_http = "https://opt-mainnet.g.alchemy.com/v2/YOUR_KEY"

# [chains.arbitrum]                         # 可选
# rpc_http = "https://arb-mainnet.g.alchemy.com/v2/YOUR_KEY"

# [chains.unichain]                         # 可选
# rpc_http = "https://mainnet.unichain.org"

# [chains.hyperevm]                         # 可选
# rpc_http = "https://rpc.hyperevm.xyz"
```

> **安全提示**: 热钱包私钥强烈建议通过环境变量 `MORPHO_HOT_WALLET_KEY` 设置，而非明文写在 config.toml 中。

---

## 5. 设置环境变量

```bash
# 必需：热钱包私钥（如果不写在 config.toml 里）
export MORPHO_HOT_WALLET_KEY="0x你的私钥"

# 推荐：JWT 签名密钥（固定值确保重启后 token 仍有效）
export MORPHO_JWT_SECRET="$(uuidgen)"

# 可选：覆盖 config.toml 中的其他配置
export MORPHO_ADMIN_ADDRESS="0x..."
export MORPHO_GQL_URL="https://api.morpho.org/graphql"
export RPC_ETH_HTTP="https://..."
export RPC_BASE_HTTP="https://..."
export RPC_OPTIMISM_HTTP="https://..."
export RPC_ARBITRUM_HTTP="https://..."
export RPC_UNICHAIN_HTTP="https://..."
export RPC_HYPEREVM_HTTP="https://..."
```

---

## 6. 启动服务

```bash
# 创建数据目录
mkdir -p data

# 启动
./target/release/morpho_monitor
```

日志输出示例：

```
2026-06-17T10:00:00Z  INFO morpho_monitor starting on 0.0.0.0:16800
2026-06-17T10:00:00Z  INFO Frontend: http://0.0.0.0:16800
2026-06-17T10:00:00Z  INFO Spawned monitor for chain 'ethereum'
2026-06-17T10:00:00Z  INFO Spawned monitor for chain 'base'
```

---

## 7. 后台运行（可选）

### 使用 nohup

```bash
nohup ./target/release/morpho_monitor > morpho_monitor.log 2>&1 &
echo $! > morpho_monitor.pid
```

停止：

```bash
kill $(cat morpho_monitor.pid)
```

### 使用 systemd（推荐）

创建 `/etc/systemd/system/morpho_monitor.service`：

```ini
[Unit]
Description=morpho_monitor - Multi-chain Morpho Risk Hedging
After=network.target

[Service]
Type=simple
User=ubuntu
WorkingDirectory=/home/ubuntu/morpho_monitor
Environment="MORPHO_HOT_WALLET_KEY=0x..."
Environment="MORPHO_JWT_SECRET=your-secret-here"
ExecStart=/home/ubuntu/morpho_monitor/target/release/morpho_monitor
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

启动：

```bash
sudo systemctl daemon-reload
sudo systemctl enable morpho_monitor
sudo systemctl start morpho_monitor
```

查看状态：

```bash
sudo systemctl status morpho_monitor
sudo journalctl -u morpho_monitor -f
```

---

## 8. GitHub Pages 部署前端

前端是一个纯静态 HTML 文件，通过 GitHub Pages 发布。

### 8.1 启用 GitHub Pages

1. 打开仓库 → **Settings** → **Pages**
2. **Source**: `Deploy from a branch`
3. **Branch**: `main`，文件夹选择 `/static`
4. 点击 **Save**

等待几分钟后，页面地址为：

```
https://hexiaoyuan.github.io/morpho_monitor/
```

### 8.2 首次使用

1. 浏览器打开 `https://hexiaoyuan.github.io/morpho_monitor/`
2. 页面左上角确认服务器地址为 `http://localhost:16800`（或修改为你的服务器地址）
3. 点击 💾 保存
4. 连接钱包 → 签名登录
5. 开始使用

---

## 9. 热钱包充值

执行清退交易需要 ETH 作为 Gas 费。向热钱包地址转入 ETH：

```bash
# 查看热钱包地址
cast wallet address --private-key "0x你的私钥"

# 转入适量 ETH（建议 ≥ 0.5 ETH）
```

---

## 10. 飞书通知配置（可选）

飞书通知采用全用户级配置，每个用户独立设置自己的飞书应用凭证：

1. 在 [飞书开放平台](https://open.feishu.cn) 创建自建应用
2. 获取 `app_id` 和 `app_secret`
3. 在应用中获取目标用户的 `user_openid`（ou_xxx 格式）
4. 用户登录后在页面的 **🔔 飞书配置** Tab 中填入昵称、App ID、App Secret、OpenID
5. 点击保存 → 使用 **🧪 通知测试** 验证配置是否正确

配置持久化在服务端 `data/alerts.json`，按用户地址索引。

---

## 11. 验证部署

```bash
# 1. 健康检查
curl http://localhost:16800/api/health
# → {"status":"ok","service":"morpho_monitor"}

# 2. 获取登录 Nonce
curl "http://localhost:16800/api/auth/nonce?address=0x1234567890123456789012345678901234567890"
# → {"success":true,"data":"<uuid>"}

# 3. 检查前端页面
curl -s http://localhost:16800/ | head -5
# → <!DOCTYPE html>...
```

---

## 12. 目录结构

```
morpho_monitor/
├── Cargo.toml
├── config.example.toml    # 配置模板
├── config.toml            # 实际配置（需手动创建，勿提交）
├── deploy.md              # 本文档
├── prompt.md              # 项目设计文档
├── static/
│   └── index.html         # 前端页面（GitHub Pages 根目录）
├── data/                  # 运行时数据（自动创建）
│   ├── orders.json
│   ├── whitelist.json
│   └── alerts.json
└── src/
    ├── main.rs            # 入口
    ├── lib.rs             # 模块注册
    ├── config.rs          # 配置加载
    ├── models.rs          # 数据结构
    ├── error.rs           # 错误类型
    ├── auth.rs            # JWT + SIWE
    ├── alert.rs           # 飞书通知 + 防抖
    ├── monitor.rs         # 多链监控
    ├── gql_monitor.rs         # GraphQL 零配置回退
    ├── executor.rs        # 交易执行
    └── api/               # HTTP API
        ├── mod.rs
        ├── auth.rs
        ├── orders.rs
        ├── alerts.rs
        └── admin.rs
```
