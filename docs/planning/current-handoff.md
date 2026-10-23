# 当前阶段状态

日期：2026-10-23
阶段：**S0 工程骨架已完成，S1 尚未开始**

## 已完成

- git 仓库、Tauri 2 + React 19 + TypeScript + Vite 6 工程。
- 无边框自绘标题栏、深色应用外壳、侧栏、欢迎页、设置弹窗、新建/打开图库弹窗、通知与确认弹窗。
- 设置持久化（图库列表、AI 参数、性能参数、界面参数），原子写入与损坏备份。
- 图库清单 `.artbank/library.json`、SQLite schema v1、单写 + 多读连接池、WAL/FTS5。
- 图库创建/打开/关闭/移除/重命名、状态查询、窗口控制命令。
- 浏览器预览 mock：无 Tauri 时可用界面创建/打开演示图库，不写磁盘。
- 性能基准：
  - 20 万图片 + 60 万标签关系写入 7.01s；
  - 首屏 150 条 0.12ms、深分页 0.53ms、文件夹 0.07ms、标签 1.95ms、FTS 0.48ms；
  - 150×20KB 随机读：pack 0.60ms vs 逐文件 9.69ms；
  - 150 张 512px：JPEG 冷 456ms/热 236ms，WebP 冷 437ms。
- 文档架构、路线图、性能报告、验证策略。

## 验证结果

已运行并通过：

```text
npm run typecheck
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml   # 9 passed
cargo build --manifest-path src-tauri/Cargo.toml  # debug exe 11 MB
```

debug exe 的导入表只有系统 DLL（kernel32/user32/UCRT API 集等），没有 `WebView2Loader.dll`、`vcruntime140.dll` 或项目自带 DLL。发布构建的依赖清单在 S9 再核对一次。

浏览器预览（Chromium 150）检查过：欢迎页、创建图库弹窗、mock 建库后进入工作区。未使用输入注入驱动真实窗口。

## 未验证

- 真实 Tauri 窗口的启动、窗口拖动/最小化/最大化/关闭、设置持久化（需用户手动验收）。
- `cargo build` 链接后的 exe 与发布产物的 DLL 清单。
- WebView2 自定义 `thumb://` 协议（S1 第一半）。
- WebP 编码器选型（S1 第一半）。
- 真实图库导入/浏览/编辑（S1 起）。

## 下一步

S1 第一半：性能原型。

1. 注册 `thumb://` 协议，从 pack 按偏移读字节。
2. 最小 pack 写入器/读取器 + 150 张触发页，在真实 Tauri 窗口测冷/热加载。
3. 确认 WebP 编码方案与发布产物无额外 DLL。
4. 门槛通过后再做导入管线。

## 约束

- `G:\Lilith Image Library` 未修改；所有基准使用临时目录。
- 不读旧 `.data`；迁移脚本不在本仓库。
- 不做 PinBoard/PasteBoard/调色盘/色彩测试/截屏。
