# 主网迁移方案

目标：从 xdagj（白名单网络）迁移到 xdagd（开放网络），**账本不变、历史不丢**，并在安全的前提下去掉白名单、启用智能合约。

## 关键约束：先修共识，再开放网络

[BUGS.md](BUGS.md) 中的 C1（非候选块按 sha256d 计算难度）和 C2（重复 IN 增发）在旧规则下是共识的一部分，
只有白名单挡住了它们。因此顺序必须是：

1. 在白名单仍然有效的情况下，所有白名单节点切换到 xdagd；
2. 在一个约定的 epoch 激活 Nova（修复共识漏洞，同时启用批量交易与 EVM）；
3. Nova 激活并稳定之后，才向公众开放节点。

如果在 Nova 激活之前开放网络，任何拥有 SHA256 矿机的人都可以抢主块、回滚历史。

## 阶段 0：准备（预计数周）

- 安全审计：共识（`crates/chain`）、P2P（`crates/net`）、EVM 集成（`crates/evm`）、导出工具。
- 确定并写死主网参数：Nova `activation_epoch`、`chain_id`（默认预留 30820）、`min_native_fee`、`min_gas_price`、`min_link_pow_bits`。
  这些参数必须随发布版本一起分发，**不能**让各节点在配置文件里各自设置（配置覆盖只用于测试网络）。
- 在测试网完整演练以下所有阶段，包括 xdagj 与 xdagd 混跑。

## 阶段 1：数据导出与校验

在每个现有节点（或至少两个互相独立的节点）上：

```bash
# 1. 停止 xdagj，备份数据目录
# 2. 导出（见 tools/xdagj-exporter/README.md）
java -Xmx4g -cp xdagj-0.8.4-executable.jar:out XdagjExporter \
     --store ./mainnet/rocksdb/xdagdb --network mainnet --out state.xsnp --blocks blocks.dat
sha256sum state.xsnp
```

- 在同一主块高度停止的不同节点导出的快照，内容应当完全一致（`sha256sum` 相同）。建议由多个运营方独立导出并公布哈希，
  作为迁移的"新起点"。
- 导入新节点并核对：

```bash
xdagd --network mainnet snapshot import state.xsnp
xdagd --network mainnet archive import-raw blocks.dat [旧 C 版 storage/**/*.dat ...]
xdagd --network mainnet status
```

  逐项比对：主块高度、顶端区块、每个账户的余额与 nonce（可以用 RPC 批量比对 `xdag_getBalance`）、主块余额、账户余额总和。

## 阶段 2：白名单节点切换到 xdagd

- 所有白名单运营方停止 xdagj，用阶段 1 发布的快照启动 xdagd（`snapshot import` 后 `run`）。
- xdagd 在 P2P 层与 xdagj 兼容，所以也可以逐个节点切换：未切换的 xdagj 节点仍能与已切换的节点同步。
  但注意 xdagj 节点仍受白名单限制，需要把 xdagd 节点的 IP 加入 xdagj 的白名单。
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

- 发布种子节点列表；任何人都可以运行 `xdagd --network mainnet run` 加入网络。
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
