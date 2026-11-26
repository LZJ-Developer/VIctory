# VIctory · RTX 4060 Laptop 性能交接

VIctory 是把 Xbox Series 游戏的系统与图形调用接到 Windows 和 Direct3D 12 的兼容层。这个分支保留了 2026-11-26 至 27 日在一台 8 GiB 独显笔记本上实测的源码。它解决了一个具体问题：小缓冲先占用显存预算，一块高频读写的大缓冲曾落到系统内存，GPU 反复跨总线取数。给大缓冲留出空间后，同序 **19 次 GPU 计算的采样中位数由 117.36 降至 3.98 ms**。旧组专采超过 50 ms 的慢列表，因此这个比值只适用于捕获的样本。

**整帧还差 55.87 ms 才到平均 30 FPS。** 另一次暖缓存实机运行连续采了 240 个呈现间隔，平均 89.20 ms，即 **11.21 FPS**，最慢的 1% 附近达到 **325.20 ms（P99）**。下一项最值得做的实验，是让 CPU 更早准备好 GPU 要用的资源索引，检查下一帧能否更早提交。

## 已经测到了什么

| 问题 | 做了什么、测到多少 | 对下一项优化的判断 |
|---|---|---|
| GPU 为什么在一组固定计算上耗时过长？ | 在 768 MiB 缓冲池中为大缓冲预留最多 256 MiB。按顺序、着色器标识和调度尺寸匹配 19 次计算，耗时之和的中位数为 117.363 → 3.975 ms，旧组 16 份、新组 24 份。 | 优先让反复访问的数据驻留显存。这个数字描述采到的 GPU 工作段；整帧用呈现间隔单独计时。 |
| Windows 解除映射拖住了多少时间？ | 一次 511.86 ms 的清页批次内，840 次 Unmap 占 439.16 ms，恢复剩余映射占 54.91 ms。固定请求顺序做 6 对原生回放，合并相邻清页只省 **0.079 ms**（配对中位数）。四轮游戏 B/A/A/B 各 240 个间隔，匹配到的同负载热区清页慢帧为 **0 对**。 | 真实慢帧省时仍未知。独立慢批的 169 个线程状态样本中，162 个在等推锁，0 个处于 Ready；下一次追踪持锁方和唤醒位置。 |
| 读取纹理指纹能绕过原来的视图吗？ | 在 16 GiB 窗口固定同一映射世代，从游戏地址与后备页各读一次。5,956 次中 705 次完整可比，字节不一致 0 次，5,251 次因锁忙未判定。452 次跨页调用中，27 次可判定。在全部 705 次可比调用中，700 次纯用后备页、5 次同时覆盖后备页和空洞。 | 双读仍执行原路径，实减 Map/Unmap **0/0 次**。这段指纹计时只占 0.170 ms/呈现间隔；消除映射还得覆盖其他读者。 |
| GPU 和 CPU 在等谁？ | 在另一组较重的 16 个热间隔里，DIRECT 队列工作段平均 47.96 ms，到下一次发送前的空档平均 89.11 ms。这 16 个间隔累计等索引扫描 144.68 ms（平均 9.04 ms/间隔），其中 144.27 ms 与扫描区间重叠。 | 先测试提前请求扫描能隐藏多少等待。在另行测量的 240 个呈现间隔里，152 个与映射批完全没有重叠，其中 148 个仍超过 33.33 ms。 |
| 值得再做一次全量回读去重吗？ | 固定输入回放中，跳过一次内容相同的 20.07 MiB 上传，GPU 复制段配对中位节省 1.639 ms。源数据改 1 字节后，无条件跳过即产生错误。读回分块从 64 改为 4 KiB，在一份前后像上只少传 92 KiB。 | 优先寻找便宜的版本证据。CPU 直接读 GPU_UPLOAD 的同样大小缓冲，本机实测为 435.20 ms，缓存读回缓冲为 1.45 ms。 |

硬件为 i7-12800HX、32 GB RAM、RTX 4060 Laptop 8 GiB。两臂沿用 Series S 配置和 1920×1080 宿主呈现设置，没有主动降低分辨率。逐帧内部渲染尺寸尚未采集。帧率按宿主两次呈现之间的墙钟计算，30 FPS 的预算是 1000/30 = 33.33 ms。表中各项沿用各自的测量窗口。[实验数据、样本数与方法](docs/benchmarks/results.json)

## 两种复核入口

**只复核帧率，不需要游戏或显卡。** 以下命令在 PowerShell 7 中读取仓库内的 240 个相对时间间隔，输出 FPS、P50/P95/P99、超预算帧数，以及有多少个间隔和映射批次重叠。对应结果为 11.2108 FPS、234/240 超过 33.33 ms。[公开样本](docs/benchmarks/present-window.json)

```powershell
git clone --single-branch https://github.com/LZJ-Developer/VIctory.git
cd VIctory
pwsh -NoProfile -File ./scripts/Measure-PresentWindow.ps1
```

**在自己的 Windows 机器上对照显存分配策略。** 运行需要 Windows x64、MSVC C++ 构建工具、Windows SDK、仓库固定的 Rust 1.98.1，以及自备游戏文件。以下命令在克隆目录的 Developer PowerShell 中执行。日志中的 `size-tiered card buffer admission enabled` 表示新策略已启用；随后 `size-tiered allocation outcome for observed buffer` 的 `local` 字段给出目标缓冲的分配结果。

```powershell
cargo build --locked --release --features diagnostics -p victory --bin victory
$env:VICTORY_GAME_DIR = Read-Host '输入本机游戏文件所在目录'
$env:VICTORY_CARD_BUFFERS_MB = '768'
$env:VICTORY_LOG = 'info'
$env:VICTORY_HOT_BUFFER_PRIORITY = '1'
.\Play-VIctory.cmd pareto-residency
```

把最后一个开关改为 `0` 后重新启动，即可在相同池预算下运行对照。每轮复用同一存档、镜头路线和预热状态，同时记下每帧 draw、dispatch 和请求页数，便于识别可比的慢帧。[显存分配实现](crates/victory-gpu/src/shadow.rs) · [Windows 映射实现](crates/victory-hle/src/modules/xmem.rs) · [双路取样实现](crates/victory-gpu/src/fingerprint_dual.rs)

## 社区接力的第一项实验

**只前移描述符扫描请求。** 描述符告诉 GPU 要读哪块缓冲或纹理。当前提交线程可能到准备发送时才等索引扫描结束。候选改法是在游戏命令刚入队时请求扫描，让后台线程利用排队时间工作；如果此后有相关写入，提交线程仍要等写入之后才开始的扫描轮次。Belobug 分支保留了这个实现，也同时改了日志锁分片。把两个改动拆开，才能给提前扫描单独定价。[本地同步代码](crates/victory-gpu/src/descriptor.rs) · [上游设计和轮次契约](https://github.com/Belobug/VIctory-Belobug-AMD/blob/cd8dfc06ace7ccb91a19431e74bc2d0de2a10145/crates/victory-gpu/src/port_journal.rs#L5-L53)

验收时在同一构建内切换 A/B，分别报告同负载呈现间隔、提交线程等待和 GPU 时间，同时核对相同纹理代次的输出。若提交等待减少而呈现没有提前，记录这段等待转移到了哪里。复现报告附上提交号、GPU/驱动、内部与输出分辨率（可观测者）、样本数、P50/P99 和工作量匹配数即可。[提交性能报告](https://github.com/LZJ-Developer/VIctory/issues/new?template=performance.yml) · [贡献指南](CONTRIBUTING.md)

本分支沿用 VIctory 及其社区贡献者的代码和许可，从公开可取的 `f4b48ad` 源码提交派生。构建和启动输入的改动共 18 个 Rust 文件。版权及格式表例外沿用上游记录。[上游基线](https://github.com/avatarmanz/VIctory-7900XTX/commit/f4b48ad766ddbec1a1867022cd42f7645b0dd55e) · [许可证](LICENSE) · [上游资产约定](docs/asset-policy.md)

11 月 27 日交接时的源码和数据保存在[实测快照](https://github.com/LZJ-Developer/VIctory/tree/3a47dccf8558e774506d44ceedf15b4465a73e63)。需要此前 DexNom 安装器的读者可查阅[原默认版本](https://github.com/LZJ-Developer/VIctory/tree/6f866cdbcb158d5fdf328a974db5154670ef814a)。
