"""语义特征描述子：SPP 构图与 MaxSim 局部匹配。

两种描述子与全局 embedding 出自同一次图片塔前向：

  SPP     24×24 patch → 8×8 均值池化 → 白化到 128 维 → 每格 L2 归一化 → int8
  MaxSim  注意力 top-32 → 池化头 V 投影 → 白化到 64 维 → 每位 L2 归一化 → int8

白化基是离线拟合产物，与模型一起冻结：``spp_basis.npz`` / ``maxsim_basis.npz``
放在模型目录（与 ``config.json`` 同级）。基缺失时图片能力整体不可用，不做静默降级。

两种描述子都会摊平成一条 int8 记录；量化的固定 scale 写在基文件里，
所以相似度计算（余弦/点积）中 scale 会自然抵消，读取端不需要逐条元数据。
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

import numpy as np

BASIS_VERSION = 1
BASIS_SCALE = 127.0

SPP_CELLS = 8
SPP_DIM = 128
SPP_RECORD_DIM = SPP_CELLS * SPP_CELLS * SPP_DIM  # 8192

MAXSIM_TOP = 32
MAXSIM_DIM = 64
MAXSIM_RECORD_DIM = MAXSIM_TOP * MAXSIM_DIM  # 2048


class FeatureBasisMissing(RuntimeError):
    """白化基文件缺失或格式不对：图片能力不可用。"""


@dataclass(frozen=True)
class Basis:
    kind: str
    version: int
    samples: int
    in_dim: int
    out_dim: int
    mean: np.ndarray
    projection: np.ndarray
    scale: float

    def whiten(self, vectors: np.ndarray) -> np.ndarray:
        """减均值、投影、逐行 L2 归一化，再量化成 int8。"""
        centred = vectors.astype(np.float32) - self.mean
        if centred.shape[-1] != self.in_dim:
            raise ValueError(
                f"{self.kind} 白化基输入维度 {self.in_dim} 与特征 {centred.shape[-1]} 不一致"
            )
        projected = centred @ self.projection
        norms = np.linalg.norm(projected, axis=-1, keepdims=True)
        normalized = projected / np.maximum(norms, 1e-8)
        quantized = np.rint(np.clip(normalized * self.scale, -127.0, 127.0))
        return quantized.astype(np.int8)


def load_basis(model_dir: Path, kind: str) -> Basis:
    path = model_dir / f"{kind}_basis.npz"
    if not path.is_file():
        raise FeatureBasisMissing(
            f"缺少{_label(kind)}白化基 {path.name}；请先用 server/tools/fit_basis.py 生成"
        )
    try:
        with np.load(path, allow_pickle=False) as data:
            stored_kind = str(data["kind"].item()) if "kind" in data else kind
            version = int(data["version"])
            samples = int(data["samples"])
            in_dim = int(data["in_dim"]) if "in_dim" in data else int(data["mean"].size)
            out_dim = int(data["out_dim"]) if "out_dim" in data else int(data["projection"].shape[1])
            mean = np.asarray(data["mean"], dtype=np.float32).reshape(-1)
            projection = np.asarray(data["projection"], dtype=np.float32)
            scale = float(data["scale"])
    except Exception as error:
        raise FeatureBasisMissing(f"{path.name} 无法读取：{error}") from error

    expected_out = SPP_DIM if kind == "spp" else MAXSIM_DIM
    if stored_kind != kind:
        raise FeatureBasisMissing(f"{path.name} 的类型是 {stored_kind}，应为 {kind}")
    if version != BASIS_VERSION:
        raise FeatureBasisMissing(f"{path.name} 版本 {version} 不受支持")
    if samples <= 0:
        raise FeatureBasisMissing(f"{path.name} 没有训练样本数")
    if mean.size != in_dim or projection.ndim != 2:
        raise FeatureBasisMissing(f"{path.name} 的 mean 或 projection 形状无效")
    if projection.shape != (in_dim, out_dim) or out_dim != expected_out:
        raise FeatureBasisMissing(
            f"{path.name} 的 projection 形状 {projection.shape} 无效，应为 ({in_dim}, {expected_out})"
        )
    if not math.isfinite(scale) or scale <= 0.0:
        raise FeatureBasisMissing(f"{path.name} 的 int8 scale 无效")
    return Basis(
        kind=kind,
        version=version,
        samples=samples,
        in_dim=in_dim,
        out_dim=out_dim,
        mean=mean,
        projection=projection,
        scale=scale,
    )


def load_image_bases(model_dir: Path) -> tuple[Basis, Basis]:
    """图片塔加载时一并载入两份基；任一缺失都让图片能力不可用。"""
    return load_basis(model_dir, "spp"), load_basis(model_dir, "maxsim")


def v_projection(model: Any) -> tuple[Any, Any]:
    """池化头注意力里的 V 投影（in_proj_weight 的后 1/3 段），转 fp32。"""
    weight = model.vision_model.head.attention.in_proj_weight
    bias = model.vision_model.head.attention.in_proj_bias
    hidden = weight.shape[1]
    return weight[2 * hidden : 3 * hidden].float(), bias[2 * hidden : 3 * hidden].float()


def capture_attention(store: dict) -> Callable[..., None]:
    """返回前向钩子：把池化头注意力的权重存进 `store['weights']`。

    池化头默认 `need_weights=True`，output 是 (池化结果, 注意力权重)。
    """

    def hook(_module: Any, _inputs: Any, output: Any) -> None:
        if isinstance(output, tuple) and len(output) > 1 and output[1] is not None:
            store["weights"] = output[1].detach()

    return hook


def pool_cells(hidden: np.ndarray, cells: int = SPP_CELLS) -> np.ndarray:
    """(576, 1536) → (64, 1536)；只接受能整除的正方形 patch 网格。"""
    tokens = hidden.shape[0]
    side = int(round(math.sqrt(tokens)))
    if side * side != tokens or side % cells != 0:
        raise ValueError(f"patch 数量 {tokens} 不是 {cells} 段可整除的正方形网格")
    block = side // cells
    grid = hidden.reshape(cells, block, cells, block, -1).mean(axis=(1, 3))
    return grid.reshape(cells * cells, -1)


def attention_top(
    attention: Any, top: int = MAXSIM_TOP, tokens: int | None = None
) -> np.ndarray:
    """注意力权重转成一维 numpy 数组，返回 top-k 位置（升序）。

    池化头通常只有一个注意力头；如果服务端返回了多头权重，按头平均。
    """
    array = _to_numpy(attention).reshape(-1)
    if tokens is not None and array.size != tokens:
        if array.size % tokens != 0:
            raise ValueError(f"注意力 {array.size} 个位置与 {tokens} 个 patch 不匹配")
        array = array.reshape(-1, tokens).mean(axis=0)
    if array.size < top:
        raise ValueError(f"注意力只有 {array.size} 个位置，选不出 {top} 个局部")
    index = np.argsort(-array)[:top]
    return np.sort(index)


def encode(
    hidden: Any,
    attention: Any,
    projected: Any,
    spp_basis: Basis,
    maxsim_basis: Basis,
) -> dict[str, np.ndarray]:
    """同一次前向的 hidden / attention / V 投影 patch 算出两种摊平的 int8 记录。

    ``projected`` 是池化头 Value 投影后的 patch 特征；调用方在设备上算好后传进来，
    这里只把结果搬到 CPU，避免每张图都把 1536×1536 的投影权重搬一遍。
    """
    hidden_array = _to_numpy(hidden)
    if hidden_array.ndim != 2:
        raise ValueError(f"last_hidden_state 形状无效：{hidden_array.shape}")
    cells = pool_cells(hidden_array)
    spp = spp_basis.whiten(cells)

    top = attention_top(attention, MAXSIM_TOP, tokens=hidden_array.shape[0])
    projected_array = _to_numpy(projected)
    if projected_array.shape[0] != hidden_array.shape[0]:
        raise ValueError(
            f"V 投影 patch 数 {projected_array.shape[0]} 与 hidden {hidden_array.shape[0]} 不一致"
        )
    # 与研究阶段一致：先逐 patch L2 归一化，再进白化基；去掉幅度差异后
    # 白化基只拟合“局部特征朝哪走”，不会把整体亮度/对比度带进描述子。
    local = projected_array[top]
    local /= np.maximum(np.linalg.norm(local, axis=-1, keepdims=True), 1e-8)
    maxsim = maxsim_basis.whiten(local)

    return {
        "spp": np.ascontiguousarray(spp.reshape(-1)),
        "maxsim": np.ascontiguousarray(maxsim.reshape(-1)),
    }


def _to_numpy(value: Any) -> np.ndarray:
    if hasattr(value, "detach"):
        value = value.detach()
    if hasattr(value, "float"):
        value = value.float()
    if hasattr(value, "cpu"):
        value = value.cpu()
    if hasattr(value, "numpy"):
        value = value.numpy()
    return np.asarray(value, dtype=np.float32)


def _label(kind: str) -> str:
    return {"spp": "构图", "maxsim": "主体"}.get(kind, kind)
