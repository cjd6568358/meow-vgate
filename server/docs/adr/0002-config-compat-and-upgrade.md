# ADR 0002: Single-process drop-in replacement and replayable fork delta

- **Status:** Proposed — 待机主签字
- **Date:** 2026-11-23（同日修订：废弃"配置拆分 + 双进程"方案）
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
- feature `server-inbound = ["meow-server/tunnel-bridge", "meow-config/server-inbound"]`。
- `listener_feature_gate` 与 listener match 各新增分支；spawn 时传 `tunnel.clone()`。
- 默认 feature 不含 `server-inbound`，因此 8 MiB client cap 与默认发布物不受影响。

### 4. fork delta 清单（唯一允许修改的上游位置）

| 文件 | hook | 性质 |
|---|---|---|
| `crates/meow-config/Cargo.toml` | `server-inbound` feature | 新增 |
| `crates/meow-config/src/raw.rs` | `RawListener` 字段 | 新增、feature 内 |
| `crates/meow-config/src/lib.rs:171` | `ListenerSpec` 变体 | 新增 |
| `crates/meow-config/src/lib.rs:4267` | `parse_listener_spec` arm | 新增 |
| `crates/meow-app/Cargo.toml` | feature + optional path dep | 新增 |
| `crates/meow-app/src/main.rs:695` | `listener_feature_gate` | 新增 |
| `crates/meow-app/src/main.rs:1355` | listener match | 新增分支 |

不改任何既有分支语义；`meow-transport` 与 `route_inbound_tcp` 的实现完全不动。

### 5. 升级流程

从新 upstream tag 建分支（不 merge）→ 按上表重放 7 处新增 → 复制 `server/` → 跑门禁
（根 workspace fmt/clippy/rustdoc/测试 + server 门禁 + R-01/C-01..03/DROP-01）→ 机主签字。
版本矩阵维护在 ARCHITECTURE.md §9.4。

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

1. 按 §4 清单在 fork 内实现 meow-config / meow-app 的 7 处新增。
2. `meow-server` 实现 `tunnel-bridge` feature 与 `spawn_inbound`，共享 `Tunnel`。
3. 补 C-01..03、R-01、DROP-01 测试。
4. 机主授权后在光猫 `/tmp` 单进程试跑，再受控切换入口。
5. upstream 新 tag 按 §5 重放并填版本矩阵。

## Open questions

- 设备单进程组合二进制的总体积上限（flash 余量以实采为准）。
- `mkcp-config` 字段是否全部逐项生效，还是先覆盖现役取值集合。
- 升级节奏：跟每个 upstream tag，还是选定版本。

## References

- `crates/meow-config/src/raw.rs:266`、`crates/meow-config/src/lib.rs:171,4267`
- `crates/meow-app/src/main.rs:695,1163,1350`
- `crates/meow-tunnel/src/tcp.rs:378`、`crates/meow-tunnel/src/lib.rs:24`
- `server/docs/ARCHITECTURE.md` §5、§6、§9
- `D:/workspace/server/armbian/bin/mihomo/`、`D:/workspace/server/f4610u/`（**不入库**）
