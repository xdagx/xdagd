# xdagj ↔ xdagd 对接测试的复现方法

在一台机器上同时运行一个真实的 xdagj 开发网节点和 xdagd，互相同步、出块、转账，并比对结果。
测试结果见 [docs/INTEROP.md](../../docs/INTEROP.md)。

两个节点都作为带内存上限的临时 systemd 单元运行，网络访问被限制在回环地址，
所以测试既不会占满机器，也不会连到任何真实网络（需要 root 和 cgroup v2）。

## 准备

1. **JDK 21 与 Maven**（解压到任意目录即可，不需要安装到系统）。
2. **构建 xdagj**：

   ```bash
   git clone https://github.com/XDagger/xdagj && cd xdagj      # 测试使用的提交：3d8f8271 (0.8.4)
   mvn -B -DskipTests -Dlicense.skip=true package              # 得到 target/xdagj-0.8.4-executable.jar
   ```

   `pom.xml` 里还列着已经停止服务的 jcenter，构建时可以用 Maven 的 `settings.xml` 把所有仓库镜像到 Maven Central。
3. **构建 xdagd**：`cargo build --release`。
4. **工作目录**：

   ```bash
   export IT=/path/to/workdir XDAGD=/path/to/xdagd JAVA=/path/to/jdk/bin/java
   source tools/interop/interop.sh
   interop_setup                                   # 生成两边的配置文件
   cp xdagj/target/xdagj-0.8.4-executable.jar xdagj/src/main/resources/log4j2.xml $IT/xdagj-node/
   ```

5. **钱包**：xdagj 首次启动要交互式地创建钱包。用 xdagd 生成一个同格式的钱包文件给它用（这本身也是一项兼容性检查）：

   ```bash
   XDAG_WALLET_PASSWORD=interop-pw $XDAGD --network devnet --datadir $IT/walletgen wallet create
   cp $IT/walletgen/wallet/wallet.data $IT/xdagj-node/devnet/wallet/
   XDAG_WALLET_PASSWORD=xdagd-pw $XDAGD --config $IT/xdagd-node/interop.toml wallet create      # xdagd 自己的钱包
   ```

6. **RandomX 库（仅 Ubuntu 22.04 等 libstdc++ 早于 GCC 13 的系统）**：xdagj 自带的库加载不了。
   用 xdagd 已经编译好的 RandomX 静态库链接一个共享库，放到 xdagj 工作目录的 `native/` 下（类路径里 `.` 在 jar 之前，所以会优先用它）：

   ```bash
   O=$(ls -d target/release/build/xdag-randomx-*/out | head -1)
   mkdir -p $IT/xdagj-node/native
   g++ -shared -O2 -o $IT/xdagj-node/native/librandomx_linux_x86_64.so -Wl,-soname,librandomx.so -Wl,-z,noexecstack \
       -Wl,--whole-archive $O/librandomx_cpp.a $O/librandomx_c.a $O/librandomx_ssse3.a $O/librandomx_avx2.a -Wl,--no-whole-archive -lpthread
   ```

## 运行

```bash
start_xdagj          # 先让 xdagj 自己出几个主块
start_xdagd          # xdagd 晚加入：先补块，再实时跟随，并开始出块
python3 tools/interop/compare.py 10201 <地址>...     # 比对主链与余额
```

开发网的 epoch 是 64 秒，主块在 2 个 epoch 后确认，奖励在 16 个 epoch 后发放，所以完整跑一遍大约需要一小时。

常用操作：

| 目的 | 命令 |
|---|---|
| xdagd 钱包转账 | `rpcd xdag_personal_sendTransaction '[{"to":"<地址>","value":"100.5","remark":"x"},"xdagd-pw"]'` |
| xdagj 钱包转账 | `rpcj xdag_personal_sendTransaction '[{"from":"<地址>","to":"<地址>","value":"1","remark":"x","nonce":"<n>"},"interop-pw"]'` |
| xdagj 把区块余额转到地址 | `python3 tools/interop/telnet.py xfertonew`（区块需早于 32 个 epoch） |
| 任意 nonce / 金额的转账 | `$XDAGD tx --url http://127.0.0.1:10101 --key <私钥文件> legacy <地址> <金额> --nonce <n>` |
| xdagj 状态 | `python3 tools/interop/telnet.py stats`、`... state`、`... net -l` |
| 内存占用 | `interop_mem` |

## 导出工具与状态比对

```bash
stop_xdagd; stop_xdagj                         # 在一个 epoch 的中间停，两边才会停在同一高度
javac -proc:none -cp $XDAGJ_JAR -d $IT/exporter-out tools/xdagj-exporter/src/XdagjExporter.java
$JAVA -cp $XDAGJ_JAR:$IT/exporter-out XdagjExporter --store $IT/xdagj-node/devnet/rocksdb/xdagdb --network devnet --out j.xsnp
XDAG_WALLET_PASSWORD=xdagd-pw $XDAGD --config $IT/xdagd-node/interop.toml snapshot export d.xsnp
$XDAGD snapshot diff j.xsnp d.xsnp             # xdagj 数据库里的状态 与 xdagd 自己算出的状态
```

## RandomX 阶段

xdagj 的开发网在第 4096 个主块才分叉到 RandomX。要在几分钟内到达，两边都用缩短的参数从一条新链开始：

```bash
interop_randomx_config                                        # 给 xdagd 的配置加上 [randomx] 一节
javac -proc:none -cp $XDAGJ_JAR -d $IT/wrapper tools/interop/InteropMain.java
start_xdagj_rx; start_xdagd                                   # 两边都用全新的数据目录
IT=$IT python3 tools/interop/own_blocks.py 10                 # xdagj 自己生成的区块在两边的难度
rpcd xdag_getChainInfo                                        # xdagd 的分叉 epoch 与种子
grep -E "fork time|switch time|Next Memory Seed" $IT/xdagj-node/logs/xdag-debug.log     # xdagj 的（种子是打印值的字节反序）
```

`InteropMain` 只是在启动 xdagj 之前设置它的三个测试用常量，xdagj 本身没有改动。
RandomX 阶段 xdagj 会为每个种子分配两份 256 MB 的缓存（两个种子同时存在时约 1 GB）。

## 从主网快照开始的开发网

把一份真实的主网快照载入两个节点，在这份状态上做同样的对接测试。节点仍然是开发网身份（网络号 2、`HEAD_TEST` 区块），
并且运行在只能访问回环地址的沙箱里，**不会连接主网，主网节点也不会接受它们的区块**。
没有任何主网账户的密钥，所以动不了任何主网余额；能花的只有测试网络里新挖出来的币。

快照是 xdagj 节点升级时载入的 `SNAPSHOT` 目录（`ADDRESS` 和 `BLOCKS` 两个 RocksDB），文件名里是高度和时间，
例如 `snapshot-4240061-1aae42c0000.tar.gz`。下面用 `H=4240061`、`T=1aae42c0000`。

### 离线核对（不启动节点，约十分钟）

```bash
tar xzf snapshot-$H-$T.tar.gz                                   # 得到 SNAPSHOT/
cargo build --release -p xdag-chain --example blockdump
XDAGJ_JAR=$XDAGJ_JAR XDAGD=$XDAGD tools/interop/snapshot_offline.sh ./SNAPSHOT $H $IT/offline
```

脚本依次：用导出工具转换快照；`xdagd snapshot verify` 审计（重算带数据的区块的哈希、签名、难度——包括用主网参数做 RandomX——
并检查主链索引是不是一条链）；导入一个新的数据库、再导出、与转换结果逐项比较；把快照里所有带数据的区块分别交给 xdagj 和 xdagd 解析，
比较两边的输出。任何一步不符都会以非零状态退出。需要约 4 GB 磁盘、1 GB 内存。

### 两个节点在主网状态上运行

先按"准备"一节建好两个节点的目录、钱包和 RandomX 库（用全新的 `$IT`），并按"RandomX 阶段"一节编译 `InteropMain`
（这里用它把 xdagj 开发网的 RandomX 参数设成主网的：分叉高度 1540096、周期 4096、滞后 128）。
xdagj 这时要同时保留两个种子、各两份 256 MB 的 RandomX 缓存，首次启动时连同载入快照约占 1.7 GB 内存，之后约 1.3 GB；xdagd 约 0.4 GB。

```bash
# xdagj：快照放到它的数据目录里（它会以读写方式打开，所以用副本）
mkdir -p $IT/xdagj-node/devnet/rocksdb/xdagdb && cp -r ./SNAPSHOT $IT/xdagj-node/devnet/rocksdb/xdagdb/
# xdagd：转换成开发网身份的 XSNP 并导入
java -Xmx1g -cp $XDAGJ_JAR:$IT/exporter-out XdagjExporter --snapshot ./SNAPSHOT --height $H --network devnet --out $IT/devnet.xsnp
interop_snapshot_config                                         # interop.toml：主网的 RandomX 参数
XDAG_WALLET_PASSWORD=xdagd-pw $XDAGD --config $IT/xdagd-node/interop.toml snapshot import $IT/devnet.xsnp

start_xdagj_snapshot $H $T        # 首次启动载入快照约 3 分钟；它打印的三个合计数应与导出工具打印的相同
start_xdagd
python3 tools/interop/compare.py                                # 最新的 100 个主块
python3 tools/interop/compare_state.py $IT/devnet.xsnp         # 全部账户和抽样区块的余额：xdagj、xdagd、快照三方一致
```

两个节点都从快照里的主块取 RandomX 种子（`rpcd xdag_getChainInfo` 可以看到 xdagd 取到的）。之后的操作与前面各节相同：转账、
`own_blocks.py`、停机后导出比对。另外可以验证"别人的余额花不了"：

```bash
$XDAGD tx --url http://127.0.0.1:10101 --key <任意私钥文件> from-block <主网区块地址> <收款地址> 1000
# 两个节点都应回答 INVALID_BLOCK ... input can't be used
```

停机比对时，xdagj 的数据库有 450 万条区块记录，导出约两分半钟；`snapshot diff` 约十秒。

### 单独比较区块解析

`snapshot_offline.sh` 的最后一步也可以单独用在任何一批原始区块上（512 字节一条、首尾相接的文件）：

```bash
python3 tools/interop/xsnp_blocks.py state.xsnp blocks.dat                       # 从 XSNP 里取出带数据的区块
javac -proc:none -cp $XDAGJ_JAR -d out tools/interop/BlockDump.java
java -cp $XDAGJ_JAR:out BlockDump blocks.dat xdagj.txt                           # xdagj 的解析结果
target/release/examples/blockdump blocks.dat xdagd.txt                           # xdagd 的解析结果
cmp xdagj.txt xdagd.txt
```

每个区块一行：哈希、时间、类型、手续费、每个输入输出的类型/目标/金额、公钥、签名位置、输出签名、nonce、验签通过的公钥、sha256d 难度。

## 结束

```bash
stop_xdagd; stop_xdagj
```
