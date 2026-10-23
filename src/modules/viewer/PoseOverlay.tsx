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

interface PoseOverlayProps {
  pose: PoseData;
  width: number;
  height: number;
}

/** 33 点骨架且至少一个点可见时，叠加才有意义；按钮可用性也用它判断。 */
export function hasVisiblePose(pose: PoseData | null): pose is PoseData {
  return (
    pose != null &&
    pose.landmarks.length >= 33 &&
    pose.landmarks.some((point) => point.visibility >= VISIBLE)
  );
}

/** 33 点 MediaPipe Pose 骨架；坐标按原图归一化，跟随旋转/翻转容器。 */
export function PoseOverlay({ pose, width, height }: PoseOverlayProps) {
  if (!hasVisiblePose(pose) || width <= 0 || height <= 0) return null;
  const visible = (index: number) => (pose.landmarks[index]?.visibility ?? 0) >= VISIBLE;
  const stroke = Math.max(1, Math.min(width, height) * 0.0025);
  const radius = Math.max(1.5, Math.min(width, height) * 0.004);

  return (
    <svg
      className="viewer-pose"
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      aria-hidden="true"
    >
      {CONNECTIONS.map(([from, to]) => {
        if (!visible(from) || !visible(to)) return null;
        const a = pose.landmarks[from];
        const b = pose.landmarks[to];
        return (
          <line
            key={`${from}-${to}`}
            x1={a.x * width}
            y1={a.y * height}
            x2={b.x * width}
            y2={b.y * height}
            stroke="rgba(104, 226, 181, 0.85)"
            strokeWidth={stroke}
            strokeLinecap="round"
          />
        );
      })}
      {pose.landmarks.map((point, index) =>
        point.visibility >= VISIBLE ? (
          <circle
            key={index}
            cx={point.x * width}
            cy={point.y * height}
            r={radius}
            fill="rgba(240, 255, 248, 0.95)"
            stroke="rgba(20, 80, 58, 0.9)"
            strokeWidth={stroke * 0.5}
          />
        ) : null,
      )}
    </svg>
  );
}
