# ADR 0001: Server-side footprint budget and workspace isolation

- **Status:** Proposed — 隔离决策为现行约束；实测已完成（2026-11-24），正式 cap 数值待机主签字
- **Date:** 2026-11-23
- **Author(s):** meow-vgate maintainers
- **Supersedes:** —
- **Related:** [ADR 0007](../../../docs/adr/0007-m2-footprint-budget.md)、[ADR 0011](../../../docs/adr/0011-m2-footprint-targets.md)、[ARCHITECTURE.md](../ARCHITECTURE.md)

## Context

meow-rs 的 client 发布物在 `aarch64-unknown-linux-musl` 上有硬体积上限
8 MiB（8,388,608 B），见 `docs/adr/0007-m2-footprint-budget.md:142` 和
`.github/workflows/build.yml:93`。历史实测 client minimal binary 为 8,213,192 B
（余量约 175 KB）；2026-11-24 在当前分支工具链上复测为 7,683,456 B（余量 705 KB）。
预算余量随工具链波动，必须每次在 CI 上实测而非引用旧数字。

服务端入站（VLESS/VMess）对 client 用户毫无价值，却被 CONTRIBUTING.md:15-22 明确排除在
upstream 之外，必须在本 fork 内永久自持。因此问题不是"怎么把 server 塞进现有预算"，
而是"怎么让 server 有自己独立的、可度量的预算，同时完全不改变 client 的发布物"。

目标架构由机主指定为 `aarch64-unknown-linux-musl` 与 `armv7-unknown-linux-gnueabihf`。

实测数据点：

| 日期 | 构建 | stripped 体积 | 说明 |
|---|---|---|---|
| 2026-11-23 | `x86_64-pc-windows-gnu` server skeleton release | 2,540,544 B | Windows PE，仅说明依赖图规模可控 |
| 2026-11-24 | `x86_64-pc-windows-gnu` server release (with profile) | 728,064 B | strip + opt-level=z 生效后的 PE |
| 2026-11-24 | `aarch64-unknown-linux-musl` **client minimal**（`--no-default-features --features minimal`） | **7,683,456 B** | 8 MiB cap 余量 705,152 B；sha3/Chunk-Masking 未撞线 |
| 2026-11-24 | `aarch64-unknown-linux-musl` **full baseline**（默认 `full`，无 server-inbound） | **12,931,304 B** | 对照基准，同 profile/工具链 |
| 2026-11-24 | `aarch64-unknown-linux-musl` **single-process `full + server-inbound`** | **13,086,672 B**（~12.5 MiB） | 机主终局目标二进制；cargo-zigbuild 0.23.2 + zig 0.13.0 |
| 2026-11-24 | **server-inbound delta（fair）** | **+155,368 B**（~152 KB） | 与 full baseline 同 feature 集合作差 |

⚠️ **不要把 7,683,456（minimal）与 13,086,672（full+server）直接作差**：两者
feature 集合不同，约 5.2 MiB 差额来自 `full` bundle 本身（hysteria2/quiche、
logbook/SQLite、public-suffix ~500 KB、anytls、kcptun、mux、多 listener 等），
与 fork 的 server 代码无关。真正的 fork 成本是同基准作差的 **+155,368 B**,
主要是 meow-server（VLESS/VMess 解析 + mKCP + WS）与 sha3（Chunk Masking）。

ARM 数值由 `server-inbound.yml` 的 `client-size` job 在 CI 上用真实交叉工具链测得
（`cargo zigbuild --release --locked`，profile 与本节一致）。前两行是 Windows PE，
**不能**当 ARM 数据；ARM 行才是预算依据。

## Decision

### 1. 两个 workspace，server 通过 optional feature 接入单进程

```
根 workspace（client，15 crates）        ← 8 MiB cap 只作用于这里
   └─ meow-app 的 optional dep: meow-server（feature server-inbound 开启时才编译）
server/       独立 workspace（协议入站 + tunnel-bridge feature）
```

- 不在根 `Cargo.toml` 的 `members` 中加 `server`。
- `meow-app` 用 optional path dep 引入 `meow-server`，默认 feature 不含 `server-inbound`；
  因此普通 client 构建与 8 MiB minimal cap 不变。
- 不修改 `build.yml` 的 `minimal_budget_bytes`。

验证方式：根 `cargo metadata --no-deps` 仍为 15 个包且不含 `meow-server`；
device 构建用 `cargo build --features server-inbound` 单进程同时包含 client 与 server。

### 2. server 默认纯 Rust，`tunnel-bridge` feature 才接上游

`server/Cargo.toml` 当前依赖：`tokio`、`tokio-tungstenite`、`futures-util`、`bytes`、
`clap`、`tracing`、`async-trait`、`thiserror`、`uuid`。不含 `boring-sys`、OpenSSL、
cmake 依赖，两个 ARM 目标的 `cargo check --all-targets` 不需要 C 交叉工具链（已实测通过）。

规划中的 `tunnel-bridge` feature 增加 `meow-common` / `meow-tunnel` path 依赖，让入站与
客户端共享同一个 `Arc<Tunnel>`（单进程目标）。它会引入 boring-sys，但那是 client 已有的
同一套 TLS 栈，不是第二套；该 feature 只在 device 构建开启。

### 3. 测量方法（与 client 对齐）

在配备 musl 交叉链接器的 CI runner 上执行：

```bash
cd server
cargo build --release --target aarch64-unknown-linux-musl
llvm-strip target/aarch64-unknown-linux-musl/release/meow-server
stat -c '%s' target/aarch64-unknown-linux-musl/release/meow-server
```

发布 profile 应与根 `Cargo.toml` 一致：`lto = true`、`strip = true`、
`codegen-units = 1`、`panic = "abort"`、`opt-level = "z"`。当前 `server/Cargo.toml`
尚未声明该 profile，属 Migration 步骤。

armv7hf 只做类型级 `cargo check`，不设体积 hard gate（无部署 cap 契约）。

### 4. 阈值（实测已得，正式 cap 待签字）

早期骨架阶段曾用 4 MiB（4,194,304 B）作 PR 警告线。首次真实 aarch64-musl
测量已完成（见 Context 表）：单进程 server-inbound 二进制为 **13,085,328 B**，
因此 4 MiB 线已不适用，撤销。

建议的正式 hard cap：**14 MiB（14,680,064 B）stripped**，相对当前 13,085,328 B
留约 1.5 MiB（~12%）余量，容纳后续入站协议与 bugfix 增量。该 cap **仅作用于
单进程 server-inbound 构建**，与 client 的 8 MiB cap 独立、不相加；尚未接入
阻断 CI（当前 CI 只上报体积、不设 gate），**须机主签字后**才升级为 hard gate。
armv7hf 仍只做类型级 `cargo check`，无部署 cap 契约。

## Consequences

**Positive**

- client 的 8 MiB hard gate 与发布流程零改动，回归风险最低。
- server 侧可以独立演进依赖与 feature，不受 `meow-app` 的 feature powerset 影响。
- 纯 Rust 依赖让两个目标架构的类型检查在普通开发机上就能完成。

**Negative / risks**

- 两套 lockfile、两类检查命令（默认纯 Rust / `tunnel-bridge` 含上游依赖），容易漏跑；
  必须在文档中写清。
- `tunnel-bridge` 引入 `boring-sys` 后需要 C++ 交叉工具链，device 单进程构建比 client minimal 更重。
- `route_inbound_tcp` 是 `#[doc(hidden)]` 的内部契约，契约测试必须在每次 upstream 换 tag
  后重跑（ARCHITECTURE.md 的 R-01）。
- 单进程二进制体积大于 client minimal，8 MiB cap 不覆盖它，需按 ADR 0002 另定上限。

**Neutral**

- 本 ADR 不决定 server 与 client 是否最终合并成一个发行包；见 Open questions。

## Alternatives considered

| 方案 | 结论 | 理由 |
|---|---|---|
| 把 `server` 加入根 workspace，用 feature gate 排除 | 拒绝 | 即使默认关闭，feature powerset / rustdoc / clippy 全量 job 仍会看到它，8 MiB 余量仅 175 KB |
| 把入站代码直接写进 `meow-app` 的 `#[cfg(feature = "server")]` | 拒绝 | 违反 CONTRIBUTING 的 server-side out-of-scope，且 delta 会散落在多个 client crate |
| 只做纯 Rust server，永不对接 tunnel | 拒绝 | 无法形成可用网关，且会诱使后续复制路由逻辑 |
| 不设 server 体积阈值 | 拒绝 | 重演 client 的 cap 危机；先定测量方法再定数值 |

## Migration

1. ✅ 在 `server/Cargo.toml` 补 release profile（与根一致）— 2026-11-24 完成。
2. ✅ CI server 专属 job：fmt、clippy `-D warnings`、rustdoc `-D warnings`、
   `cargo test --all-targets`、两个 ARM 目标 `cargo check` — 2026-11-24 完成。
3. ✅ 首次实测 aarch64 stripped 体积并回填本 ADR — 2026-11-24 完成
   （13,085,328 B 单进程；client minimal 7,683,456 B）。
4. ⏳ 机主签字确定正式 hard cap（建议 14 MiB），把体积上报升级为阻断门槛。

## Open questions

- 正式 aarch64-musl cap 数值：实测 13,085,328 B，建议 14 MiB，待机主签字。
- server 与 client 是分发为两个二进制，还是同一镜像内的两个进程？影响是否需要合并体积预算。
- ~~bridge 所需 C++ 交叉工具链由哪个 CI runner 提供~~ — 已解决：`client-size` job
  用 `cargo-zigbuild 0.23.2` + `zig 0.13.0`（与 build.yml 同 pin）成功构建单进程
  aarch64-musl 二进制， boringssl 栈复用 client 同一套。

## References

- `docs/adr/0007-m2-footprint-budget.md:142,158-161,204-226,397-414`
- `docs/adr/0011-m2-footprint-targets.md:220-230`
- `.github/workflows/build.yml:87-94,315-330`
- `.github/workflows/test.yml:707-779`
- `CONTRIBUTING.md:15-22`
- `server/Cargo.toml` 的 workspace 隔离注释
- `server/docs/ARCHITECTURE.md` §2、§5
