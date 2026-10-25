"""把请求体里的原始图片字节解码成 RGB PIL Image。"""

from __future__ import annotations

import io

from PIL import Image, ImageOps


class ImageDecodeError(ValueError):
    pass


def decode_image(data: bytes) -> Image.Image:
    if not data:
        raise ImageDecodeError("图片内容为空")
    try:
        image = Image.open(io.BytesIO(data))
        image.load()
        image = ImageOps.exif_transpose(image)
        return image.convert("RGB")
    except Exception as error:
        raise ImageDecodeError(f"无法解码图片：{error}") from error
