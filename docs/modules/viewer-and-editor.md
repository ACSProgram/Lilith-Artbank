# 查看器与几何编辑

查看器负责“看”和“轻度几何编辑”；S5 的图像处理也复用同一条“新内容发布”路径。

## 当前实现（S4）

入口：双击网格/列表、Enter、右键“打开查看器”。查看器覆盖标题栏以下的工作区，标题栏仍可拖动窗口；Esc 关闭。上一张/下一张按当前图库查询的已加载顺序移动，接近页尾时请求下一页，不重新查询全库。关闭后网格保留最后一张的选择。

### 显示与导航

- 适应窗口 / 1:1 / 滚轮缩放（以光标为中心）/ 拖拽平移 / 双击在适应与原尺寸之间切换；缩放范围 2%-800%。
- 全屏通过 `window_toggle_fullscreen` 在 Rust 侧读写窗口状态，前端不需要额外的窗口权限。
- 键盘：Esc（裁剪中先退出裁剪；姿势叠加打开时先关闭叠加）、←/→、F、0、1、+/-；输入框聚焦时忽略，弹窗打开时不响应。打开查看器时把焦点移进覆盖层，避免网格的容器级键盘处理与查看器同时响应方向键与 Esc。
- 大图：浏览器原生格式且 ≤2400 万像素时直接加载原图；超过阈值、格式不能由 WebView2 直接解码（TIFF/TGA）、或带 EXIF 方向时，由 Rust 生成最长边 4096 的 WebP 预览，缓存在 `.artbank/temp/viewer/<id>-<hash>.webp`。打开图库会清空 `temp`，保存会删除该图旧哈希的预览。
- 带 EXIF 方向的图片也走 Rust 预览：预览已转正，避免 WebView2 的自然尺寸/方向语义与数据库宽高不一致，从而保证裁剪坐标可预测。编辑保存始终重新解码原图并应用方向。
- `img://` 协议：Windows 为 `http://img.localhost/<id>?v=<hash>`（原图）与 `http://img.localhost/preview/<id>?v=<hash>`（预览）。响应带 `Cache-Control: immutable`、ETag=内容哈希；id 不是整数返回 400，图片行/原图/预览缺失返回 404（前端据此走失败态），路径解析失败返回 500。前端统一经 `src/shared/imgUrl.ts` 生成 URL，重试参数由 `withRetry` 追加。
- 姿势叠加：33 点 MediaPipe 骨架与可见点过滤；`get_image_pose` 从 S6 的 feature pack 读取 landmark。没有可见姿势时按钮调用 `ai_generate_pose`（512px 缩略图输入），保存后立即叠加；服务返回空点集时提示未检测到人体。
- 复制图片：`copy_image` 解码原图、应用 EXIF 方向后以 RGBA 写入系统剪贴板；超过 5000 万像素给出可行动错误。
- 拖出到资源管理器（Windows OLE）：评估后暂缓到后续阶段；S4 保留“复制图片”和已有的“复制文件路径”。
- 元数据侧栏：由 `src/app/ViewerHost.tsx` 复用图库详情侧栏，评分、标签、描述、文件夹可直接编辑；回收站/恢复/永久删除成功后关闭查看器，刷新由图库侧统一完成。
- 大图内存：查看器原图由 WebView2 解码；Rust 侧编辑/预览都受 512 MiB 分配、单边 65535、1 亿像素的解码上限约束，并对 >5000 万像素的图片禁用编辑。
- 配色跟随全局主题：查看器不再固定深色，顶栏/工具面板/状态栏/输入框/裁剪框都用 `src/styles/tokens.css` 的语义变量；画布用 `--canvas`（浅色下中性灰、深色下比面板再暗一档），透明区域显示 `--checker` 棋盘格。

### 几何编辑

- 旋转 90°（0-3 次顺时针）、水平/垂直翻转、裁剪、调尺寸；全部只在保存时落到像素。
- 顺序固定为：**翻转 → 顺时针旋转 → 裁剪 → 缩放**。前端 CSS 变换使用相同顺序，裁剪选区在旋转/翻转后的显示坐标上绘制。
- 裁剪保存时按显示坐标 → 原图坐标换算；旋转为奇数时宽高比例互换，降采样预览同样按显示/原图比例换算。旋转或翻转会清除裁剪与尺寸设置，避免旧坐标失效。
- 调尺寸：锁定纵横比、百分比预设、直接输入宽高；输出上限 5000 万像素、单边 65535。
- 输出：JPEG / PNG / WebP；JPEG、WebP 质量 1-100，PNG 无损；JPEG 透明区域合成白底。默认沿用 JPEG/PNG/WebP；GIF/TIFF/TGA/BMP 按是否有 alpha 默认转 PNG/JPEG，转换本身算一次修改。GIF 编辑只保留首帧。
- 超过 5000 万像素的图片在查看器中只读（仍可看降采样预览）；回收站中的图片只读。
- 工具面板实时显示原图、裁剪后、输出尺寸；“未保存”标记只反映几何/尺寸修改，格式转换本身不点亮标记但会启用保存。

### 保存语义（重要）

编辑保存不是“覆盖原文件”：

1. Rust 读取原图并应用 EXIF 方向，按 `翻转 → 旋转 → 裁剪 → 缩放` 得到新像素。
2. 编码到 `.artbank/temp/edit-<uuid>.<ext>`，边写边计算 BLAKE3，写完 `sync_all`。
3. 新哈希与**另一张**图片冲突：拒绝保存并提示，不自动合并。
4. 新哈希与当前图片相同：不产生新文件，返回 `changed=false`。
5. 在同一个写事务里：检查冲突 → 发布 `<folder>/<newHash>.<ext>`（同内容孤儿文件存在时复用）→ 追加缩略图到 pack → 更新 `images` 行（id 不变；内容哈希、扩展名、大小、宽高、格式、alpha、pHash、主色、缩略图指针、`thumb_version + 1`、`modified_at`）→ 删除该图的 `feature_blobs`（embedding/pose）。
6. 标签、评分、描述、集合、导入时间与 `description_source` 保留；描述默认不随内容变化而清空。
7. 提交后删除旧原图（若已无引用）与该图旧哈希的预览缓存；删除失败只记日志，留给维护任务。
8. 失败时保留旧数据库行与旧文件；已发布的新文件、临时文件与缩略图包内的未引用 blob 由清理路径尽量处理；包内未引用 blob 允许残留。
9. 保存与导入、图库切换、文件夹移动、永久删除共用 `maintenance` 守卫：保存期间拒绝导入与关闭/切换图库；导入进行中拒绝保存。
10. 保存是异步命令，解码/编码在阻塞线程池执行；列表刷新后缩略图通过 `thumb_version` 失效，浏览器缓存的旧 URL 不会复用。

### 命令与协议

| 命令 | 输入 | 输出 |
| --- | --- | --- |
| `get_viewer_source` | `{ imageId }` | `ViewerSource` |
| `save_image_edit` | `{ request }` | `EditResult` |
| `get_image_pose` | `{ imageId }` | `PoseData \| null`（读取 feature pack） |
| `ai_generate_pose` | `{ imageId }` | `PoseData`（调用 AI 服务并写入 feature pack） |
| `copy_image` | `{ imageId }` | `null` |
| `window_toggle_fullscreen` | — | `boolean` |

`ViewerSource`：原图/显示尺寸、格式、大小、内容哈希、`preview`、`downsampled`、`editable`、`deleted`、`maxEditPixels`。`EditRequest`：`imageId`、`rotate`（0-3）、`flipHorizontal`、`flipVertical`、`crop`（原图坐标）、`resize`、`outputFormat`、`quality`。`EditResult`：新哈希/尺寸/格式/大小/`thumbVersion`、`changed`、`oldFileRemoved`。

前端结构：

- `src/modules/viewer/`：types、api、model、ImageViewer、ViewerTools、PoseOverlay。
- `src/app/ViewerHost.tsx`：查看器与详情侧栏的编排层；两个前端模块不互相导入。
- `src/shared/viewerSession.ts`：图库向查看器提供已加载条目、翻页、选择同步与元数据操作的桥接契约。
- `src/shared/imgUrl.ts`：`img://` URL 生成。

## 图像处理（S5）

参考原 `image_editor`，但把“全分辨率 Mat 快照栈”改为“参数化操作链”。

### 操作

| 操作 | 参数 | 说明 |
| --- | --- | --- |
| 色彩变换 | 输入/输出 gamma，3×3 RGB 矩阵 | 在去 gamma 的线性空间做矩阵，再回 gamma |
| LUT3D | `.cube` 文件、强度 0-1 | 三线性插值 |
| 频域滤波 | 频率曲线（控制点）、强度 0-1 | 在 Lab-L 通道做 DFT，按径向频率曲线缩放幅度，再 IDFT |
| 卷积 | 7×7 核、强度 0-1、预设 | 在 Lab-L 通道卷积；预设来自设置/内置 JSON |

后续可加：曲线、色温、锐化。先保证原功能。

### 预览

- 预览在**降采样副本**（最长边约 1280px）上由 Rust 渲染，返回 JPEG/WebP 字节或临时文件 URL。
- 参数变化防抖（约 80-120ms）；只允许一个预览任务在跑，新请求取消旧任务。
- 预览与最终保存调用同一份算子实现，保证 WYSIWYG；不在前端用 CSS/WebGL 复制算法。
- 1MP 预览的色彩变换/卷积通常在数毫秒到数十毫秒；DFT 较慢，单独显示“计算中”。
- 操作链是数组 `[{kind, params}]`；撤销/重做移动索引，不保存像素副本。
- 预览可从最近一次中间结果继续，但缓存只保留有限步，内存有上限。

### 保存

- 把操作链应用到全分辨率原图；按目标格式编码。
- 与查看器共用“新内容发布”流程。
- 原图格式不支持透明/高位深时，按输出格式转换；转换规则在 UI 中显示。
- 失败时保留编辑会话，用户可以继续改或另存。

## 独立编辑器

原软件有独立的“图像处理”窗口，可从剪贴板/文件载入。这里保留为同一编辑器组件的“独立会话”：

- 不绑定 `images` 行时，“保存”走“另存为文件”而不是更新图库。
- 绑定图片时走上面的图库保存。
- 不引入第二套算子实现。

## 验收

S4 已自动验证：

- 几何算子：翻转先于旋转、裁剪坐标、裁剪后缩放、非法参数拒绝；JPEG/PNG/WebP 可解码，PNG 保留 alpha，JPEG 透明合成白底。
- 保存事务：旋转保存后宽高交换、旧文件删除、新文件存在、缩略图指针可读、`thumb_version` 递增；标签/评分/描述/集合保留，`feature_blobs` 清空；冲突拒绝；无修改拒绝；回收站图片拒绝。
- 查看器源：小图直接服务；小图强制预览会缓存并复用；5000px 长边预览被压到 4096；EXIF 方向 6 的 JPEG 使用已转正预览；原图缺失报错；回收站只读。
- 前端 `npm run typecheck`、`npm run build` 通过；无头 Chromium 预览验证了自适应布局、旋转、裁剪拖拽→1,024×1,024、调尺寸、重置、上下张与侧栏联动、侧栏开关、滚轮缩放。
- Rust 侧 `cargo fmt --check`、`cargo check`、`cargo clippy --all-targets`、`cargo test --lib` 通过。

仍待用户手工验收：

- 真实 WebView2 的原图/大图预览加载、缩放平移帧率、全屏、EXIF 方向图显示与裁剪一致性。
- 真实图库中的保存 round-trip：旧原图删除、缩略图立即更新、pHash/主色、embedding/pose 清除、标签/评分/集合保留。
- 剪贴板图片在真实 Windows 程序中的粘贴；4K/8K 保存耗时与内存峰值。
- 拖出到资源管理器仍暂缓，未实现。
