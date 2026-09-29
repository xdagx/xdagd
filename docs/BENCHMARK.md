# 吞吐测试（TPS）

## 方法

`xdagd bench` 在单个进程内走完整的导入与执行路径（与节点运行时是同一套代码：并行预校验、`tryToConnect`、主块确认、执行、落盘），
不经过网络，也不等待真实的 64 秒 epoch（使用模拟时钟），测的是**单节点的处理能力**。

```bash
cargo build --release
./target/release/xdagd bench --txs 50000 --senders 500 --legacy
```

- **Nova 批量模式**：500 个发送方各发 100 笔原生转账，共 5 万笔，每个批量块最多 8192 笔（共 7 个批量块）。
  - A 准入：并行恢复签名 + 无状态检查（交易池的入口）；
  - B 构建批量块 → 校验 → 导入：从交易到 DAG 区块入库；
  - B 端到端：再加上主块确认与全部交易执行、历史与索引写入。
- **每笔交易一个区块（xdagj 的结构）**：同样的转账，每笔构造成一个 512 字节交易块（含 Nova 反垃圾工作量），
  逐块导入，再用链接块（每块 12 个引用）逐层汇聚，最后由主块执行。**执行引擎是本实现**，只有区块结构不同。
- 签名由钱包完成，不计入时间。

## 结果

测试机：4 vCPU / 5.9 GB 内存的共享服务器（测试时有其他生产负载，load average 约 1.3–1.7），release 构建（opt-level 3、thin LTO），rayon 4 线程。
三次运行的范围：

| 场景 | 吞吐 |
|---|---|
| Nova 准入（签名校验） | 36,000 – 45,000 tx/s |
| Nova 批量块构建 + 校验 + 导入 | 36,500 – 39,100 tx/s |
| **Nova 端到端（含执行）** | **12,600 – 16,800 tx/s** |
| 每笔一个区块：校验 + 导入 | 12,600 – 16,800 tx/s |
| **每笔一个区块：端到端（含链接块与执行）** | **1,780 – 2,190 tx/s** |

原始输出见本文末尾。

## 为什么批量模式快

1. **DAG 顶点数下降三到四个数量级**：一个批量块承载 8192 笔交易，而不是 8192 个区块。
   区块需要落盘、建立时间索引与校验和、被链接块引用、在主块执行时被深度优先遍历——这些成本按区块数计，而不是按交易数。
2. **更小的交易**：一笔原生转账约 124 字节（无备注），旧式交易块固定 512 字节，而且还要额外的链接块把它接入 DAG。
3. **并行校验**：载荷中所有交易的签名在进入共识锁之前用 rayon 并行恢复。
4. **批量提交**：导入流水线每批（最多 4096 个区块）只提交一次存储事务。
   （作为对照：逐块提交时，每笔一个区块的模式只有约 340 tx/s。）

## 解读与限制

- 这是**单节点处理能力**。真实网络中的吞吐还取决于带宽与区块传播：按每笔约 124 字节计，15,000 tx/s 约需 1.9 MB/s（约 15 Mbit/s）的广播带宽。
- **没有测量 xdagj 本身**（开发环境没有 Java）。表中"每笔一个区块"一行用的是本实现的执行引擎，
  它只说明区块结构带来的差别，不代表 xdagj 的实际数值。
- EVM 交易另有 gas 上限：每个主块（64 秒）最多执行 3 亿 gas（`main_gas_limit`），约合 470 万 gas/s，
  即每秒约 220 笔简单转账（21,000 gas）或约 90 笔 ERC-20 转账（约 52,000 gas）。原生转账不受 gas 上限约束。
  这个上限是可调的共识参数。
- 端到端时间包含历史索引写入（每笔交易写发送方与接收方两条历史记录）。

## 原始输出（最后一次测试）

```text
xdagd bench — 4 CPU threads (rayon), release build recommended

[Nova] 50000 native transfers from 500 senders (100 each), batches of up to 8192
  A admission (verify signatures):         40349 tx/s   (1.239s)
  B block build + verify + import:         39126 tx/s   (1.278s, 7 batch blocks)
  B end-to-end incl. execution:            14923 tx/s   (3.350s)

[block-per-transaction model, as in xdagj] 20000 transfers, one 512-byte block each
  verify + import tx blocks:               16796 tx/s   (1.191s)
  end-to-end incl. link blocks + exec:      2192 tx/s   (9.126s)

[Nova] ...
  A admission (verify signatures):         45438 tx/s   (1.100s)
  B block build + verify + import:         36512 tx/s   (1.369s, 7 batch blocks)
  B end-to-end incl. execution:            12634 tx/s   (3.958s)
[block-per-transaction model] ...
  verify + import tx blocks:               12646 tx/s   (1.581s)
  end-to-end incl. link blocks + exec:      1782 tx/s   (11.221s)

[Nova] ...
  A admission (verify signatures):         36196 tx/s   (1.381s)
  B block build + verify + import:         36612 tx/s   (1.366s, 7 batch blocks)
  B end-to-end incl. execution:            16785 tx/s   (2.979s)
[block-per-transaction model] ...
  verify + import tx blocks:               15380 tx/s   (1.300s)
  end-to-end incl. link blocks + exec:      1783 tx/s   (11.218s)
```
