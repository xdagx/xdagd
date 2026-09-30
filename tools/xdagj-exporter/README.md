# xdagj 数据导出工具

把一个 **已停止** 的 xdagj 节点的 RocksDB 数据库（只读打开）导出为：

1. `state.xsnp` —— XSNP 快照（所有账户的余额与 nonce、节点知道的所有区块、主链索引），由 `xdagd snapshot import` 载入；
2. `blocks.dat`（可选）—— 该节点保存的全部原始区块（连续的 512 字节记录），由 `xdagd archive import-raw` 导入，用于历史查询。

> ⚠️ **本工具未在真实 xdagj 数据上运行过。** 开发环境没有 Java，也无法接入主网（白名单）。代码按 xdagj 0.8.4 源码编写
> （`BlockStoreImpl`、`AddressStoreImpl`、`BlockInfo`、`SnapshotInfo` 的实际字段与键前缀），但正式迁移前必须先在测试网/主网数据副本上演练，
> 并用下文的"核对"步骤比对结果。

## 为什么用 Java、跑在 xdagj 的 classpath 上

xdagj 用 Kryo 按类结构序列化 `BlockInfo` 等记录（这正是它每次升级都要清库的根本原因之一）。
只有与写入数据库时**完全相同版本**的 xdagj 类才能可靠地解码这些记录，所以导出工具直接复用 xdagj 自己的
`BlockStoreImpl`（经一个只读 `KVSource` 适配器），不自己重新实现 Kryo 格式。

## 编译与运行

需要 JDK 21（与 xdagj 0.8.4 相同），以及节点当时运行的那个 jar（如 `xdagj-0.8.4-executable.jar`）。

```bash
# 1. 停止 xdagj 节点（RocksDB 只能被一个进程打开）
# 2. 最好先备份数据目录，然后在副本上操作
javac -cp xdagj-0.8.4-executable.jar -d out src/XdagjExporter.java

java -Xmx4g -cp xdagj-0.8.4-executable.jar:out XdagjExporter \
     --store ./mainnet/rocksdb/xdagdb \
     --network mainnet \
     --out state.xsnp \
     --blocks blocks.dat
```

| 参数 | 说明 |
|---|---|
| `--store` | xdagj 的 `storeDir`，即 `<rootDir>/rocksdb/xdagdb`，其下有 `INDEX`、`BLOCK`、`ADDRESS` 等目录 |
| `--network` | `mainnet` / `testnet` / `devnet`，写入快照头，导入时会校验 |
| `--out` | 快照输出文件 |
| `--blocks` | 可选，原始区块归档输出文件 |

然后在新节点上：

```bash
xdagd --network mainnet snapshot import state.xsnp      # 必须是全新的数据目录
xdagd --network mainnet archive import-raw blocks.dat   # 可以再加上旧 C 版 xdag 的 storage/*.dat 文件
xdagd --network mainnet --seed <xdagj 节点 IP:端口> run
```

快照文件的大小与 xdagj 节点的 `BLOCK` 库相当（每个区块约 0.7 KB）。

## 导出内容

节点知道的东西全部导出，不做任何裁剪：xdagd 必须和它要对接的 xdagj 节点知道同样的区块、同样的"哪些区块已经执行过"，
否则两边的账本迟早会分叉（原因见 `docs/DESIGN.md` 第 5 节）。

- **账户**：`ADDRESS` 库中 `0x30 + 地址` 的余额（xdagj C 单位，1 XDAG = 2^32，原样保留，无精度损失），
  以及 `0x50 + 地址` 的已执行 nonce。零余额账户也导出（xdagj 的"地址存在"语义依赖它）。
- **区块**（`INDEX` 库 `0x30 + hashlow` 的每一条 `BlockInfo`）：
  - `BLOCK` 库里有原始数据的区块：带原始数据导出，连同难度、标志、余额、手续费、ref 等元数据；
  - 没有原始数据的区块（xdagj 从它自己的快照继承来的区块）：导出元数据，加上 xdagj 为它保留的公钥或区块数据作为验签材料。
    即使余额已经花光也导出——它仍然可能被别的区块引用；
  - `BI_OURS`、`BI_EXTRA` 标志被清除（属于旧节点本地的钱包/内存状态）。
- **主链索引**：由各区块自身的 `BI_MAIN` 标志和高度重建（xdagj 的高度键在回滚后不会被清理，不可靠）。
  xdagj 自身从快照启动时并不保存快照高度以下的全部主块，这部分高度会缺失，属正常现象。
- **RandomX 状态**：不导出，由 `xdagd` 在导入时根据主链重算（分叉 epoch 与最近两个种子），与 xdagj 的计算规则一致。
- 只存在于 xdagj 内存里的区块（还没落盘的主块候选）不会被导出，xdagd 启动后会从其他节点重新拿到。

## 已知限制

- **不要在 xdagj 节点自己刚从快照启动后的几天内导出。** xdagd 导入时要用主链上的四个主块重算 RandomX 种子
  （最近两个 4096 整数倍高度的主块，以及它们各自往前 128 个高度的主块）。xdagj 从快照启动时只保留仍有余额的旧区块，
  这几个主块如果落在它的快照高度之前，就可能不在数据库里，导入会报
  `snapshot lacks main block <高度> needed for the RandomX schedule` 并中止。
  节点在自己的快照高度之后再运行满 8320 个主块（约 6 天）就一定没有这个问题。
- 导出的是节点**停止那一刻**的状态。之后的区块由 xdagd 启动后通过 P2P 从其他节点补齐，
  所以 xdagd 还需要至少一个肯接受它连接的节点（xdagj 节点要把 xdagd 的 IP 加进自己的 `node.whiteIPs`）。
- 快照顶端之前的主块在 xdagd 上是最终确定的（快照里没有撤销它们所需的记录）。如果导出之后主网把导出时最新的一两个主块重组掉了
  （很少见），xdagd 无法跟到新链上，日志里会反复出现 `forks below the snapshot checkpoint`，这时需要重新导出。
- 导入时每个带数据的区块都会重新解析和验签。如果某个区块 xdagj 接受了而 xdagd 不接受，导入会指出这个区块并中止——
  这说明 xdagd 的移植有缺陷，需要先修复，请把报错发回来。

## 导入时的保证（由 xdagd 提供）

- 快照顶端之前的主块视为最终确定，节点不会接受从它们之下分叉的链；
- 导入失败或被中断时，数据库会留下"未完成"标记，节点拒绝用它启动；删除 `<数据目录>/chain.redb` 后重新导入即可
  （不要删除整个数据目录，里面可能有 `node.key` 和钱包）；
- 导入只接受空的链数据库。先导入快照，再导入归档。

## 核对（强烈建议）

导入后，用 RPC 比对新旧节点：

```bash
# 旧节点（xdagj）与新节点（xdagd）分别执行
curl -s -d '{"jsonrpc":"2.0","id":1,"method":"xdag_blockNumber","params":[]}' http://127.0.0.1:10001
curl -s -d '{"jsonrpc":"2.0","id":1,"method":"xdag_getBalance","params":["<地址或区块>"]}' http://127.0.0.1:10001
curl -s -d '{"jsonrpc":"2.0","id":1,"method":"xdag_getBlockByNumber","params":["<高度>"]}' http://127.0.0.1:10001
```

至少抽查：主块高度、若干大户与矿池地址的余额、若干主块的哈希与余额。xdagd 接上网络继续同步之后，
过一段时间再比对一次——两边的主块哈希和余额应当一直相同。
