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
          Lilith Artbank 面向数十万张图片、数百 GB 的美术素材仓库。一个图库、一棵真实文件夹树，
          原图留在原处，浏览和搜索都在本地完成。
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
          <p>几十万张素材也能顺畅翻看，打开就能用。</p>
        </article>
        <article>
          <Gauge size={18} />
          <h3>翻到哪都有图</h3>
          <p>停下的地方立刻出缩略图，不需要等整页加载。</p>
        </article>
        <article>
          <ShieldCheck size={18} />
          <h3>原图不动</h3>
          <p>导入只复制不移动；编辑会另存为新文件，原图不会被悄悄覆盖。</p>
        </article>
      </div>
    </div>
  );
}
