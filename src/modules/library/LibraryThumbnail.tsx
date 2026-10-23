import { useEffect, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import { ImageOff, RefreshCw } from "lucide-react";
import { thumbnailUrl } from "../../shared/thumbUrl";
import type { ImageRow } from "./types";

interface LibraryThumbnailProps {
  image: ImageRow;
  className?: string;
}

export function LibraryThumbnail({ image, className }: LibraryThumbnailProps) {
  const [attempt, setAttempt] = useState(0);
  const [failed, setFailed] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const imgRef = useRef<HTMLImageElement>(null);

  useEffect(() => {
    setAttempt(0);
    setFailed(false);
    setLoaded(false);
  }, [image.id, image.thumbVersion]);

  useEffect(() => {
    // 缓存命中时 load 事件可能在 React 挂上监听前就已触发，挂载后补一次检查。
    const element = imgRef.current;
    if (element?.complete && element.naturalWidth > 0) setLoaded(true);
  }, [image.id, image.thumbVersion, attempt]);

  const retry = (event: ReactMouseEvent) => {
    event.stopPropagation();
    event.preventDefault();
    setFailed(false);
    setLoaded(false);
    setAttempt((value) => value + 1);
  };

  return (
    <span
      className={`thumb${loaded ? " is-loaded" : ""}${failed ? " is-failed" : ""}${
        className ? ` ${className}` : ""
      }`}
    >
      {failed ? (
        <button
          type="button"
          className="thumb-retry"
          title="缩略图加载失败，点击重试"
          onClick={retry}
        >
          <ImageOff size={15} />
          <RefreshCw size={11} />
        </button>
      ) : (
        <img
          ref={imgRef}
          src={thumbnailUrl(image.id, image.thumbVersion, attempt)}
          alt=""
          draggable={false}
          decoding="async"
          onLoad={() => setLoaded(true)}
          onError={() => {
            setLoaded(false);
            setFailed(true);
          }}
        />
      )}
    </span>
  );
}
