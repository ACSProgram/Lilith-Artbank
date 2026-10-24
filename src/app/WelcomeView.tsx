import { FolderPlus, Gauge, Images, ShieldCheck } from "lucide-react";
import { Button } from "../shared/components/Button";
import { browserPreview } from "./api";

export function WelcomeView({ onAddLibrary }: { onAddLibrary: () => void }) {
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
          Lilith Artbank 面向数十万张图片、数百 GB 的美术素材仓库。一个图库、一棵真实文件夹树；
          元数据走 SQLite，缩略图走追加式包文件，原图保持内容寻址。
        </p>
        <div className="welcome-actions">
          <Button variant="primary" size="md" icon={<FolderPlus size={17} />} onClick={onAddLibrary}>
            打开或创建图库
          </Button>
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
    </div>
  );
}
