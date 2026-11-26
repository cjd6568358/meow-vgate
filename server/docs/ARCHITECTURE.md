# meow-server 架构设计

- **状态**：Implemented（分层与边界已全部落地；14 MiB 体积上限已签字，见体积预算 ADR；2026-11-26 复核后同步）
- **日期**：2026-11-23（2026-11-26 修订：mKCP/VMess 已实现、单进程已真机部署、§5.2 delta 清单对齐实现）
- **终局目标**：在 ZTE F4610U 光猫上**单进程**客户端无感替换现役 mihomo——同一份 `config.yaml`、同一监听端口、同一启动入口；fork 同时保留跟随 upstream meow-rs 新版本升级的能力
- **适用范围**：`server/` 下的服务端入站实现，以及为接入它而必须在根 workspace 中新增的最小 fork delta
- **上游基线**：meow-rs tag `v0.29.0` / `de176935`（分支 `server-inbound` 的锚点）
- **行为基准**：`server/zig-docs/go-baseline/` = mihomo v1.19.34 / `0455d0d2` 逐字节
- **目标架构**：`aarch64-unknown-linux-musl`（设备现役二进制实测 ELF64 AArch64，machine `0xb7`）；`armv7-unknown-linux-musleabihf` 为次级目标
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
- 不在本阶段实现 TLS/REALITY、gRPC、H2、HTTPUpgrade、XTLS vision、**UDP/Mux relay**
  （UDP 命令显式拒绝而非静默误路由，见 §4 与 ADR-0002 分歧登记）、用户热更新、多租户计费。
- 不 merge `upstream/main`；只在需要时逐个 cherry-pick。
- 不改变根 workspace 的 `members`；**client 默认构建不含服务端代码**，8 MiB cap 不受影响。

---

## 2. 工作区与依赖边界

```
根 workspace（client crates，含 meow-config / meow-app / meow-tunnel）
    │  meow-app 的 optional feature "server-inbound"
    │  └─ optional path dep：meow-server（纯协议库，**无 feature**）
    └──────────────► server/（meow-server，独立 workspace）
                        纯 Rust：VLESS/WS、mKCP 栈，不依赖任何 meow-* crate
```

- 根 `Cargo.toml` 以 `exclude = ["server"]` 阻止自动收编；`server/Cargo.toml`
  **不声明** `[workspace]`（成员不能 path-dep 另一 workspace 根，否则单进程
  构建会断），靠 exclude 实现隔离。
- `meow-app` 通过 optional path dep 引入 `meow-server`，由 feature
  `server-inbound` 控制；默认关闭，普通 client build 与 8 MiB minimal cap 不变。
- 与上游的对接（共享 `Arc<Tunnel>`、`route_inbound_tcp` 桥接）**不在**
  `meow-server` 内实现，而是收敛在 meow-app 的两个 fork-only 文件：
  `server_inbound.rs`（VLESS/RouteSink）与 `vmess_inbound.rs`（VMess 头/正文）。
  协议库因此保持 tunnel-free、纯 Rust，两个 ARM 目标的 `cargo check`
  不需要 C 交叉工具链。

### 硬规则

| 编号 | 规则 | 理由 |
|---|---|---|
| R1 | 根 `members` 不得加入 server（仅允许 exclude 行）；client 默认 feature 不得开启 `server-inbound` | 保护 8 MiB client cap |
| R2 | `meow-transport` 不得新增 `accept()`，也不得依赖本 crate | ADR-0001 的 client 契约冻结 |
| R3 | 服务端协议栈不依赖任何 `meow-*` crate；上游对接代码只出现在 meow-app 的 fork-only 文件 | 保持协议层可独立测试 |
| R4 | fork delta 只允许落在 §5.2 登记的位置，且必须逐项登记在 ADR-0002 | 升级时可重放、可核对 |
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
                │  19001 TCP → WsAccept（WS 0-RTT early data 支持）
                │  19002 UDP → meow-server::mkcp listener + KcpStream（duplex 桥）
                ▼
        协议握手层：vless::read_request / vmess_inbound 的 AEAD 头解析
                │  VlessRequest{user_id, command, port, destination}
                │  VmessRequest{req_key, req_iv, security, ...}
                ▼
        RouteSink / vmess_inbound handler（meow-app feature 内）
          → 同一个 Arc<Tunnel> → route_inbound_tcp → 规则匹配 → outbound → relay
```

关键点：**client 与 server 共用同一个 `Tunnel` 实例**，因此规则、DNS、连接统计、API
都是同一套，不需要二次路由、不需要进程间通信。meow-app 的 `main.rs` listener
循环里 `Vless`/`Vmess` 两个分支调 `server_inbound::serve_vless` /
`vmess_inbound::serve_vmess_mkcp`，各自传 `tunnel.clone()`。

### 3.1 传输层

`ServerStream = AsyncRead + AsyncWrite + Unpin + Send + Sync`（`server/src/stream.rs`）；
`ServerTransport::accept()` 在 `server/src/transport.rs`。它与 `meow_transport::Stream`
结构等价但类型不兼容（后者多 `Any + as_any_mut()`），这是刻意的：不为对齐签名而改
client trait。

`WsByteStream` 是必要适配层：tokio-tungstenite 0.29 只暴露 `Stream<Message>/Sink<Message>`，
没有字节 IO。它把 Binary 帧映射成字节流，回复 Ping，Close/EOF 映射为读 EOF，Text 帧按
Class A 硬错。另支持 Xray/v2ray/mihomo 客户端的 **WS 0-RTT early data**：
`Sec-WebSocket-Protocol` 里的 base64url（无 padding）解码后注入读缓冲，先于任何 WS 帧
到达 VLESS 读取方，握手响应不回 echo（对齐 go-baseline/transport/vmess/websocket.go）。

mKCP 传输在 `server/src/mkcp/`：auth（simple/AES-128-GCM）、big-endian u16-conv 分段、
发送/接收窗口、RFC6298 RTO、UDP listener（会话按 (peer, conv) 分流，`KcpStream` 经
tokio duplex 与 pump 桥接；handler drop 流时立即走 close 阶梯发 TERMINATE，
pump 感知 Terminated 后让对端读 EOF）。

### 3.2 协议层

VLESS 请求头：`version(1) | uuid(16) | addons_len(1) | addons(N) | command(1) | port(2,BE) | atyp(1) | addr`。
注意 **PortThenAddress** 与 **command 从 1 开始**（TCP=1/UDP=2/Mux=3）。响应头 `[0,0]` 只在首次写前置
一次；UDP/Mux 命令硬错拒绨（Class A）。

VMess 请求头（AEAD）：auth_id + 密封长度 + conn_nonce + 密封 payload，实现位于
`crates/meow-app/src/vmess_inbound.rs`，复用 `meow-proxy::vmess::header/body` 的
golden-vector 鎻锁原语（`server_open_request_header` / `request_header_frame_len` /
`seal_response_header` / `new_server_full[_masked]`）。正文按 chunk-stream 记录分帧
（含 Chunk Masking 0x04），双向独立泵避免“请求未完等响应”死锁。

参考快照（只读、不入库）：`server/zig-docs/source/listener/vmess/` 与
`server/zig-docs/go-baseline/transport/mkcp/`。

---

## 4. 能力边界矩阵

| 能力 | 当前状态 | 边界说明 |
|---|---|---|
| 19001 TCP listen + WS Upgrade | **已实现（含真机部署）** | 路径精确匹配；接进 meow-app 单进程 |
| VLESS 头解析 / UUID 鉴权 | **已实现** | TCP 命令；UDP/Mux 显式拒绝（Class A，响应头前拒） |
| WS 0-RTT early data | **已实现** | `Sec-WebSocket-Protocol` base64url，不回 echo |
| 19002 mKCP 传输 | **已实现（含真机部署）** | `mkcp/` 全栈：auth/分段/窗口/RTO/会话生命周期；simple 认证（seed 不支持，解析期告警） |
| VMess AEAD 头 / 正文 relay | **已实现** | meow-app `vmess_inbound.rs`，复用 meow-proxy golden 原语；TCP 命令，UDP 显式拒绝（Class B 分歧） |
| 响应头与载荷透传 | **已实现** | 到共享 `Arc<Tunnel>`（`route_inbound_tcp`） |
| TLS / WSS | 未实现 | 部署当前用明文 WS；后置 |
| gRPC / H2 / HTTPUpgrade / vision | 不计划 | 无部署契约 |
| 配置 | **已实现** | meow-config 完整解析 vless/vmess listener（含 mkcp 范围校验、seed 告警） |
| 单进程接入 | **已实现（真机方案 B 部署）** | 双 e2e（vless_ws/vmess_mkcp）+ 单进程双入站 e2e 均进 CI |
| client 默认构建 | 不受影响 | feature 关闭时无 server 代码；8 MiB cap CI 看护 |
| accept/recv 错误韧性 | **已实现** | 瞬时错误退避重试（连续 100 次→退役），Class B 分歧见 ADR-0002 |

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

### 5.2 fork delta 清单（换 tag 时需重放的全部 upstream 文件改动）

2026-11-26 复核对齐：不再是早期的 7 处 hook 设想，以下为实仓现状。

**（a）upstream 既有文件的增量**（git diff 可直接对拍）：

| # | 文件 | 改动 |
|---|---|---|
| 1 | 根 `Cargo.toml` | `[workspace] exclude = ["server"]`（阻止自动收编，FORK DELTA 注释） |
| 2 | `crates/meow-config/Cargo.toml` | 新增 `server-inbound` feature（空实现，纯开关） |
| 3 | `crates/meow-config/src/raw.rs` | `RawListener` 新增 `users`/`ws-path`/`allow-insecure`/`mkcp-config`（feature 内）+ 新类型 `RawInboundUser`/`MkcpCongestion`/`RawMkcpConfig`（含 `seed` 告警字段） |
| 4 | `crates/meow-config/src/lib.rs` | `ListenerSpec::{Vless,Vmess}` 变体、`InboundUserConfig`/`MkcpInboundConfig`/`VlessListenerConfig`/`VmessListenerConfig`、`parse_listener_spec` 两 arm（含 mkcp tti/mtu 范围硬错与 seed 告警）、`build_inbound_users`/`build_mkcp_config`、`SUPPORTED_LISTENER_TYPES` |
| 5 | `crates/meow-config/tests/server_inbound_listener_test.rs` | 新测试文件（C-01/C-02 + 范围校验/seed 用例） |
| 6 | `crates/meow-app/Cargo.toml` | `server-inbound = ["meow-config/server-inbound", "dep:meow-server"]` + optional path dep |
| 7 | `crates/meow-app/src/main.rs` | `mod server_inbound; mod vmess_inbound;` 声明、`listener_feature_gate` 分支、listener match `Vless`/`Vmess` 两 arm |

**（b）fork-only 新文件**（upstream 不存在，重放 = 整体复制，无合并冲突面）：

| # | 文件 | 内容 |
|---|---|---|
| 8 | `crates/meow-app/src/server_inbound.rs` | `RouteSink`/`build_metadata`/`serve_vless`：VLESS 会话 → 共享 `Arc<Tunnel>` |
| 9 | `crates/meow-app/src/vmess_inbound.rs` | VMess AEAD 头/响应头/正文 relay + `serve_vmess_mkcp` 接 mKCP |
| 10 | `meow-proxy` 的两处可见性/新增（FORK DELTA 内标注） | `vmess/header.rs`：`server_open_request_header`/`request_header_frame_len`/`seal_response_header` 由 test-only 提升 pub；`vmess/body.rs`：`new_server_full`/`new_server_full_masked` 新增 |

**（c）fork 自有树**（不入 upstream，全部整体复制）：`server/`，`.github/workflows/server-inbound.yml`，
`tests/e2e/{vless_ws,vmess_mkcp,single_process}_e2e.sh + *.yaml`，
`scripts/build-armv7-musl.sh` 的 `MEOW_EXTRA_CARGO_ARGS` 参数，`server/docs/`，根 `.gitignore`
的 `/server/zig-docs/`（参考快照本地化，不入库），根 `Cargo.lock`（optional dep 引入的
解析产物，重放时重新生成）。

（a）与（b）均为**新增分支/新文件**，不改动任何既有分支语义（除 #10 的可见性提升与
新方法，签名不变）；upstream 换 tag 时按本表逐项重放并跑 §7 门禁。

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

**为什么必须改 meow-config**：upstream 的 `parse_listener_spec` 只认
mixed/http/socks5/tproxy/shadowsocks，遇 `vless` 硬错。这是上游的**能力边界**，
正是本 fork 扩展的地方（已落地：C-01/C-02 测试锁定）。扩展后直接把原文件交给
同一个 `meow -f config.yaml`，客户端和入站同时启动——真机已验证零改动加载。

---

## 7. 测试策略

### 7.1 分层

| 层 | 命令 / 载体 | 门槛 |
|---|---|---|
| server 单元 | `server/` 下 `cargo test --all-targets` | 全部通过；2026-11-26 起 58 个 |
| server 协议 e2e | `server/tests/vless_ws.rs`：ephemeral 端口 + 真实 WS 握手 | 6 个：响应头、回显、错误路径 |
| VMess/mKCP 入站测试 | `meow-app --features server-inbound`（`vmess_inbound::tests`） | 头/正文分帧、UDP 拒绨、drop→TERMINATE 等逐字节语义 |
| meow-config 测试 | `server_inbound_listener_test.rs` | C-01/C-02 + mkcp 范围校验/seed 告警 |
| 真实客户端 e2e（CI） | `tests/e2e/*.sh`：真 mihomo v1.19.34 客户端对拍 | vless_ws、vmess_mkcp、单进程双入站三条链路 HTTP 非 000 |
| 交叉类型检查 | 两个 ARM 目标 `cargo check` | 均通过 |
| fork delta 门禁 | 根 workspace fmt/clippy/rustdoc/测试 + server 门禁（`server-inbound.yml`，含 fmt/clippy/rustdoc/test） | 全绿 |
| 体积 | 8 MiB client cap + 14 MiB 单进程双目标 cap | 均阻断（已签字） |

### 7.2 现有测试映射（2026-11-26 实点清：server 58 单元 + 6 e2e；meow-app vmess 7；meow-config 8）

| 组 | 数量 | 覆盖 |
|---|---|---|
| `vless::tests` | 13 | 头解码、addons、截断/atyp/空域名、鉴权、响应头 |
| `transport::tests` | 5 | WS 路径精确匹配、404，0-RTT early data 解码/拒绝 |
| `config::tests` | 4 | 19001/19002 锚定、WS 路径、空配置 |
| `vmess::tests` | 1 | smoke bin 的显式 not-implemented |
| `mkcp::{auth,segment,window,receiving,rto,conn,listener}` | 34 | auth 轮回/拒销、分段解析、窗口/快重传/丢包、缓存重排、RFC6298 golden、双向 loopback（epoch 时钟）、会话生命周期 |
| `tests/vless_ws.rs` | 6 | e2e echo、错误路径、未知用户、UDP 拒绨、payload 不预读 |
| `vmess_inbound::tests`（meow-app） | 7 | 明文头解析、客户端密封头开放、帧长=精确头长、正文分频偏移、**UDP 命令拒绨** |
| `server_inbound_listener_test.rs`（meow-config） | 8 | C-01 全字段、C-01 默认值、C-02 未知 type/空 users、真机形快回归、tti/mtu 范围硬错、seed 告警 |

### 7.3 测试状态（原待补清单收敛）

| ID | 用例 | 状态 |
|---|---|---|
| R-01 | `route_inbound_tcp` 契约 | **隐式守护**：server_inbound.rs 直接调用它，每次 CI 编译即验；换 tag 后由 C-03 e2e 双保险 |
| C-01 / C-02 | meow-config 解析 / 未知 type 硬错 | **已落地**（含 tti/mtu/seed 扩展） |
| C-03 | 单进程 7891/19001/19002 同进程 | **已落地**（`single_process_e2e.sh` 进 CI，真机已部署） |
| B-01 | Metadata 映射 | **已落地**（`server_inbound.rs` 单测） |
| M-01..M-08 | mKCP 栈 | **大部分已落地**：auth/分段/乱序/重传/RTO/MTU 均有单测 + 真客户端 e2e；smux 未做（无部署契约） |
| V-WS-TLS / V-VM | TLS 与 VMess 向量对拍 | 向量机（`zig-docs`）不入库不可进 CI；以真 mihomo 客户端 e2e 代理，TLS 仍未实现 |
| DROP-01 | drop-in 冒烟 | **已落地**（真机方案 B 部署验证，重启自启通过） |

真实节点 smoke 是 opt-in，**不得进 CI**；密钥、UUID、订阅文件只放环境变量或临时文件。

---

## 8. 风险与已知分歧

| 风险 / 事项 | 处置 |
|---|---|
| `route_inbound_tcp` 是 doc(hidden) 内部契约 | 钉版本 + 每次 CI 编译隐式验证（R-01）；不 merge upstream |
| fork delta 落在 meow-config/meow-app/meow-proxy | 严格限制在 §5.2 清单，逐项登记，升级时重放 |
| 已知 Class B 分歧（UDP relay 拒绨、accept/recv 退避、congestion 不接入、seed 不支持等） | 集中登记于 ADR-0002「已知 Class B 分歧」节 |
| 基准冲突（mainline vs v1.19.34） | 部署契约 + 机主签字，记录到 ADR |
| 单进程体积 | 14 MiB 确定限制已接入阻断 CI（实测余量 ~1.5 MiB） |
| 会话资源驻留（对端静默不关且持续发包） | close 阶梯（drop→TERMINATE）已修；无上限会话表的风险见 ADR-0002（并发上限待拍板） |
| 设备内存/flash 极紧 | 禁用 UPX；禁止触碰大量内存页的探针；受控窗口操作 |
| zig-docs 向量快照不入库 | 不可作为 CI 门禁；行为基准以真机与真 mihomo 客户端 e2e 代行 |
| 7891 无认证暴露面（运行期风险，未改设备） | 见 ADR-0002 运行期观察；加固选项待机主拍板 |

---

## 9. 跟随上游升级与光猫替换路线

### 9.1 升级原则

fork delta 被限制在 §5.2 的清单（upstream 文件增量 + fork-only 新文件），且均为
新增分支/新文件、不改既有语义。上游新 tag 到来时：从新 tag 建分支 → 按清单重放
delta → 复制 fork 自有树 → 跑门禁 → 签字切换。仍不 merge `upstream/main`，
只在需要时 cherry-pick 单个 commit。

### 9.2 升级步骤

1. 记录基线（`v0.29.0` / `de176935`）与 delta 清单状态。
2. 从新 upstream tag 建分支，不 merge 旧分支。
3. 重放 §5.2（a/b）的 upstream 文件增量（可按清单逐项核对 diff），整体复制（c）。
4. 跑门禁：根 workspace fmt/clippy/rustdoc/测试 + server 门禁 + 三条 e2e + 体积双 cap。
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

### 9.4 版本矩阵

| upstream tag | 基线提交 | delta 重放 | 门禁结果 | 部署/签字 |
|---|---|---|---|---|
| v0.29.0 | de176935 | 首轮已实施（2026-11-24，清单见 §5.2，2026-11-26 复核对齐） | `server-inbound` workflow 全绿（server 门禁、双 ARM check、单元+e2e、8/14 MiB 双 cap 阻断） | 真机方案 B 已部署并重启验证（2026-11-24）；14 MiB cap 已签字；ADR-0002 正式归档签字待补 |

## 10. 参考

- `server/zig-docs/README.md`（本地参考快照，不入库）
- `docs/adr/0001-meow-transport-crate.md`、`docs/adr/0002-upstream-divergence-policy.md`
- `crates/meow-config/src/lib.rs`（`ListenerSpec`、`parse_listener_spec`）、`crates/meow-config/src/raw.rs`（`RawListener` fork delta 块）
- `crates/meow-app/src/main.rs`（`listener_feature_gate`、listener spawn 循环）、`crates/meow-app/src/server_inbound.rs`、`crates/meow-app/src/vmess_inbound.rs`
- `crates/meow-tunnel/src/tcp.rs:378`、`crates/meow-tunnel/src/lib.rs:24`
- `CONTRIBUTING.md:15-22`

> AI生成
