# ADR 0001: Server-side footprint budget and workspace isolation

- **Status:** Proposed — 隔离决策为现行约束；数值阈值待机主签字
- **Date:** 2026-11-23
- **Author(s):** meow-vgate maintainers
- **Supersedes:** —
- **Related:** [ADR 0007](../../../docs/adr/0007-m2-footprint-budget.md)、[ADR 0011](../../../docs/adr/0011-m2-footprint-targets.md)、[ARCHITECTURE.md](../ARCHITECTURE.md)

## Context

meow-rs 的 client 发布物在 `aarch64-unknown-linux-musl` 上有硬体积上限
8 MiB（8,388,608 B），见 `docs/adr/0007-m2-footprint-budget.md:142` 和
`.github/workflows/build.yml:93`。实测 client minimal binary 为 8,213,192 B，
余量约 175 KB（~2%）。任何额外入站协议都足以撞线。

服务端入站（VLESS/VMess）对 client 用户毫无价值，却被 CONTRIBUTING.md:15-22 明确排除在
upstream 之外，必须在本 fork 内永久自持。因此问题不是"怎么把 server 塞进现有预算"，
而是"怎么让 server 有自己独立的、可度量的预算，同时完全不改变 client 的发布物"。

目标架构由机主指定为 `aarch64-unknown-linux-musl` 与 `armv7-unknown-linux-gnueabihf`。

本机数据点（2026-11-23，GNU host 1.98.1，当前骨架）：

| 构建 | stripped 体积 |
|---|---|
| `x86_64-pc-windows-gnu` release | 2,540,544 B（未 strip 3,815,785 B） |

这是 Windows PE 数据，**不能**直接当 ARM 数据；仅说明当前依赖图规模可控。

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

### 4. 临时阈值（待签字）

在首次真实 aarch64-musl 测量完成前，采用 **4 MiB（4,194,304 B）stripped** 作为
PR 侧警告线，不作为阻断门槛。测量后由机主签字确定正式 hard cap，再接入独立 CI job。
该数值不与 client 的 8 MiB 共享，也不相加。

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

1. 在 `server/Cargo.toml` 补 release profile（与根一致）。
2. 在 CI 新增 server 专属 job：fmt、clippy `-D warnings`、rustdoc `-D warnings`、
   `cargo test --all-targets`、两个 ARM 目标 `cargo check`。
3. 配好 musl 交叉链接器后首次实测 aarch64 stripped 体积，把结果回填本 ADR。
4. 机主签字确定正式 hard cap，把 4 MiB 警告线升级为阻断门槛。

## Open questions

- 正式 aarch64-musl cap 数值：等待首次真实测量。
- server 与 client 是分发为两个二进制，还是同一镜像内的两个进程？影响是否需要合并体积预算。
- bridge 所需 C++ 交叉工具链由哪个 CI runner 提供（client release 矩阵已有 boringssl 构建能力，可复用）。

## References

- `docs/adr/0007-m2-footprint-budget.md:142,158-161,204-226,397-414`
- `docs/adr/0011-m2-footprint-targets.md:220-230`
- `.github/workflows/build.yml:87-94,315-330`
- `.github/workflows/test.yml:707-779`
- `CONTRIBUTING.md:15-22`
- `server/Cargo.toml` 的 workspace 隔离注释
- `server/docs/ARCHITECTURE.md` §2、§5
