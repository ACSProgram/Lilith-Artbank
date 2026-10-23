# ImageLibrary（图库）模块架构分析报告 —— 附 Tauri + Rust 迁移方案

> **归档说明（2026-10-23）**：这是迁移前的分析报告，保留作背景资料，不再作为执行依据。
> 当前有效计划见 [分阶段路线图](../../planning/roadmap.md)。文中“逐文件 WebP 缩略图”“AI 可选后置”
> “PinBoard/调色盘等小功能可不迁”“pHash 作为主键”等结论已被新决策替代；实测数据见
> [性能基准](../../guides/performance.md)。

> 分析对象：`F:\programs\Lilith` 项目 · 模块目录 `Lilith/Modules/Image/ImageLibrary/`
> 分析日期：2026-09-05
> **只读分析**：本次未修改任何项目文件；未深入 DX12 引擎、OpenCV 内部等底层实现，仅依据模块自身代码及其直接调用的接口文件（`.ixx` 接口层）进行抽象分析。

---

## 0. 结论摘要（Executive Summary）

1. **模块本质**：一个运行在桌面"附件（accessory）"宿主框架内的**自包含图片资料库**，核心是"文件夹 + 内容哈希去重 + 标签/描述/评分元数据 + 缩略图"的本地图库，附带浏览（网格/列表）、筛选/排序/集合运算、全屏查看、像素级编辑、批量导入等能力。
2. **技术栈特征**：C++20 Module（partition）组织；UI 用 ImGui/ImPlot；图像处理用 OpenCV（CPU）；缩略图采用 **512px 预览 → DirectXTex 压缩为 BC1(DDS) → 存入自研 16KB 块分配容器 `previews.pak/.idx` → 读取时 `load_bc` 直接上传为 GPU 纹理**，绕开了"每帧 CPU 解码"；另有 LRU + 优先级队列的 `preview_loader` 做按需加载。
3. **最值得迁移借鉴的三点**：① 元数据与缩略图**物理分离、按需加载**的分层缓存思想；② 内容哈希（pHash）+ 文件按 `hash.ext` 命名实现天然去重；③ 后台线程 + 原子指针交换 + 帧末释放的"不卡 UI"并发模型。
4. **最值得在迁移中重构的三点**：① 全部元数据常驻内存 + 全量序列化保存（`image_data.bin` 每次整写）→ 大数据量下扩展性差，应换为**数据库 + 增量事务写入**；② 以 hash 作为唯一键而注释警告"hash 会变"→ 应引入**独立代理键 id**，外键/选集/列表引用它；③ 缩略图存 BC1 + 自研容器 + 碎片整理 → Tauri 场景**不再需要**（WebView 自带硬件解码与缓存），应改用**常规图片格式逐文件缓存**，量级更小、无碎片、跨平台。
5. **迁移结论**：采用 **Tauri + Rust core（领域/存储/图像管线）+ 前端视图**的三层结构；数据管理 → SQLite（事务、FTS、增量）；图像解码/显示 → 尽量下沉到前端系统 codec + GPU（免费且快）；Rust 只做"解码-特征-落盘"这类必须的像素处理与调度；编辑采用"前端 Canvas 交互预览 + Rust 全分辨率落盘"模式。缩略图建议 **WebP（单文件缓存目录）**，生成阶段用 `image` crate + rayon 并行，配合 SQLite 记录路径/尺寸。

---

## 1. 模块文件架构地图（供下次直接查阅，无需全项目搜索）

### 1.1 目录与规模

```
Lilith/Modules/Image/
├── ImageLibrary/                ★ 本次分析对象（图库）
│   ├── ImageLibrary.ixx/.cpp          模块门面 GUI（"图库"附件窗口/菜单/设置/截图）
│   ├── ImageLibraryCore.ixx/.cpp      :Core   领域对象 image_info + 缩略图写入辅助
│   ├── ImageLibraryContext.ixx/.cpp   :Context 全局单例 Database + 加载/保存/清理
│   ├── ImageLibraryList.ixx/.cpp      :List   视图模型 image_list（集合/分页/网格/撤销栈）
│   ├── ImageLibraryFeatures.ixx/.cpp  :Features 查看器/筛选器/排序器/批量元数据编辑
│   ├── ImageLibraryImporter.ixx/.cpp  :Importer 批量导入/描述导入/Embedding/清理工具
│   ├── ImageLibraryEditor.ixx/.cpp    :Editor  独立"图像处理"编辑窗（色彩/滤波/卷积）
│   ├── ImageLibraryPalette.ixx/.cpp   :Palette OKLCH 调色盘（绘画辅助，小功能）
│   └── ImageLibraryTools.ixx/.cpp     :Tools   缩略图异步加载器 preview_loader
├── ImagePaste.ixx/.cpp                "ZyPaste" 兄弟附件（剪贴板粘贴源）
├── PasteBoard/…                       （截屏采集）
└── PinBoard/…                         "画板"附件（接收 ImageLibrary 粘贴的贴图）
```
统计：9 组 `(ixx + cpp)` 共 18 个文件，约 4 779 行（`.ixx` 约 551 行，`.cpp` 约 4 228 行）。

> 另存在历史遗留 `Lilith/Aborted/image_library.{cpp,h}` 与 `Aborted/ImageBoard.*`——**已不在工程编译清单内**（`Lilith.vcxproj` 无 Aborted 条目），属早期草稿，仅可作思路参考，无需迁移。

### 1.2 文件职责与导出符号速查表

| 文件（partition） | 行数 | 关键导出/职责 |
|---|---|---|
| `ImageLibrary.ixx/.cpp`（门面） | 57 / 679 | `class GUI : core::accessory_base`（标题"图库"）；管理已配置图库 `libs`(name,path)；顶部 TabBar：多工作列表(`image_lists`)+回收站+筛选/排序开关；右键菜单、列表管理窗、色彩练习小工具窗；导入窗、编辑器、调色盘的挂载；设置页（库路径、快速加载级别、快捷键、截图路径）；全局 `Update()` 里做空闲自动保存与截图热键 |
| `ImageLibraryCore.ixx/.cpp` | 74 / 329 | `class image_info`（**领域对象**，见 §3）；`image_info::Image/Preview` 的按需加载与原子换出；`SavePreview()`（生成 BC1-DDS 缩略图写容器）；`LoadTempImage`/`SaveChanges`（编辑落盘核心）；拖拽载荷收发 `DragImageSender/Receiver`；常量 `preview_size=512` |
| `ImageLibraryContext.ixx/.cpp` | 62 / 382 | `struct Context` + **全局单例 `Database`**；`images: map<hash, image_info*>`；`load()` 并行加载四大数据文件并逐条建对象；`savedata()/save_embeddings()` 全量内存流压缩写盘；`save_and_clear_deleted()`（回收站文件物理移动）；`clear()` 卸载；成员：`prev_loader`、`prev_files`、`filter/sorter/editor`、`upscaler`、`selected`、`saved_lists`、原子进度/保存位 |
| `ImageLibraryList.ixx/.cpp` | 43 / 776 | `class image_list`：**结果集视图 + 快照栈撤销**（`deque<vector<image_info*>>`）；`reset`（全库按评分降序）→ 页/网格/多选/单击/双击/Ctrl/Shift 交互；集合运算 交集/并集/差集；`store/retrieve` 撤销；`tag_search`；回收站模式复用；批量操作入口（复制/编辑/贴图/生成缩略图/生成姿势） |
| `ImageLibraryFeatures.ixx/.cpp` | 137 / 946 | `image_upscaler`（全屏查看器：旋转/翻转/裁剪/调尺寸/保存）；`image_filter`（多维筛选：扩展名/文件夹/评分/哈希汉明距离(找相似)/主色调/标签/描述，反向，多线程全量扫描）；`image_sorter`（哈希相似/姿势相似/CLIP 相似/大小/时间/评分/4色/8色排序）；`image_info_editor`（多选批量改 文件夹/评分/标签） |
| `ImageLibraryImporter.ixx/.cpp` | 58 / 496 | `image_importer` 主导入窗：多线程导入（读取→512 预览→pHash→去重→拷入库目录→写缩略图→算主色→写元数据），覆盖模式（同 hash 高评分才覆盖）；`data_importer/descreption_importer`（`output/` 下按 hash.txt 回填描述，AI 打标外部流水线）；导出无描述图片；批量 CLIP Embedding；清理孤儿文件；缩略图容器整理 |
| `ImageLibraryEditor.ixx/.cpp` | 60 / 360 | `class image_editor`："图像处理"独立编辑器（支持剪贴板/文件载入）。历史 = 全分辨率 `cv::Mat` 快照栈 + 撤销/重做；色彩变换（输入/输出伽马 + 3×3 RGB 矩阵，float 归一化并行逐行）；`.cube` LUT3D 滤镜；Lab-L 通道傅里叶频域曲线滤波；7×7 卷积；操作日志面板 |
| `ImageLibraryPalette.ixx/.cpp` | 25 / 153 | `class OKHCLpalette`：OKLCH 调色盘（GPU shader 画 CL/HC 色图 + 屏幕取色），**绘画辅助小功能**，仅与 `Zy.D3D12.Tools` 绑定 |
| `ImageLibraryTools.ixx/.cpp` | 35 / 107 | `class preview_loader`：缩略图异步管线，参数 `(512 队列 / 64 并发 / 1024 缓存 / 核心数线程)`；单帧 `update()` 从队列派发线程池任务，成功后进 LRU 缓存，超限逐出；`load(info, high_priority)` 插队、`unload()` 显式清出 |

### 1.3 模块分区依赖图

```
Zy.Modules.ImageLibrary（伞模块，导出 GUI；import :List :Palette :Editor :Importer + Zy.App.Core）
  ├─ :List        ──import──▶ :Core
  ├─ :Palette     ──import──▶ Zy.Math.Core / Zy.Image.Core / Zy.D3D12.Tools   （无 :Core）
  ├─ :Editor      ──import──▶ :Core / Zy.Math.*
  ├─ :Importer    ──(.cpp)──▶ :Context
  ├─ :Context     ──import──▶ :Core, :Tools, :Features + Zy.Win32.File
  ├─ :Tools       ──import──▶ :Core + Zy.Concurrent
  ├─ :Features    ──import──▶ :Core（.cpp 又回引 :Context, 兄弟模块 ImagePaste）
  └─ :Core        ──import──▶ Zy.Image.Core / Zy.Image.Algorithm / Zy.String / Zy.Math.Core
```
架构要点（含"坑"）：
- **`Database` 全局单例**几乎被每个 partition 读写，`:Context ↔ :Features/:Tools` 互为引用 → 模块内部**强耦合中心**，与宿主框架（`CoreGui`、`GlobalThreadPool`、`MessageUI` 等）同样大量直接耦合。这是迁移时最难"干净切走"的部分，必须靠接口抽象。
- 图片数据来源与宿主窗口服务全部经由 `Zy.App.Core`（附件协议：`accessory_base` 提供 `GetTitle/DrawFloatWindow/DrawPopupWindow/DrawSettingUI/Update/End`），`App/AppCoreGui.cpp` 仅一行注册：`accessory_initializer["图库"] = [] { return new Zy::Modules::ImageLibrary::GUI(); }` → **模块对宿主唯一的静态耦合点**（另通过 `Zy.Modules.ImagePaste::AddPastedImage` 把图贴到"画板"）。

### 1.4 对 Zy 内核的依赖面（模块视角的"外部依赖清单"）

| 依赖 | 用途 | 迁移后的对应物（抽象） |
|---|---|---|
| `Zy.Image.Core`（ZyImage 等） | GPU 纹理封装、`imread_CV_8UC4`、BC 压缩 | 删除（WebView 自身解码/合成）或 Rust `image` |
| `Zy.Image.Algorithm` | `ToPreview`、pHash、`ImageColorScheme`(Lab-kmeans+EMD)、LUT/DFT/卷积 | Rust `image` + 自研 pHash/主色；DFT/卷积自研或绑定 |
| `Zy.Win32.File`（FileSystem 块容器） | `previews.pak/.idx` 自研块分配容器（16KiB block、碎片整理） | 删除；改逐文件缩略图或 DB BLOB |
| `Zy.Concurrent` | 线程池 | `rayon` / `tokio` |
| `Zy.App.Core/Tools` | CoreGui 窗口调度、GlobalFiles 配置、Hotkeys、MessageUI、ProgressWindow、ConfirmWindow、Gpaths | Tauri 窗口/事件/插件；前端组件化 |
| `Zy.Win32` | 剪贴板、文件对话框、屏幕截图、外部拖放 | Tauri 官方插件（clipboard/dialog/拖放） |
| `Http::PythonServer` | 外挂 Python 服务：MediaPipe 姿势、CLIP(1024d) 推理 | 可选旁路服务（HTTP/RPC 或本地模型） |
| 第三方：OpenCV、DirectXTex、ImGui/ImPlot、nlohmann/json | 像素处理 / BC1 压缩 / UI / 配置 | `image`+… / 前端 canvas / serde |

---

## 2. 领域模型与核心数据流

### 2.1 领域对象 `image_info`（图库中一张图 = 文件 + 元数据 + 派生特征）

```
image_info（不可拷贝；unique_ptr/原子指针管理；生命周期归 Database）
├─ 标识：hash(pHash 十六进制串)  + extension  → filename = hash + extension（物理文件名！）
│        folder(UTF-8, 库内子目录)          → ImagePath() = lib_path/folder/filename
├─ 属性：size、import_time、score(0~5)、tags(vector, UTF-8)、description
├─ 特征：colors4 / colors8（ImageColorScheme<4|8>: Lab k-means 主色 + 占比权重）
│        pose?(MediaPipe 33 关键点)        —— 可选, 外挂 Python 推理
│        embedding?(1024 维 CLIP 向量)     —— 可选, 外挂 Python 推理
├─ 像素：image  (atomic<ZyImage*>, 全分辨率 GPU 纹理, 按需 Load/Read/WaitForClear)
│        preview(atomic<ZyImage*>, 512px BC1 GPU 纹理, 同上协议)
└─ 状态：delete_mark（软删除 → 进回收站；卸载时物理移至 recycle/ 或清空）
```
三条关键设计约定（迁移时需显式保留或替换）：
1. **内容寻址**：hash 由 512 预览图的 pHash 决定 → 天然去重（导入时同 hash 只提示/覆盖）；但**内容编辑后 hash 会变** → 代码注释明确警告"不要存 hash 引用，指针不变"。抽象结论：**文件身份 ≠ 内容指纹**，二者必须解耦（迁移引入自增/雪花 id 作代理键）。
2. **两级像素态**：`preview` 小图高频（网格滚动），`image` 全图低频（双击查看/编辑时才从磁盘解码上纹理），UI 线程只持 GPU 句柄读指针，绝不在 UI 帧内做解码。
3. **编辑即换名**：`SaveChanges` → 重算预览 → 新 pHash → 若变化：map 删旧键/插新键、写新文件、重建缩略图、丢弃 pose/embedding 缓存 → 旧文件变"孤儿"，由"清理图库文件"工具扫目录把未登记文件移入回收站。**说明当前模型没有事务性的"编辑原子化"**，依赖事后清扫（迁移改进点）。

### 2.2 图库在磁盘上的布局（抽象）

```
<lib_path>/                        （一个图库 = 一个根目录）
├─ <folder>/hash.ext …            用户图片本体（folder 由导入时指定/批量编辑改）
├─ recycle/<folder>_hash.ext      回收站（卸载/清空时物理删除）
├─ output/hash.txt                描述导入流水线：导出→外部打标→回填（覆盖 description）
└─ .data/
   ├─ image_data.bin    主元数据：全量 image_info（含 colors/pose），压缩二进制整写
   ├─ previews.pak      缩略图容器（16KiB 块分配、可碎片化，存 512px BC1-DDS 数据）
   ├─ previews.idx      缩略图索引（hash → 块表/分配器状态）
   ├─ saved_lists.dat   已保存图片集（list 名 + hash 序列）
   └─ embeddings.bin    CLIP 1024 维向量（hash + float[1024]），压缩整写
```
图库元信息（哪些库、路径、快速加载级别、截图目录、热键）在宿主配置 JSON `accessories/image_library/configs`，非 UTF-8 路径直接存（模块内自注释）。

### 2.3 加载/保存的并发与一致性模型

- **加载**（`Context::load`，后台线程 + `disable_gui` 门闩 + ProgressWindow）：
  - 并行三路：缩略图容器打开 / embeddings 解压 / 主数据解压顺序读；
  - 主数据逐条 `new image_info` 填充 → `unique_lock` 批量入 map；
  - 可选步骤由 `quick_load_level` 裁剪：≥1 跳过"磁盘文件存在性校验"（快但丢丢失检测）；≥2 跳过 embeddings 加载（省内存）；
  - saved_lists 按 hash 回填指针，缺失仅告警。
- **保存**：`saving` 原子位防重入；`MemoryStream(64MB 预分配)` 全量序列化 → 压缩落盘（主数据与 saved_lists 一起，embeddings 单独按钮/命令触发）；**每次保存都是全量写**。
- **脏标记**：所有编辑仅置 `need_save`；`GUI::Update` 每帧检测"窗口关闭 + need_save" → 后台空闲保存。
- **卸载/退出**：`End()` 流程 = savedata → `save_and_clear_deleted()`（把 delete_mark 文件 rename 到 recycle/ 并删缩略图）→ clear → 清临时文件；回收站"清空"动作也走同样路径后卸载。
- **UI 安全**：纹理对象换出用 `atomic` exchange + `CoreGui.SubmitAfterFrameTask` 延迟删除，避免"正在绘制的 GPU 资源被释放"；所有 UI 回调包 `TryInvoke(异常捕获+日志)`；`disable_gui` 原子门闩在长任务期禁用交互。

### 2.4 核心数据流（四段）

```
① 导入   外部文件(拖放/剪贴板/选择器)
          → OpenCV 解码 8UC4 → ToPreview(512) → pHash
          → 查重(命中则跳过或按评分覆盖) → copy 文件至 lib/<folder>/<hash>.<ext>
          → SavePreview: ToPreview→CompressImage_BC(BC1)→写 .pak/.idx
          → colors4/8.calculate(预览) → new image_info → map 插入 → 异步全量保存
② 浏览   网格帧循环：visible 项无纹理 → prev_loader.load(info, 高优先级=当前页/低=邻页预取)
          → update() 派发线程池 → Preview(Load): 读 .pak(hash) → DDS 解析 → load_bc 上 GPU → LRU 缓存
          → 命中缓存/占位图标 icon_ghost 直接 AddImage；滚动翻页/缩放行列数
③ 检索   筛选器/排序器对当前集合全量内存扫描（多线程）：
          hash 汉明距离(找相似 O(n²), 上万张作者自注会卡)、EMD 色距、姿势相似、CLIP 余弦、
          标签/描述/文件夹/评分/扩展名过滤；结果入 image_list 快照栈（可撤销）
④ 编辑   查看器/编辑器改动(旋转裁剪/调尺寸/色彩/滤波/LUT/卷积) → cv::Mat
          → SaveChanges：重算预览+pHash → 换名写文件 → 重建缩略图 → 清派生特征 → need_save
```

---

## 3. 核心能力拆解与评估（迁移决策输入）

### 3.1 为什么原实现"快"（绩效来源，迁移要保持）

| 环节 | 原方案 | 提速机理 |
|---|---|---|
| 缩略图生成 | OpenCV `resize`（质量好、SIMD）+ DirectXTex BC1 压缩 | CPU 一次性成本，导入时后台并行 |
| 缩略图显示 | 磁盘存 **BC1 DDS** → `load_bc` 直接创建 GPU 纹理 | 存储即显存格式，无"解码→上传"两步；网格高频路径零 CPU 解码 |
| 全图查看 | 仅双击时解码一次为 GPU 纹理，之后每帧只是采样 | 无重复 IO/解码 |
| 大量图片滚动 | `preview_loader` 队列上限 512、并发 64、缓存 1024 张 LRU + 邻页预取 | 可控水位、显式淘汰、后台不卡 UI |
| 元数据访问 | 全库常驻内存 map，O(1) 按 hash 取对象 | 浏览/筛选都在内存指针上跑 |
| 批量导入 | `MaxThreads` 线程并行读图/计算，结果互斥合入 | CPU 核数直接换吞吐 |

### 3.2 原实现的隐含代价（迁移时一并解决）

1. 元数据**全量常驻 + 整写**：库越大，启动 IO/内存、每次保存的序列化成本线性上升；无事务，中途断电易损（加载时对四个文件逐一校验、丢一个即拒载）。
2. 缩略图容器：**单一大文件** 16KiB 块分配会随时间碎片化（需手动 defrag、有空间利用率指示）；BC1 每张 512px ≈ 128 KiB；DDS 尺寸需 4 对齐（源码注释承认有对齐黑边问题）。
3. hash 即主键 + 换名编辑 → 键漂移、孤儿文件依赖事后清扫。
4. 图像编辑历史栈 = 全分辨率 `cv::Mat` 副本（内存随操作数线性增长）；DFT/卷积在 Lab 单通道，每步全图重算，交互为"点应用才执行"（非实时预览）。
5. 视图逻辑与 ImGui 绘制、筛选排序算法、领域对象高度交织（`show()` 几千行内含交互/快捷键/绘制），**逻辑可复用性差**——迁移的最大工作量不在算法而在解耦。

### 3.3 功能分级（用于迁移范围决策）

| 级别 | 功能 | 迁移建议 |
|---|---|---|
| 核心（必保） | 导入/去重/文件整理；元数据管理（标签/评分/描述/文件夹/批量编辑）；缩略图浏览（网格/分页/多选/拖放）；筛选与排序；图片查看与基础编辑落盘 | 完整重设计并保留行为语义 |
| 重要 | 集合运算 + 撤销栈、已存列表、回收站、查找相似图（pHash 距离） | 保留语义，底层改为 SQL/索引化 |
| 可降级 | CLIP 语义排序、姿势相似排序（依赖外挂 Python 服务） | 设计 `SimilarityProvider` trait，初期可后置 |
| 小功能（可不迁） | OKLCH 调色盘、色彩练习小窗、快速截图、贴图到画板、LUT 预设、二维码工具、描述外部打标流水线 | 视产品定位取舍，或作独立插件 |
| 必删 | ZyImage/BC1/DDS/FileSystem 块容器/DX12 纹理路径 | 被 Tauri 体系取代 |

---

## 4. Tauri + Rust 迁移目标架构（抽象层设计）

> 本节给出**与具体数据库/环境无关**的分层设计与取舍；括号内为技术示例，仅用于说明能力画像，不代表环境绑定。

### 4.1 迁移总原则

1. **三层分离**：`前端视图层`（渲染/交互/即时反馈） / `Rust 应用与领域层`（命令、工作流、状态机） / `存储与图像处理层`（DB、文件、像素、特征）。
2. **借力浏览器**：Tauri 的 WebView 自带**操作系统级图像编解码与 GPU 合成**（JPEG/WebP/AVIF/HEIC 等由系统 codec 提供，解码有硬件加速）。原方案里"解码 + 缩放 + 上纹理 + 逐帧采样"的整套自研管线，在 Tauri 里大部分可以**免费下放给浏览器**（`<img>`/`ImageBitmap`/canvas），Rust 只保留"必须离开浏览器的像素工作"。
3. **一次写入、按需读出**：导入时把"特征提取 + 缩略图生成 + 元数据落库"做成一个后台事务管线；浏览时 DB 只出元数据 + 轻量缩略图路径，避免大数据量 IO。
4. **代理键 + 不可变内容文件**：图片文件仍按内容指纹命名（去重），但所有引用走数据库自增 id；编辑 = 新文件 + 新行 + 旧行标记废弃（可事务化、可回收），彻底消除"hash 漂移 + 孤儿文件清扫"。
5. **异步边界统一**：所有耗时操作经 `async command + 事件推送进度`；前端用乐观 UI。

### 4.2 目标分层与模块映射

```
┌─ 前端（Tauri WebView）─────────────────────────────────────────────┐
│ Views:  图库网格 / 列表 / 详情查看器 / 导入向导 / 设置              │
│         ↓ 懒加载缩略图(IntersectionObserver)  ↓ 全图查看走 <img>    │
│ 交互态: 多选/拖放/快捷键/右键菜单; 筛选面板(计数徽标)               │
│ 编辑预览: Canvas/WebGL 实时调参(色温/曲线/裁剪预览)，原图不落地     │
└──────────────┬──────────────────────────────────────────────────────┘
        Tauri IPC（command + event），一次调用一页数据
┌──────────────▼──────────────────────────────────────────────────────┐
│ Rust App/领域层                                                      │
│  LibraryService: open/save/switch 图库状态机(借鉴 loaded/disable_gui)│
│  ImportJob: 批处理工作流(校验→处理→进度事件→失败清单)               │
│  EditJob:   保存时全分辨率落盘(hash 重算/事务换行/缩略图重建)        │
│  QueryService: 过滤/排序/分页/集合运算(SQL 下推 + 内存兜底)          │
│  SimilarityProvider: pHash/主色/[CLIP/姿势可选] → 统一 trait         │
└──────────────┬──────────────────────────────────────────────────────┘
┌──────────────▼──────────────────────────────────────────────────────┐
│ 存储与处理层                                                          │
│  Store(repository trait) ── SQLite: 事务、WAL、FTS5、外键            │
│  ThumbService   ── 解码→512/256 缩放→WebP 落盘(线程池/rayon)         │
│  RasterOps      ── 编辑落盘/裁剪/缩放/格式转换 (image crate)          │
│  FileStore      ── 图库根目录布局: <folder>/<hash>.<ext>, recycle/   │
│  [可选] Clip/Pose worker（独立进程或 onnxruntime）                    │
└──────────────────────────────────────────────────────────────────────┘
```

### 4.3 数据管理设计（替换整写文件）

**推荐：嵌入式关系库（SQLite 家族）单文件放 `<lib>/.data/library.db`（WAL 模式）。**理由（抽象层面）：元数据天然是"实体 + 多对多标签 + 排序 + 文本检索"的结构化数据；单文件便于备份/移动；事务解决"编辑原子性/断电安全"；索引解决"万级以上筛选计数"。

建议表结构（抽象）：

```
images(id PK, content_hash UNIQUE, ext, folder_id FK, size,
       imported_at, score, description, colors4 BLOB, colors8 BLOB,
       deleted_at NULL, deleted_tombstone_at)      -- 回收站=软删+延迟物理删
folders(id PK, path UNIQUE)
tags(id PK, name UNIQUE);            image_tags(image_id, tag_id, PK 联合)
lists(id PK, name, sort_order);      list_items(list_id, image_id, position)
thumbnails(image_id PK, path, width, height, bytes, updated_at)  -- 或并入 images
features(image_id PK, embed BLOB /*1024×f32*/, pose BLOB)        -- 可选旁挂
FTS: description/tag 内容 → FTS5 虚拟表
索引：content_hash、folder、score、deleted_at；编辑=新 content_hash 时开事务：新行+新文件+旧行标记
```
要点：
- 读写分离：浏览/筛选走**只读事务 + 小页查询**；导入/编辑走**写事务批处理**；彻底替代"全量序列化 + 整写压缩文件 + 启动全量载入"。
- `saved_lists`（图片集）由"hash 序列"改为 `list_items` 引用 id → 图片重编辑换名后集合引用不丢（原实现靠指针/当前 hash 会被破坏）。
- 进度与断点：导入任务记录到 `import_jobs`，崩溃后支持续跑/跳过已存在 hash。

### 4.4 缩略图与图像管线的重新设计（重点）

**先算清账：** 原 512px BC1 ≈ 128 KiB/张；WebP q80 的 512px 自然图通常 15~45 KiB，仅体积即缩小 **3~8×**，且无对齐黑边、无碎片整理、通用工具可读。浏览器对 WebP 解码有系统级硬件加速与内存缓存，滚动网格天然高效。

**推荐方案（分层缓存，三级）：**

1. **持久层 = 每图一个缩略图文件**（哈希/代理键命名），存于图库 `.data/thumbs/`（或用户缓存目录，可重建）；尺寸策略：默认 512px 上限对齐原体验，另在宽网格缩略尺寸（如 256）时由前端 CSS 缩放即可（成本可忽略），**不**多级存盘；格式 **WebP**（质量 80~85，带 α 用无损通道选项）。
2. **生成管线 = Rust 后台任务**：解码（`image` crate，解码线程池 rayon）→ 等比缩放（`image::imageops`，LANCZOS3 兼顾质量/速度）→ WebP 编码（libwebp 绑定）→ 写缓存目录 → 事务里回填 `thumbnails` 行。**导入主流程不阻塞**：先返回占位/进度事件，后台按序生成；支持"低优先级补全历史库"（对应原"生成缩略图"右键批量重建）。
3. **运行时 = 交给浏览器**：网格用 `<img loading="lazy" decoding="async">` + IntersectionObserver 预取当前与相邻视口；Rust 侧提供**本地静态资源协议或自定义协议**输出缩略图字节并带缓存头，浏览器自身做 LRU/内存缓存——**不再需要 preview_loader 三队列代码**。全图查看同样 `<img>`（系统 codec 解码原格式）。

**备选与适用边界（决策表）：**

| 方案 | 优点 | 缺点 | 适用 |
|---|---|---|---|
| A. 逐文件缩略图目录（推荐） | 简单、可重建、浏览器友好、无碎片 | 小文件多（10 万张时 inode/枚举压力） | 常规规模 ~≤20 万张 |
| B. 缩略图入 DB（BLOB） | 单文件、备份简单、事务一致 | DB 膨胀、随机读放大、需自管理压缩 | 小库/便携单文件偏好 |
| C. 单文件容器（仿原 .pak） | 极少文件数、顺序读好 | 复杂度高、需自研分配/碎片整理 | 超大库（数十万+）或嵌入式终端 |
| 混合（A + 分层目录 `thumbs/ab/<id>.webp`） | 兼顾枚举与缓存 | 略复杂 | 大库推荐形态 |

若追求"重建缩略图 = 用原图重新生成"的可恢复性，任何方案都应把缩略图视为**可派生缓存**（丢了可再生成），只有元数据与文件本体不可派生——这是与原 .pak 设计最大的理念差异。

### 4.5 图像加载与编辑设计（核心功能）

**查看流（推荐"浏览器为主"）：**
- 原图查看/旋转/缩放/上一张下一张：直接 `<img>`/canvas + 前端手势，零 Rust 参与；
- 超大图（如 >8K）可加瓦片或浏览器 `createImageBitmap` 降载，属渐进增强。

**编辑流（推荐"前端预览 + Rust 落盘"）：**
- 保留原模块最有价值的编辑语义：**内容变更 → 预览重算 pHash → 换名/换行 → 重建缩略图 → 清派生特征**；
- 裁剪/旋转/翻转/调尺寸：前端用 canvas 完成并实时预览，提交时上传**编辑参数**（无损、可审计）或导出位图；
- 色彩/滤波（原 Gamma+矩阵、LUT3D、频域曲线、卷积）：交互调参在 WebView 用 **CSS filter + canvas/WebGL 实时预览**（原实现"点应用才执行"，实时化反而是体验升级）；保存时 Rust 以 `image` crate 实现同参渲染（这些算子每张一次性执行，CPU 毫秒~百毫秒级，无需 GPU 管线）；
- 撤销/重做：不再存全分辨率 Mat 栈，改存**参数化操作链**（记忆开销趋近于 0，原实现每步一张全图副本）。

**特征侧**（为筛选/排序/查重服务）：
- pHash：512 预览灰度 + DCT/均值比较（纯 Rust 数十行，导入即算）；
- 主色 4/8：缩小到如 64px 后 Lab 空间 k-means（`kmeans`/自实现少量迭代）或流行色量化；相似度用加权距离代替原 EMD（结果近似、快几个数量级，或保留 EMD 库实现）；
- 语义（CLIP 1024d）/姿势：定义 `SimilarityProvider` trait 接口，可先留空再接入外部推理进程；向量检索初期**暴力扫描 + rayon**（与原实现等价），量级上来再换 ANN。

### 4.6 检索/排序/集合的 SQL 化

- 多维筛选（文件夹/评分/扩展名/标签/删除状态）→ 单条 SQL + LIMIT 分页；"计数徽标"用 GROUP BY 或维护计数物化（原实现每次开窗全量扫描建 count）。
- 描述/标签文本搜索 → **FTS5**（替代原 `std::string::find` 全量扫描）；标签自动补全也走 DB。
- 集合运算（交/并/差）→ SQL `JOIN/EXCEPT/UNION`；撤销栈 → 前端命令历史（结果集是查询产物，无需原快照栈）。
- 回收站 → `deleted_at` 软删 + 卸载/手动时批量物理迁移或清除（保留原"卸载时清扫"时机语义亦可）。

### 4.7 平台服务映射（原 Win32/宿主服务 → Tauri 插件/前端）

| 原能力 | 迁移对应 |
|---|---|
| 剪贴板图片/路径 | tauri-plugin-clipboard-manager / 浏览器 Clipboard API |
| 文件对话框 / 拖放导入 | tauri-plugin-dialog / HTML5 drag-drop（原 DropHandler 语义） |
| 全局快捷键（开关窗/快速截图） | tauri-plugin-global-shortcut |
| 设置存取（libs 列表、截图路径…） | Rust 侧 serde 配置文件 + 前端设置页（替代 GlobalFiles JSON） |
| 截图 | 可选：系统截图插件或裁剪为独立小工具 |
| 贴图到画板 | 同一 Tauri 应用内事件总线（模块解耦后的天然优势） |

### 4.8 模块重组的建议文件布局（对应原 9 partition）

```
src/
├─ domain/        image.rs(实体, 替代 image_info) / library.rs(图库聚合根)
├─ store/         db.rs / schema.rs / migrations / thumb_store.rs
├─ pipeline/      import.rs(原 importer 语义) / edit.rs(SaveChanges 语义)
│                 feature.rs(pHash+主色) / thumb.rs(缩略图生成)
├─ query/         filter.rs sorter.rs list_ops.rs(原 Features/List 中可复用算法)
├─ ipc/           commands.rs / events.rs / asset_serve.rs
└─ workers/       background jobs(rayon) + 进度 channel
frontend/src/     views/ (grid, list, viewer, editor, import, settings)
                  components/(filter panel, thumb grid...) store/(前端状态)
```
原则：把原 `show()` 巨型函数里"绘制"与"逻辑"分开——逻辑下沉到 Rust `query/`，前端只渲染查询结果与发送命令。

### 4.9 关键性能设计参数建议（替换 preview_loader 参数）

- 缩略图：512px WebP q82（细节质量参考原 512 设定）；网格项 < 200px 时 CSS 缩放。
- 前端懒加载：视口 + 前后 1~2 屏预取；浏览器缓存上限交给 WebView 自身（约等于原 1024 LRU 语义）。
- 导入并发：rayon 默认线程池（≈ 原 MaxThreads）；写 DB 走单写事务通道，避免 SQLite 写放大。
- 分页：滚动虚拟化（网格只挂载可视 + 缓冲项），数据源一次 200~500 行元数据查询。

---

## 5. 分阶段迁移路线（建议）

| 阶段 | 内容 | 验收 |
|---|---|---|
| P0 骨架 | Tauri 工程 + 图库目录/DB schema + 代理键模型 + 打开/关闭图库状态机 | 空库可建/开/关，SQLite 迁移可重复 |
| P1 导入与浏览（MVP 核心） | 拖放导入(去重/拷入/元数据) + 网格浏览 + 懒加载缩略图 + 基本信息查看 | 1 万张导入+滚动流畅；缩略图后台生成不卡 UI |
| P2 数据管理 | 标签/评分/描述/批量编辑/文件夹管理/回收站/搜索(FTS)/筛选计数 | 语义对齐原模块（哈希冲突策略、批量编辑一致） |
| P3 查看与编辑 | 全屏查看/旋转/裁剪/调尺寸 + 参数化色彩/滤波 + 事务化保存 | 编辑后换名/去重/缓存失效行为与原实现一致 |
| P4 高级检索 | pHash 查相似/主色/集合运算/已存列表(引用 id) | 万级规模交互无卡顿（原 O(n²) 卡顿点消失） |
| P5 可选能力 | SimilarityProvider 接 CLIP/姿势、批量重建缩略图、LUT 预设迁移、旧 .data 一次性转换工具 | 按产品取舍 |
> 旧图库数据：`.pak/.idx`、`image_data.bin` 为私有格式；若需延续旧库，写一个**一次性导出工具**（读容器→产出逐文件缩略图+元数据转 DB），不建议在 Rust 版里长期维护该格式读取。

---

## 6. 风险与开放问题

1. **编辑语义保真**：原卷积/频域滤波在 Lab-L 上执行且有具体数值约定，Canvas 实时预览与 Rust 落盘必须**同一份算子参数与色彩管理**，避免"预览 ≠ 结果"。建议落盘算子先在 Rust 端按原代码逐行移植并做回归样例对比。
2. **WebView 解码差异**：网格直接显示原图格式（而非统一缩略图）时会依赖 WebView codec 覆盖范围；统一缩略图（WebP）策略可规避。
3. **超大库**：10 万张以上建议上"分层缩略图目录 + DB 分离（元数据/缩略图可选分文件）"，并做文件枚举的 benchmark。
4. **回收站时机语义**：原"卸载时才物理移动/清空"，容易让用户困惑；迁移建议改为显式"移入回收站=立即移动+DB 软删"，语义更清晰（需要产品确认）。
5. **并发写**：SQLite 单写者适合此场景；若未来多进程/多窗口写同一库，再评估服务化。

---

## 附录 A：跨模块引用点清单（迁移时需同时处理的耦合）

- `Lilith/App/AppCoreGui.cpp`：`import Zy.Modules.ImageLibrary;` + `accessory_initializer["图库"] = …GUI()` —— 唯一宿主注册点。
- `ImageLibrary(Features/List).cpp` → `Zy.Modules.ImagePaste::AddPastedImage`（"贴图"到画板）；`ImagePaste`/`PinBoard` 为兄弟附件，若整体迁 Tauri，可统一改为应用内事件。
- 图库目录外的全局资源路径：`Gpaths("image_library_temp_files")`、`Gpaths("image_library_temp_imported_image")`、`Gpaths("image_library_clip_model")`（Python CLIP 模型路径）、配置 `accessories/image_library/{configs,kernels}`。
- 依赖外置服务 `Http::PythonServer`（MediaPipe 姿势 / CLIP），与图库本体生命周期解耦，可后置。
