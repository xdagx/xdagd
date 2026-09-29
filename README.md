# xdagd — XDAG 全节点（Rust 重写）

`xdagd` 是 [xdagj](https://github.com/XDagger/xdagj)（0.8.4 "Firefly"）的 Rust 重写版本：

- **与现网兼容**：512 字节区块格式、DAG 共识（`tryToConnect` / `checkNewMain` / `setMain` / `applyBlock`）、RandomX、
  金额换算、P2P 帧与握手、`wallet.data`、`xdag_*` JSON-RPC、矿池 WebSocket 接口都按 xdagj 源码逐项移植，
  旧规则下的金额运算逐位复现 xdagj 的结果（包括它经过 `double` 的有损换算）。
- **去掉节点白名单**：任何人都可以运行节点、互相发现（节点交换 `GET_PEERS`/`PEERS`），
  取而代之的是按 IP / 子网的连接上限、令牌桶限流、节点打分与临时封禁。
- **升级不再丢历史**：存储使用显式版本化的记录编码 + schema 迁移，从不清库；交易历史在**执行时**写入并随回滚撤销；
  可导入 xdagj 快照与全部原始区块归档。
- **智能合约**：内置 EVM（[revm](https://github.com/bluealloy/revm)，Prague 规则），提供 `eth_*` RPC，MetaMask / ethers / Foundry 可直接使用。
- **更高 TPS**：Nova 批量区块——一个 DAG 区块通过载荷根承载最多 8192 笔交易，签名并行校验、批量落盘。

> 状态：**1.0.0-alpha.1，尚未审计，未接入真实主网验证。** 详见文末"已知限制"。

## 为什么用 Rust 而不是 Go

两者都能胜任；选择 Rust 的理由：

1. **智能合约引擎**：revm 是 geth 之外最成熟、可嵌入的 EVM 实现，原生 Rust、无需 CGO，状态接口可以直接接到我们自己的存储和回滚日志上。
2. **去掉白名单后节点直接面对所有人**：处理不可信网络输入的代码（区块解析、帧解码、快照/载荷解码）在 Rust 中没有越界、空指针、数据竞争这类问题。
3. **没有 GC 停顿**：导入、主块确认、出块这些对时间敏感的路径延迟可预测。
4. **类型区分金额单位**：`CAmount`（xdagj C 单位，2^-32 XDAG）、`Nano`（10^-9）、wei（10^-18）是不同类型，
   xdagj 里因为 `double` 混用而产生的一类 bug 在编译期就被挡住。
5. **数据并行**：rayon 让签名校验、载荷校验这类工作几乎零成本地并行化。

Go 的优势（上手更快、可以直接复用 geth、对 Java 背景的开发者更友好）也是真实的；如果维护团队以 Go 为主，Go 同样是合理选择。

## 与 xdagj 对比

| | xdagj 0.8.4 | xdagd |
|---|---|---|
| 节点准入 | 白名单（`whiteIPs` 为空则拒绝所有入站，节点只能来自配置） | 开放；节点发现 + 限流 / 打分 / 封禁 |
| 升级 | Kryo 按 Java 类结构序列化，类一变就要清库、从余额快照重建，历史丢失 | 版本化记录编码 + schema 迁移；更新版本的库会被拒绝打开而不是被破坏 |
| 交易历史 | 收到区块时写入 MySQL；回滚不撤销；被拒绝的交易也会进入历史 | 主块执行时写入（带状态：成功 / 拒绝 / 失败但扣费）；回滚时精确撤销 |
| 金额 | 多处经 `double` 换算；RPC / 命令行转账金额被四舍五入到 0.01 XDAG | 整数精确（十进制解析，9 位小数） |
| 智能合约 | 无 | EVM（revm，Prague），`eth_*` RPC |
| 吞吐 | 每笔交易一个 512 B 区块 | Nova 批量区块（每块至多 8192 笔） |
| 共识漏洞 | 见 [docs/BUGS.md](docs/BUGS.md)（两处严重问题此前被白名单掩盖） | Nova 分叉修复 |

## 构建

```bash
# 需要 Rust（rust-toolchain.toml 固定 1.98.1）和 C/C++ 编译器（用于内置的 RandomX）
cargo build --release          # 生成 target/release/xdagd
cargo test --workspace         # 运行全部测试
```

## 快速开始：本地开发网

开发网默认从创世即启用 Nova（EVM + 批量交易），RandomX 关闭。建一个配置文件 `dev.toml`：

```toml
network = "devnet"
datadir = "./dev-a"

[p2p]
listen = "127.0.0.1:28001"
allow_private = true        # 允许本机/内网节点互相发现（仅开发网默认开启）

[rpc]
listen = "127.0.0.1:30001"

[mining]
generate_blocks = true      # 生产主块候选、链接块、批量块
threads = 1                 # 内置 CPU 矿工线程

[nova]
epoch_bits = 12             # 仅开发网：4 秒一个 epoch（主网为 16，即 64 秒）
```

```bash
xdagd --config dev.toml run
# 第二个节点：只需指向第一个节点作为种子，其余节点通过节点交换自动发现
xdagd --network devnet --datadir ./dev-b --p2p 127.0.0.1:28002 --rpc 127.0.0.1:30002 --seed 127.0.0.1:28001 run
```

发交易（`--key` 是十六进制私钥或包含它的文件）：

```bash
xdagd tx --url http://127.0.0.1:30001 --key user.key address                 # 显示 XDAG 地址与 EVM 地址
xdagd tx --url http://127.0.0.1:30001 --key user.key native <地址> 1.5        # Nova 原生转账
xdagd tx --url http://127.0.0.1:30001 --key user.key evm --data 0x6080...    # 部署合约
xdagd tx --url http://127.0.0.1:30001 --key user.key evm --to 0x... --data 0x...
```

MetaMask：添加网络，RPC 填 `http://127.0.0.1:30001`，链 ID 用 `xdag_getChainInfo` / `eth_chainId` 返回的值（开发网 30822）。

## 命令一览

| 命令 | 作用 |
|---|---|
| `xdagd run` | 运行节点（默认） |
| `xdagd init [file]` | 输出当前生效配置（TOML） |
| `xdagd wallet create / list / new-account / restore <助记词>` | 钱包（与 xdagj `wallet.data` v4 互通，密码取自 `XDAG_WALLET_PASSWORD`） |
| `xdagd snapshot export <file>` / `import <file>` | XSNP 状态快照导出 / 导入 |
| `xdagd archive import-raw <files...>` / `history <地址或区块>` | 导入原始区块归档（xdagj 导出或旧 C 版 `storage/*.dat`）并查询历史 |
| `xdagd status` | 本地数据库状态 |
| `xdagd bench [--txs N] [--senders N] [--legacy]` | 在本机测量吞吐 |
| `xdagd tx ...` | 通过 RPC 签名并发送交易 |

全局参数：`--config`、`--network mainnet|testnet|devnet`、`--datadir`、`--p2p`、`--rpc`、`--seed`（可重复）、`--mine`、`--threads`。

## 代码结构

```
crates/
  types     协议基础类型：区块解析/构建、金额、地址、签名、难度、网络参数、Nova 载荷
  storage   redb 存储：表定义、版本化 schema、迁移、批量写
  chain     共识核心：导入（tryToConnect）、主链选择、执行与回滚日志、Nova 执行、交易池、快照、归档、查询
  evm       revm 集成：交易解码/验签、执行、状态差异
  randomx   内置 RandomX v1.2.1（cc 编译，无需 CMake）
  wallet    xdagj wallet.data v4、BIP32/BIP44（m/44'/586'/0'/0/i）
  net       P2P：xdagj 兼容帧/握手/消息 + 扩展消息、节点发现、限流与封禁、同步
  rpc       JSON-RPC：xdag_*（兼容 xdagj）+ eth_* / net_* / web3_*
  pool      矿池 WebSocket 接口（兼容 xdagj）、内置矿工、奖励分配
  node      xdagd 可执行文件：配置、导入流水线、出块、各子命令
tools/xdagj-exporter   把 xdagj 节点数据导出为 XSNP 快照 + 原始区块归档（Java）
docs/                  设计、Bug 清单、迁移方案、性能测试、RPC 文档
```

## 文档

- [docs/DESIGN.md](docs/DESIGN.md) —— 架构、Nova 分叉规范、存储与快照格式、P2P 扩展
- [docs/BUGS.md](docs/BUGS.md) —— 重写过程中在 xdagj 中发现的问题及处理方式
- [docs/MIGRATION.md](docs/MIGRATION.md) —— 主网迁移方案（数据导出、开放网络、Nova 激活）
- [docs/BENCHMARK.md](docs/BENCHMARK.md) —— TPS 测试方法与结果
- [docs/RPC.md](docs/RPC.md) —— RPC 接口

## 已知限制

- 没有在真实主网数据或与 xdagj 节点的实网互联中验证过（开发环境没有 Java，主网有白名单）。
  兼容性依据是逐行对照 xdagj 源码移植，以及 xdagj / xdagj-crypto 自带的测试向量。
- `tools/xdagj-exporter` 未编译运行过，迁移前必须在数据副本上演练。
- Nova 在主网的激活 epoch、链 ID 等参数尚未确定（主网默认不激活）。
- Nova 激活后所有非候选块（包括交易块）都需要少量反垃圾工作量，xdagj 旧钱包构造的交易块会被拒绝，
  钱包 / 交易所需要在激活前升级（见 [docs/MIGRATION.md](docs/MIGRATION.md) 阶段 3）。
- 共识代码需要独立安全审计后才能用于主网。
- EVM：不支持 blob（EIP-4844）与 EIP-7702 交易；没有状态根，`eth_getProof` 不可用，区块 `logsBloom` 为零。
- 交易池不持久化，节点重启后未打包的交易需要重新提交。
- 更完整的清单见 [docs/DESIGN.md](docs/DESIGN.md#已知限制与后续工作)。

## 许可证

MIT
