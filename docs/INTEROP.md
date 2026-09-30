# 与 xdagj 的对接测试

xdagd 和一个**真实的 xdagj 0.8.4 节点**（从源码 `3d8f8271` 构建，未做任何修改）在同一台机器上对接，逐项比对两边的结果。
做了两轮（都在 2026-09-30）：

1. **全新的开发网**：两个节点从零开始互相同步、出块、转账（本文前半部分）；
2. **从真实主网快照开始的开发网**：把主网高度 4,240,061 的官方快照导入两个节点，先离线核对，再让两个节点在这份主网状态上继续运行（本文后半部分）。

复现方法见 [tools/interop/README.md](../tools/interop/README.md)。

> 两轮测试都没有连接主网。节点使用开发网身份（网络号 2、`HEAD_TEST` 类型的区块，主网节点不会接受），
> 并且运行在只允许访问本机回环地址的沙箱里。它们验证的是"两个实现对同一组数据得出同样的结果"；
> 真实的多节点网络环境、长时间运行都还没有验证（见文末）。

# 第一轮：全新的开发网

## 结论

| 项目 | 结果 |
|---|---|
| 钱包文件 | xdagd 生成的 `wallet.data` 被 xdagj 直接解锁，地址一致 |
| 握手与帧 | 双向握手成功；xdagj 忽略 xdagd 多声明的能力项 |
| xdagd 从 xdagj 同步 | 晚加入的 xdagd 通过 sums / 区块范围请求补齐，之后实时跟随 |
| xdagj 从 xdagd 同步 | 清空数据库的 xdagj 完全从 xdagd 节点同步回来，状态一致 |
| 主链 | 两边每个主块的哈希、余额、累计难度、状态全部相同（sha256d 阶段 55 个主块，RandomX 阶段 32 个主块） |
| xdagd 出的块 | xdagj 接受 xdagd 挖出的主块候选（`IMPORTED_BEST`），并成为两边的主块 |
| xdagd 构造的交易 | 奖励归集（区块余额 → 地址）、钱包转账：xdagj 接受并执行，两边余额、nonce 相同 |
| xdagj 构造的交易 | `xfertonew`（3 个输入的主交易）、钱包转账：xdagd 执行结果与 xdagj 相同 |
| 异常交易 | 余额不足（消耗 nonce、不转账）、nonce 跳号、nonce 过期：两边的区块状态、标志、手续费归属完全相同 |
| RandomX | 分叉 epoch、种子、切换 epoch 两边逐字节相同；xdagd 挖出的 RandomX 主块被 xdagj 接受；xdagj 生成的 RandomX 区块在两边算出的难度相同；跨种子切换、双方重启之后仍然一致 |
| 导出工具 | 在真实的 xdagj 数据库上运行；导出的状态与 xdagd 自己同步得到的状态逐项相同（见下） |
| 快照导入 | 导入 xdagj 导出的快照的 xdagd 节点，接上 xdagj 后继续同步，结果一致；RandomX 种子由主链重算，与实时节点相同 |
| RPC | 同一个区块在两边 `xdag_getBlockByHash` 的输出一致（xdagd 多出的字段除外） |

整个过程中 xdagj 的错误日志为空，xdagd 没有把 xdagj 的任何区块判为无效。
xdagj 一共记录了约 250 次来自对端或 RPC 的区块导入，其中 2 次在它重新同步时被暂时判为无效，原因是 xdagj 自身的一个问题（见"关于 xdagj 的发现"），随后被重新取回并接受。

### 状态逐项比对

两个节点在同一主块高度停下后：

- Java 导出工具从 **xdagj 的数据库**导出一份快照；
- `xdagd snapshot export` 从**一直通过 P2P 跟随的 xdagd 节点**导出一份快照；
- `xdagd snapshot diff` 比较两者。

```text
main height: 49 / 49
accounts: 4 / 4, 0 differ
main-chain index: 49 / 49 entries, 49 in common, 0 differ
blocks: 83 / 83, 83 in common of which 0 differ; 0 only in the first, 0 only in the second
the consensus state of the blocks and accounts both snapshots contain is identical
```

比较的内容包括每个区块的时间、标志（MAIN / MAIN_CHAIN / APPLIED / MAIN_REF / REF）、主块高度、累计难度、
最大难度链接、区块余额、手续费、ref，以及每个账户的余额和 nonce。也就是说，对 xdagj 保存的每一个区块，
xdagd 独立算出了同样的结果。RandomX 链上重复了同样的比对（29 个主块、44 个区块），同样没有差异。

### RandomX

xdagj 的开发网要到第 4096 个主块才进入 RandomX（约 3 天）。测试中把分叉高度设为 16、种子周期 8、滞后 2：
xdagj 一侧通过一个启动包装类设置它自己留给测试用的三个非 final 常量（xdagj 的代码没有改动），xdagd 一侧用只在开发网生效的配置项。

```text
xdagj: From block height:16, ... set fork time to:27980744      xdagd: forkEpoch 27980744
xdagj: Set switch time to 1aaf3c9 (= 27980745)                  xdagd: switchEpoch 27980745
种子（两边相同）: 10b74536e0e5a98827c14958b7dd78870c951c22c71915440000000000000000
第二个种子（高度 24，两边相同）: ee2b85d8c9f0668d1ab53d756f1f72604319bf67b41a04a9…，切换 epoch 27980754
```

xdagj 自己生成并用它的 RandomX 计算难度的区块，在 xdagd 上得到的累计难度相同（跨越种子切换的 5 个连续 epoch 全部一致）；
反过来，xdagd 用 RandomX 挖出的主块被 xdagj 接受，主链累计难度两边相同。

两边用的是同一份 RandomX v1.2.1 源码编译出来的库（原因见下面第 1 条），所以这项测试验证的是两边**怎么用** RandomX——
种子从哪个区块来、何时切换、哈希的输入是什么、结果怎么换算成难度——而不是 RandomX 算法本身（后者由官方测试向量覆盖）。

## 测试中发现并修复的问题

对接暴露了几处只有真实运行才能发现的问题：

1. **xdagj 的数据库目录名与内容是反的**：`Kernel` 把数据库按 `INDEX, BLOCK, TIME` 的顺序传给参数顺序为 `index, time, block` 的
   `BlockStoreImpl`，所以原始区块实际存放在名为 `TIME` 的目录里，名为 `BLOCK` 的目录里是时间索引。
   导出工具原先按名字读取，结果一个区块的数据都没读到。现在按内容的形状识别原始区块库，并且在发现区块既没有数据也没有快照信息时直接报错，
   不会再导出一份"看起来正常"的空壳。
2. **导入时推导交易状态**：因 nonce 跳号而被跳过的交易，和被拒绝的交易，在 xdagj 的标志里长得一样，区别只在有没有 ref。
3. **同步状态判断**：`is_synced` 从数据库里查链顶，而链顶通常是只在内存中的候选块。
4. **RPC 细节**：主块的交易列表里奖励出现两次；区块标志里多了 xdagj 从不设置的 0x80；查不到只在内存中的候选块（xdagj 可以）；
   `xdag_personal_sendTransaction` 的结果码与 xdagj 不同、缺少 `resInfo` 字段；"下一个 nonce" 把跳号之后的交易也算了进去。
5. **待发奖励只存在内存里**：节点重启后，重启前 16 个 epoch 内挖到的主块永远不会被发奖（xdagj 同样如此）。现在保存在 `rewards.json`。

另外新增了几个在对接中用得上的功能：`xdagd snapshot diff`（比较两份快照的共识状态）、`xdagd tx legacy`
（按 xdagj 的区块格式发送转账，可以直接发给 xdagj 节点的 RPC）、`xdag_getChainInfo` 里的 RandomX 状态。

## 关于 xdagj 的发现

- **xdagj 0.8.4 自带的 RandomX 库在 Ubuntu 22.04 上无法加载**：它依赖 GCC 13 的 libstdc++（`GLIBCXX_3.4.32`），而 22.04 只有 GCC 12 的。
  节点会在启动时抛出 `UnsatisfiedLinkError`。测试中用同一版本的 RandomX 源码另编了一个库放在 xdagj 的工作目录里。
- 没有矿池连接时，xdagj 对自己挖到的主块不发奖（日志：`This block is not produced by mining and belongs to the node`），
  奖励一直留在主块余额里，需要手动 `xfertonew`。
- 数据库目录名与内容相反（见上），RocksDB 的前缀配置因此也设在了错误的库上。
- **同步顺序影响区块有效性**（[BUGS.md](BUGS.md) 的 C6，这次在真实的 xdagj 上观察到）：xdagj 清库重新同步时，
  有两个交易块先于"给发送地址打钱"的那笔交易被执行之前到达，xdagj 以 `Address isn't exist` 把它们判为 `INVALID_BLOCK` 丢弃。
  这一次它们后来被主块引用，xdagj 重新请求并接受了，最终状态一致；但没有被引用的此类区块会永久缺失。
  xdagd 对这种情况是"暂缓、稍后重试"。

# 第二轮：从真实主网快照开始

使用的快照是 `snapshot-4240061-1aae42c0000.tar.gz`（351,189,792 字节，sha256 `2db141d3…17cb82`，主网高度 4,240,061）。
里面是 xdagj 节点升级时用 `--enablesnapshot` 载入的两个 RocksDB：`ADDRESS`（账户）和 `BLOCKS`（区块元数据）。

| 内容 | 数量 |
|---|---|
| 账户 | 11,768 个，合计 738,692,452.583338452 XDAG |
| 区块 | 4,497,937 个，区块余额合计 576,975,211.216659258 XDAG |
| 其中只带公钥的区块 | 4,472,342 个 |
| 其中带原始区块数据的 | 25,595 个 |
| 主链索引 | 高度 2,232,192 到 4,240,061 连续，共 2,007,870 个主块 |

## 结论

| 项目 | 结果 |
|---|---|
| 快照转换 | 导出工具新增的 `--snapshot` 模式直接把快照转成 XSNP；结果与**真实 xdagj 节点载入这份快照之后的数据库**逐项相同（0 处差异） |
| 导入 xdagd | 73 秒，内存峰值 411 MB，数据库 2.3 GB；再导出后与转换结果逐项相同 |
| 区块解析与验签 | 快照里全部 25,595 个原始区块，xdagj 和 xdagd 解析出的每一个字段、验签结果完全相同 |
| 主链结构 | 2,007,612 对相邻主块全部符合 xdagd 实现的主链规则（另有 257 对因快照缺少链接信息无法检查） |
| 难度（sha256d） | 2,031 个区块的累计难度由 xdagd 重新算出，与主网记录的相同 |
| 难度（RandomX） | **2,112 个真实主网主块**，跨越 228 个不同的种子，xdagd 算出的难度与主网记录的全部相同 |
| 两个节点继续运行 | 35 分钟内在主网状态之上新出 29 个主块（4,240,062 到 4,240,090），两边每个主块的哈希、余额、累计难度相同 |
| RandomX（运行中） | 种子取自真实主网主块；xdagj 自己生成的候选块在两边算出的难度相同；xdagd 挖出的主块被 xdagj 接受 |
| 余额查询 | 全部 11,768 个主网账户和抽查的 4,000 个区块，两边 `xdag_getBalance` 的回答相同，并与快照一致 |
| 转账 | 向真实存在的主网地址转账、由 xdagj 钱包发出的转账、余额不足的转账：两边的余额、nonce、区块状态相同 |
| 伪造的花费 | 用别人的密钥去花主网区块里的余额（试了 1,530 万、2,021 万 XDAG 等 4 个区块）：两边都拒绝 |
| 停机恢复 | xdagd 停机 3.5 分钟后重启，从 xdagj 补齐区块，结果一致 |
| 最终状态 | 两个节点停在同一高度后逐项比对：11,771 个账户、4,497,991 个区块、2,007,899 条主链索引，**0 处差异** |

整个过程中 xdagj 的错误日志为空，xdagd 没有警告或错误。

## 离线核对

不启动任何节点，只用快照文件本身。`tools/interop/snapshot_offline.sh` 把下面几步串在一起，换一份新的快照可以直接重跑。

**1. 转换。** `XdagjExporter --snapshot <SNAPSHOT 目录> --height 4240061` 读取快照，写出 xdagj 载入它之后应有的状态。
转换工具打印的三个合计数与 xdagj 载入同一份快照时自己打印的完全相同：

```text
amount in address: 738692452.583338452
amount in blocks:  576975211.216659258
All amount:        1315667663.799997710
```

之后让真实的 xdagj 节点（开发网身份）载入这份快照，用导出工具导出它的数据库，再与直接转换的结果比较：

```text
accounts: 11768 / 11768, 0 differ
main-chain index: 2007870 / 2007870 entries, 2007870 in common, 0 differ
blocks: 4497938 / 4497937, 4497937 in common of which 0 differ; 1 only in the first, 0 only in the second
```

多出的那一个区块是 xdagj 首次启动时为自己创建的地址块。

**2. 审计（`xdagd snapshot verify`，新增）。** 凡是能从文件本身重新算出来的，都重新算一遍并与文件里记录的值比较：

```text
main chain: 2007870 entries, heights 2232192..4240061
  consecutive main blocks: 2007612 linked by the max-difficulty path with growing difficulty and epoch, 0 wrong;
      not checkable: 128 (no link recorded), 129 (path leaves the file)
RandomX: 491 of the 660 seeds since the fork can be derived from the main blocks in the file
blocks with data: 25595 parsed and hashed, 0 wrong
  difficulty: 2105 recomputed from all their links, 2038 from their max-difficulty link, 0 wrong;
      not checkable: 21395 (none recorded), 38 (linked blocks not in the file), 0 (seed blocks not in the file)
  of these, 2116 main-block candidates were hashed with RandomX under 228 seeds
```

这是目前最有分量的一项证据：这些主块是真实矿工在 2022 到 2026 年间挖出来的，难度是主网节点当时算出并记录下来的。
xdagd 用主网参数（分叉高度 1,540,096、周期 4,096、滞后 128）从主链上取出种子，对每个区块重新做 RandomX 哈希，
得到的累计难度与记录值逐一相等。第一轮测试里"两边用的是同一份 RandomX 源码"的保留意见因此不再成立——
这里比对的对象是主网自己留下的数据。

**3. 区块解析的差分测试。** 把快照里带原始数据的 25,595 个区块分别交给 xdagj 的 `Block` 类和 xdagd 的解析器，
各自输出哈希、时间、类型、手续费、每个输入输出的类型/目标/金额、公钥、签名、nonce、验签通过的公钥和 sha256d 难度，
两份输出（各 9.9 MB）**逐字节相同**。其中有 160 个旧式交易块，98 个验签通过、62 个不通过，两边的判断一致。

**4. 导入再导出。** `xdagd snapshot import` 载入 450 万个区块用时 73 秒；`snapshot export` 32 秒；
`snapshot diff` 比较两份各 450 万区块的快照用时 10 秒、内存 217 MB，结果为 0 处差异。

## 两个节点在主网状态上继续运行

xdagj（开发网身份，`--enablesnapshot true 4240061 1aae42c0000`）和 xdagd（导入同一份状态）互相连接后：

- 两边的 RandomX 状态相同。xdagd 由主链重算的结果：分叉 epoch 27,976,129，种子来自主块 4,235,136 和 4,239,232
  （`40af4b74…b11933`、`10928f28…352358`），切换 epoch 27,972,004 和 27,976,130。
- 35 分钟内新出 29 个主块：27 个由 xdagd 的内置矿工用 RandomX 挖出并被 xdagj 接受（`IMPORTED_BEST`），
  2 个是 xdagd 停机期间 xdagj 自己出的，xdagd 重启后补齐。xdagj 自己生成的候选块在两边算出的难度相同。
- xdagd 的奖励归集（从主块余额转到地址）被 xdagj 执行，两边余额相同。
- 转账：xdagd 钱包 → 真实主网地址 `Dqd43Pai…`（27,130,563.096122425 → 27,130,568.096122425，两边相同）；
  xdagd 钱包 → xdagj 钱包 → 另一个主网地址；第三个密钥经 xdagj 的 RPC 发出转账；余额不足的转账在两边都是 `Rejected`（标志 0x18）并消耗 nonce。
- 这些只是测试网络里的账本变化。转出的币是测试网络里新挖出来的，主网上不存在；没有任何主网账户的密钥，也就动不了任何主网余额。

结束时两个节点停在高度 4,240,090，各自导出后比对：

```text
main height: 4240090 / 4240090
accounts: 11771 / 11771, 0 differ
main-chain index: 2007899 / 2007899 entries, 2007899 in common, 0 differ
blocks: 4497991 / 4497991, 4497991 in common of which 0 differ; 0 only in the first, 0 only in the second
```

全部余额之和从 1,315,667,663.799997710 变为 1,315,668,591.799997710，正好多出 29 × 32 XDAG 的出块奖励。

## 主网数据里观察到的现象

这些是快照本身的内容，与 xdagd 无关，供 xdagj 的开发者参考。原因只能从历史交易里查，快照里没有。

1. **余额总和比累计出块奖励多 29,839.8 XDAG。** 高度 4,240,061 为止的出块奖励合计 1,315,637,824 XDAG，
   快照里全部余额合计 1,315,667,663.799997710 XDAG，多出 29,839.799997710（约占 0.0023%）。
2. **两个主块的余额是负数**：高度 3,720,009（2025-09-06，`TLLcEWqg…`）和 3,854,292（2025-12-14，`k3GVDFvE…`），都是 −0.642 XDAG。
   两个块各自收入 64.2（奖励 64 + 手续费 0.2），而 0.642 正好是 64.2 的 1%：如果一笔交易里对同一个主块引用了两次、每次取走 1%，
   结果就是这个数。这与 [BUGS.md](BUGS.md) C2（同一来源区块的重复输入使余额变负）描述的情形在数值上吻合。
3. **三笔矿池发奖交易自己成了主块**（高度 3,039,096、3,122,371、3,132,645，2024 年 4 到 6 月），各得到 64 XDAG 奖励，至今留在区块余额里。
   它们不是挖出来的，当时 xdagj 对交易块也按哈希计算难度，那个 epoch 又没有别的候选块。这是 BUGS.md C1 在主网上自然发生的例子；
   xdagj 后来改为"带输入的区块难度记为 1"。
4. 另有 11 个主块（2024 年初）带 `MAIN` 标志却没有 `APPLIED` 标志，余额为 0。
5. 192 万个早期区块（C 版节点时代）没有时间戳（记为 0），47.5 万个带 `MAIN` 标志的早期区块没有高度；
   区块标志字段的高位带有无意义的数据（转换时只取低 8 位，xdagj 的逻辑也只用低 8 位）。
6. 5.77 亿 XDAG 仍然存放在旧式的"区块余额"里，绝大部分在 3.7 万个非主块上，其中最大的一个区块有 2,021 万 XDAG。
7. 16 个作为验签材料保存的原始区块，数据的哈希与它对应的区块地址不符（都是早期的钱包地址块，合计约 3,100 XDAG）。
   两个实现都只用这份数据来验签，行为一致。

## 第二轮测试中发现并修复的问题

真实规模的数据暴露了开发网上看不出来的问题：

1. **导入极慢。** 区块按哈希存放，而快照里的顺序对 xdagd 来说是随机的；数据库每提交一批都要重写几乎所有页面。
   导入进行到 41% 时已经用了 8 分钟、写了 13.7 GB，并且越来越慢。现在先对数据做外部排序再顺序写入（`BulkLoader`），全程 73 秒。
2. **`systemctl stop` 之后重启要等半分钟。** 节点只处理 Ctrl-C，收到 SIGTERM 会直接退出；而且节点与网络模块互相持有对方，
   数据库从未被正常关闭，下次启动都要整库修复（2.3 GB 的库约 28 秒）。现在 SIGTERM 会正常收尾，重启只要 0.4 秒。
3. **`snapshot diff` 的内存。** 原来把两份快照都读进内存，主网规模需要约 1.6 GB。现在只保留每个区块 32 字节的摘要，流式比较（217 MB）。
4. **RandomX 分叉时间的取法。** xdagj 从快照启动后，把"快照高度向下取整到 4096 的那个主块"当作分叉起点，更早的时间戳一律按 sha256d 计分。
   xdagd 原来取的是"已知的最早主块"，对时间戳很旧的候选块会与 xdagj 算出不同的难度。现在与 xdagj 取同一个点。
5. **RPC**：主块列表里，从快照继承的主块类型应为 `Main`（xdagd 原来写成 `Snapshot`）；`xdag_sendRawTransaction` 的错误信息里混入了内部格式。
6. 新增 `xdagd tx from-block`：花费区块余额（相当于 xdagj 的 `xfertonew` 发出的交易），也用来做上面"伪造的花费"测试。

## 没有覆盖的内容

- **主网的实时数据流**：两轮测试都没有连接主网。快照是某一时刻的状态；主网上正在产生的区块、真实的交易流量、
  多个 xdagj 节点之间的交互都还没有经过 xdagd。这需要按 [MIGRATION.md](MIGRATION.md) 的阶段 0 第 2 步，在 xdagj 开发者同意之后进行。
- **快照之前的历史**：快照只带有 25,595 个区块的原始数据，其余 447 万个区块只有元数据，无法重新验证。
- **长时间运行**：主网状态上只连续运行了 35 分钟（29 个主块），没有跨越 RandomX 种子切换（每 4096 个主块一次，约 3 天）。
  种子切换在第一轮的缩短参数下测试过。
- **多节点、公网环境**：只有一个 xdagj 节点，全部在本机回环地址上；没有测试高延迟、丢包。
- **xdagj 的主网身份**：测试中 xdagj 以开发网身份运行，RandomX 参数通过它留给测试用的常量设成主网的值；
  它在主网身份下读取配置的那部分代码没有运行过（公式相同，常量的来源不同）。
- **矿池接口**：没有用真实的矿池软件连接 xdagd 的 WebSocket 接口。
- **旧钱包的区块余额**：xdagd 能验证并执行花费区块余额的交易，也能用 `tx from-block` 发出；
  但还没有像 xdagj `xfertonew` 那样"自动找出钱包名下所有旧区块并归集"的命令。
- **独立的安全审计**：上面都是功能上的一致性测试，不能代替审计。
