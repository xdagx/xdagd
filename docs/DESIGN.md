# 设计文档

本文描述 `xdagd` 的架构、与 xdagj 保持兼容的部分、Nova 分叉引入的新规则，以及存储、快照、P2P 的格式。
代码中的注释以英文为主，本文是中文的总体说明。

## 1. 总体架构

```
            ┌──────────── xdag-net（tokio）────────────┐
 peers ───▶ │ 帧解码 → 握手 → 消息分发 → 限流/打分/封禁 │ ──┐
            └──────────────────────────────────────────┘   │ 区块 / 载荷 / 交易
                                                            ▼
 RPC（axum）──▶ 交易池（TxPool）            导入流水线（node::import_loop）
      │              │                       1. 攒批（≤4096 个区块）
      │              ▼                       2. rayon 并行：解析 + 签名/载荷校验（preverify）
      │         出块（producer）             3. 串行：Chain::import_uncommitted（tryToConnect）
      │     候选块 / 批量块 / 链接块          4. 整批一次提交（redb 事务）
      │              │                       5. 等待队列：缺父块 / 缺载荷 / 暂缓的区块
      ▼              ▼
 查询（redb 读事务，MVCC，不阻塞导入）  ◀── xdag-chain：共识、执行、回滚日志、历史索引
```

- **xdag-chain** 是纯同步库，所有共识逻辑都在这里，便于确定性测试（`testkit::Sim` 用手动时钟模拟完整的 DAG）。
- 区块在进入共识锁之前完成全部无状态校验（签名、载荷根、载荷内每笔交易的签名），并行执行；
  共识部分只做有状态的检查与执行。
- 写入先进入内存覆盖层（`Overlay`），每批导入或每个主块确认后一次性提交；读路径（RPC）使用 redb 的 MVCC 读事务。

## 2. 旧规则：与 xdagj 保持一致的部分

以下内容按 xdagj 0.8.4 源码逐项移植，在 Nova 激活前的区块上行为一致：

| 部分 | 说明 |
|---|---|
| 区块格式 | 16 个 32 字节字段；头字段类型半字节；`sha256d` 哈希；24 字节 hashlow；链接字段 = hashlow + 8 字节 C 单位金额；地址字段第 4..24 字节为反序 hash160；`TX_NONCE` 小端 |
| 签名 | secp256k1，RFC6979 + low-S；拒绝 high-S；`(0,0)` 视为 `(1,1)`；签名字段配对规则保留 xdagj 的重叠配对（(13,14)、(14,15)）；`getSubRawData` 摘要；输出签名必需 |
| 导入 | `tryToConnect` 的校验顺序与错误条件；额外块（内存候选块池，上限 65536）；`removeOrphan` |
| 难度与主链 | `calculateBlockDiff`（同 epoch 规则）、`findAncestor`、`unWindMain`、`updateNewChain`、`checkNewMain`（被引用、后面至少还有一个链块、时间过去 2 秒） |
| 执行 | `setMain` / `applyBlock`：深度优先、子块优先；返回值语义（-1 跳过 / 0 拒绝 / 手续费）；账户 nonce 规则；余额不足时消耗 nonce 但不转账 |
| 手续费与奖励 | `MIN_GAS` = 0.1 XDAG / 输出；`outPutLimit`；奖励 1024 → 128 XDAG（apollo 高度 1017323）后每 2^21 个主块减半 |
| 金额 | 账户余额按 C 单位（2^-32 XDAG）保存；`ofXAmount` / `toXAmount` 通过复现 IEEE-754 `double` 运算逐位一致 |
| RandomX | 分叉高度 1540096，种子每 4096 个主块更新、滞后 128；输入为 `sha256(前 480 字节) ‖ nonce` |
| 钱包 | `wallet.data` v4（bcrypt + AES-192-CBC），BIP44 路径 `m/44'/586'/0'/0/i` |

与 xdagj **不同但不影响共识结果**的实现方式：

- **回滚**：xdagj 用手写的 `unApplyBlock` 反向运算（有漂移，见 BUGS.md B4）。这里每个主块的执行都记录一份撤销日志
  （被改动的每个键的旧值），回滚时原样恢复，结果与"从未执行过"逐字节相同。
- **交易历史**：在执行时写入，带状态，随撤销日志一起回滚（xdagj 在收到区块时写入 MySQL）。
- **依赖状态的有效性**：xdagj 要求 `INPUT` 地址"已存在"，这让导入结果依赖到达顺序。这里把这类区块暂缓（`Deferred`），
  等下一个主块确认后重试，而不是永久丢弃。
- **额外块在重启后丢失**：顶端候选块只在内存中时重启，节点会把顶端恢复到最后一个主块并清理过期的主链标记，而不是继续指向一个已不存在的区块。

这些差异的完整清单与理由见 [BUGS.md](BUGS.md)。

## 3. Nova 分叉

Nova 是按 epoch 激活的硬分叉：`区块时间所在 epoch ≥ activation_epoch` 的区块按 Nova 规则校验和执行。
开发网从创世激活；测试网、主网默认不激活，激活参数需要全网统一发布（见 [MIGRATION.md](MIGRATION.md)）。

### 3.1 参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `chain_id` | 主网 30820 / 测试网 30821 / 开发网 30822 | 原生转账与 EVM 交易的重放保护 |
| `min_native_fee` | 0.1 XDAG | 原生转账最低手续费 |
| `min_gas_price` | 1 gwei | EVM 最低 gas 价格 |
| `batch_gas_limit` | 3000 万 | 单笔 EVM 交易 / 单个批量块的 gas 上限 |
| `main_gas_limit` | 3 亿 | 一个主块执行的 EVM gas 总上限 |
| `max_payload_bytes` | 1 MiB | 单个载荷大小上限 |
| `max_payload_txs` | 8192 | 单个载荷交易数上限 |
| `min_link_pow_bits` | 16（开发网 8） | 非候选块需要的反垃圾工作量 |

### 3.2 金额与账户

- 账户余额改为以 **wei（u128，1 XDAG = 10^18 wei）** 精确保存。旧的 C 单位余额在第一次被 Nova 执行触及时，
  按 xdagj 显示给用户的 nano 值（`ofXAmount`，HALF_UP）换算，之后所有运算都是整数精确运算。
- 区块余额（矿池从主块支付用的"块余额"）仍以 nano（i64）保存。
- 同一个 20 字节账户空间同时服务原生转账和 EVM。注意：同一把私钥的 **XDAG 地址**（`ripemd160(sha256(压缩公钥))`，
  Base58Check）与 **EVM 地址**（`keccak256(公钥)[12..]`，0x…）是两个不同的账户，它们之间可以互相转账。

### 3.3 批量区块与载荷

批量区块是一个普通的 512 字节 DAG 区块，其中一个类型为 `0xF`（Extension）的字段保存 `sha256d(payload)`。
载荷不在区块内，而是通过 P2P 扩展消息随区块传输（见第 6 节），并单独存储。

```text
payload   := "XNP1" | count:u32le | { kind:u8 | len:u32le | bytes[len] } * count
kind 1    := 原生转账（见下）
kind 2    := 以太坊交易（legacy EIP-155 / EIP-2930 / EIP-1559 的原始 RLP / typed 编码）

原生转账  := body | sig[65]
body      := 0x01 | chain_id:u64le | nonce:u64le | to[20] | amount:u64le(nano) | fee:u64le(nano)
             | remark_len:u8 (≤32) | remark
签名摘要  := sha256d("XDAG/NOVA/TRANSFER/v1" ‖ body)
sig       := r[32] ‖ s[32] ‖ v(0/1)，可恢复签名，low-S；发送方 = hash160(恢复出的压缩公钥)
交易哈希  := sha256d(body ‖ sig)
```

- 交易块（带 IN/INPUT 的区块）不能携带载荷；载荷根不匹配、交易数/大小超限、任何一笔签名或链 ID 错误，整个区块无效。
- 批量块由出块节点从交易池中挑选：按发送方 nonce 连续取用；已打包但未确认的交易标记为"在途"，10 分钟未确认会重新打包。

### 3.4 执行语义

- 批量块在所属主块的 `applyBlock` 中、按 xdagj 的深度优先顺序被执行；载荷内的交易按载荷顺序执行。
- **原生转账**：`nonce` 必须等于已执行数 + 1，否则跳过（无任何效果，可以之后再次打包）。
  余额 ≥ 金额 + 手续费 → 转账成功；余额只够手续费 → **只扣手续费并消耗 nonce（失败扣费）**；连手续费都不够 → 跳过。
  手续费计入批量块，随主块执行汇入主块余额（与旧交易的 gas 流向相同）。
- **EVM 交易**：revm（Prague 规则，不含 blob / EIP-7702）。执行环境：`block.number` = 主块高度，
  `block.timestamp` = 主块时间（秒），`block.coinbase` = 主块的 coinbase 地址，`prevrandao` = 主块哈希，basefee = 0，
  有效 gas 价格须 ≥ `min_gas_price`。EVM 手续费按以太坊语义付给 coinbase 账户。
  一个主块累计 gas 超过 `main_gas_limit` 后，剩余 EVM 交易被跳过（无效果，交易池之后重试）。
  回执、日志、合约代码、存储都进入撤销日志，随主块回滚。
- **旧式交易块在 Nova 下仍然有效**，但：金额按 nano 精确计算；账户交易同样"失败扣费"；
  主交易（从区块余额支付）对同一来源区块的多个 IN 先**汇总**再检查余额（修复旧规则下的重复 IN 负余额问题）。
- 非交易区块（候选块、链接块、批量块）的链接金额必须为 0。

### 3.5 共识安全修复（仅 Nova 规则下生效）

| 问题（旧规则） | Nova 规则 |
|---|---|
| 非候选块（epoch 中途的链接块）也按 sha256d 计算自身难度，RandomX 分叉后可以用 GPU/ASIC 研磨一个链接块抢走主块奖励；也可以用一个不相连的"幸运块"让整条链回滚 | 只有 epoch 末尾、带挖矿 nonce 的候选块有自身难度，其余区块自身难度为 0 |
| 任何人都能以零成本广播海量链接块 | 非候选块须满足 `min_link_pow_bits` 位的工作量（通过字段 15 的 nonce 研磨） |
| 同一来源区块的重复 IN 让区块余额变负（凭空增发） | 汇总后检查 |
| 金额相加溢出使 `setMain` 执行到一半中止 | 导入时检查输入/输出总和不溢出 |
| 导入结果依赖状态（地址是否存在） | 不再检查地址是否存在 |

详见 [BUGS.md](BUGS.md)。旧规则区块仍按 xdagj 行为处理，以保证与现网历史一致。

反垃圾工作量同样适用于交易块。工作量 nonce 位于字段 15（类型 SIGN_IN，在所有签名摘要中被置零），
因此签名后可以直接研磨而无需重新签名；但字段必须在签名前预留，xdagj 旧钱包构造的交易块在 Nova 激活后会被拒绝，
需要升级（见 [MIGRATION.md](MIGRATION.md) 阶段 3）。

## 4. 存储

后端为 [redb](https://github.com/cberner/redb) 2.x（纯 Rust、ACID、MVCC）。每张表存放显式编码的记录（小端、带版本字节），
不依赖任何语言运行时的对象布局。

| 表 | 键 | 值 |
|---|---|---|
| Meta | 名称 | schema 版本、链元数据（nmain、顶端、快照高度与历史边界）、RandomX 状态 |
| BlockRaw | hashlow | 原始 512 字节 |
| BlockInfo | hashlow | DAG 元数据：哈希、时间、类型、DAG 标志、累计难度、maxDiffLink、备注、快照密钥、载荷根与交易数 |
| BlockState | hashlow | 执行状态：执行标志、区块余额、手续费、ref、主块高度 |
| MainHeight | 高度（BE） | hashlow |
| TimeIndex | epoch（BE）‖ hashlow | — |
| Sums | 层级 ‖ 时间前缀 | 同步用校验和（与 xdagj 的 sums 结构相同） |
| Account | 地址 | 余额（C 单位或 wei）、nonce、合约代码哈希 |
| Code / Storage | 代码哈希 / 地址 ‖ 槽 | EVM 合约代码 / 存储 |
| Journal | 主块高度 | 该主块执行的撤销日志 |
| History | 主体 ‖ 主块高度 ‖ 序号 | 交易历史（方向、金额、对手方、时间、状态、备注） |
| TxIndex | 交易哈希 | 所在区块、序号、主块高度、状态、发送方、手续费、gas |
| Receipt / EvmTxs | 交易哈希 / 高度 ‖ 序号 | EVM 回执 / 每个主块的 EVM 交易列表 |
| Payload | hashlow | Nova 载荷 |
| NoRef / Ours | hashlow | 未被引用的区块 / 本节点钱包的区块 |
| Archive / ArchiveHistory | … | 导入的历史区块及其历史索引（不参与共识） |

- **DAG 元数据与执行状态分离**：`BlockInfo` 只在导入时写，`BlockState` 只在执行时写并进入撤销日志，回滚不会碰 DAG 结构。
- **schema 迁移**：数据库记录 schema 版本；打开时按顺序执行迁移函数；遇到**更新**版本的数据库直接拒绝打开（而不是像 Kryo 那样读出错乱数据）。
  这是"升级不再清库、不再丢历史"的根本保证。
- **历史**：主体为 `0x01 ‖ 地址` 或 `0x02 ‖ hashlow`；每条记录在执行时写入，带状态 `Applied / Rejected / Failed`；
  撤销日志覆盖历史表，所以回滚后历史与账本始终一致。

## 5. 快照（XSNP）

用于从 xdagj 迁移（由 `tools/xdagj-exporter` 生成）和新节点快速启动。**导入快照从不删除任何数据**。

```text
"XSNP" | version:u8=1 | network:u8 | nmain:u64 | top:hashlow[24] | top_diff:[32]BE | horizon_time:u64
rx_len:u64 | rx[rx_len]                         （0 = 由导入方根据主链重算 RandomX 状态）
n:u64 { address[20] | kind:u8 (0 = C 单位 u64, 1 = wei u128) | balance | nonce:u64 }
n:u64 { hashlow[24] | hash[32] | time:u64 | flags:u8 | height:u64 | diff:[32]BE
        | opt maxDiffLink | amount:i64(nano) | fee:u64(nano) | opt remark[32]
        | kind:u8 data | opt ref }
        kind 0 无密钥 / 1 压缩公钥[33] / 2 原始区块[512]（仅作签名验证材料）/ 3 完整区块[512]
             / 4 完整区块[512] + len:u64 + Nova 载荷
n:u64 { height:u64 | hashlow[24] }
（opt X = u8 标志 + X；整数均为小端，难度为 32 字节大端）
```

导出规则：

1. 所有账户；
2. **历史边界**（默认主链顶端之前 1024 个 epoch）之后的所有区块，完整携带；
3. 尚未被任何主块处理的区块，无论多旧，完整携带（之后仍可被执行）；
4. 更旧、已处理的区块：仅当持有余额或是主块时携带，只保留签名验证材料；
5. 主链索引。

导入方保证：

- **检查点**：快照高度及以下的主块视为最终确定，不接受从它们之下分叉的链（快照中没有这些主块的撤销日志）；
- **不重复执行**：历史边界之前、但快照里没有的区块如果以后才收到，会被标记为"快照前已处理"，永远不会再执行一次；
- **RandomX**：导入后根据主链重算分叉 epoch 与最近两个种子；快照若自带 RandomX 状态，必须与重算结果一致，否则拒绝导入；
- 快照中的区块若带有传输头（前 8 字节）导致哈希不符，会清零后重新校验。

这些行为由 `crates/chain/tests/snapshot.rs` 覆盖（包括"导出方与导入方继续同步后状态逐项一致"、
"晚到的旧区块不会被二次执行"、"不会回滚到快照之下"、"未执行的 Nova 批量块在导入后正常执行"、"RandomX 状态重算"）。

## 6. P2P

### 6.1 与 xdagj 兼容

- 帧：16 字节大端头（版本、压缩=snappy raw、类型、包 ID、包大小、体大小），单帧体 ≤ 128 KiB，单包 ≤ 16 MiB，
  修复了 xdagj 在长度恰为分片大小整数倍时的分片错误。
- 握手：`INIT` / `HELLO` / `WORLD`，对基本信息的 `sha256` 做可恢复签名，校验 peerId = Base58(hash160(公钥))、时间偏差 ≤ 5 分钟、网络 ID。
- 消息 0x00–0x1A：`BLOCKS_REQUEST/REPLY`、`SUMS_REQUEST/REPLY`、`BLOCK_REQUEST`、`NEW_BLOCK`、`SYNC_BLOCK` 等。

### 6.2 开放网络（去白名单）

- 任何节点都可以连入和连出；种子节点只用于初始发现。
- 节点发现：扩展消息 `GET_PEERS`（0x20）/ `PEERS`（0x21），约每 60 秒交换一次，已知节点持久化到 `peers.json`。
- 防护：入站 ≤ 128、出站 ≤ 16、单 IP ≤ 4、单个 IPv4 /24（IPv6 /48）子网 ≤ 16；按消息类别（区块、请求、范围请求、交易、节点交换）的令牌桶限流；
  违规（坏帧、坏区块、超限请求）扣分，分数过低断开并封禁 1 小时；拒绝自连接与重复连接；可配置本地拒绝列表。
- 请求有界：`BLOCKS_REQUEST` 时间跨度 ≤ 2^20、单次最多回复 5 万个区块；sums 同步每轮 ≤ 512 个请求、≤ 128 个窗口。
- **从不信任对端提供的状态**：对端的统计信息（区块数、难度）只用于选择同步对象；`SYNC_BLOCK` 携带的执行状态被忽略
  （xdagj 会直接采用对端给出的交易执行结果）。

### 6.3 Nova 扩展

握手中声明能力 `NOVA_V1` 的节点之间使用扩展消息（其余节点照常使用 xdagj 消息，不会收到扩展消息）：

| 代码 | 消息 | 说明 |
|---|---|---|
| 0x22 | `NEW_BLOCK_EXT` | 区块 + 载荷 |
| 0x23 / 0x24 | `GET_PAYLOAD` / `PAYLOAD` | 按 hashlow 请求 / 返回载荷 |
| 0x25 | `NEW_TXS` | 交易池广播（原生转账、EVM 交易） |

## 7. 出块与矿池

- 出块节点（`mining.generate_blocks = true`）每个 epoch 生成主块候选任务，前一个顶端变化时刷新；
  内置 CPU 矿工或外部矿池（WebSocket，任务/份额 JSON 与 xdagj 相同）求解；epoch 末尾导入最好的解。
- 有待打包交易时每 `batch_interval_ms` 生成一个批量块；未被引用的区块达到 8 个时生成链接块把它们接入 DAG。
- 奖励：主块确认 16 个 epoch 后按比例从主块余额支付——基金会 `fund_percent`（默认 5%）、节点 `node_percent`（默认 5%）、
  其余给矿池；独立挖矿（内置矿工）时剩余部分转入节点自己的账户（xdagj 中这部分会留在主块余额里）。

## 8. RPC

见 [RPC.md](RPC.md)。

## 已知限制与后续工作

- **未经实网验证**：没有与 xdagj 节点互联测试，没有用真实主网数据回放；兼容性依据是源码对照与 xdagj 自带测试向量。
  在打开主网之前，应当：（1）用导出工具导出一份主网数据并导入，逐账户比对余额；（2）在测试网与 xdagj 节点混合运行。
- **导出工具未编译运行过**（开发环境没有 Java）。
- **安全审计**：共识、P2P、EVM 集成都需要独立审计。
- **EVM**：不支持 blob（EIP-4844）与 EIP-7702；没有状态根（`eth_getProof` 不可用），`logsBloom` 为零；
  RPC 返回的交易 `v/r/s` 字段为 0；`SELFDESTRUCT` 清理存储时只扫描已提交的数据。
- **交易池**不持久化，重启后需重新提交；不会定期重新广播。
- **矿池**：不支持 RandomX 分叉前的 SHA256 任务格式（主网早已进入 RandomX 阶段）；算力字段固定为 "0.0"。
- **与 xdagj 的已知行为差异**（均在 BUGS.md 中说明）：`personal_sendTransaction` 由发送方承担手续费、接收方收到全额；
  主块执行因溢出中止时，这里精确撤销，而 xdagj 会留下部分状态；`rollTx` 中清除 `BI_REF` 的本地行为未复制。
- Nova 主网激活 epoch、链 ID 需要社区确定。
