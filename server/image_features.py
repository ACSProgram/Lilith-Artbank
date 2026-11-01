"""语义特征描述子：SPP 构图与 MaxSim 局部匹配。

两种描述子与全局 embedding 出自同一次图片塔前向：

  SPP     24×24 patch → 8×8 均值池化 → 白化到 128 维 → 每格 L2 归一化 → int8
  MaxSim  注意力 top-32 → 池化头 V 投影 → 白化到 64 维 → 每位 L2 归一化 → int8

白化基属于图库派生数据：``spp_basis.npz`` / ``maxsim_basis.npz`` 放在图库内的
``.artbank/basis/<clip_model>/``。模型目录里的同名文件是通用兜底，只在小库凑不够
样本时复制到库内。基缺失时全局 embedding 仍可用，但语义描述子不可用。

两种描述子都会摊平成一条 int8 记录；量化的固定 scale 写在基文件里，
所以相似度计算（余弦/点积）中 scale 会自然抵消，读取端不需要逐条元数据。
"""

from __future__ import annotations

import io
import math
import threading
import uuid
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


def load_basis(basis_dir: Path, kind: str) -> Basis:
    path = basis_dir / f"{kind}_basis.npz"
    if not path.is_file():
        raise FeatureBasisMissing(
            f"缺少{_label(kind)}白化基 {path.name}；请等待应用准备语义特征基"
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


def load_image_bases(basis_dir: Path) -> tuple[Basis, Basis]:
    """从库内基目录（或模型目录的通用兜底）载入两份基。"""
    return load_basis(basis_dir, "spp"), load_basis(basis_dir, "maxsim")


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


class SppAccumulator:
    """流式累加 64 格的均值与协方差，避免把所有图同时放进内存。"""

    def __init__(self) -> None:
        self.sum: np.ndarray | None = None
        self.gram: np.ndarray | None = None
        self.cells = 0
        self.images = 0

    def add(self, hidden: np.ndarray) -> None:
        cells = pool_cells(np.asarray(hidden, dtype=np.float32), SPP_CELLS).astype(
            np.float64
        )
        if self.sum is None:
            dim = cells.shape[1]
            self.sum = np.zeros(dim, dtype=np.float64)
            self.gram = np.zeros((dim, dim), dtype=np.float64)
        self.sum += cells.sum(axis=0)
        self.gram += cells.T @ cells
        self.cells += len(cells)
        self.images += 1

    def finish(self, out_dim: int) -> tuple[np.ndarray, np.ndarray]:
        if self.sum is None or self.gram is None or self.cells < 2:
            raise ValueError("SPP 基至少需要 2 个格子样本")
        mean = self.sum / self.cells
        cov = (self.gram - self.cells * np.outer(mean, mean)) / (self.cells - 1)
        values, vectors = np.linalg.eigh(cov)
        return _top_whitening(mean, values, vectors, out_dim)


class MaxsimAccumulator:
    """用蓄水池抽样的方式保留固定数量的图片 top-32 局部。

    拟合接口不知道总共会收到多少张；先按索引收集再抽样会把分布偏向先到的图片。
    蓄水池在流式输入上仍是均匀抽样，内存上限由 ``reserve`` 决定。
    """

    def __init__(self, reserve: int) -> None:
        self.reserve = max(1, int(reserve))
        self.blocks: list[np.ndarray] = []
        self.seen = 0
        self._rng = np.random.default_rng(0)

    def add(self, attention: np.ndarray, projected: np.ndarray) -> None:
        top = attention_top(attention, MAXSIM_TOP, tokens=projected.shape[0])
        local = projected[top].astype(np.float32)
        # 与研究阶段一致：拟合前来一次逐 patch L2 归一化；服务端计算时同样处理。
        local /= np.maximum(np.linalg.norm(local, axis=-1, keepdims=True), 1e-8)
        self.seen += 1
        if len(self.blocks) < self.reserve:
            self.blocks.append(local)
            return
        slot = int(self._rng.integers(0, self.seen))
        if slot < self.reserve:
            self.blocks[slot] = local

    @property
    def samples(self) -> int:
        return len(self.blocks)

    def finish(self, out_dim: int) -> tuple[np.ndarray, np.ndarray]:
        if not self.blocks:
            raise ValueError("MaxSim 基没有训练样本")
        samples = np.concatenate(self.blocks, axis=0)
        if len(samples) < 2:
            raise ValueError("MaxSim 基至少需要 2 个局部样本")
        mean = samples.mean(axis=0, dtype=np.float64)
        centred = samples - mean
        cov = centred.T @ centred / (len(centred) - 1)
        values, vectors = np.linalg.eigh(cov)
        return _top_whitening(mean, values, vectors, out_dim)


def _top_whitening(
    mean: np.ndarray, values: np.ndarray, vectors: np.ndarray, out_dim: int
) -> tuple[np.ndarray, np.ndarray]:
    """取方差最大的 out_dim 个方向，除以各自标准差，得到白化投影。

    只保留主轴、不补齐残差方向，等于「截断白化」：普通 PCA 保留的正是所有图共有的
    大方差分量，MaxSim 降维后局部特征会朝同一处偏、退化成整图相似，所以这里必须
    除以标准差。
    """
    order = np.argsort(-values)[:out_dim]
    scales = np.sqrt(np.maximum(values[order], 1e-8))
    projection = (vectors[:, order] / scales).astype(np.float32)
    return mean.astype(np.float32), projection


def basis_npz_bytes(
    *,
    kind: str,
    mean: np.ndarray,
    projection: np.ndarray,
    in_dim: int,
    out_dim: int,
    samples: int,
    **extra: int,
) -> bytes:
    """把一份白化基写成 npz 字节；服务端直接把字节交给应用落盘。"""
    buffer = io.BytesIO()
    np.savez(
        buffer,
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
    return buffer.getvalue()


@dataclass
class BasisSample:
    model: str
    index: int
    spp: SppAccumulator
    maxsim: MaxsimAccumulator

    def add(self, hidden: Any, attention: Any, projected: Any) -> int:
        self.spp.add(_to_numpy(hidden))
        self.maxsim.add(_to_numpy(attention), _to_numpy(projected))
        return self.index

    def finish(self) -> tuple[bytes, bytes, int]:
        spp_mean, spp_projection = self.spp.finish(SPP_DIM)
        maxsim_mean, maxsim_projection = self.maxsim.finish(MAXSIM_DIM)
        in_dim = int(spp_mean.size)
        spp_bytes = basis_npz_bytes(
            kind="spp",
            mean=spp_mean,
            projection=spp_projection,
            in_dim=in_dim,
            out_dim=SPP_DIM,
            samples=self.spp.images,
            cells=SPP_CELLS,
        )
        maxsim_bytes = basis_npz_bytes(
            kind="maxsim",
            mean=maxsim_mean,
            projection=maxsim_projection,
            in_dim=in_dim,
            out_dim=MAXSIM_DIM,
            samples=self.maxsim.samples,
            top=MAXSIM_TOP,
        )
        return spp_bytes, maxsim_bytes, self.spp.images


class BasisRegistry:
    """`/v1/basis/*` 的内存累积器；一次只允许一个拟合会话。"""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._samples: dict[str, BasisSample] = {}

    def start(self, model: str, maxsim_sample: int) -> str:
        if not model.strip():
            raise ValueError("model 不能为空")
        if maxsim_sample <= 0:
            raise ValueError("maxsimSample 必须大于 0")
        with self._lock:
            sample_id = uuid.uuid4().hex
            self._samples.clear()
            self._samples[sample_id] = BasisSample(
                model=model,
                index=0,
                spp=SppAccumulator(),
                maxsim=MaxsimAccumulator(maxsim_sample),
            )
            return sample_id

    def add(
        self, sample_id: str, hidden: Any, attention: Any, projected: Any
    ) -> int:
        with self._lock:
            sample = self._samples.get(sample_id)
            if sample is None:
                raise KeyError("拟合会话不存在或已结束")
            sample.index += 1
            return sample.add(hidden, attention, projected)

    def finish(self, sample_id: str) -> tuple[bytes, bytes, int]:
        with self._lock:
            sample = self._samples.pop(sample_id, None)
            if sample is None:
                raise KeyError("拟合会话不存在或已结束")
            return sample.finish()

    def cancel(self, sample_id: str) -> bool:
        with self._lock:
            return self._samples.pop(sample_id, None) is not None
