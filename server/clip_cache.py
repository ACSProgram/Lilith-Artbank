"""SigLIP2 图片塔/文本塔的按需加载、单塔驻留与空闲卸载。

两座塔从同一份 safetensors 权重里按前缀读取，不会为了加载一个塔而把另一个塔
实例化到内存里。CUDA 上用 fp16，CPU 上用 fp32。
"""

from __future__ import annotations

import gc
import json
import logging
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator, Optional

LOGGER = logging.getLogger("artbank.clip")

TOWER_PREFIX = {"image": "vision_model.", "text": "text_model."}
TOWER_LABEL = {"image": "图片塔", "text": "文本塔"}

# 文本塔一次 forward 的上下文长度；与 /v1/embed/text 的 max_length 一致。
WARMUP_TEXT = "warmup"
WARMUP_TEXT_LENGTH = 64


def import_deps() -> tuple[Any, dict[str, Any]]:
    """导入 torch 与 transformers 的 SigLIP 具体类，返回 ``(torch, 类字典)``。

    首次 AI 请求的 7.7–8.0 s 几乎全花在这几行。预热与真正加载共用这一处：
    两边各写一份导入列表时，预热很容易漏掉某个具体类，第一次请求又要多等几秒。
    """
    import torch
    from transformers import (
        AutoTokenizer,
        SiglipImageProcessor,
        SiglipTextConfig,
        SiglipTextModel,
        SiglipVisionConfig,
        SiglipVisionModel,
    )

    return torch, {
        "AutoTokenizer": AutoTokenizer,
        "SiglipImageProcessor": SiglipImageProcessor,
        "SiglipTextConfig": SiglipTextConfig,
        "SiglipTextModel": SiglipTextModel,
        "SiglipVisionConfig": SiglipVisionConfig,
        "SiglipVisionModel": SiglipVisionModel,
    }


def prewarm_imports() -> float:
    """把依赖导入成本从“第一次请求”挪到服务启动时，返回耗时（秒）。

    导入失败只记日志：真正的加载会再报一次带上下文的错误。
    """
    started = time.monotonic()
    import_deps()
    return time.monotonic() - started


class CapabilityUnavailable(RuntimeError):
    """模型目录缺失、依赖不可用或加载失败。"""


class ModelNotReady(RuntimeError):
    """请求的塔当前没有驻留。"""


@dataclass(frozen=True)
class ClipModelInfo:
    model_name: str
    dim: int
    model_dir: Path


@dataclass
class _LoadedTower:
    kind: str
    model: Any
    processor: Any
    device: Any
    dtype: Any
    last_used: float


class ClipCache:
    """同一时间只驻留一个 CLIP 塔。

    加载新塔时旧塔立即让位，避免两塔同时占显存；当前塔在最后一次推理后
    经过 ``idle_unload_seconds`` 秒没有使用才会被后台线程卸载。
    """

    def __init__(self, model_dir: Optional[Path], idle_unload_seconds: int = 300):
        self._model_dir = model_dir
        self._idle_seconds = max(0, int(idle_unload_seconds))
        self._lock = threading.RLock()
        self._state_lock = threading.Lock()
        self._tower: Optional[_LoadedTower] = None
        self._info = self._read_info(model_dir)
        self._closing = False
        if self._info is not None and self._idle_seconds > 0:
            threading.Thread(
                target=self._reap_loop,
                name="clip-idle-unload",
                daemon=True,
            ).start()

    @property
    def info(self) -> Optional[ClipModelInfo]:
        return self._info

    def ready(self, kind: str) -> bool:
        with self._state_lock:
            return self._tower is not None and self._tower.kind == kind

    def ensure(self, kind: str) -> None:
        if kind not in TOWER_PREFIX:
            raise CapabilityUnavailable(f"未知的 CLIP 塔：{kind}")
        if self._info is None:
            raise CapabilityUnavailable("CLIP 模型目录不可用，请检查模型路径")
        with self._lock:
            # 不把旧塔存进局部变量：局部引用会活到加载新塔的那一刻，等于两块塔
            # 同时占显存，和“同一时间只驻留一个塔”的前提相冲突。
            if self._tower is not None and self._tower.kind == kind:
                self._tower.last_used = time.monotonic()
                return
            if self._tower is not None:
                self._unload_locked("切换模型塔")
            self._load_locked(kind)

    @contextmanager
    def use(self, kind: str) -> Iterator[_LoadedTower]:
        with self._lock:
            tower = self._tower
            if tower is None or tower.kind != kind:
                raise ModelNotReady(
                    f"{TOWER_LABEL.get(kind, kind)}未加载，请先调用 /v1/models/ensure"
                )
            tower.last_used = time.monotonic()
            try:
                yield tower
            finally:
                tower.last_used = time.monotonic()

    def shutdown(self) -> None:
        self._closing = True
        with self._lock:
            self._unload_locked("服务关闭")

    def _load_locked(self, kind: str) -> None:
        started = time.monotonic()
        try:
            torch, deps = import_deps()
        except Exception as error:
            raise CapabilityUnavailable(f"无法导入 CLIP 依赖：{error}") from error

        model = None
        state: dict[str, Any] = {}
        try:
            raw = json.loads(
                (self._info.model_dir / "config.json").read_text(encoding="utf-8")
            )
            device = torch.device("cuda:0" if torch.cuda.is_available() else "cpu")
            dtype = torch.float16 if device.type == "cuda" else torch.float32
            if kind == "image":
                config = deps["SiglipVisionConfig"].from_dict(raw["vision_config"])
                model_class = deps["SiglipVisionModel"]
                processor = deps["SiglipImageProcessor"].from_pretrained(
                    self._info.model_dir
                )
                expected_dim = int(config.hidden_size)
            else:
                config = deps["SiglipTextConfig"].from_dict(raw["text_config"])
                model_class = deps["SiglipTextModel"]
                processor = deps["AutoTokenizer"].from_pretrained(self._info.model_dir)
                expected_dim = int(config.projection_size)
            # 维度先比，别把不匹配的权重搬上显存再报错。
            if expected_dim != self._info.dim:
                raise CapabilityUnavailable(
                    f"{TOWER_LABEL[kind]}维度 {expected_dim} 与模型声明 "
                    f"{self._info.dim} 不一致"
                )

            state = self._load_tower_state(kind, device, dtype)
            with torch.device("meta"):
                model = model_class(config)
            model.load_state_dict(state, strict=True, assign=True)
            self._restore_position_ids(model, device)
            model.to(device)
            model.eval()
            warmup = self._warmup_locked(kind, model, processor, config, device, dtype)
        except CapabilityUnavailable:
            raise
        except Exception as error:
            model = None
            state = {}
            gc.collect()
            torch.cuda.empty_cache()
            raise CapabilityUnavailable(
                f"加载{TOWER_LABEL[kind]}失败：{error}"
            ) from error
        state = {}

        with self._state_lock:
            self._tower = _LoadedTower(
                kind=kind,
                model=model,
                processor=processor,
                device=device,
                dtype=dtype,
                last_used=time.monotonic(),
            )
        LOGGER.info(
            "已加载%s：model=%s device=%s dtype=%s 耗时=%.1fs（含预热 %.1fs）",
            TOWER_LABEL[kind],
            self._info.model_name,
            device,
            dtype,
            time.monotonic() - started,
            warmup,
        )

    @staticmethod
    def _warmup_locked(
        kind: str,
        model: Any,
        processor: Any,
        config: Any,
        device: Any,
        dtype: Any,
    ) -> float:
        """加载后立刻跑一次前向。

        权重是按需分页的 mmap，``ensure`` 返回时物理内存里还没有它们；第一次
        真正的请求要再花几秒把权重读进来。在这里付掉这段时间，“加载完成”才等于
        “可以直接用”。失败按加载失败处理：连一次前向都跑不通的塔不能算就绪。
        """
        import torch

        started = time.monotonic()
        if kind == "image":
            size = int(getattr(config, "image_size", 384) or 384)
            pixel_values = torch.zeros((1, 3, size, size), device=device, dtype=dtype)
            with torch.inference_mode():
                model(pixel_values=pixel_values)
        else:
            inputs = processor(
                [WARMUP_TEXT],
                padding="max_length",
                max_length=WARMUP_TEXT_LENGTH,
                truncation=True,
                return_tensors="pt",
            )
            tensors = {
                name: value.to(device)
                for name, value in inputs.items()
                if name in ("input_ids", "attention_mask")
            }
            with torch.inference_mode():
                model(**tensors)
        return time.monotonic() - started

    def _load_tower_state(self, kind: str, device: Any, dtype: Any) -> dict[str, Any]:
        from safetensors import safe_open

        prefix = TOWER_PREFIX[kind]
        model_dir = self._info.model_dir
        index_path = model_dir / "model.safetensors.index.json"
        if index_path.is_file():
            weight_map = json.loads(index_path.read_text(encoding="utf-8"))["weight_map"]
            by_shard: dict[str, list[str]] = {}
            for key, shard in weight_map.items():
                if key.startswith(prefix):
                    by_shard.setdefault(shard, []).append(key)
        else:
            single = model_dir / "model.safetensors"
            if not single.is_file():
                raise CapabilityUnavailable("模型目录里没有 model.safetensors")
            with safe_open(single, framework="pt", device="cpu") as handle:
                keys = [key for key in handle.keys() if key.startswith(prefix)]
            by_shard = {"model.safetensors": keys}

        if not by_shard:
            raise CapabilityUnavailable(f"权重里没有 {prefix}* 张量")

        state: dict[str, Any] = {}
        for shard, keys in by_shard.items():
            path = model_dir / shard
            try:
                handle = safe_open(path, framework="pt", device=str(device))
            except Exception:
                # 个别平台不支持直接从 CUDA 读取，退回 CPU 再搬到目标设备。
                handle = safe_open(path, framework="pt", device="cpu")
            with handle:
                for key in keys:
                    state[key] = handle.get_tensor(key).to(dtype=dtype)
        return state

    @staticmethod
    def _restore_position_ids(model: Any, device: Any) -> None:
        import torch

        for module_name, module in model.named_modules():
            for name, buffer in list(module._buffers.items()):
                if buffer is None or not buffer.is_meta:
                    continue
                if name == "position_ids" and hasattr(module, "position_embedding"):
                    count = module.position_embedding.num_embeddings
                    module._buffers[name] = torch.arange(
                        count, device=device
                    ).expand((1, -1))
                    continue
                raise CapabilityUnavailable(
                    f"模型有未恢复的缓冲区：{module_name}.{name}"
                )

    def _unload_locked(self, reason: str) -> None:
        tower = self._tower
        if tower is None:
            return
        kind = tower.kind
        with self._state_lock:
            self._tower = None
        # 这里的局部引用是模型的最后一个持有者；先放掉它，gc 与 empty_cache
        # 才能真正把张量还给驱动。
        del tower
        gc.collect()
        try:
            import torch

            torch.cuda.empty_cache()
        except Exception:
            pass
        LOGGER.info("已卸载%s（%s）", TOWER_LABEL[kind], reason)

    def _reap_loop(self) -> None:
        interval = max(1.0, min(30.0, self._idle_seconds / 2))
        while not self._closing:
            time.sleep(interval)
            if self._closing:
                return
            with self._lock:
                # 只读属性，不存局部变量：局部引用会让卸载后的模型继续活着。
                if self._tower is None:
                    continue
                if time.monotonic() - self._tower.last_used < self._idle_seconds:
                    continue
                self._unload_locked("空闲")

    @staticmethod
    def _read_info(model_dir: Optional[Path]) -> Optional[ClipModelInfo]:
        if model_dir is None:
            return None
        try:
            config = json.loads(
                (model_dir / "config.json").read_text(encoding="utf-8")
            )
            model_type = str(config.get("model_type", ""))
            text = config.get("text_config") or {}
            vision = config.get("vision_config") or {}
            dim = text.get("projection_size") or vision.get("hidden_size")
            if model_type not in ("siglip", "siglip2") or not dim:
                raise ValueError(f"不支持的模型类型或缺少投影维度：{model_type}")
            return ClipModelInfo(model_dir.name, int(dim), model_dir)
        except Exception as error:
            LOGGER.error("CLIP 模型不可用：%s（%s）", model_dir, error)
            return None
