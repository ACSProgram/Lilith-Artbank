"""拟合 SPP / MaxSim 白化基。

输入是一批图片的 last_hidden_state（以及池化头注意力与 V 投影 patch）。两种取数方式：

  --features features.npz    预先抽好的特征：
                             patch / last_hidden_state  (n, 576, 1536)
                             attn / attention           (n, 576)
                             patch_v（可选，V 投影后的 patch）
  --images <目录或路径清单> --model-dir <模型目录>
                             现场跑图片塔抽特征

输出到 `--out-dir`（默认模型目录，即通用兜底位置）：

  spp_basis.npz     mean(1536) + projection(1536×128 f32) + scale
  maxsim_basis.npz  mean(1536) + projection(1536×64 f32) + scale

正式库由应用自动在库内 `.artbank/basis/<clip_model>/` 拟合，不需要手工运行本脚本；
这里主要用于准备模型目录里的通用兜底基，或研究阶段对比不同样本。基文件带 version
与训练样本数；换一次基，全库描述子按新包名重新生成。

示例：

  python server/tools/fit_basis.py --features research/patch-retrieval/out/features.npz \
      --model-dir resources/models/siglip2-giant-opt-patch16-384

  python server/tools/fit_basis.py --images samples.txt \
      --model-dir resources/models/siglip2-giant-opt-patch16-384
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np

SERVER_DIR = Path(__file__).resolve().parent.parent
if str(SERVER_DIR) not in sys.path:
    sys.path.insert(0, str(SERVER_DIR))

from image_features import (  # noqa: E402
    BASIS_SCALE,
    BASIS_VERSION,
    MAXSIM_DIM,
    MAXSIM_TOP,
    SPP_CELLS,
    SPP_DIM,
    MaxsimAccumulator,
    SppAccumulator,
    capture_attention,
    v_projection,
)

VISION_PREFIX = "vision_model."
IMAGE_EXTS = {".jpg", ".jpeg", ".png", ".webp", ".bmp"}


def pick_device(torch, args) -> tuple[object, object]:
    """请求 cuda 但没有可用显卡时退回 cpu；cpu 一律 fp32。"""
    device = torch.device(
        args.device if args.device == "cpu" or torch.cuda.is_available() else "cpu"
    )
    dtype = torch.float32 if args.fp32 or device.type == "cpu" else torch.float16
    return device, dtype


def resolve_images(path: Path) -> list[Path]:
    if path.is_dir():
        return sorted(
            item for item in path.rglob("*") if item.suffix.lower() in IMAGE_EXTS
        )
    lines = path.read_text(encoding="utf-8").splitlines()
    images = [Path(line.strip()) for line in lines if line.strip() and not line.startswith("#")]
    missing = [str(item) for item in images if not item.is_file()]
    if missing:
        raise SystemExit("清单里有不存在的图片：\n" + "\n".join(missing[:10]))
    return images


def load_vision_model(model_dir: Path, device, dtype):
    import torch
    from safetensors import safe_open
    from transformers import SiglipVisionConfig, SiglipVisionModel

    raw = json.loads((model_dir / "config.json").read_text(encoding="utf-8"))
    config = SiglipVisionConfig.from_dict(raw["vision_config"])
    index_path = model_dir / "model.safetensors.index.json"
    if index_path.is_file():
        weight_map = json.loads(index_path.read_text(encoding="utf-8"))["weight_map"]
        by_shard: dict[str, list[str]] = {}
        for key, shard in weight_map.items():
            if key.startswith(VISION_PREFIX):
                by_shard.setdefault(shard, []).append(key)
    else:
        by_shard = {}
        with safe_open(model_dir / "model.safetensors", framework="pt", device="cpu") as handle:
            keys = [key for key in handle.keys() if key.startswith(VISION_PREFIX)]
        if keys:
            by_shard["model.safetensors"] = keys
    if not by_shard:
        raise SystemExit(f"{model_dir} 的权重里没有 {VISION_PREFIX}* 张量")

    state = {}
    for shard, keys in by_shard.items():
        try:
            handle = safe_open(model_dir / shard, framework="pt", device=str(device))
        except Exception:
            handle = safe_open(model_dir / shard, framework="pt", device="cpu")
        with handle:
            for key in keys:
                state[key] = handle.get_tensor(key).to(dtype=dtype)
    with torch.device("meta"):
        model = SiglipVisionModel(config)
    model.load_state_dict(state, strict=True, assign=True)
    for module in model.modules():
        for name, buffer in list(module._buffers.items()):
            if buffer is None or not buffer.is_meta:
                continue
            if name == "position_ids" and hasattr(module, "position_embedding"):
                count = module.position_embedding.num_embeddings
                module._buffers[name] = torch.arange(count).expand((1, -1))
    model.to(device).eval()
    return model


def read_npz_features(path: Path):
    with np.load(path, allow_pickle=False) as data:
        patch = data.get("patch")
        if patch is None:
            patch = data.get("last_hidden_state")
        attention = data.get("attn")
        if attention is None:
            attention = data.get("attention")
        if patch is None or attention is None:
            raise SystemExit("特征文件必须包含 patch/last_hidden_state 与 attn/attention")
        patch_v = data.get("patch_v")
        return patch, attention, patch_v


def main() -> int:
    parser = argparse.ArgumentParser(description="拟合 SPP / MaxSim 白化基")
    parser.add_argument("--features", type=Path, help="预先抽好的特征 npz")
    parser.add_argument("--images", type=Path, help="图片目录或每行一个路径的清单")
    parser.add_argument("--model-dir", type=Path, help="模型目录；V 投影权重与默认输出位置")
    parser.add_argument("--out-dir", type=Path, help="输出目录，默认与 model-dir 相同")
    parser.add_argument(
        "--maxsim-sample",
        type=int,
        default=1400,
        help="拟合 MaxSim 基的图片数，与应用内默认值一致",
    )
    parser.add_argument("--limit", type=int, default=0, help="只处理前 N 张，0 表示全部")
    parser.add_argument("--device", choices=["cuda", "cpu"], default="cuda")
    parser.add_argument("--fp32", action="store_true", help="显卡上也用 fp32 推理")
    args = parser.parse_args()

    if bool(args.features) == bool(args.images):
        raise SystemExit("请二选一：--features 或 --images")

    out_dir = args.out_dir or args.model_dir
    if out_dir is None:
        raise SystemExit("需要 --model-dir 或 --out-dir")
    out_dir.mkdir(parents=True, exist_ok=True)

    if args.features:
        patch, attention, patch_v = read_npz_features(args.features)
        total = int(patch.shape[0])
        if args.limit:
            total = min(total, args.limit)
        if total == 0:
            raise SystemExit("特征文件里没有样本")
        spp = SppAccumulator()
        maxsim = MaxsimAccumulator(args.maxsim_sample)
        weight_np = bias_np = None
        if patch_v is None:
            if args.model_dir is None:
                raise SystemExit("特征文件没有 patch_v，需要 --model-dir 计算 V 投影")
            import torch

            device, dtype = pick_device(torch, args)
            model = load_vision_model(args.model_dir, device, dtype)
            weight, bias = v_projection(model)
            weight_np = weight.detach().cpu().numpy()
            bias_np = bias.detach().cpu().numpy()

        for index in range(total):
            hidden = np.asarray(patch[index], dtype=np.float32)
            weights = np.asarray(attention[index], dtype=np.float32)
            if patch_v is not None:
                projected = np.asarray(patch_v[index], dtype=np.float32)
            else:
                projected = hidden @ weight_np.T + bias_np
            spp.add(hidden)
            maxsim.add(weights, projected)
            if (index + 1) % 100 == 0 or index + 1 == total:
                print(f"  已处理 {index + 1}/{total} 张", flush=True)
    else:
        import torch
        from PIL import Image, ImageOps
        from transformers import SiglipImageProcessor

        if args.model_dir is None:
            raise SystemExit("--images 模式需要 --model-dir")
        paths = resolve_images(args.images)
        if args.limit:
            paths = paths[: args.limit]
        if not paths:
            raise SystemExit("没有找到图片")
        device, dtype = pick_device(torch, args)
        model = load_vision_model(args.model_dir, device, dtype)
        processor = SiglipImageProcessor.from_pretrained(args.model_dir)
        weight, bias = v_projection(model)
        captured: dict = {}
        model.vision_model.head.attention.register_forward_hook(capture_attention(captured))
        spp = SppAccumulator()
        maxsim = MaxsimAccumulator(args.maxsim_sample)

        for index, path in enumerate(paths):
            with Image.open(path) as source:
                image = ImageOps.exif_transpose(source).convert("RGB")
            pixel_values = processor(images=image, return_tensors="pt")[
                "pixel_values"
            ].to(device, dtype)
            image.close()
            with torch.inference_mode():
                output = model(pixel_values=pixel_values)
                hidden = output.last_hidden_state[0].float()
                projected = hidden @ weight.T + bias
            weights = captured.get("weights")
            if weights is None:
                raise SystemExit("没有捕获到池化头注意力")
            spp.add(hidden.detach().cpu().numpy())
            maxsim.add(
                weights.detach().cpu().numpy(), projected.detach().cpu().numpy()
            )
            if (index + 1) % 50 == 0 or index + 1 == len(paths):
                print(f"  已处理 {index + 1}/{len(paths)} 张", flush=True)

    print("拟合 SPP 白化基…", flush=True)
    spp_mean, spp_projection = spp.finish(SPP_DIM)
    print("拟合 MaxSim 白化基…", flush=True)
    maxsim_mean, maxsim_projection = maxsim.finish(MAXSIM_DIM)

    in_dim = spp_mean.size
    save_basis(
        out_dir / "spp_basis.npz",
        kind="spp",
        mean=spp_mean,
        projection=spp_projection,
        in_dim=in_dim,
        out_dim=SPP_DIM,
        samples=spp.images,
        cells=SPP_CELLS,
    )
    save_basis(
        out_dir / "maxsim_basis.npz",
        kind="maxsim",
        mean=maxsim_mean,
        projection=maxsim_projection,
        in_dim=in_dim,
        out_dim=MAXSIM_DIM,
        samples=maxsim.samples,
        top=MAXSIM_TOP,
    )
    print(
        f"已写入 {out_dir}：SPP {spp.images} 张 · MaxSim {maxsim.samples} 张 · "
        f"scale={BASIS_SCALE}",
        flush=True,
    )
    return 0


def save_basis(
    path: Path,
    *,
    kind: str,
    mean: np.ndarray,
    projection: np.ndarray,
    in_dim: int,
    out_dim: int,
    samples: int,
    **extra: int,
) -> None:
    np.savez(
        path,
        version=np.int32(BASIS_VERSION),
        kind=np.array(kind),
        samples=np.int32(samples),
        in_dim=np.int32(in_dim),
        out_dim=np.int32(out_dim),
        mean=np.asarray(mean, dtype=np.float32),
        projection=np.asarray(projection, dtype=np.float32),
        scale=np.float32(BASIS_SCALE),
        **{key: np.int32(value) for key, value in extra.items()},
    )


if __name__ == "__main__":
    raise SystemExit(main())
