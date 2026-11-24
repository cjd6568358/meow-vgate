# meow-server 架构设计

- **状态**：Proposed（分层与边界为现行约束；具体 hook 与体积数值待机主签字）
- **日期**：2026-11-23
- **终局目标**：在 ZTE F4610U 光猫上**单进程**客户端无感替换现役 mihomo——同一份 `config.yaml`、同一监听端口、同一启动入口；fork 同时保留跟随 upstream meow-rs 新版本升级的能力
- **适用范围**：`server/` 下的服务端入站实现，以及为接入它而必须在根 workspace 中新增的最小 fork delta
- **上游基线**：meow-rs tag `v0.29.0` / `de176935`（分支 `server-inbound` 的锚点）
- **行为基准**：`server/zig-docs/go-baseline/` = mihomo v1.19.34 / `0455d0d2` 逐字节
- **目标架构**：`aarch64-unknown-linux-musl`（设备现役二进制实测 ELF64 AArch64，machine `0xb7`）；`armv7-unknown-linux-gnueabihf` 为次级目标
- **相关**：[ADR-0001 上游 transport 契约](../../docs/adr/0001-meow-transport-crate.md)、[ADR-0002 分歧策略](../../docs/adr/0002-upstream-divergence-policy.md)、[体积预算 ADR](adr/0001-server-footprint-budget.md)、[配置兼容与升级 ADR](adr/0002-config-compat-and-upgrade.md)

---

## 1. 目标与非目标

### 目标

1. 单进程运行：一个 `meow` 二进制同时承担原有客户端能力和 VLESS/VMess 服务端入站。
2. **配置文件零改动**：直接读取设备现行 `config.yaml`，包括它的 `listeners:` 段；不拆文件、不生成临时配置、不要求客户端改设置。
3. 端口语义保持：7891 mixed、9090 API、19001 vless+ws、19002 vmess+mkcp。
4. 为 ADR-0001 未覆盖的服务端传输提供对称 `accept()`，但不修改 `meow-transport` 的 client 契约。
5. 协议行为以 v1.19.34 Go 基准对拍；与 meow-rs mainline 冲突时以部署契约和机主签字裁决。
6. **可升级性**：upstream 新 tag 到来时，fork delta 必须是明确、最小、可重放的一小组改动。

### 非目标

- 不把 Zig 参考快照当可编译工程或进度源（只读，冻结于台账 161/214）。
- 不在本阶段实现 TLS/REALITY、gRPC、H2、HTTPUpgrade、XTLS vision、UDP/Mux、用户热更新、多租户计费。
- 不 merge `upstream/main`；只在需要时逐个 cherry-pick。
- 不改变根 workspace 的 `members`；**client 默认构建不含服务端代码**，8 MiB cap 不受影响。

---

## 2. 工作区与依赖边界

```
根 workspace（client crates，含 meow-config / meow-app / meow-tunnel）
   │  meow-app 的 optional feature "server-inbound"
   └──────────────► server/（meow-server，独立 workspace）
                       │  feature "tunnel-bridge"（device 构建开启）
                       └──────► meow-common + meow-tunnel（path 依赖）
```

- `server/Cargo.toml` 自带 `[workspace] members = ["."]`，不被根 `members` 收编。
- 根 `meow-app` 通过 **optional path dependency** 引入 `meow-server`，由 feature
  `server-inbound` 控制；默认关闭，所以普通 client build 与 8 MiB minimal cap 不变。
- `meow-server` 自身保持两个特征：
  - 默认（无 `tunnel-bridge`）：纯 Rust 依赖，可在开发机上做两个 ARM 目标的 `cargo check`。
  - `tunnel-bridge`：加入 `meow-common` / `meow-tunnel` path 依赖，共享 `Arc<Tunnel>`，
    会引入 boring-sys，但需要 C 工具链的只发生在 device 构建。

### 硬规则

| 编号 | 规则 | 理由 |
|---|---|---|
| R1 | 根 `members` 不得加入 server；client 默认 feature 不得开启 `server-inbound` | 保护 8 MiB client cap |
| R2 | `meow-transport` 不得新增 `accept()`，也不得依赖本 crate | ADR-0001 的 client 契约冻结 |
| R3 | 服务端协议栈不依赖任何 `meow-*` crate；只有 `tunnel-bridge` feature 能依赖 | 保持协议层可独立测试 |
| R4 | fork delta 只允许落在本文件 §5.2 登记的 hook 点，且必须逐项登记在 ADR-0002 | 升级时可重放、可核对 |
| R5 | 未知命令/未知用户/畸形头一律硬错关闭，不得静默降级 | ADR-0002 上游分歧策略 Class A 语义 |
| R6 | 新功能命名与配置键跟 mihomo mainline；每个改动带测试 | 交接简报约定 |
| R7 | 设备 `config.yaml` 与凭证不入库；只引用结构键名与设备镜像路径 | 节点凭据/订阅 token 只在设备侧 |

---

## 3. 运行时分层（单进程）

```
config.yaml（唯一事实源，原样读取）
        │  meow-config 解析：mixed/dns/proxies/rules/sniffer + listeners[vless,vmess]
        ▼
meow-app 单一运行时（tokio）
        │
        ├── client 链路：Tunnel::new_with_slot → 规则/拨号/统计 → 7891 mixed / 9090 API
        │
        └── server 链路：meow-server 入站监听 19001/19002
                │  accept
                ▼
        ServerTransport 层：WsAccept / MkcpAccept（未来 TlsAccept）
                │  accept(inner: DynStream) -> DynStream  ← ADR-0001 connect() 的对称面
                ▼
        协议握手层：vless::read_request / vmess 头解析
                │  VlessRequest{user_id, command, port, destination}
                ▼
        RouteSink（tunnel-bridge）→ 共享的同一个 Arc<Tunnel> → 规则匹配 → outbound → relay
```

关键点：**client 与 server 共用同一个 `Tunnel` 实例**，因此规则、DNS、连接统计、API 都是同一套，
不需要二次路由、不需要进程间通信。`meow-app/src/main.rs:1163` 已经创建
`let tunnel = Tunnel::new_with_slot(...)`；listener 在 `:1350` 起的循环里用 `tunnel.clone()`
spawn。server 入站就是这条 match 的新增分支。

### 3.1 传输层

`ServerStream = AsyncRead + AsyncWrite + Unpin + Send + Sync`（`server/src/stream.rs`）；
`ServerTransport::accept()` 在 `server/src/transport.rs`。它与 `meow_transport::Stream`
结构等价但类型不兼容（后者多 `Any + as_any_mut()`），这是刻意的：不为对齐签名而改 client trait。

`WsByteStream` 是必要适配层：tokio-tungstenite 0.29 只暴露 `Stream<Message>/Sink<Message>`，
没有字节 IO。它把 Binary 帧映射成字节流，回复 Ping，Close/EOF 映射为读 EOF，Text 帧按 Class A 硬错。

### 3.2 协议层

VLESS 请求头：`version(1) | uuid(16) | addons_len(1) | addons(N) | command(1) | port(2,BE) | atyp(1) | addr`。
注意 **PortThenAddress** 与 **command 从 1 开始**（TCP=1/UDP=2/Mux=3）。响应头 `[0,0]` 只在首次写前置一次。
VMess 头与 mKCP 栈见 `zig-docs/source/listener/vmess/` 与 `zig-docs/go-baseline/transport/mkcp/`。

---

## 4. 能力边界矩阵

| 能力 | 当前状态 | 边界说明 |
|---|---|---|
| 19001 TCP listen + WS Upgrade | 已实现（独立 crate） | 路径精确匹配；未接进 meow-app |
| VLESS 头解析 / UUID 鉴权 | 已实现 | TCP 命令；UDP/Mux 显式拒绝 |
| 响应头与载荷透传 | 已实现 | 到 `SessionHandler` 为止 |
| 19002 mKCP 传输 | 占位 | `MkcpAccept::accept` 返回 `NotImplemented` |
| VMess AEAD 头 | 未实现 | 需按 v1.19.34 逐字节对拍 |
| TLS / WSS | 未实现 | 部署当前用明文 WS；后置 |
| gRPC / H2 / HTTPUpgrade / vision | 不计划 | 无部署契约 |
| 配置 | 骨架 CLI | 必须扩展 meow-config 支持 vless/vmess listener，见 §5 |
| 单进程接入 | 设计完成，未实现 | 见 §5.2 的 hook 清单 |
| client 默认构建 | 不受影响 | feature 关闭时无 server 代码 |

---

## 5. 与上游功能代码的对接（fork delta）

### 5.1 对接点：不需要改上游的路由实现

`meow_tunnel::route_inbound_tcp` 虽标 `#[doc(hidden)]`，但它是真实 `pub` 且从 crate 根导出：

- `crates/meow-tunnel/src/tcp.rs:378`：`pub async fn route_inbound_tcp<C>(inner: &TunnelInner, conn: &mut C, metadata: Metadata, prefix: &[u8]) where C: AsyncRead + AsyncWrite + Unpin + Send + 'static`
- `crates/meow-tunnel/src/lib.rs:24`：`pub use tcp::{route_inbound_tcp, ConnectionGuard};`
- `crates/meow-tunnel/src/tunnel.rs:1087`：`pub fn inner(&self) -> &Arc<TunnelInner>`

`DynStream = Box<dyn ServerStream>` 直接满足其泛型上界，**不需要**转换成
`meow_transport::Stream`，也不需要实现 `as_any_mut()`。Rust 代码里没有 `ConnContext`
类型（AGENTS.md 里是旧称），替代物是 `meow-common/src/conn.rs:6` 的 `ProxyConn`，本对接用不到。

Metadata（`crates/meow-common/src/metadata.rs:14`）最小映射：

| 字段 | 来源 |
|---|---|
| `network` | `"tcp"` |
| `src_ip` / `src_port` | accept 的 peer 地址 |
| `host` / `dst_ip` / `dst_port` | VLESS `Destination` 与 port |
| `in_name` / `in_port` | `"vless-ws"` / `19001` |

### 5.2 fork delta 清单（唯一允许改的上游文件）

| 文件 | hook | 改动 |
|---|---|---|
| `crates/meow-config/Cargo.toml` | features | 新增 `server-inbound` feature |
| `crates/meow-config/src/raw.rs:266` 附近 | `RawListener` | 在 feature 内增加 `users`（`alterId` 逐用户）、`ws-path`、`allow-insecure`、`mkcp-config` 字段 |
| `crates/meow-config/src/lib.rs:171` | `ListenerSpec` | 新增 `Vless(VlessListenerConfig)`、`Vmess(VmessListenerConfig)` 变体 |
| `crates/meow-config/src/lib.rs:4267` | `parse_listener_spec` | 新增 `"vless"` / `"vmess"` 两个 arm；未知 type 仍按 Class A 硬错 |
| `crates/meow-app/Cargo.toml` | features/deps | `server-inbound = ["meow-server/tunnel-bridge", "meow-config/server-inbound"]` + optional path dep |
| `crates/meow-app/src/main.rs:695` | `listener_feature_gate` | 新 spec 的 feature gate |
| `crates/meow-app/src/main.rs:1355` | listener match | 新增分支：`Vless`/`Vmess` → `meow_server::spawn_inbound(tunnel.clone(), spec)` |

这些都是**新增分支**，不改动任何既有分支语义；upstream 换 tag 时按 ADR-0002 的清单重放。

### 5.3 显式不做的对接方式

- 不拆配置文件、不生成 sanitized 临时配置：那会让"配置一致"变成假的。
- 不把 server 入站塞进 `lib.rs` 的独立进程：单进程共享 `Tunnel` 是目标。
- 不复制 `route_inbound_tcp` 实现：会产生静默行为漂移。
- 不 merge `upstream/main`：只 cherry-pick 单个 commit。

---

## 6. 配置兼容

设备现行配置（只读核查，**不入库**）：`D:/workspace/server/armbian/bin/mihomo/config.yaml`，
mihomo v1.19.34-slim-inb，30,867,580 B，ELF64 AArch64。

顶层键：`mixed-port`、`allow-lan`、`bind-address`、`mode`、`log-level`、
`external-controller`、`dns`、`proxies`、`proxy-groups`、`rules`、`secret`、
`profile`、`sniffer`、`listeners`。

`listeners` 两项：

- `type: vless`：`listen`、`port`、`users[{uuid, flow}]`、`ws-path`、`allow-insecure`
- `type: vmess`：`listen`、`port`、`users[{uuid, alterId}]`、`mkcp-config{enable, mtu, tti,
  uplink-capacity, downlink-capacity, congestion, read-buffer, write-buffer}`

**为什么必须改 meow-config**：`parse_listener_spec` 今天只认 mixed/http/socks5/tproxy/shadowsocks，
遇 `vless` 硬错。这是上游的**能力边界**，正是本 fork 要扩展的地方——不是绕开它的理由。
扩展后直接把原文件交给同一个 `meow -f config.yaml`，客户端和入站同时启动。

骨架阶段仍只有 `meow-server` 冒烟 CLI；配置解析扩展属后续 card。

---

## 7. 测试策略

### 7.1 分层

| 层 | 命令 | 门槛 |
|---|---|---|
| 单元 | 在 `server/` 下 `cargo test --all-targets` | 全部通过；当前 27 个 |
| 协议 e2e | `tests/vless_ws.rs`：ephemeral 端口 + 真实 WS 握手 | 响应头、载荷回显、错误路径 |
| Go 对拍 | `zig-docs/tests/vectors/g0_batch*.txt` | 逐字节；基准 v1.19.34 |
| 交叉类型检查 | 两个 ARM 目标 `cargo check --all-targets` | 均通过 |
| fork delta 门禁 | 根 workspace fmt/clippy/rustdoc/测试 + server 门禁 | 全绿 |
| 体积 | 见体积预算 ADR | 待阈值签字 |

### 7.2 现有测试映射（27 个，全部通过）

| 组 | 数量 | 覆盖 |
|---|---|---|
| `vless::tests` | 13 | 头解码、addons、截断/atyp/空域名、鉴权、响应头 |
| `transport::tests` | 3 | WS 路径精确匹配与 404 |
| `config::tests` | 4 | 19001/19002 锚定、WS 路径、空配置 |
| `vmess::tests` | 1 | 占位错误显式 |
| `tests/vless_ws.rs` | 6 | e2e echo、错误路径、未知用户、UDP 拒绝、payload 不预读 |

### 7.3 待补测试

| ID | 用例 | 期望 |
|---|---|---|
| R-01 | 上游契约 | `route_inbound_tcp` 签名可调用；每次 upstream 换 tag 必跑 |
| C-01 | meow-config 解析 | 脱敏 fixture 的 `listeners[vless,vmess]` 解析成新 `ListenerSpec` 变体，字段逐项相等 |
| C-02 | 未知 listener type | 仍按 Class A 硬错并报索引 |
| C-03 | meow-app 单进程 | 同一进程内 7891/19001/19002 同时监听，共享同一 `Tunnel` |
| B-01 | Metadata 映射 | VLESS IPv4/domain → Metadata 字段逐项相等 |
| M-01..M-08 | mKCP 栈 | auth/分片/乱序/重传/crypt/smux/MTU/超时，按 `go-baseline/transport/mkcp/` |
| V-WS-TLS / V-VM | TLS 与 VMess 对拍 | `g0_batch36/37`、`g0_batch34/35` |
| DROP-01 | drop-in 冒烟 | 脱敏配置下 mihomo v1.19.34 与 meow-vgate 端口/客户端零改动连通 |

真实节点 smoke 是 opt-in，**不得进 CI**；密钥、UUID、订阅文件只放环境变量或临时文件。

---

## 8. 风险与待决

| 风险 | 缓解 |
|---|---|
| `route_inbound_tcp` 是 doc(hidden) 内部契约 | 钉版本 + R-01；不 merge upstream |
| fork delta 落在 meow-config/meow-app | 严格限制在 §5.2 清单，逐项登记，升级时重放 |
| VMess/mKCP 最大未知 | 先做 Go oracle 向量再实现 |
| 基准冲突（mainline vs v1.19.34） | 部署契约 + 机主签字，记录到 ADR |
| 单进程体积 | 设备组合二进制需实测，见体积 ADR |
| 设备内存/flash 极紧 | 禁用 UPX；禁止触碰大量内存页的探针；受控窗口操作 |

---

## 9. 跟随上游升级与光猫替换路线

### 9.1 升级原则

fork delta 被限制在 §5.2 的 7 个 hook 点，且全部是新增分支、不改既有语义。
上游新 tag 到来时：从新 tag 建分支 → 按清单重放 delta → 复制 `server/` → 跑门禁 → 签字切换。
仍不 merge `upstream/main`，只在需要时 cherry-pick 单个 commit。

### 9.2 升级步骤

1. 记录基线（`v0.29.0` / `de176935`）与 delta 清单状态。
2. 从新 upstream tag 建分支，不 merge 旧分支。
3. 重放 §5.2 的 7 个 hook（可按清单逐项核对 diff）。
4. 跑门禁：根 workspace fmt/clippy/rustdoc/测试 + server 门禁 + R-01 + C-01..03 + DROP-01。
5. 机主签字后切换部署基线；旧分支保留回滚。

### 9.3 光猫替换步骤（需机主授权）

凭证与设备镜像只在 `D:/workspace/server/f4610u` 与 `D:/workspace/server/armbian/`，不进本仓库。

1. 先在 `/tmp`（tmpfs）用单进程二进制 + 脱敏配置试跑，不碰 `/opt` 持久层。
2. 验证 7891 / 9090 / 19001 / 19002 与客户端零改动连通。
3. 受控窗口内持久化（2026-11-24 落地，机主选方案 B）：
   - 二进制落 `/opt/armbian/bin/mihomo/meow-vgate`（**新名字**，旧 `mihomo` 保留作回滚锚点）。
   - 仅改 `mihomo_start.sh` 的 `APP=` 一行指向新二进制；`user_init.sh` 只调脚本路径，无需改。
   - `Country.mmdb` 落 `/opt/armbian/bin/mihomo/geo/`，并把脚本的 geo 预置列表
     从 `GeoSite.dat geoip.metadb` 扩为三项——我们的二进制需要 Country.mmdb（现役 mihomo 不需要）。
   - CI artifact 名：`meow-vgate-v0.29.0-aarch64-linux-musl` /
     `meow-vgate-v0.29.0-armv7hf-linux-musl`。
4. 保留原二进制、原脚本备份与配置的回滚路径（备份 `mihomo_start.sh.bak-<ts>`；
   回滚 = 恢复备份脚本 + stop/start）；第一轮不改持久配置、不执行 `save`。
5. 设备 512 MB RAM、flash 余量紧，禁止 UPX 与触碰大量内存页的探针。

### 9.4 版本矩阵（待填）

| upstream tag | 基线提交 | delta 重放 | 门禁结果 | 签字 |
|---|---|---|---|---|
| v0.29.0 | de176935 | 待做 | 待跑 | 待签 |

## 10. 参考

- `server/zig-docs/README.md`
- `docs/adr/0001-meow-transport-crate.md`、`docs/adr/0002-upstream-divergence-policy.md`
- `crates/meow-config/src/lib.rs:171,4267`、`crates/meow-config/src/raw.rs:266`
- `crates/meow-app/src/main.rs:695,1163,1355`
- `crates/meow-tunnel/src/tcp.rs:378`、`crates/meow-tunnel/src/lib.rs:24`
- `CONTRIBUTING.md:15-22`
