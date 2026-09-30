# 主网迁移方案

目标：从 xdagj（白名单网络）迁移到 xdagd（开放网络），**账本不变、历史不丢**，并在安全的前提下去掉白名单、启用智能合约。

## 关键约束：先修共识，再开放网络

[BUGS.md](BUGS.md) 中的 C1（非候选块按 sha256d 计算难度）和 C2（重复 IN 增发）在旧规则下是共识的一部分，
只有白名单挡住了它们。因此顺序必须是：

1. 在白名单仍然有效的情况下，所有白名单节点切换到 xdagd；
2. 在一个约定的 epoch 激活 Nova（修复共识漏洞，同时启用批量交易与 EVM）；
3. Nova 激活并稳定之后，才向公众开放节点。

如果在 Nova 激活之前开放网络，任何拥有 SHA256 矿机的人都可以抢主块、回滚历史。

## 阶段 0：验证与准备（预计数周）

xdagd 已经用一份真实的主网快照做过离线核对，并在隔离的测试网络上与 xdagj 对接过，但**还没有连接过主网**。
在动任何生产节点之前，按下面的顺序验证。前两步完全不接触主网；后两步需要 xdagj 节点的运营方同意
（xdagd 在主网模式下默认不出块、不发交易，Nova 不激活，只按 xdagj 的规则验证和转发）。

1. **本机对接 xdagj 开发网** —— **已完成**（2026-09-30，结果见 [INTEROP.md](INTEROP.md) 第一轮）：
   一个真实的 xdagj 0.8.4 节点和 xdagd 在开发网上互相同步、出块、转账，包括 RandomX 阶段；
   导出工具在真实的 xdagj 数据库上运行，导出的状态与 xdagd 自己同步得到的状态逐项相同。
2. **主网快照 + 隔离网络** —— **已完成一次**（2026-09-30，高度 4,240,061 的快照，结果见 INTEROP.md 第二轮），**应当反复做**：
   - 离线部分：转换快照、审计（重算 2,112 个真实主块的 RandomX 难度、核对 200 万个主块的链接关系）、
     导入、再导出比对、两个实现解析同一批区块的差分测试。一条命令：`tools/interop/snapshot_offline.sh`，约十分钟。
   - 在线部分：xdagj 和 xdagd 以开发网身份从同一份主网状态启动，互相出块、转账，最后逐项比对。
   - 每出一份新的主网快照、xdagj 每发一个新版本、xdagd 每次改动共识相关代码之后，都重跑一遍。
     在线部分可以让两个节点连续运行数天，跨过 RandomX 种子切换（每 4096 个主块一次）。
3. **主网只读跟随**（需要 xdagj 开发者同意，并把 xdagd 的 IP 加入一台 xdagj 节点的 `node.whiteIPs`）：
   在一台 xdagj 主网节点上停机导出（阶段 1 的命令），把快照导入一个新的 xdagd 节点，让它连接这台 xdagj 节点并持续同步。
   xdagd 不能直接从零同步主网：xdagj 节点自己就是从余额快照启动的，没有快照之前的区块。
4. **长期比对**：让 xdagd 跟随主网运行数周，定期比对两边的主块高度、主块哈希、主块余额和抽样账户余额
   （`tools/interop/compare.py`、`compare_state.py`，以及停机后的 `snapshot diff`）。任何差异都说明移植有缺陷，需要在切换前修复。

### 两种取得主网状态的办法

| | 从停止的 xdagj 节点导出（`--store`） | 转换官方快照（`--snapshot`） |
|---|---|---|
| 需要什么 | 一台 xdagj 主网节点的数据目录 | 公开发布的快照文件 |
| xdagd 知道的区块 | 与那台节点完全相同，包括它启动以来的所有区块 | 与"刚从这份快照启动的 xdagj 节点"完全相同 |
| 适用于 | 接入正在运行的主网（第 3 步、正式迁移） | 测试（第 2 步）；以及全网节点同时从这份快照重启的那一刻 |

区别在于快照不包含余额为零的已执行交易块：正在运行的 xdagj 节点还记得它们，从快照启动的节点不记得
（[BUGS.md](BUGS.md) S3）。所以只要主网节点不是同时从这份快照重启，接入主网就应当用第一种办法。

同时进行：

- 安全审计：共识（`crates/chain`）、P2P（`crates/net`）、EVM 集成（`crates/evm`）、导出工具。
- 确定并写死主网参数：Nova `activation_epoch`、`chain_id`（默认预留 30820）、`min_native_fee`、`min_gas_price`、`min_link_pow_bits`。
  这些参数必须随发布版本一起分发，**不能**让各节点在配置文件里各自设置（配置覆盖只用于测试网络）。
- 在测试网完整演练以下所有阶段，包括 xdagj 与 xdagd 混跑。

## 阶段 1：数据导出与校验

在每个现有节点（或至少两个互相独立的节点）上：

```bash
# 1. 停止 xdagj，备份数据目录
# 2. 导出（见 tools/xdagj-exporter/README.md）
java -Xmx1g -cp xdagj-0.8.4-executable.jar:out XdagjExporter \
     --store ./mainnet/rocksdb/xdagdb --network mainnet --out state.xsnp --blocks blocks.dat
sha256sum state.xsnp
```

- 快照携带节点知道的全部区块和全部账户，不做裁剪（原因见 DESIGN.md 第 5 节），大小与 xdagj 的 `BLOCK` 库相当。
- `xdagd snapshot info state.xsnp` 会打印快照的主块高度、各部分的条数和**账户部分的摘要**。
  在同一主块高度停止的不同节点，账户摘要应当相同（区块部分可能因各节点持有的未确认区块不同而略有差异）。
  建议由多个运营方独立导出并公布账户摘要，作为迁移的"新起点"。
- 导入新节点并核对：

```bash
xdagd --network mainnet snapshot import state.xsnp      # 全新的数据目录
xdagd --network mainnet archive import-raw blocks.dat [旧 C 版 storage/**/*.dat ...]
xdagd --network mainnet status
```

  逐项比对：主块高度、顶端区块、抽样账户的余额与 nonce（`xdag_getBalance`、`xdag_getTransactionNonce`）、主块的哈希与余额。

## 阶段 2：白名单节点切换到 xdagd

- 所有白名单运营方停止 xdagj，用阶段 1 发布的快照启动 xdagd（`snapshot import` 后 `run`）。
- xdagd 在 P2P 层与 xdagj 兼容，所以也可以逐个节点切换：未切换的 xdagj 节点仍能与已切换的节点同步。
  但注意 xdagj 节点仍受白名单限制，需要把 xdagd 节点的 IP 加入 xdagj 的白名单。
- Nova 激活之前，xdagd 自己也是封闭的：只和 `p2p.seeds`、`p2p.allow` 里的节点通信。各运营方把彼此的节点写进这两项，
  相当于继续沿用现有的白名单；不需要、也不应该在这个阶段用 `allow = ["0.0.0.0"]` 打开它。
- 矿池：xdagd 提供与 xdagj 相同的 WebSocket 矿池接口（`[pool] enabled = true`），矿池软件无需修改；
  也可以使用内置矿工。
- 钱包：`wallet.data` 与 xdagj 互通，直接复制到 `<datadir>/wallet/wallet.data`，通过 `XDAG_WALLET_PASSWORD` 提供密码。

## 阶段 3：Nova 激活

- 在约定 epoch，所有节点（必须全部是包含相同激活参数的 xdagd 版本）开始按 Nova 规则处理区块。
- 激活后：
  - 账户余额在首次被触及时转换为 wei 精确表示；
  - 旧式交易块的格式与语义仍然有效（金额改为精确计算、失败扣费）；
  - **所有非候选块（包括交易块）都必须满足反垃圾工作量**：区块哈希中用于计算难度的 96 位（与 xdag 难度计算取同一段）需有至少 `min_link_pow_bits`（默认 16）个前导零位。
    工作量 nonce 放在第 15 个字段（类型 SIGN_IN，签名摘要中该字段被置零），所以要在**签名之前**预留这个字段、签名之后再研磨；
    约 65536 次 sha256d，普通 CPU 为毫秒级。xdagd 自带的钱包、`xdagd tx`、出块与矿池奖励支付都已自动完成；
  - EVM 可用，`eth_*` RPC 可用。
- 需要升级的外部软件：
  - **发送旧式交易块的钱包 / 交易所必须在激活前升级**：xdagj 钱包构造的交易块没有预留 nonce 字段，第三方也无法事后补上
    （改动字段类型会破坏签名），激活后会被拒绝（`insufficient anti-spam proof of work`）。
    升级方式二选一：按上述方式预留字段 15 并研磨；或改用 Nova 原生转账（`xdag_sendNovaTransaction`）/ EVM 交易；
  - 矿池：无需修改（主块候选块不需要额外工作量）。

## 阶段 4：开放网络

- Nova 激活后 xdagd 自动转为开放模式（运营方此时应清空 `p2p.allow`，否则节点仍只和列表里的节点通信）。
- 发布种子节点列表；任何人都可以运行 `xdagd --network mainnet run` 加入网络。
  新节点需要一份状态才能起步：某个节点用 `xdagd snapshot export` 导出的快照，或者直接复制的数据库文件。
- 运营建议：
  - 对外只开放 P2P 端口；RPC 默认只监听 `127.0.0.1`，对外提供 RPC 时放在反向代理之后并限流；
  - 矿池接口只允许自己的矿池连接（`[pool] allowed`）。

## 回退方案

- 在 Nova 激活前的任何时刻，都可以回退到 xdagj：xdagd 不修改 xdagj 的数据目录，xdagj 节点的数据仍在。
- Nova 激活后无法回退到 xdagj（xdagj 不认识 Nova 区块）；此时的问题只能通过发布新的 xdagd 版本解决。

## 历史数据

- `archive import-raw` 导入的区块写入独立的归档表，不参与共识，用于历史查询（`xdagd archive history <地址或区块>`）。
- 可以导入的来源：xdagj 节点的 `BLOCK` 库（导出工具 `--blocks`），旧 C 版 xdag 的 `storage/YYYY/MM/DD/HH.dat` 文件。
- 快照导入之后的所有交易历史都由节点在执行时记录，之后的任何升级都不会再清除。
