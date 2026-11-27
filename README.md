# VIctory · RTX 4060 Laptop 性能交接

VIctory 是把 Xbox Series 游戏的系统与图形调用接到 Windows 和 Direct3D 12 的兼容层。这个分支保留了 2026-11-26 至 27 日在一台 8 GiB 独显笔记本上实测的源码。它解决了一个具体问题：小缓冲先占用显存预算，一块高频读写的大缓冲曾落到系统内存，GPU 反复跨总线取数。给大缓冲留出空间后，同序 **19 次 GPU 计算的采样中位数由 117.36 降至 3.98 ms**。旧组专采超过 50 ms 的慢列表，因此这个比值只适用于捕获的样本。

**整帧收益仍待同负载验证。** 较早的一次暖缓存测试记录了 240 个呈现间隔，平均 89.20 ms，即 **11.21 FPS**，距 30 FPS 的平均预算还差 **55.87 ms/帧**。11 月 27 日的四轮诊断各取 240 个间隔，各轮为 **6.22–8.16 FPS**。实验组的 383 次热缓冲比较全部省去了 CPU 暂存写入，两组之间匹配到同负载慢帧 **0 对**。下一项实验追踪两组 mesh/pixel 着色器。按着色器标识配对，从游戏首次绑定到宿主就绪日志各相隔约 0.9 秒，期间游戏已呈现两帧。还需完整管线键才能定位受影响的绘制。[四轮结果、管线时间线与接手步骤](docs/benchmarks/2026-11-27-warm-pso.md)

## 已经测到了什么

| 问题 | 做了什么、测到多少 | 对下一项优化的判断 |
|---|---|---|
| GPU 为什么在一组固定计算上耗时过长？ | 在 768 MiB 缓冲池中为大缓冲预留最多 256 MiB。按顺序、着色器标识和调度尺寸匹配 19 次计算，耗时之和的中位数为 117.363 → 3.975 ms，旧组 16 份、新组 24 份。 | 优先让反复访问的数据驻留显存。这个数字描述采到的 GPU 工作段；整帧用呈现间隔单独计时。 |
| Windows 解除映射拖住了多少时间？ | 一次 511.86 ms 的清页批次内，840 次 Unmap 占 439.16 ms，恢复剩余映射占 54.91 ms。固定请求顺序做 6 对原生回放，合并相邻清页只省 **0.079 ms**（配对中位数）。 | 后续一帧的 285.07 ms 映射等锁期间，游戏自身至少有 227.91 ms 在 GPU 上执行。线程等锁时长只有与提交和呈现对齐后，才能用于估计整帧收益。 |
| 读取纹理指纹能绕过原来的视图吗？ | 在 16 GiB 窗口固定同一映射世代，从游戏地址与后备页各读一次。5,956 次中 705 次完整可比，字节不一致 0 次，5,251 次因锁忙未判定。452 次跨页调用中，27 次可判定。在全部 705 次可比调用中，700 次纯用后备页、5 次同时覆盖后备页和空洞。 | 双读仍执行原路径，实减 Map/Unmap **0/0 次**。这段指纹计时只占 0.170 ms/呈现间隔；消除映射还得覆盖其他读者。 |
| 提前同步资源索引有效吗？ | 移植社区提交 `dd799de` 后，两组对照中，提交线程分别少等 **5.99、15.71 ms/帧**。同负载慢帧匹配为 **0 对**。 | 提交线程确实少等了。对应 Present 提前多少、画面是否一致，仍待配对验证。[社区原始改动](https://github.com/Yaekai/victory-scarlett-3070/commit/dd799de4565f9b0f3f1d913ed911256054c42c76) |
| 存在暖缓存时，哪些绘制还要等？ | 一段 33 帧的记录里，所有录制线程合计等管线约 1,462 线程·毫秒，折合 **44.3 ms/帧**。两组 mesh/pixel 着色器从首次绑定到宿主就绪日志分别相隔 **901.74、908.93 ms**，两段重叠期间已有两次 Present。 | 按完整管线键区分库载入、驱动创建和工作线程排队，再核对首次实际绘制。[候选与事件口径](docs/benchmarks/2026-11-27-warm-pso.md) |
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

**先找出真正挡住绘制的一条 PSO。** PSO 是驱动执行绘制或计算所需的管线状态对象。用同一个高精度时钟记下候选的首次需求、后台取工、库锁和驱动调用、结果发布，以及绘制成功或因超时跳过。通过命令列表及其重置代次，把该次绘制连到实际 Direct 提交和对应 Present。[候选标识与源码入口](docs/benchmarks/2026-11-27-warm-pso.md)

选中一个确实挡住提交的对象后，只提前准备该对象，用相同路线和分辨率做 A/B。分别报告少等多少毫秒、Direct 和 Present 各提前多少，以及成功绘制、跳过绘制与像素的变化。提前预热若补回原先丢弃的绘制，应计为画面完整性收益。[提交性能报告](https://github.com/LZJ-Developer/VIctory/issues/new?template=performance.yml) · [贡献指南](CONTRIBUTING.md)

本分支沿用 VIctory 及其社区贡献者的代码和许可，从公开可取的 `f4b48ad` 源码提交派生。构建和启动输入的改动共 18 个 Rust 文件。版权及格式表例外沿用上游记录。[上游基线](https://github.com/avatarmanz/VIctory-7900XTX/commit/f4b48ad766ddbec1a1867022cd42f7645b0dd55e) · [许可证](LICENSE) · [上游资产约定](docs/asset-policy.md)

11 月 27 日交接时的源码和数据保存在[实测快照](https://github.com/LZJ-Developer/VIctory/tree/3a47dccf8558e774506d44ceedf15b4465a73e63)。需要此前 DexNom 安装器的读者可查阅[原默认版本](https://github.com/LZJ-Developer/VIctory/tree/6f866cdbcb158d5fdf328a974db5154670ef814a)。
