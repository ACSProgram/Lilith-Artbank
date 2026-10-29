"""把请求体里的原始图片字节解码成 RGB PIL Image。"""

from __future__ import annotations

import io

from PIL import Image, ImageOps

# 单张图解码上限，与导入侧的解码上限一致；超过就不解，避免一张巨图把进程内存顶爆。
MAX_PIXELS = 100_000_000


class ImageDecodeError(ValueError):
    pass


def decode_image(data: bytes) -> Image.Image:
    if not data:
        raise ImageDecodeError("图片内容为空")
    try:
        image = Image.open(io.BytesIO(data))
        # 尺寸在 load() 之前就能读到，先拦一次：巨图一旦真解码，内存已经顶上去了。
        width, height = image.size
        if width * height > MAX_PIXELS:
            raise ImageDecodeError(
                f"图片过大：{width}×{height}，单张上限 {MAX_PIXELS:,} 像素"
            )
        image.load()
        return ImageOps.exif_transpose(image).convert("RGB")
    except ImageDecodeError:
        raise
    except Exception as error:
        raise ImageDecodeError(f"无法解码图片：{error}") from error
