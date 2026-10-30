# 查看器与几何编辑

查看器负责“看”和“轻度几何编辑”。编辑保存复用图库统一的“新内容发布”路径（新内容哈希 → 新文件 → 事务更新行 → 旧文件清理），不覆盖原文件。

## 当前实现

入口：双击网格/列表、Enter、右键“打开查看器”。查看器覆盖标题栏以下的工作区，标题栏仍可拖动窗口；Esc 关闭。上一张/下一张按当前图库查询的已加载顺序移动，接近页尾时请求下一页，不重新查询全库。关闭后网格保留最后一张的选择。

### 显示与导航

- 适应窗口 / 1:1 / 滚轮缩放（以光标为中心）/ 拖拽平移 / 双击在适应与原尺寸之间切换；缩放范围 2%-800%。
- 渐进加载：切换图片时立刻按会话项的 `thumbVersion` 取 512px 缩略图铺满画布，按原图宽高比与适应窗口缩放占位；高清层 `onLoad` 后 140ms 淡入，缩略层随淡入结束隐藏。有缩略图时不显示整块“正在读取原图”，超过 150ms 仍未就绪才显示画布中央的小 spinner；缩略图缺失或加载失败才回到整块加载态。有 alpha 的图片不做交叉淡入，高清层就绪后直接换层。
- 切换与拟合：`source` 与媒体加载状态都按当前图片 id 归属，上一张的图、姿势、裁剪与缩放不会出现在新图首帧；拟合缩放与复位在 layout effect 中完成，从缩略图占位到高清层、从原图尺寸占位到 4096px 预览都不改变屏幕上的实际大小与位置。手动缩放遇到降采样预览时按显示宽度等比换算（`rescaleZoom`）。
- 全屏状态由 `AppRuntimeProvider` 持有：`window_set_fullscreen(value)` 写入后回读真实值，前端另外用 `isFullscreen()` + `onResized` 同步，系统快捷键（F11、Win+↑）改变全屏时按钮图标也跟着变。全屏时隐藏标题栏与查看器自身的顶栏、工具栏、状态栏、侧栏、胶片带，只留画布；关闭查看器会先退出全屏。F11 在 `AppShell` 里全局响应，Esc 在全屏时先退全屏再关查看器。
- 键盘：Esc（裁剪中先退出裁剪；姿势叠加打开时先关闭叠加；全屏时退全屏）、←/→、F、0、1、+/-、`Ctrl+Z` 撤销、`Ctrl+Shift+Z` / `Ctrl+Y` 重做；输入框聚焦时忽略（滑块聚焦时撤销快捷键仍然生效），弹窗打开时不响应。打开查看器时把焦点移进覆盖层，避免网格的容器级键盘处理与查看器同时响应方向键与 Esc。
- 大图：浏览器原生格式且 ≤2400 万像素时直接加载原图；超过阈值、格式不能由 WebView2 直接解码（TIFF/TGA）、或带 EXIF 方向时，由 Rust 生成最长边 4096 的 WebP 预览，缓存在 `.artbank/temp/viewer/<id>-<hash>.webp`。打开图库会清空 `temp`，保存会删除该图旧哈希的预览。
- GIF：≤2400 万像素且无 EXIF 方向时直接加载原文件，由 WebView2 播放动画；超过阈值时走上面的静态 WebP 预览，工具面板与状态栏分别显示“已生成静态预览，动画不可用；保存仍读取原图。”与“静态预览 · 动画不可用”。网格、列表与“找相似”参考缩略图始终是首帧静态图，并带 `GIF` 角标（见[浏览与检索](browse-and-query.md)）。
- 单次会话内 `temp/viewer/` 上限 2 GB：累计写入每满 256 MB 扫一次目录，按 mtime 从旧到新删到上限以内。查看路径本身不扫目录，清理失败只记日志。
- 带 EXIF 方向的图片也走 Rust 预览：预览已转正，避免 WebView2 的自然尺寸/方向语义与数据库宽高不一致，从而保证裁剪坐标可预测。编辑保存始终重新解码原图并应用方向。
- `img://` 协议：Windows 为 `http://img.localhost/<id>?v=<hash>`（原图）与 `http://img.localhost/preview/<id>?v=<hash>`（预览）。响应带 `Cache-Control: immutable`、ETag=内容哈希；id 不是整数返回 400，图片行/原图缺失返回 404（前端据此走失败态），路径解析失败返回 500。`preview/` 分支在缓存文件缺失时按当前行重新生成，原图也不存在才返回 404。前端统一经 `src/shared/imgUrl.ts` 生成 URL，重试参数由 `withRetry` 追加。
- 姿势叠加：33 点 MediaPipe 骨架与可见点过滤；`get_image_pose` 从 feature pack 读取 landmark。没有可见姿势时按钮调用 `ai_generate_pose`（512px 缩略图输入），保存后立即叠加；服务返回空点集时提示未检测到人体。骨架线宽与关节点半径按屏幕像素换算（`2 / zoom`、`5 / zoom` 与 `3.5 / zoom`），缩放时粗细不变；主线外加深色描边保证在明暗画面上都能看清，置信度低于 0.5 的连线与关节点半透明，头部与躯干节点比四肢大。画布右上的姿势面板显示可见点数（N/33）与不透明度滑块，不透明度存在 `localStorage`。
- 复制图片：`copy_image` 解码原图、应用 EXIF 方向后以 RGBA 写入系统剪贴板；超过 5000 万像素给出可行动错误。
- 拖出到资源管理器（Windows OLE）：评估后暂缓；当前提供“复制图片”和“复制文件路径”。
- 元数据侧栏：由 `src/app/ViewerHost.tsx` 复用图库详情侧栏，评分、标签、描述、文件夹可直接编辑；每次写入携带 `DataChange` 交给图库统一刷新，回收站/恢复/永久删除成功后关闭查看器。侧栏里的标签可以点击按范围筛选（见[浏览与检索](browse-and-query.md)的“按标签筛选”），点击后查看器关闭并把控制权交回网格。
- 与数据源同步：原图加载 key 除图片 id 外还含 `contentHash` 与文件大小，空间优化或像素编辑换掉内容后自动重取源，不会停在旧内容或坏图上。`ViewerHost` 订阅 `session.getItems()`：活动图不在当前列表里（评分/标签改动让它不再符合筛选、集合成员变化、移入回收站等）就提示并关闭查看器；列表为空表示正在换结果集，此时不判定。
- 大图内存：查看器原图由 WebView2 解码；Rust 侧编辑/预览都受 512 MiB 分配、单边 65535、1 亿像素的解码上限约束，并对 >5000 万像素的图片禁用编辑。Rust 侧生成预览（大图、EXIF 方向与非浏览器格式）时还要申请全进程 1.5 GiB 字节预算，导入或压缩正在解码时会排队等额度。
- 配色跟随全局主题：查看器不再固定深色，顶栏/工具面板/状态栏/输入框/裁剪框都用 `src/styles/tokens.css` 的语义变量；画布用 `--canvas`（浅色下中性灰、深色下比面板再暗一档），透明区域显示 `--checker` 棋盘格。
- 胶片带：画布底部一条 68px 的横向缩略图带，当前项高亮并自动居中，点击切换，滚轮转横向滚动，滚到末尾沿用 `session.loadMore()` 继续加载。只渲染视口附近的条目（`OVERSCAN` 8 条），20 万条也不会一次挂满 DOM。开关写在 `settings.ui.viewerFilmstrip`，状态栏左侧按钮切换。

### 几何编辑

- 旋转 90°（0-3 次顺时针）、水平/垂直翻转、裁剪、调尺寸；全部只在保存时落到像素。
- 顺序固定为：**翻转 → 顺时针旋转 → 裁剪 → 缩放**。前端 CSS 变换使用相同顺序，裁剪选区在旋转/翻转后的显示坐标上绘制。
- 几何操作可组合：旋转会把裁剪矩形一起转过去（`(x, y, w, h) → (H - y - h, x, h, w)`），翻转按显示空间镜像裁剪矩形，尺寸按新基底等比重算，不再清空前面的结果。翻转按显示空间语义：旋转奇数次时，界面上的“水平翻转”对应原图的垂直翻转；后端的翻转先于旋转，因此换算在 `model.ts` 里完成（`rotateGeometry` / `flipGeometry` / `resizeForNewBase`，均有单测）。
- 裁剪保存时按显示坐标 → 原图坐标换算；旋转为奇数时宽高比例互换，降采样预览同样按显示/原图比例换算。
- 调尺寸：锁定纵横比、百分比预设、直接输入宽高；输出上限 5000 万像素、单边 65535。
- 输出：JPEG / PNG / WebP；JPEG、WebP 质量 1-100，PNG 无损；JPEG 透明区域合成白底。默认沿用 JPEG/PNG/WebP；GIF/TIFF/TGA/BMP 按是否有 alpha 默认转 PNG/JPEG，转换本身算一次修改。GIF 编辑只保留首帧。
- 超过 5000 万像素的图片在查看器中只读（仍可看降采样预览）；回收站中的图片只读。
- 工具面板实时显示原图、裁剪后、输出尺寸；“未保存”标记与保存按钮都看同一份 `hasChanges`：几何变化、格式变化、以及**有损格式把质量调到非默认值**都算修改（后端 `has_changes` 规则一致）。
- 撤销栈：会话内最多 50 步，记录 `GeometryState` 快照与操作名（“旋转 90°”“裁剪”“调整尺寸”“输出 JPEG q60”），工具栏底部两个按钮显示“撤销：裁剪”这类可读标签。同一滑块的连续调整在 600ms 内合并成一步；切换图片与保存成功后清空。保存本身不进入这个会话内撤销栈，但会写入图库操作记录，可从工具栏“记录”撤销或重做（见 [图库与元数据](library.md#操作记录与撤销)）。
- 体积预估：输出区在几何/格式/质量变化后防抖 500ms 调 `estimate_edit_size`，返回字节数与是否精确；大图（>2000 万像素）按 1/4 边长编码后按面积比外推并标“约”。同一时间只跑一次编码，新请求只保留最新一份，过期结果直接丢弃。有损格式同时提示“重新编码会损失画质、原始 EXIF 不保留”。

### 空间优化与编辑的边界

空间优化不经过查看器的编辑会话：它不产生新的图片 id，不写编辑快照，也没有编辑器内的撤销栈。编码、PSNR 与 EXIF/ICC 搬运在 `src-tauri/src/imageops/optimize.rs`，与 `imageops/edit.rs` 的几何编辑分开；执行时按内容哈希发布新文件并删除旧图，但 `feature_blobs`、标签、集合与 `modified_at` 不动。见 [维护与批量工作流](maintenance.md#图片空间优化)。

### 保存语义（重要）

编辑保存不是“覆盖原文件”：

1. Rust 读取原图并应用 EXIF 方向，按 `翻转 → 旋转 → 裁剪 → 缩放` 得到新像素。
2. 编码到 `.artbank/temp/edit-<uuid>.<ext>`，边写边计算 BLAKE3，写完 `sync_all`。
3. 新哈希与**另一张**图片冲突：拒绝保存并提示，不自动合并。
4. 新哈希与当前图片相同：不产生新文件，返回 `changed=false`。
5. 在同一个写事务里：检查冲突 → 把旧原图硬链接到 `.artbank/undo/<operationId>/<oldHash>.<ext>`（失败退化为复制，原路径此时仍然有效）→ 发布 `<folder>/<newHash>.<ext>`（同内容孤儿文件存在时复用）→ 追加缩略图到 pack → 更新 `images` 行（id 不变；内容哈希、扩展名、大小、宽高、格式、alpha、pHash、主色、缩略图指针、`thumb_version + 1`、`modified_at`）→ 删除该图的 `feature_blobs`（embedding/pose）→ 写入编辑前快照（含 `phash` / `colors*` / `modified_at`）与记录。
6. 标签、评分、描述、集合、导入时间与 `description_source` 保留；描述默认不随内容变化而清空。
7. 提交后删除旧原图（若已无引用）与该图旧哈希的预览缓存；暂存链接继续保留供撤销与重做。删除失败只记日志，留给维护任务。
8. 撤销编辑时，当前哈希必须等于本次新哈希（已是旧哈希则视为已完成）；用暂存文件恢复内容列、重新编码缩略图（`thumb_version + 1`）、清空 `feature_blobs`，把新原图移入 `.artbank/undo/<op>/redo-*` 而不是删除，并清掉它的预览缓存。记录保留为 `undone`。
9. 重做编辑时，当前哈希必须等于撤销后的旧哈希；把暂存的新原图移回、恢复内容列与缩略图（`thumb_version + 1`）、清空 `feature_blobs`，删除旧原图与预览缓存；旧图暂存保留，供再次撤销。`feature_blobs` 不恢复。
10. 编辑记录被新操作清空、裁剪或“清空操作记录”时，整个暂存目录删除；启动时清理没有对应 `operations.id` 的目录。
11. 失败时保留旧数据库行与旧文件；已发布的新文件、临时文件与缩略图包内的未引用 blob 由清理路径尽量处理；包内未引用 blob 允许残留。
12. 保存与导入、图库切换、文件夹移动、永久删除共用 `maintenance` 守卫：保存期间拒绝导入与关闭/切换图库；导入进行中拒绝保存。
13. 保存是异步命令，解码/编码在阻塞线程池执行；列表刷新后缩略图通过 `thumb_version` 失效，浏览器缓存的旧 URL 不会复用。

### 命令与协议

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `get_viewer_source` | `{ imageId }` | `ViewerSource` |
| `save_image_edit` | `{ request }` | `EditResult` |
| `estimate_edit_size` | `{ request }` | `EditSizeEstimate` |
| `get_image_pose` | `{ imageId }` | `PoseData \| null`（读取 feature pack） |
| `ai_generate_pose` | `{ imageId }` | `PoseData`（调用 AI 服务并写入 feature pack） |
| `copy_image` | `{ imageId }` | `null` |
| `window_set_fullscreen` | `{ value }` | `boolean`（写入后回读） |

`ViewerSource`：原图/显示尺寸、格式、大小、内容哈希、`preview`、`downsampled`、`editable`、`deleted`、`maxEditPixels`。`EditRequest`：`imageId`、`rotate`（0-3）、`flipHorizontal`、`flipVertical`、`crop`（原图坐标）、`resize`、`outputFormat`、`quality`。`EditResult`：新哈希/尺寸/格式/大小/`thumbVersion`、`changed`、`oldFileRemoved`。`EditSizeEstimate`：`{ fileSize, exact }`——只回答“大概多大”，尺寸由前端自己按几何状态算。

保存与体积预估共用同一份前置校验（`library/edit.rs` 的 `resolve`：几何参数、回收站、像素上限、输出格式、质量范围），所以两者对“能不能保存”的判断不会分叉。

前端结构：

- `src/modules/viewer/`：types、api、model、ImageViewer、ViewerTools、Filmstrip、PoseOverlay、useEditHistory（撤销栈）、useEditEstimate（体积预估）、model.test。
- `src/app/ViewerHost.tsx`：查看器与详情侧栏的编排层；全屏状态与胶片带开关也从这里接进来。详情侧栏的写入经 `session.refresh(change)` 交给图库，生成姿势后同样发特征变化；外部变化后按活动图复核存在与回收站状态，并按数据源列表复核它是否还在结果里，离开时关闭。两个前端模块不互相导入。
- `src/shared/viewerSession.ts`：图库向查看器提供已加载条目、翻页、选择同步、保存后就地更新行（`ViewerSessionUpdate`）、`refresh(change)`、`expectTrashed`、标签筛选与元数据操作的桥接契约。
- `src/shared/tagFilter.ts`：标签筛选的范围与动作类型，图库与查看器共用的最小契约。
- `src/shared/imgUrl.ts`：`img://` URL 生成。

保存后前端不重新查询首屏：查看器把 `EditResult` 里的内容哈希、扩展名、体积、宽高、格式、alpha 与 `thumbVersion` 经 `session.onSaved(imageId, update)` 交给图库，网格/相似结果只就地替换这一行的字段（`patchItem`），并追加一条 `content` 变化刷新特征/分面、退出相似结果；排序、滚动位置与深分页上下文都保留。当前排序依赖修改时间/大小/像素或 id 游标还有下一页时，后台重取已加载范围而不清空列表。

## 未实现与验收边界

- 色彩/LUT/频域滤波/卷积等图像处理已按用户决定取消，不实现，也不再保留独立编辑器入口。
- 拖出到资源管理器（Windows OLE）评估后暂缓；当前提供“复制图片”与“复制文件路径”。
- 超过 5000 万像素的图片只读；4K/8K 保存仍是单次阻塞任务，是否需要进度/取消待决策（见[待办](../planning/todo.md)）。
- 界面行为（缩放平移帧率、全屏、胶片带手感、裁剪映射、体积预估误差、剪贴板、保存耗时与内存）只能由用户在真实窗口验收，清单见[待办](../planning/todo.md)。
