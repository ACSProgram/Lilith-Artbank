import { AlertTriangle, ImageOff } from "lucide-react";
import { Button } from "../../shared/components/Button";
import { Spinner } from "../../shared/components/Spinner";
import { hasActiveFilters, hasActiveQuery, type BrowseState } from "./model";
import type { QueryStatus } from "./useImageQuery";
import type { LibraryView as LibraryViewType } from "./types";

interface LibraryStateOverlayProps {
  status: QueryStatus;
  hasItems: boolean;
  error: string | null;
  effectiveView: LibraryViewType;
  browse: BrowseState;
  similarityActive: boolean;
  similarMode: boolean;
  similarityStage: string | null;
  onRetry: () => void;
  onResetFilters: () => void;
  onClearSearch: () => void;
}

/** 网格/列表上方的整页状态：首次载入、读取失败与空结果。 */
export function LibraryStateOverlay({
  status,
  hasItems,
  error,
  effectiveView,
  browse,
  similarityActive,
  similarMode,
  similarityStage,
  onRetry,
  onResetFilters,
  onClearSearch,
}: LibraryStateOverlayProps) {
  const noResults = status === "ready" && !hasItems;
  return (
    <>
      {status === "loading" && !hasItems ? (
        <div className="library-state">
          <Spinner size={22} />
          <strong>
            {similarityActive
              ? similarMode
                ? "正在查找相似图片"
                : "正在理解描述"
              : "正在读取图片"}
          </strong>
          <span>
            {similarityActive
              ? similarMode
                ? "正在搜索…"
                : (similarityStage ?? "首次搜索需要先加载文本模型，请稍候。")
              : "正在读取…"}
          </span>
        </div>
      ) : null}

      {status === "error" ? (
        <div className="library-state library-state-error">
          <AlertTriangle size={24} />
          <strong>图片列表读取失败</strong>
          <span>{error}</span>
          <Button size="sm" variant="primary" onClick={onRetry}>
            重试
          </Button>
        </div>
      ) : null}

      {noResults ? (
        <div className="library-state">
          <ImageOff size={24} />
          <strong>
            {similarityActive
              ? similarMode
                ? "没有找到相似的图片"
                : "没有语义匹配的图片"
              : effectiveView === "trash"
                ? "回收站是空的"
                : hasActiveQuery(browse)
                  ? "没有符合条件的图片"
                  : "当前视图没有可显示的图片"}
          </strong>
          <span>
            {similarityActive
              ? similarMode
                ? "这张图片还没有可比较的特征，或者库里没有足够相似的图片。"
                : "换一种说法再试。"
              : hasActiveQuery(browse)
                ? "调整搜索或筛选条件，或者清除后查看全部图片。"
                : effectiveView === "trash"
                  ? "从图库移入回收站的图片会显示在这里，可以恢复或永久删除。"
                  : effectiveView === "collection"
                    ? "把选中的图片加入这个集合后，它们会出现在这里。"
                    : "导入图片后会显示在这里。"}
          </span>
          {similarityActive ? null : hasActiveFilters(browse.filters) ? (
            <Button size="sm" variant="primary" onClick={onResetFilters}>
              清除筛选
            </Button>
          ) : browse.search ? (
            <Button size="sm" variant="primary" onClick={onClearSearch}>
              清除搜索
            </Button>
          ) : null}
        </div>
      ) : null}
    </>
  );
}
