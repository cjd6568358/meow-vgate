# ADR 0002: Single-process drop-in replacement and replayable fork delta

- **Status:** Proposed — 待机主正式归档签字（2026-11-24 方案 B 已真机部署并重启验证；核心决策均按本 ADR 执行中）
- **Date:** 2026-11-23（同日修订：废弃"配置拆分 + 双进程"方案；2026-11-26 复核后同步 delta 清单与分歧登记）
- **Author(s):** meow-vgate maintainers
- **Supersedes:** —
- **Related:** [ADR 0001](0001-server-footprint-budget.md)、[ARCHITECTURE.md](../ARCHITECTURE.md)、[ADR 0002 上游分歧策略](../../../docs/adr/0002-upstream-divergence-policy.md)

## Context

机主终局目标：在 ZTE F4610U 光猫上**单进程**客户端无感替换现役 mihomo，配置文件保持一致，
且 fork 能持续跟随 upstream meow-rs 新版本升级。

设备只读事实（凭证不入库）：

- 现役二进制 `/opt/armbian/bin/mihomo/mihomo`：mihomo v1.19.34-slim-inb，30,867,580 B，
  ELF64 AArch64（machine `0xb7`）。主目标 `aarch64-unknown-linux-musl`。
- 端口：7891 mixed、9090 API、19001 vless+ws、19002 vmess+mkcp。
- `config.yaml` 顶层 14 个键，含 `listeners` 两项：vless（`users[{uuid, flow}]`、
  `ws-path`、`allow-insecure`）与 vmess（`users[{uuid, alterId}]`、
  `mkcp-config{...}`）。

**上游能力边界**：`meow-config` 已解析 `listeners:`（`crates/meow-config/src/raw.rs:266`），
但 `parse_listener_spec`（`crates/meow-config/src/lib.rs:4267`）只接受
`mixed/http/socks5/tproxy/shadowsocks`；`ListenerSpec` 枚举（`lib.rs:171`）也没有 vless/vmess
变体。遇到 `type: vless` 会按 ADR-0002 Class A **硬错**——即拒绝未知类型而非静默误路由。
这是上游的安全语义，也正说明这里是需要 fork 扩展的能力边界。

**meow-app 的单进程接入点已存在**：`crates/meow-app/src/main.rs:1163` 创建
`Tunnel::new_with_slot(...)`；`:1350` 起的循环按 `ListenerSpec` 分支 spawn listener，
各分支用 `tunnel.clone()`。新增两个分支即可让 server 入站与客户端共享同一个 `Tunnel`。

**升级约束**：fork delta 若散落在多个上游文件、改动既有语义，会让新 tag 的换基线变成冲突解决。
因此 delta 必须最小、隔离、可重放。

## Decision

### 1. 单进程，不拆配置

同一个 `meow` 二进制直接读取设备原 `config.yaml`（一个字节不改），同时启动客户端链路与
19001/19002 入站；client 与 server 共享同一个 `Arc<Tunnel>`。端口和启动入口保持不变。

**废弃方案**（本 ADR 同日早稿）：配置拆分器写临时 sanitized 文件 + 双进程 wrapper。
它让"配置一致"变成假象，且双进程带来额外内存/端口管理成本，与单进程目标冲突。

### 2. 扩展 meow-config，不改它的安全语义

- `ListenerSpec` 新增 `Vless(...)` / `Vmess(...)` 变体（feature `server-inbound` 内）。
- `RawListener` 增加 `users`（`alterId` 逐用户）、`ws-path`、`allow-insecure`、`mkcp-config`。
- `parse_listener_spec` 新增 `"vless"` / `"vmess"` 两个 arm。
- **未知 type 仍 Class A 硬错**，上报 listener 索引与 type，不静默忽略。

### 3. meow-app 以 optional feature 接入

- `meow-app` 增加 optional path dep：`meow-server = { path = "../../server", optional = true }`。
- feature `server-inbound = ["meow-config/server-inbound", "dep:meow-server"]`。
- **实现注（2026-11-24）**：早期草案写的是 `meow-server/tunnel-bridge`，但落地时
  `server/` 是纯协议库（VLESS/VMess 解析、mKCP、WS），没有也不需要 `tunnel-bridge`
  feature；`meow-app/src/server_inbound.rs` 的 `RouteSink` 直接用 `meow_server::vless::*`
  / `meow_server::mkcp::*` 协议类型，自己调 `meow_tunnel::route_inbound_tcp` 共享
  `Arc<Tunnel>`。功能等价且 delta 更小（fair +155 KB），但文档以本行为准。
- `listener_feature_gate` 与 listener match 各新增分支；spawn 时传 `tunnel.clone()`。
- 默认 feature 不含 `server-inbound`，因此 8 MiB client cap 与默认发布物不受影响。

### 4. fork delta 清单（换 tag 时需重放的全部 upstream 文件改动）

2026-11-26 复核对齐（详表见 ARCHITECTURE.md §5.2，两处同步维护）：

**upstream 既有文件增量**：

| 文件 | 改动 | 性质 |
|---|---|---|
| 根 `Cargo.toml` | `exclude = ["server"]` | 新增 |
| `crates/meow-config/Cargo.toml` | `server-inbound` feature | 新增 |
| `crates/meow-config/src/raw.rs` | `RawListener` 4 字段 + `RawInboundUser`/`MkcpCongestion`/`RawMkcpConfig`（含 seed 告警字段） | 新增、feature 内 |
| `crates/meow-config/src/lib.rs` | `ListenerSpec::{Vless,Vmess}` 变体及配置结构、`parse_listener_spec` 两 arm（含 mkcp tti/mtu 硬错、seed 告警）、`build_inbound_users`/`build_mkcp_config`、`SUPPORTED_LISTENER_TYPES` | 新增分支 |
| `crates/meow-config/tests/server_inbound_listener_test.rs` | C-01/C-02 + 范围校验/seed 用例 | 新增测试文件 |
| `crates/meow-app/Cargo.toml` | feature + optional path dep | 新增 |
| `crates/meow-app/src/main.rs` | 两 `mod` 声明、`listener_feature_gate` 分支、listener match 两 arm | 新增分支 |

**fork-only 新文件**（重放 = 整体复制）：

| 文件 | 性质 |
|---|---|
| `crates/meow-app/src/server_inbound.rs`（RouteSink/serve_vless） | 新文件 |
| `crates/meow-app/src/vmess_inbound.rs`（VMess AEAD 入站 + mKCP 接入） | 新文件 |
| `crates/meow-proxy/src/vmess/header.rs`：三个 test-only 助手提升 pub（`server_open_request_header`/`request_header_frame_len`/`seal_response_header`） | 可见性提升，签名不变 |
| `crates/meow-proxy/src/vmess/body.rs`：新增 `new_server_full`/`new_server_full_masked` | 新增方法 |

不改任何既有分支语义；`meow-transport` 与 `route_inbound_tcp` 的实现完全不动。

### 5. 升级流程

从新 upstream tag 建分支（不 merge）→ 按 §4 清单重放 → 复制 fork 自有树
（`server/`、workflow、e2e、脚本、`server/docs/`）→ 跑门禁
（根 workspace fmt/clippy/rustdoc/测试 + server 门禁 + 三条 e2e + 8/14 MiB 双 cap）
→ 机主签字。版本矩阵维护在 ARCHITECTURE.md §9.4。

### 6. 设备纪律

凭证只在 `D:/workspace/server/f4610u`，不进仓库。设备操作需机主授权：先 `/tmp` 试跑、
受控窗口切换、保留回滚；第一轮不改持久配置、不 `save`；禁止 UPX 与触碰大量内存页的探针。

## Consequences

**Positive**

- 真正 drop-in：单进程、单配置、单入口，客户端与运维脚本零改动。
- client 默认构建不受影响；服务端只在 `server-inbound` feature 下编译。
- delta 全是新增分支，升级可逐项核对与重放，冲突面收敛到 7 个 hook。

**Negative / risks**

- meow-config / meow-app 出现 fork delta，upstream 同位置改动需人工重放（由门禁捕获）。
- 单进程二进制体积大于 client minimal；需实测设备组合体积（ADR 0001）。
- `route_inbound_tcp` 是 doc(hidden) 契约，升级时可能变化（R-01 捕获）。

**Neutral**

- `meow-server` 在默认纯 Rust 检查下不依赖 meow crate；`tunnel-bridge` feature 下会引入
  boring-sys，与 client 已有依赖合并，不新增第二套 TLS 栈。

## Alternatives considered

| 方案 | 结论 | 理由 |
|---|---|---|
| 配置拆成客户端段 + server 段、双进程 wrapper | **拒绝（同日废弃）** | 配置一致性是假的；双进程与单进程目标冲突 |
| 不改 meow-config，启动前改写原文件 | 拒绝 | 违反配置文件保持一致，且破坏回滚 |
| 把 main.rs wiring 抽成库 API 再内嵌 | 暂拒绝 | 改动面更大、升级冲突更多；当前 hook 分支已足够 |
| 只部署 server，不替换 client | 拒绝 | 不满足终局目标 |

## Migration

1. ✅ 按 §4 清单在 fork 内实现 meow-config / meow-app 的 delta（2026-11-24）。
2. ✅ 上游对接以 meow-app 的 `server_inbound.rs` + `vmess_inbound.rs` 落地
   （无 `tunnel-bridge` feature；见 §3 实现注，2026-11-24）。
3. ✅ C-01..03、B-01、DROP-01 补齐（e2e 进 CI；真机部署验证，2026-11-24）。
4. ✅ 光猫 `/tmp` 单进程试跑 → 方案 B 受控窗口持久化（2026-11-24，重启自启验证通过）。
5. ⚠ 待办：upstream 新 tag 的首轮重放演练（清单已按实仓对齐，2026-11-26）。

## 已知 Class B 分歧（Go v1.19.34 基准）

2026-11-26 系统复核后集中登记（以前零散在各处，此节为正本）：

| # | 分歧 | 状态 |
|---|---|---|
| 1 | `mkcp-config.congestion` 只解析不接入状态机（无 BBR 开关） | 既知；不影响连通性 |
| 2 | VMess UDP（XUDP/packet-addr）与 VLESS UDP/Mux 命令**显式拒绝**而非 relay | VLESS 侧历来如此；VMess 侧 2026-11-26 起从"静默误路由成 TCP 拨号"改为硬错（P1 修复）。上游 mihomo 支持 UDP，属功能缺口非缺陷 |
| 3 | TCP accept / UDP recv_from 瞬时错误退避重试（连续 100 次≈10s 才退役） | Go 侧首错即停；长期网关场景取韧性。2026-11-26 起 |
| 4 | mKCP `seed` 不支持（仅 keyless simple 认证）；AES-128-GCM auth 代码在库内但不可达 | 2026-11-26 起解析期显式告警（不再静默丢弃该键） |
| 5 | ~~发侧每段独立 datagram，未按 MTU 聚合多段~~ | **已对齐 Go flush 行为（2026-11-26）**：发侧整段打包至 MTU 后再密封发送；段永不跨 datagram 拆分。锁定于 `mkcp::listener` 的 pack 单测 |
| 6 | 会话表无并发上限；新建会话仅需一个合法 auth 包 | 19002 公网暴露已见扫描；**背压已闭环（2026-11-26）**：accept 队列满 64 时拒绝并走 close 阶梯通知客户端、显式告警（已有 e2e 单测锁定）；会话总数上限/限流本身仍待机主拍板 |

## 运行期观察（2026-11-25 真机）

部署后（v0.29.0 单进程，重启开机自启验证通过）的两项澄清：

### REST API 与内置 UI
- `external-controller: 192.168.1.1:9090` **同时提供 REST API 和内置管理面板**。
  ~~9090 只有 API、无 UI~~ 是误判（测试时 shell 把 `/ui` 当本地路径展开）。
- 面板正确路径 **`http://192.168.1.1:9090/ui`（无尾斜杠）**，28KB 单文件 HTML
  （title `meow-rs`）；`/ui/` 404、`/ui/index.html` 200。
- 鉴权：页面有 API Secret 输入框 → localStorage；REST 带 `Authorization: Bearer`，
  WS 用 `?token=`，另支持 URL hash `#token=` 自动注入。
- 面板依赖的 8 个端点全部实测可用：`/api/config/save`(POST)、`/api/proxy-groups`、
  `/api/subscriptions`、`/configs`、`/proxies`、`/rules`、`/connections`、
  `/rules/reorder`(POST+JSON)。可查看/排序规则、切换节点、保存配置。
- **注意**：`/api/config/save` 会真写配置，勿误点。在线 yacd 不再是必需。

### 日志统一与调试
- 启动脚本 `mihomo_start.sh` 已导出 `RUST_LOG`（info + `meow`/`meow_server` debug，
  health_check 降 error 降噪），所有输出经 `nohup >> /tmp/mihomo.log` 统一落盘；
  `/tmp` 为 tmpfs，重启自动清空。二进制无需重编即支持 debug。

### 7891 暴露面（风险记录，未改设备）
- 设备 7891：`mixed-port 7891` + `allow-lan: true` + `bind-address 192.168.1.1`
  + `authentication: null`（**无认证**）；`iptables -S INPUT` 无任何 7891 规则，
  INPUT 默认 ACCEPT。
- 现有 `port_guard.sh`（护 7890 easytier）**未覆盖 7891**。
- 判断：7891 是代理口而非登录口，无密码可猜 ⇒ "爆破"意义有限，真正风险是
  **免密被当开放代理滥用**。可选加固（机主未决定，均未执行）：
  ① 挂 `port_guard.sh -p 7891 -c -t 80 -w 600` 挡扫描器短连；
  ② config `authentication` 加用户名密码（会影响现有客户端，需同步配置）。
- 设备公网侧已见扫描器打 19001：2026-11-25 记录签名 `invalid VLESS address type: 0`；
  2026-11-27 全量审查见 20 条 VLESS/WS 握手失败（来源 `204.76.203.x` / `85.217.149.x` /
  `212.73.148.x` / `165.232.167.247`；错误为 `invalid token` / `Handshake not finished` /
  `Unsupported HTTP method`），服务均正确拒绝。端口暴露面真实存在，非理论风险。

### 运行期日志量化审查（2026-11-27，17 小时窗口）

对 `/tmp/mihomo.log`（`2026-11-26T15:43:05Z` → `2026-11-27T08:43:12Z`，6680 行）全量审查：

- **0 ERROR / 0 FATAL / 0 panic**；DEBUG 5442 / INFO 999 / WARN 239。无崩溃、无重启、无 fd/OOM 告警。
- 239 条 WARN 全部可解释：134 `meow_tunnel::tcp` dial 5s 超时（直连节点链路抖动，集中
  `11-26T16–17Z`、`11-27T00–01Z` 与 `08:00–08:11Z`；最近 30 分钟 0 WARN）；72 TLS
  `skip-cert-verify=true` 提示；20 VLESS/WS 握手失败（公网扫描器，见上）；12 XTLS Vision
  UDP Class B 分歧（§表 #2）；1 QUIC sniff 未实现。
- DEBUG 级 relay error 74 条：63 `Connection reset by peer`、7 `Broken pipe`、
  3 `hysteria2 driver closed`、1 `vless: server closed before response header`——均为对端断开，
  非本地故障。
- 启动日志锚点：`INFO meow: VLESS/WS listener 'vless-19001' started on 0.0.0.0:19001` +
  `VMess/mKCP listener 'vmess-19002' started on UDP 0.0.0.0:19002`，确认单进程双入站在现网运行。
- 日志带 ANSI 转义（`\x1b[2m` 等），统计前须 `sed 's/\x1b\[[0-9;]*m//g'` 清理，否则字段解析错位。

## Open questions

- ~~设备单进程组合二进制的总体积上限~~ — 机主 2026-11-24 签字定为 14 MiB，
  两个静态 stripped 产物共用，已接入阻断 CI（见 ADR 0001 §4）。
- ~~`mkcp-config` 字段是否全部逐项生效~~ — 2026-11-24 真机验证：现役取值集合
  （mtu/tti/capacity/buffer/congestion）全部能解析并加载；其中发现并修复了
  **buffer 单位 bug**：mihomo 的 `read-buffer`/`write-buffer` 是 MiB，我们曾当字节
  传入 KcpConfig，导致 `sending_buffer_size = 4/1250 = 0`、mKCP 响应发不出去
  （真机 curl 超时，CI 因测试配置 buffer 组合不同而未暴露）。已在 meow-app 传入时
  `saturating_mul(1024*1024)`，真机用原 config（buffer=4 不改）验证 VMess/mKCP HTTP 200。
  另：2026-11-26 起 `seed` 显式告警（见 Class B 分歧 #4），`congestion` 仍不接入
  （分歧 #1）。
- 升级节奏：跟每个 upstream tag，还是选定版本。

## References

- `crates/meow-config/src/raw.rs:266`、`crates/meow-config/src/lib.rs:171,4267`
- `crates/meow-app/src/main.rs:695,1163,1350`
- `crates/meow-tunnel/src/tcp.rs:378`、`crates/meow-tunnel/src/lib.rs:24`
- `server/docs/ARCHITECTURE.md` §5、§6、§9
- `D:/workspace/server/armbian/bin/mihomo/`、`D:/workspace/server/f4610u/`（**不入库**）

> AI生成
