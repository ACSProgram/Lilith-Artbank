import type { PoseData } from "./types";

const CONNECTIONS: [number, number][] = [
  [0, 1],
  [1, 2],
  [2, 3],
  [3, 7],
  [0, 4],
  [4, 5],
  [5, 6],
  [6, 8],
  [9, 10],
  [11, 12],
  [11, 13],
  [13, 15],
  [15, 17],
  [15, 19],
  [15, 21],
  [12, 14],
  [14, 16],
  [16, 18],
  [16, 20],
  [16, 22],
  [11, 23],
  [12, 24],
  [23, 24],
  [23, 25],
  [25, 27],
  [27, 29],
  [29, 31],
  [27, 31],
  [24, 26],
  [26, 28],
  [28, 30],
  [30, 32],
  [28, 32],
];

const VISIBLE = 0.3;
/** 低于此置信度的骨骼画成虚线并降低不透明度。 */
const CONFIDENT = 0.5;
/** 脸与躯干关节点；其余按四肢画小一点。 */
const LARGE_JOINTS = new Set([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 23, 24]);

interface PoseOverlayProps {
  pose: PoseData;
  width: number;
  height: number;
  /** 画布缩放；线宽与点半径按屏幕像素换算，缩放时粗细不变。 */
  zoom: number;
  opacity?: number;
}

/** 33 点骨架且至少一个点可见时，叠加才有意义；按钮可用性也用它判断。 */
export function hasVisiblePose(pose: PoseData | null): pose is PoseData {
  return (
    pose != null &&
    pose.landmarks.length >= 33 &&
    pose.landmarks.some((point) => point.visibility >= VISIBLE)
  );
}

export function visiblePoseCount(pose: PoseData | null): number {
  if (!pose) return 0;
  return pose.landmarks.filter((point) => point.visibility >= VISIBLE).length;
}

/** 33 点 MediaPipe Pose 骨架；坐标按原图归一化，跟随旋转/翻转容器。 */
export function PoseOverlay({ pose, width, height, zoom, opacity = 1 }: PoseOverlayProps) {
  if (!hasVisiblePose(pose) || width <= 0 || height <= 0 || zoom <= 0) return null;
  const visible = (index: number) => (pose.landmarks[index]?.visibility ?? 0) >= VISIBLE;
  const stroke = 2 / zoom;
  const halo = 4 / zoom;
  const joint = 5 / zoom;
  const smallJoint = 3.5 / zoom;
  const jointHalo = 1.2 / zoom;

  return (
    <svg
      className="viewer-pose"
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      aria-hidden="true"
      style={{ opacity }}
    >
      {CONNECTIONS.map(([from, to]) => {
        if (!visible(from) || !visible(to)) return null;
        const a = pose.landmarks[from];
        const b = pose.landmarks[to];
        const points = {
          x1: a.x * width,
          y1: a.y * height,
          x2: b.x * width,
          y2: b.y * height,
        };
        const low = Math.min(a.visibility, b.visibility) < CONFIDENT;
        return (
          <g key={`${from}-${to}`}>
            <line
              {...points}
              className="pose-line-halo"
              strokeWidth={halo}
              strokeLinecap="round"
            />
            <line
              {...points}
              className={`pose-line${low ? " is-low" : ""}`}
              strokeWidth={stroke}
              strokeLinecap="round"
            />
          </g>
        );
      })}
      {pose.landmarks.map((point, index) => {
        if (point.visibility < VISIBLE) return null;
        const low = point.visibility < CONFIDENT;
        const radius = LARGE_JOINTS.has(index) ? joint : smallJoint;
        return (
          <g key={index} className={low ? "pose-joint-group is-low" : "pose-joint-group"}>
            <circle
              className="pose-joint-halo"
              cx={point.x * width}
              cy={point.y * height}
              r={radius + jointHalo}
            />
            <circle
              className="pose-joint"
              cx={point.x * width}
              cy={point.y * height}
              r={radius}
            />
          </g>
        );
      })}
    </svg>
  );
}
