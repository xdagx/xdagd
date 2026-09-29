# JSON-RPC 接口

HTTP POST，JSON-RPC 2.0，支持批量请求（每批最多 1000 个）。默认监听 `127.0.0.1`：
主网 10001、测试网 20001、开发网 30001（与 xdagj 相同的主网端口）。

```bash
curl -s -H 'content-type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"xdag_getBalance","params":["<地址>"]}' http://127.0.0.1:10001
```

约定：

- 金额是字符串，9 位小数（例如 `"12.500000000"`），解析与输出都是精确的十进制；
- 地址：XDAG 账户地址为 Base58Check（xdagj 格式）；区块可以用 xdagj 的 base64 区块地址或 hashlow 十六进制表示；
  EVM 地址为 `0x` 十六进制；
- 高度、数量等整数在 `xdag_*` 中按 xdagj 的习惯以字符串返回，在 `eth_*` 中按以太坊习惯以 `0x` 十六进制返回。

## xdag_*（与 xdagj 兼容）

| 方法 | 参数 | 说明 |
|---|---|---|
| `xdag_blockNumber` | — | 主块数（字符串） |
| `xdag_protocolVersion` | — | 节点版本 |
| `xdag_netType` | — | `mainnet` / `testnet` / `devnet` |
| `xdag_coinbase` | — | 节点地址 |
| `xdag_getBalance` | 地址或区块 | 账户余额或区块余额 |
| `xdag_getTotalBalance` | — | 节点自身账户的余额 |
| `xdag_getTransactionNonce` | 地址 | 下一个可用 nonce（包含交易池中待执行的交易） |
| `xdag_getRewardByNumber` | 高度 | 该高度主块奖励 |
| `xdag_getBalanceByNumber` | 高度 | 该高度主块的区块余额 |
| `xdag_getStatus` | — | 区块数、主块数、难度、供应量、是否同步完成、额外块数、交易池大小（算力字段固定为 `"0.0"`） |
| `xdag_getBlockByHash` / `xdag_getTransactionByHash` | 区块或地址, [页码, 每页条数] 或 [页码, 起始毫秒, 结束毫秒, [每页条数]] | 区块详情（引用、交易列表、分页）；参数为地址时返回账户信息与交易历史。每页默认 100、最多 500 条；此接口的分页最多覆盖最近 10 万条记录，更早的记录用 `xdag_getHistory` |
| `xdag_getBlockByNumber` | 高度, 分页参数同上 | 主块详情 |
| `xdag_getBlocksByNumber` | 数量（≤1000） | 最近的主块列表（简要） |
| `xdag_sendRawTransaction` | 512 字节区块的十六进制 | 提交旧式交易块。与 xdagj 相同，返回字符串：成功为区块地址，失败为 `INVALID_BLOCK <原因>` |
| `xdag_personal_sendTransaction` / `xdag_personal_sendSafeTransaction` | `{"to","value","remark"?,"from"?}`, 钱包密码 | 从节点钱包转账（需要节点加载钱包）。`value` 精确解析；发送方另付手续费，接收方收到 `value` 全额 |
| `xdag_syncing` | — | `{currentBlock, highestBlock, isSyncDone}` |
| `xdag_netConnectionList` | — | 连接列表（另含 `nova`、`score` 字段） |
| `xdag_getAverageFee` | — | 最低手续费 |

## xdag_*（新增）

| 方法 | 参数 | 返回 |
|---|---|---|
| `xdag_getChainInfo` | — | `network`、`novaActivationEpoch`、`chainId`、`epochSeconds`、`minGas`、`minNativeFee`、`minGasPrice`、`client` |
| `xdag_getHistory` | 地址或区块, [cursor], [limit 1–1000，默认 100] | 执行时记录的历史，新的在前。每条：`direction`（0 转出、1 转入、2 奖励/手续费收入、3 失败交易支付的手续费）、`hashlow`、`address`、`counterparty`、`amount`、`time`（毫秒）、`remark`、`height`、`status`（`applied` / `rejected` / `failed`）、`cursor`。翻页时把上一页最后一条的 `cursor`（形如 `"高度.序号"`）作为参数传入；传整数高度表示从该主块之前开始 |
| `xdag_sendNovaTransaction` | 原生转账编码的十六进制（格式见 DESIGN.md 3.3） | 交易哈希 |
| `xdag_getNovaTransaction` | 交易哈希（原生转账或 EVM 交易） | `{block, index, height, status, sender, fee, gasUsed}`；未执行时为 `null` |
| `xdag_getPendingTransactions` | — | 交易池：`[{hash, sender, nonce, inFlight}]` |
| `xdag_getPeers` | — | 同 `xdag_netConnectionList` |

构造原生转账最简单的方式是 `xdagd tx native`；自己实现时：

```text
body   = 0x01 | chain_id u64le | nonce u64le | to[20] | amount u64le (nano) | fee u64le (nano) | remark_len u8 | remark
digest = sha256d("XDAG/NOVA/TRANSFER/v1" || body)
sig    = secp256k1 可恢复签名（r 32 | s 32 | v 1，low-S）
tx     = body | sig
```

`nonce` = `xdag_getTransactionNonce` 的返回值；`chain_id` 与最低手续费来自 `xdag_getChainInfo`。

## eth_* / net_* / web3_*（EVM，Nova 激活后可用）

区块号即主块高度，一个"区块"中的交易是该主块执行的全部 EVM 交易。

| 方法 | 说明 |
|---|---|
| `web3_clientVersion`、`web3_sha3` | |
| `net_version`、`net_listening`、`net_peerCount` | |
| `eth_chainId` | Nova 链 ID（开发网 `0x7866`） |
| `eth_blockNumber`、`eth_syncing` | |
| `eth_gasPrice`、`eth_maxPriorityFeePerGas` | 最低 gas 价格 |
| `eth_feeHistory` | basefee 恒为 0 |
| `eth_getBalance`、`eth_getTransactionCount`、`eth_getCode`、`eth_getStorageAt` | 只支持最新状态（`latest` / `pending`） |
| `eth_call`、`eth_estimateGas` | 在最新状态上执行；估算用二分查找 |
| `eth_sendRawTransaction` | legacy（EIP-155）、EIP-2930、EIP-1559；不支持 blob（EIP-4844）与 EIP-7702 |
| `eth_getTransactionByHash`、`eth_getTransactionReceipt` | 回执含日志；交易的 `v/r/s` 字段为 0 |
| `eth_getBlockByNumber`、`eth_getBlockByHash`、`eth_getBlockTransactionCountByNumber` | `logsBloom` 为零，没有状态根 |
| `eth_getLogs` | 单次查询范围最多 10,000 个区块 |
| `eth_accounts` | 恒为空（节点不托管以太坊密钥） |

单位：EVM 内 1 XDAG = 10^18 wei；原生接口的金额单位是 nano（10^-9 XDAG），两边是同一个余额。
