import { ArrowRight, Database, FolderPlus, Gauge, Images, ShieldCheck } from "lucide-react";
import { Button } from "../shared/components/Button";
import { browserPreview } from "./api";
import { useNotices } from "./noticeContext";
import { useAppRuntime } from "./runtimeContext";
import { errorMessage } from "../shared/errors";

export function WelcomeView({ onAddLibrary }: { onAddLibrary: () => void }) {
  const { settings, openLibrary } = useAppRuntime();
  const { notify } = useNotices();
  const libraries = settings?.libraries ?? [];

  const handleOpen = async (libraryId: string) => {
    try {
      await openLibrary(libraryId);
    } catch (error) {
      notify("打开图库失败", "error", errorMessage(error));
    }
  };

  return (
    <div className="welcome">
      <div className="welcome-hero">
        <div className="welcome-mark">A</div>
        <span className="welcome-eyebrow">LOCAL ART ASSET LIBRARY</span>
        <h1>
          把几十万张素材
          <br />
          收进一个真正快的图库
        </h1>
        <p className="welcome-lede">
          Lilith Artbank 面向数十万张图片、数百 GB 的美术素材仓库。元数据走 SQLite，
          缩略图走追加式包文件，原图保持内容寻址；界面只是视图，重活都在 Rust 里完成。
        </p>
        <div className="welcome-actions">
          <Button variant="primary" size="md" icon={<FolderPlus size={17} />} onClick={onAddLibrary}>
            打开或创建图库
          </Button>
          <span className="welcome-stage">Stage 2 · 虚拟网格与查询</span>
        </div>
        {browserPreview ? (
          <div className="welcome-preview-note">
            当前是浏览器预览模式：界面可用，但不会读写真实图库。
          </div>
        ) : null}
      </div>

      <div className="welcome-features">
        <article>
          <Images size={18} />
          <h3>为大规模设计</h3>
          <p>页面只取可见条目，元数据不整库载入内存；分页使用键集游标，深翻不退化。</p>
        </article>
        <article>
          <Gauge size={18} />
          <h3>按加载路径优化</h3>
          <p>缩略图包顺序写、按偏移读；浏览器只解码当前页的图片，缓存可随时重建。</p>
        </article>
        <article>
          <ShieldCheck size={18} />
          <h3>原图不动</h3>
          <p>导入只复制到图库目录并按内容哈希命名；编辑产生新文件，旧版本不会被悄悄覆盖。</p>
        </article>
      </div>

      {libraries.length > 0 ? (
        <section className="recent-libraries">
          <div className="recent-heading">
            <Database size={16} />
            <h2>最近的图库</h2>
          </div>
          <div className="recent-list">
            {libraries
              .slice()
              .sort((left, right) => right.addedAt - left.addedAt)
              .slice(0, 6)
              .map((library) => (
                <button
                  key={library.id}
                  type="button"
                  className="recent-item"
                  onClick={() => void handleOpen(library.id)}
                >
                  <span className="recent-item-text">
                    <span className="recent-item-name">{library.name}</span>
                    <span className="recent-item-path">{library.path}</span>
                  </span>
                  <ArrowRight size={16} />
                </button>
              ))}
          </div>
        </section>
      ) : null}
    </div>
  );
}
