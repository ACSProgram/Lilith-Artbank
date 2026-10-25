"""Lilith Artbank 本机 AI 服务。

实现 docs/modules/ai-service.md 里的 /v1 契约。只监听 127.0.0.1，
模型按需加载，CLIP 图片塔/文本塔同一时间只驻留一个，空闲后自动卸载。
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, urlsplit

from clip_cache import CapabilityUnavailable, ClipCache, ClipModelInfo, ModelNotReady
from image_io import ImageDecodeError, decode_image
from pose_engine import PoseEngine

LOGGER = logging.getLogger("artbank.server")

MAX_BODY_BYTES = 64 * 1024 * 1024
TEXT_MAX_LENGTH = 64
POSE_MODEL = "mediapipe-pose"
CLIP_CAPABILITIES = {"imageEmbedding": "image", "textEmbedding": "text"}
KNOWN_CAPABILITIES = {"imageEmbedding", "textEmbedding", "pose", "description"}


def _torch() -> tuple[Any, Any]:
    """延迟导入 torch；缺依赖按“能力不可用”返回，而不是 500。"""
    try:
        import torch
        import torch.nn.functional as functional
    except Exception as error:
        raise CapabilityUnavailable(f"无法导入 torch：{error}") from error
    return torch, functional


class ApiError(Exception):
    def __init__(self, status: int, code: str, message: str):
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message


class ApiState:
    def __init__(self, cache: ClipCache, pose: PoseEngine):
        self.cache = cache
        self.pose = pose


class ApiServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False
    request_queue_size = 16

    def __init__(self, server_address, state: ApiState):
        self.state = state
        super().__init__(server_address, ApiHandler)


class ApiHandler(BaseHTTPRequestHandler):
    server: ApiServer

    def do_GET(self) -> None:
        path = urlsplit(self.path).path
        started = time.monotonic()
        status = 500
        try:
            if path == "/v1/health":
                self._send_json(200, self._health_payload())
                status = 200
                return
            if path in (
                "/v1/models/ensure",
                "/v1/embed/image",
                "/v1/embed/text",
                "/v1/pose",
                "/v1/describe",
                "/v1/shutdown",
            ):
                raise ApiError(405, "method_not_allowed", "该端点只接受 POST")
            raise ApiError(404, "not_found", f"未知端点：{path}")
        except ApiError as error:
            status = error.status
            self._send_json(status, {"error": error.code, "message": error.message})
        except Exception as error:  # pragma: no cover - 兜底，避免线程静默退出
            LOGGER.exception("GET %s 失败", path)
            status = 500
            self._send_json(
                status, {"error": "inference_failed", "message": str(error)}
            )
        finally:
            LOGGER.debug(
                "GET %s -> %s %.1fms", path, status, (time.monotonic() - started) * 1000
            )

    def do_POST(self) -> None:
        path = urlsplit(self.path).path
        started = time.monotonic()
        status = 500
        try:
            if path == "/v1/health":
                raise ApiError(405, "method_not_allowed", "health 只接受 GET")
            if path == "/v1/shutdown":
                self._send_json(200, {"status": "success", "message": "server shutdown"})
                status = 200
                threading.Thread(target=self.server.shutdown, daemon=True).start()
                return
            payload = self._route_post(path)
            self._send_json(200, payload)
            status = 200
        except ApiError as error:
            status = error.status
            self._send_json(status, {"error": error.code, "message": error.message})
        except ImageDecodeError as error:
            status = 422
            self._send_json(
                status, {"error": "decode_failed", "message": str(error)}
            )
        except CapabilityUnavailable as error:
            status = 422
            self._send_json(
                status, {"error": "model_unavailable", "message": str(error)}
            )
        except ModelNotReady as error:
            status = 503
            self._send_json(
                status, {"error": "model_not_ready", "message": str(error)}
            )
        except Exception as error:  # pragma: no cover - 兜底，避免线程静默退出
            LOGGER.exception("POST %s 失败", path)
            status = 500
            self._send_json(
                status, {"error": "inference_failed", "message": str(error)}
            )
        finally:
            elapsed = (time.monotonic() - started) * 1000
            if status >= 400:
                LOGGER.warning("POST %s -> %s %.1fms", path, status, elapsed)
            else:
                LOGGER.debug("POST %s -> %s %.1fms", path, status, elapsed)

    def log_message(self, *args) -> None:
        LOGGER.debug("%s", args)

    def _route_post(self, path: str) -> dict:
        if path == "/v1/models/ensure":
            return self._ensure_models()
        if path == "/v1/embed/image":
            return self._embed_image()
        if path == "/v1/embed/text":
            return self._embed_text()
        if path == "/v1/pose":
            return self._pose()
        if path == "/v1/describe":
            raise ApiError(404, "capability_missing", "未配置描述模型")
        raise ApiError(404, "not_found", f"未知端点：{path}")

    def _health_payload(self) -> dict:
        capabilities: dict[str, dict] = {}
        info = self.server.state.cache.info
        if info is not None:
            for name, kind in CLIP_CAPABILITIES.items():
                capabilities[name] = {
                    "ready": self.server.state.cache.ready(kind),
                    "model": info.model_name,
                    "dim": info.dim,
                }
        capabilities["pose"] = {
            "ready": self.server.state.pose.ready(),
            "model": POSE_MODEL,
        }
        return {"status": "ok", "version": "1", "capabilities": capabilities}

    def _ensure_models(self) -> dict:
        payload = self._read_json()
        requested = payload.get("capabilities")
        if not isinstance(requested, list) or not all(
            isinstance(name, str) for name in requested
        ):
            raise ApiError(400, "bad_request", "capabilities 必须是字符串数组")
        requested = list(dict.fromkeys(requested))
        unknown = [name for name in requested if name not in KNOWN_CAPABILITIES]
        if unknown:
            raise ApiError(
                404, "capability_missing", f"能力不存在：{', '.join(unknown)}"
            )
        if "description" in requested:
            raise ApiError(404, "capability_missing", "未配置描述模型")
        clip = [name for name in requested if name in CLIP_CAPABILITIES]
        if len(clip) > 1:
            raise ApiError(
                400,
                "clip_tower_conflict",
                "一次只能加载一个 CLIP 塔，请分别加载图片塔与文本塔",
            )

        try:
            if "imageEmbedding" in requested:
                self.server.state.cache.ensure("image")
            if "textEmbedding" in requested:
                self.server.state.cache.ensure("text")
            if "pose" in requested:
                self.server.state.pose.ensure()
        except CapabilityUnavailable:
            raise
        except ModelNotReady:
            raise
        except Exception as error:
            raise ApiError(422, "model_load_failed", str(error)) from error

        return {"capabilities": self._health_payload()["capabilities"]}

    def _embed_image(self) -> dict:
        info = self._check_model(self._query_value("model"))
        if not self.server.state.cache.ready("image"):
            raise ModelNotReady("图片塔未加载，请先调用 /v1/models/ensure")
        body = self._read_body()
        torch, functional = _torch()
        image = decode_image(body)
        try:
            with self.server.state.cache.use("image") as tower:
                inputs = tower.processor(images=image, return_tensors="pt")
                pixel_values = inputs["pixel_values"].to(
                    device=tower.device, dtype=tower.dtype
                )
                with torch.inference_mode():
                    output = tower.model(pixel_values=pixel_values)
                vector = output.pooler_output[0].float()
                vector = functional.normalize(vector, p=2, dim=-1)
                values = vector.detach().cpu().tolist()
        finally:
            image.close()

        if len(values) != info.dim:
            raise ApiError(
                422,
                "dimension_mismatch",
                f"模型返回 {len(values)} 维，声明 {info.dim} 维",
            )
        return {
            "status": "success",
            "model": info.model_name,
            "dim": len(values),
            "normalized": True,
            "vector": values,
        }

    def _embed_text(self) -> dict:
        payload = self._read_json()
        text = payload.get("text")
        if not isinstance(text, str) or not text.strip():
            raise ApiError(400, "bad_request", "text 不能为空")
        model = payload.get("model") or ""
        if not isinstance(model, str):
            raise ApiError(400, "bad_request", "model 必须是字符串")
        info = self._check_model(model)

        torch, functional = _torch()
        with self.server.state.cache.use("text") as tower:
            tokenizer = tower.processor
            full = tokenizer(text, add_special_tokens=True, truncation=False)[
                "input_ids"
            ]
            if full and isinstance(full[0], list):
                full = full[0]
            truncated = len(full) > TEXT_MAX_LENGTH
            inputs = tokenizer(
                [text],
                padding="max_length",
                max_length=TEXT_MAX_LENGTH,
                truncation=True,
                return_tensors="pt",
            )
            tensors = {
                name: value.to(tower.device)
                for name, value in inputs.items()
                if name in ("input_ids", "attention_mask")
            }
            with torch.inference_mode():
                output = tower.model(**tensors)
            vector = functional.normalize(output.pooler_output[0].float(), p=2, dim=-1)
            values = vector.detach().cpu().tolist()

        if len(values) != info.dim:
            raise ApiError(
                422,
                "dimension_mismatch",
                f"模型返回 {len(values)} 维，声明 {info.dim} 维",
            )
        return {
            "status": "success",
            "model": info.model_name,
            "dim": len(values),
            "normalized": True,
            "truncated": truncated,
            "vector": values,
        }

    def _pose(self) -> dict:
        model = self._query_value("model")
        if model and model != POSE_MODEL:
            raise ApiError(404, "model_not_found", f"模型不存在：{model}")
        body = self._read_body()
        image = decode_image(body)
        try:
            width, height = image.size
            try:
                self.server.state.pose.ensure()
            except Exception as error:
                raise ApiError(422, "model_load_failed", str(error)) from error
            landmarks = self.server.state.pose.infer(image)
        finally:
            image.close()
        return {
            "status": "success",
            "model": POSE_MODEL,
            "imageWidth": width,
            "imageHeight": height,
            "landmarks": landmarks,
        }

    def _check_model(self, requested: str) -> ClipModelInfo:
        info = self.server.state.cache.info
        if info is None:
            raise ApiError(
                404, "model_missing", "CLIP 模型目录不可用，请检查模型路径"
            )
        if requested and requested != info.model_name:
            raise ApiError(404, "model_not_found", f"模型不存在：{requested}")
        return info

    def _query_value(self, name: str) -> str:
        values = parse_qs(urlsplit(self.path).query).get(name)
        return values[0] if values else ""

    def _read_body(self) -> bytes:
        header = self.headers.get("Content-Length")
        if header is None:
            raise ApiError(411, "length_required", "请求缺少 Content-Length")
        try:
            length = int(header)
        except ValueError as error:
            raise ApiError(400, "bad_request", "Content-Length 无效") from error
        if length < 0:
            raise ApiError(400, "bad_request", "Content-Length 无效")
        if length > MAX_BODY_BYTES:
            raise ApiError(
                413, "payload_too_large", "请求体超过 64 MiB"
            )
        data = self.rfile.read(length)
        if len(data) != length:
            raise ApiError(400, "bad_request", "请求体不完整")
        return data

    def _read_json(self) -> dict:
        body = self._read_body()
        try:
            payload = json.loads(body.decode("utf-8"))
        except Exception as error:
            raise ApiError(400, "bad_json", f"JSON 解析失败：{error}") from error
        if not isinstance(payload, dict):
            raise ApiError(400, "bad_json", "JSON 顶层必须是对象")
        return payload

    def _send_json(self, status: int, payload: dict) -> None:
        data = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode(
            "utf-8"
        )
        self.send_response(int(status))
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Lilith Artbank local AI service")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--model-dir", default="")
    parser.add_argument(
        "--idle-unload-seconds",
        type=int,
        default=300,
        help="CLIP 塔空闲卸载时间；0 表示不自动卸载",
    )
    parser.add_argument("--log-level", default="INFO")
    args = parser.parse_args(argv)
    if not 1 <= args.port <= 65535:
        parser.error("--port 必须在 1-65535 之间")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    logging.basicConfig(
        level=getattr(logging, args.log_level.upper(), logging.INFO),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        stream=sys.stderr,
    )
    model_dir = (
        Path(args.model_dir).expanduser().resolve() if args.model_dir.strip() else None
    )
    cache = ClipCache(model_dir, args.idle_unload_seconds)
    pose = PoseEngine()
    state = ApiState(cache, pose)

    try:
        server = ApiServer(("127.0.0.1", args.port), state)
    except OSError as error:
        LOGGER.error("无法监听 127.0.0.1:%s：%s", args.port, error)
        cache.shutdown()
        pose.shutdown()
        return 2

    LOGGER.info(
        "AI 服务已启动：http://127.0.0.1:%s model=%s idle=%ss",
        args.port,
        model_dir if model_dir is not None else "(未配置)",
        args.idle_unload_seconds,
    )
    try:
        server.serve_forever(poll_interval=0.5)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        cache.shutdown()
        pose.shutdown()
        LOGGER.info("AI 服务已停止")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
