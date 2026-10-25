"""MediaPipe Pose 的懒加载与推理，输出 33 个归一化 landmark。"""

from __future__ import annotations

import logging
import threading
from typing import Any

import numpy as np

LOGGER = logging.getLogger("artbank.pose")


class PoseEngine:
    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._pose: Any = None

    def ready(self) -> bool:
        return self._pose is not None

    def ensure(self) -> None:
        with self._lock:
            if self._pose is not None:
                return
            try:
                import mediapipe as mp
            except Exception as error:
                raise RuntimeError(f"无法导入 MediaPipe：{error}") from error
            self._pose = mp.solutions.pose.Pose(
                static_image_mode=True,
                model_complexity=2,
                enable_segmentation=False,
                min_detection_confidence=0.3,
            )
            LOGGER.info("已加载 MediaPipe Pose（model_complexity=2）")

    def infer(self, image: Any) -> list[dict[str, float]]:
        """image 为 RGB 的 PIL Image。未检测到人体时返回空列表。"""
        with self._lock:
            self.ensure()
            results = self._pose.process(np.asarray(image, dtype=np.uint8))
            if not results.pose_landmarks:
                return []
            return [
                {
                    "x": float(point.x),
                    "y": float(point.y),
                    "z": float(point.z),
                    "visibility": float(point.visibility),
                }
                for point in results.pose_landmarks.landmark
            ]

    def shutdown(self) -> None:
        with self._lock:
            if self._pose is None:
                return
            try:
                self._pose.close()
            finally:
                self._pose = None
            LOGGER.info("已卸载 MediaPipe Pose")
