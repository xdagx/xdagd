# xdagj 数据导出工具

把一个 **已停止** 的 xdagj 节点的 RocksDB 数据库（只读打开）导出为：

1. `state.xsnp` —— XSNP 快照（账户余额与 nonce、需要的区块、主链索引），由 `xdagd snapshot import` 载入；
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
| `--horizon-epochs` | 可选，默认 1024（约 18 小时）。这段时间内的区块全部完整携带 |

然后在新节点上：

```bash
xdagd --network mainnet snapshot import state.xsnp
xdagd --network mainnet archive import-raw blocks.dat   # 可以再加上旧 C 版 xdag 的 storage/*.dat 文件
xdagd --network mainnet run
```

## 导出内容

- **账户**：`ADDRESS` 库中 `0x30 + 地址` 的余额（xdagj C 单位，1 XDAG = 2^32，原样保留，无精度损失），
  以及 `0x50 + 地址` 的已执行 nonce。零余额账户也导出（xdagj 的"地址存在"语义依赖它）。
- **区块**（`INDEX` 库 `0x30 + hashlow` 的 `BlockInfo`）：
  - 时间晚于"历史边界"（主链顶端之前 `--horizon-epochs` 个 epoch）的区块：完整携带（原始数据 + DAG 元数据 + 执行状态）；
  - 还没有被主块处理过的区块（无 `BI_MAIN_REF`）：无论多旧，完整携带，之后仍可被执行；
  - 更旧、已处理的区块：只有持有余额或是主块时才携带，且只保留公钥/原始数据作为签名验证材料；
  - `BI_OURS`、`BI_EXTRA` 标志被清除（属于旧节点本地的钱包/内存状态）。
- **主链索引**：由各区块自身的 `BI_MAIN` 标志和高度重建（xdagj 的高度键在回滚后不会被清理，不可靠）。
  xdagj 自身从快照启动时并不保存快照高度以下的全部主块，这部分高度会缺失，属正常现象。
- **RandomX 状态**：不导出，由 `xdagd` 在导入时根据主链重算（分叉 epoch 与最近两个种子），与 xdagj 的计算规则一致。

## 导入后的规则（由 xdagd 保证）

- 快照顶端之前的主块视为最终确定，节点不会接受从它们之下分叉的链；
- 历史边界之前、但快照中没有的区块，如果以后才被收到，视为"快照前已处理"，永远不会被第二次执行；
- 导入不会删除任何已有数据（例如已导入的归档历史）。

## 核对（强烈建议）

导入后，用 RPC 比对新旧节点：

```bash
# 旧节点（xdagj）与新节点（xdagd）分别执行
curl -s -d '{"jsonrpc":"2.0","id":1,"method":"xdag_blockNumber","params":[]}' http://127.0.0.1:10001
curl -s -d '{"jsonrpc":"2.0","id":1,"method":"xdag_getBalance","params":["<地址>"]}' http://127.0.0.1:10001
curl -s -d '{"jsonrpc":"2.0","id":1,"method":"xdag_getTotalBalance","params":[]}' http://127.0.0.1:10001
```

至少抽查：主块高度、若干大户与矿池地址的余额、若干主块的余额、所有账户余额总和。
