import { useEffect, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import { ImageOff, RefreshCw } from "lucide-react";
import { hasDecodedThumb, rememberThumb } from "../../shared/thumbCache";
import { thumbnailUrl } from "../../shared/thumbUrl";
import type { ImageRow } from "./types";

interface LibraryThumbnailProps {
  image: ImageRow;
  className?: string;
}

export function LibraryThumbnail({ image, className }: LibraryThumbnailProps) {
  const [attempt, setAttempt] = useState(0);
  const [failed, setFailed] = useState(false);
  const url = thumbnailUrl(image.id, image.thumbVersion, attempt);
  // 挂载时先问一次解码缓存：命中就不放骨架屏，直接出图。
  const [loaded, setLoaded] = useState(() => hasDecodedThumb(url));
  const imgRef = useRef<HTMLImageElement>(null);

  useEffect(() => {
    setLoaded(hasDecodedThumb(url));
    setFailed(false);
  }, [url]);

  useEffect(() => {
    // 缓存命中时 load 事件可能在 React 挂上监听前就已触发，挂载后补一次检查。
    const element = imgRef.current;
    if (element?.complete && element.naturalWidth > 0) {
      setLoaded(true);
      rememberThumb(url, element);
    }
  }, [url]);

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
          src={url}
          alt=""
          draggable={false}
          decoding="async"
          onLoad={(event) => {
            setLoaded(true);
            rememberThumb(url, event.currentTarget);
          }}
          onError={() => {
            setLoaded(false);
            setFailed(true);
          }}
        />
      )}
    </span>
  );
}
