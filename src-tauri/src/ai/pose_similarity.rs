//! MediaPipe pose similarity, ported 1:1 from the original program's
//! `MediaPipePoseData::similarity` in `ZyImageAlgorithm.cpp`.
//!
//! Keep formulas, indices and default weights identical. Do not "clean up"
//! the asymmetric limb indexing (the right leg compares against hip 24 but the
//! right arm against hip 23); the original behaves that way and results must
//! stay comparable.

use std::cmp::Ordering;

use crate::library::viewer::PoseLandmark;

pub const LANDMARK_COUNT: usize = 33;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub torso: f64,
    pub limb2d: f64,
    pub limb3d: f64,
    pub pca: f64,
    pub radial: f64,
    pub hull: f64,
    pub limb_arm_ratio: f64,
    pub arm_torso_vs_elbow: f64,
    pub leg_torso_vs_knee: f64,
    pub sigma_deg: f64,
    pub radial_bins: usize,
    pub vis_threshold: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            torso: 0.35,
            limb2d: 0.25,
            limb3d: 0.15,
            pca: 0.1,
            radial: 0.07,
            hull: 0.08,
            limb_arm_ratio: 0.5,
            arm_torso_vs_elbow: 0.5,
            leg_torso_vs_knee: 0.5,
            sigma_deg: 12.0,
            radial_bins: 36,
            vis_threshold: 1e-4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Vec3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vec3 {
    fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }

    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }

    fn mul(self, scale: f64) -> Self {
        Self::new(self.x * scale, self.y * scale, self.z * scale)
    }

    fn div(self, divisor: f64) -> Self {
        Self::new(self.x / divisor, self.y / divisor, self.z / divisor)
    }

    fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    fn cross(self, other: Self) -> Self {
        Self::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    fn length(self) -> f64 {
        self.dot(self).sqrt()
    }

    fn unit(self) -> Self {
        let length = self.length();
        if length == 0.0 {
            self
        } else {
            self.mul(1.0 / length)
        }
    }

    fn xy(self) -> Self {
        Self::new(self.x, self.y, 0.0)
    }

    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Point {
    pos: Vec3,
    confidence: f64,
}

#[derive(Debug, Clone)]
pub struct Pose {
    points: [Point; LANDMARK_COUNT],
}

impl Pose {
    /// 不足 33 点按零置信度补齐；超过 33 点忽略。
    pub fn from_landmarks(landmarks: &[PoseLandmark]) -> Self {
        let mut points = [Point::default(); LANDMARK_COUNT];
        for (index, landmark) in landmarks.iter().take(LANDMARK_COUNT).enumerate() {
            points[index] = Point {
                pos: Vec3::new(
                    f64::from(landmark.x),
                    f64::from(landmark.y),
                    f64::from(landmark.z),
                ),
                confidence: f64::from(landmark.visibility),
            };
        }
        Self { points }
    }

    pub fn similarity(&self, other: &Pose, weights: &Weights) -> f64 {
        let a_norm = normalize(self);
        let b_norm = normalize(other);
        let normal = self.core(&a_norm, &b_norm, other, weights);

        let mirrored = mirror(other);
        let mirrored_norm = normalize(&mirrored);
        let mirrored_score = self.core(&a_norm, &mirrored_norm, &mirrored, weights);
        normal.max(mirrored_score)
    }

    fn core(
        &self,
        a: &[Vec3; LANDMARK_COUNT],
        b: &[Vec3; LANDMARK_COUNT],
        other: &Pose,
        w: &Weights,
    ) -> f64 {
        let deg = |radians: f64| radians * 180.0 / std::f64::consts::PI;
        let gaussian = |diff_deg: f64| {
            if !diff_deg.is_finite() || w.sigma_deg <= 0.0 {
                0.0
            } else {
                (-(diff_deg * diff_deg) / (2.0 * w.sigma_deg * w.sigma_deg)).exp()
            }
        };
        let clamp01 = |value: f64| value.clamp(0.0, 1.0);

        // Torso 3D: directions from normalized positions, visibility from raw poses.
        let l_sh = a[11];
        let r_sh = a[12];
        let l_h = a[23];
        let r_h = a[24];
        let l_sh_o = b[11];
        let r_sh_o = b[12];
        let l_h_o = b[23];
        let r_h_o = b[24];

        let lv1 = safe_conf(self, 11);
        let lv2 = safe_conf(self, 12);
        let lv3 = safe_conf(self, 23);
        let lv4 = safe_conf(self, 24);
        let ov1 = safe_conf(other, 11);
        let ov2 = safe_conf(other, 12);
        let ov3 = safe_conf(other, 23);
        let ov4 = safe_conf(other, 24);

        let shoulder_dir_a = r_sh.sub(l_sh).unit();
        let vertical_a = l_h.add(r_h).mul(0.5).sub(l_sh.add(r_sh).mul(0.5)).unit();
        let normal_a = shoulder_dir_a.cross(vertical_a).unit();

        let shoulder_dir_b = r_sh_o.sub(l_sh_o).unit();
        let vertical_b = l_h_o
            .add(r_h_o)
            .mul(0.5)
            .sub(l_sh_o.add(r_sh_o).mul(0.5))
            .unit();
        let normal_b = shoulder_dir_b.cross(vertical_b).unit();

        let vertical_angle = deg(angle_between(vertical_a, vertical_b));
        let normal_angle = deg(angle_between(normal_a, normal_b));
        let torso_sim = 0.5 * (gaussian(vertical_angle) + gaussian(normal_angle));
        let torso_vis = (lv1 + lv2 + lv3 + lv4 + ov1 + ov2 + ov3 + ov4) / 8.0;
        let torso_sim = torso_sim * torso_vis;

        // Limb weights: arms vs legs, then torso-relative vs joint-angle.
        let limb_total = w.limb2d + w.limb3d;
        let arm_total = limb_total * w.limb_arm_ratio;
        let leg_total = limb_total * (1.0 - w.limb_arm_ratio);
        let arm_a = arm_total * w.arm_torso_vs_elbow;
        let arm_b = arm_total * (1.0 - w.arm_torso_vs_elbow);
        let leg_a = leg_total * w.leg_torso_vs_knee;
        let leg_b = leg_total * (1.0 - w.leg_torso_vs_knee);

        let mut arm_sim_sum = 0.0;
        let mut arm_weight_sum = 0.0;
        let mut leg_sim_sum = 0.0;
        let mut leg_weight_sum = 0.0;

        // Left arm: shoulder 11, elbow 13, wrist 15, hip 23.
        {
            let shoulder = a[11];
            let elbow = a[13];
            let wrist = a[15];
            let hip = a[23];
            let shoulder_o = b[11];
            let elbow_o = b[13];
            let wrist_o = b[15];
            let hip_o = b[23];
            let vis = [
                safe_conf(self, 11),
                safe_conf(self, 13),
                safe_conf(self, 15),
                safe_conf(other, 11),
                safe_conf(other, 13),
                safe_conf(other, 15),
            ]
            .into_iter()
            .fold(f64::INFINITY, f64::min);

            let ua = shoulder.sub(elbow).unit();
            let utorso = shoulder.sub(hip).unit();
            let ua_o = shoulder_o.sub(elbow_o).unit();
            let utorso_o = shoulder_o.sub(hip_o).unit();

            let angle_a1 = deg(angle_between(ua, utorso));
            let angle_b1 = deg(angle_between(ua_o, utorso_o));
            let sim_a1_3 = gaussian((angle_a1 - angle_b1).abs());
            let ua2 = ua.xy().unit();
            let ut2 = utorso.xy().unit();
            let ua_o2 = ua_o.xy().unit();
            let ut_o2 = utorso_o.xy().unit();
            let angle_a1_2 = deg(angle_between(ua2, ut2));
            let angle_b1_2 = deg(angle_between(ua_o2, ut_o2));
            let sim_a1_2 = gaussian((angle_a1_2 - angle_b1_2).abs());

            let angle_a2 = deg(angle_between(
                shoulder.sub(elbow).unit(),
                wrist.sub(elbow).unit(),
            ));
            let angle_b2 = deg(angle_between(
                shoulder_o.sub(elbow_o).unit(),
                wrist_o.sub(elbow_o).unit(),
            ));
            let sim_a2_3 = gaussian((angle_a2 - angle_b2).abs());
            let angle_a2_2 = deg(angle_between(ua.xy(), wrist.sub(elbow).unit().xy()));
            let angle_b2_2 = deg(angle_between(ua_o.xy(), wrist_o.sub(elbow_o).unit().xy()));
            let sim_a2_2 = gaussian((angle_a2_2 - angle_b2_2).abs());

            let a1 = if limb_total > 0.0 {
                (w.limb2d * sim_a1_2 + w.limb3d * sim_a1_3) / limb_total
            } else {
                0.0
            };
            let a2 = if limb_total > 0.0 {
                (w.limb2d * sim_a2_2 + w.limb3d * sim_a2_3) / limb_total
            } else {
                0.0
            };
            arm_sim_sum += (arm_a * a1 + arm_b * a2) * vis;
            arm_weight_sum += (arm_a + arm_b) * vis;
        }

        // Right arm: shoulder 12, elbow 14, wrist 16, hip 23 (as in the original).
        {
            let shoulder = a[12];
            let elbow = a[14];
            let wrist = a[16];
            let hip = a[23];
            let shoulder_o = b[12];
            let elbow_o = b[14];
            let wrist_o = b[16];
            let hip_o = b[23];
            let vis = [
                safe_conf(self, 12),
                safe_conf(self, 14),
                safe_conf(self, 16),
                safe_conf(other, 12),
                safe_conf(other, 14),
                safe_conf(other, 16),
            ]
            .into_iter()
            .fold(f64::INFINITY, f64::min);

            let ua = shoulder.sub(elbow).unit();
            let utorso = shoulder.sub(hip).unit();
            let ua_o = shoulder_o.sub(elbow_o).unit();
            let utorso_o = shoulder_o.sub(hip_o).unit();

            let angle_a1 = deg(angle_between(ua, utorso));
            let angle_b1 = deg(angle_between(ua_o, utorso_o));
            let sim_a1_3 = gaussian((angle_a1 - angle_b1).abs());
            let sim_a1_2 = gaussian(
                (deg(angle_between(ua.xy().unit(), utorso.xy().unit()))
                    - deg(angle_between(ua_o.xy().unit(), utorso_o.xy().unit())))
                .abs(),
            );

            let angle_a2 = deg(angle_between(
                shoulder.sub(elbow).unit(),
                wrist.sub(elbow).unit(),
            ));
            let angle_b2 = deg(angle_between(
                shoulder_o.sub(elbow_o).unit(),
                wrist_o.sub(elbow_o).unit(),
            ));
            let sim_a2_3 = gaussian((angle_a2 - angle_b2).abs());
            let sim_a2_2 = gaussian(
                (deg(angle_between(
                    shoulder.sub(elbow).unit().xy(),
                    wrist.sub(elbow).unit().xy(),
                )) - deg(angle_between(
                    shoulder_o.sub(elbow_o).unit().xy(),
                    wrist_o.sub(elbow_o).unit().xy(),
                )))
                .abs(),
            );

            let a1 = if limb_total > 0.0 {
                (w.limb2d * sim_a1_2 + w.limb3d * sim_a1_3) / limb_total
            } else {
                0.0
            };
            let a2 = if limb_total > 0.0 {
                (w.limb2d * sim_a2_2 + w.limb3d * sim_a2_3) / limb_total
            } else {
                0.0
            };
            arm_sim_sum += (arm_a * a1 + arm_b * a2) * vis;
            arm_weight_sum += (arm_a + arm_b) * vis;
        }

        // Left leg: hip 23, knee 25, ankle 27, shoulder 11.
        {
            let hip = a[23];
            let knee = a[25];
            let ankle = a[27];
            let shoulder = a[11];
            let hip_o = b[23];
            let knee_o = b[25];
            let ankle_o = b[27];
            let shoulder_o = b[11];
            let vis = [
                safe_conf(self, 23),
                safe_conf(self, 25),
                safe_conf(self, 27),
                safe_conf(other, 23),
                safe_conf(other, 25),
                safe_conf(other, 27),
            ]
            .into_iter()
            .fold(f64::INFINITY, f64::min);

            let angle_a1 = deg(angle_between(
                hip.sub(knee).unit(),
                hip.sub(shoulder).unit(),
            ));
            let angle_b1 = deg(angle_between(
                hip_o.sub(knee_o).unit(),
                hip_o.sub(shoulder_o).unit(),
            ));
            let sim_l1_3 = gaussian((angle_a1 - angle_b1).abs());
            let sim_l1_2 = gaussian(
                (deg(angle_between(
                    hip.sub(knee).unit().xy(),
                    hip.sub(shoulder).unit().xy(),
                )) - deg(angle_between(
                    hip_o.sub(knee_o).unit().xy(),
                    hip_o.sub(shoulder_o).unit().xy(),
                )))
                .abs(),
            );

            let angle_a2 = deg(angle_between(hip.sub(knee).unit(), ankle.sub(knee).unit()));
            let angle_b2 = deg(angle_between(
                hip_o.sub(knee_o).unit(),
                ankle_o.sub(knee_o).unit(),
            ));
            let sim_l2_3 = gaussian((angle_a2 - angle_b2).abs());
            let sim_l2_2 = gaussian(
                (deg(angle_between(
                    hip.sub(knee).unit().xy(),
                    ankle.sub(knee).unit().xy(),
                )) - deg(angle_between(
                    hip_o.sub(knee_o).unit().xy(),
                    ankle_o.sub(knee_o).unit().xy(),
                )))
                .abs(),
            );

            let l1 = if limb_total > 0.0 {
                (w.limb2d * sim_l1_2 + w.limb3d * sim_l1_3) / limb_total
            } else {
                0.0
            };
            let l2 = if limb_total > 0.0 {
                (w.limb2d * sim_l2_2 + w.limb3d * sim_l2_3) / limb_total
            } else {
                0.0
            };
            leg_sim_sum += (leg_a * l1 + leg_b * l2) * vis;
            leg_weight_sum += (leg_a + leg_b) * vis;
        }

        // Right leg: hip 24, knee 26, ankle 28, shoulder 12.
        {
            let hip = a[24];
            let knee = a[26];
            let ankle = a[28];
            let shoulder = a[12];
            let hip_o = b[24];
            let knee_o = b[26];
            let ankle_o = b[28];
            let shoulder_o = b[12];
            let vis = [
                safe_conf(self, 24),
                safe_conf(self, 26),
                safe_conf(self, 28),
                safe_conf(other, 24),
                safe_conf(other, 26),
                safe_conf(other, 28),
            ]
            .into_iter()
            .fold(f64::INFINITY, f64::min);

            let angle_a1 = deg(angle_between(
                hip.sub(knee).unit(),
                hip.sub(shoulder).unit(),
            ));
            let angle_b1 = deg(angle_between(
                hip_o.sub(knee_o).unit(),
                hip_o.sub(shoulder_o).unit(),
            ));
            let sim_l1_3 = gaussian((angle_a1 - angle_b1).abs());
            let sim_l1_2 = gaussian(
                (deg(angle_between(
                    hip.sub(knee).unit().xy(),
                    hip.sub(shoulder).unit().xy(),
                )) - deg(angle_between(
                    hip_o.sub(knee_o).unit().xy(),
                    hip_o.sub(shoulder_o).unit().xy(),
                )))
                .abs(),
            );

            let angle_a2 = deg(angle_between(hip.sub(knee).unit(), ankle.sub(knee).unit()));
            let angle_b2 = deg(angle_between(
                hip_o.sub(knee_o).unit(),
                ankle_o.sub(knee_o).unit(),
            ));
            let sim_l2_3 = gaussian((angle_a2 - angle_b2).abs());
            let sim_l2_2 = gaussian(
                (deg(angle_between(
                    hip.sub(knee).unit().xy(),
                    ankle.sub(knee).unit().xy(),
                )) - deg(angle_between(
                    hip_o.sub(knee_o).unit().xy(),
                    ankle_o.sub(knee_o).unit().xy(),
                )))
                .abs(),
            );

            let l1 = if limb_total > 0.0 {
                (w.limb2d * sim_l1_2 + w.limb3d * sim_l1_3) / limb_total
            } else {
                0.0
            };
            let l2 = if limb_total > 0.0 {
                (w.limb2d * sim_l2_2 + w.limb3d * sim_l2_3) / limb_total
            } else {
                0.0
            };
            leg_sim_sum += (leg_a * l1 + leg_b * l2) * vis;
            leg_weight_sum += (leg_a + leg_b) * vis;
        }

        let arm_sim = if arm_weight_sum > 0.0 {
            arm_sim_sum / arm_weight_sum
        } else {
            0.0
        };
        let leg_sim = if leg_weight_sum > 0.0 {
            leg_sim_sum / leg_weight_sum
        } else {
            0.0
        };

        // Global features use normalized coordinates and raw visibility.
        let mut pts_a = Vec::new();
        let mut pts_b = Vec::new();
        for index in 0..LANDMARK_COUNT {
            if safe_conf(self, index) > w.vis_threshold && a[index].finite() {
                pts_a.push((a[index].x, a[index].y));
            }
            if safe_conf(other, index) > w.vis_threshold && b[index].finite() {
                pts_b.push((b[index].x, b[index].y));
            }
        }

        let (cx_a, cy_a, angle_a, lambda1_a, lambda2_a) = pca2d(&pts_a);
        let (_, _, angle_b, lambda1_b, lambda2_b) = pca2d(&pts_b);

        let mut pca_angle_diff = deg(angle_a - angle_b).abs();
        if pca_angle_diff > 180.0 {
            pca_angle_diff = 360.0 - pca_angle_diff;
        }
        let pca_angle_sim = gaussian(pca_angle_diff);
        let vr_a = if lambda1_a + 1e-12 > 0.0 {
            lambda1_a / (lambda1_a + lambda2_a + 1e-12)
        } else {
            0.5
        };
        let vr_b = if lambda1_b + 1e-12 > 0.0 {
            lambda1_b / (lambda1_b + lambda2_b + 1e-12)
        } else {
            0.5
        };
        let vr_sim = 1.0 - (vr_a - vr_b).abs();
        let pca_sim = 0.6 * pca_angle_sim + 0.4 * clamp01(vr_sim);

        let (ref_cx, ref_cy) = if pts_a.is_empty() {
            (0.0, 0.0)
        } else {
            (cx_a, cy_a)
        };
        let radial_a = radial_signature(&pts_a, ref_cx, ref_cy, w.radial_bins, 1.0);
        let radial_b = radial_signature(&pts_b, ref_cx, ref_cy, w.radial_bins, 1.0);
        let radial_sim = cosine_similarity(&radial_a, &radial_b);

        let hull_a = convex_hull_2d(&pts_a);
        let hull_b = convex_hull_2d(&pts_b);
        let area_a = polygon_area(&hull_a);
        let area_b = polygon_area(&hull_b);
        let area_sim = if area_a > 1e-12 || area_b > 1e-12 {
            clamp01(area_a.min(area_b) / area_a.max(area_b))
        } else {
            0.0
        };

        let global_den = w.pca + w.radial + w.hull;
        let global_sim = if global_den > 0.0 {
            (w.pca * pca_sim + w.radial * radial_sim + w.hull * area_sim) / global_den
        } else {
            0.0
        };

        let total_weights = w.torso + arm_total + leg_total + (w.pca + w.radial + w.hull);
        if total_weights <= 1e-12 {
            return 0.0;
        }
        let final_score = (w.torso * torso_sim
            + arm_total * arm_sim
            + leg_total * leg_sim
            + (w.pca + w.radial + w.hull) * global_sim)
            / total_weights;
        clamp01(final_score)
    }
}

fn safe_conf(pose: &Pose, index: usize) -> f64 {
    let confidence = pose.points[index].confidence;
    if !confidence.is_finite() {
        0.0
    } else {
        confidence.clamp(0.0, 1.0)
    }
}

fn angle_between(left: Vec3, right: Vec3) -> f64 {
    let left_len = left.length();
    let right_len = right.length();
    if !left_len.is_finite() || !right_len.is_finite() || left_len <= 1e-12 || right_len <= 1e-12 {
        return 0.0;
    }
    let mut cosine = left.dot(right) / (left_len * right_len);
    if !cosine.is_finite() {
        cosine = 0.0;
    }
    cosine.clamp(-1.0, 1.0).acos()
}

fn hip_center(pose: &Pose) -> Vec3 {
    pose.points[23].pos.add(pose.points[24].pos).mul(0.5)
}

fn shoulder_center(pose: &Pose) -> Vec3 {
    pose.points[11].pos.add(pose.points[12].pos).mul(0.5)
}

fn torso_length(pose: &Pose) -> f64 {
    let length = shoulder_center(pose).sub(hip_center(pose)).length();
    if !length.is_finite() || length < 1e-6 {
        1.0
    } else {
        length
    }
}

fn normalize(pose: &Pose) -> [Vec3; LANDMARK_COUNT] {
    let hip = hip_center(pose);
    let scale = torso_length(pose);
    let mut out = [Vec3::default(); LANDMARK_COUNT];
    for (slot, point) in out.iter_mut().zip(pose.points.iter()) {
        *slot = point.pos.sub(hip).div(scale);
    }
    out
}

fn mirror(pose: &Pose) -> Pose {
    let mirror_map = [
        0, 1, 2, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11, 14, 13, 16, 15, 18, 17, 20, 19, 22, 21, 24, 23,
        26, 25, 28, 27, 30, 29, 32, 31,
    ];
    let mut points = [Point::default(); LANDMARK_COUNT];
    for index in 0..LANDMARK_COUNT {
        let source = mirror_map[index];
        points[index] = pose.points[source];
        points[index].pos.x = 1.0 - points[index].pos.x;
    }
    Pose { points }
}

fn radial_signature(
    points: &[(f64, f64)],
    cx: f64,
    cy: f64,
    bins: usize,
    norm_scale: f64,
) -> Vec<f64> {
    let mut signature = vec![0.0; bins];
    if points.is_empty() || bins == 0 {
        return signature;
    }
    let two_pi = 2.0 * std::f64::consts::PI;
    for &(x, y) in points {
        let dx = x - cx;
        let dy = y - cy;
        let mut angle = dy.atan2(dx);
        if angle < 0.0 {
            angle += two_pi;
        }
        let bin = ((angle / two_pi * bins as f64).floor() as usize) % bins;
        let mut radius = (dx * dx + dy * dy).sqrt();
        if norm_scale > 1e-12 {
            radius /= norm_scale;
        }
        signature[bin] = signature[bin].max(radius);
    }
    let sum_sq: f64 = signature.iter().map(|value| value * value).sum();
    if sum_sq > 1e-12 {
        let inverse = 1.0 / sum_sq.sqrt();
        for value in &mut signature {
            *value *= inverse;
        }
    }
    signature
}

fn cosine_similarity(left: &[f64], right: &[f64]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for index in 0..left.len() {
        dot += left[index] * right[index];
        left_norm += left[index] * left[index];
        right_norm += right[index] * right[index];
    }
    if left_norm <= 1e-12 || right_norm <= 1e-12 {
        return 0.0;
    }
    (dot / (left_norm.sqrt() * right_norm.sqrt())).clamp(0.0, 1.0)
}

fn pca2d(points: &[(f64, f64)]) -> (f64, f64, f64, f64, f64) {
    if points.is_empty() {
        return (0.0, 0.0, 0.0, 0.0, 0.0);
    }
    let count = points.len() as f64;
    let mut cx = 0.0;
    let mut cy = 0.0;
    for &(x, y) in points {
        cx += x;
        cy += y;
    }
    cx /= count;
    cy /= count;

    let mut sxx = 0.0;
    let mut syy = 0.0;
    let mut sxy = 0.0;
    for &(x, y) in points {
        let dx = x - cx;
        let dy = y - cy;
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    sxx /= count;
    syy /= count;
    sxy /= count;

    let trace = sxx + syy;
    let determinant = sxx * syy - sxy * sxy;
    let temp = (trace * trace * 0.25 - determinant).max(0.0).sqrt();
    let lambda1 = trace * 0.5 + temp;
    let lambda2 = trace * 0.5 - temp;
    let angle = if sxy.abs() > 1e-12 || (sxx - lambda1).abs() > 1e-12 {
        let mut vx = sxy;
        let mut vy = lambda1 - sxx;
        if vx.abs() < 1e-12 && vy.abs() < 1e-12 {
            vx = 1.0;
            vy = 0.0;
        }
        vy.atan2(vx)
    } else {
        0.0
    };
    (cx, cy, angle, lambda1, lambda2)
}

fn convex_hull_2d(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut points = points.to_vec();
    points.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.1.partial_cmp(&right.1).unwrap_or(Ordering::Equal))
    });
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };

    let mut lower: Vec<(f64, f64)> = Vec::new();
    for &point in &points {
        while lower.len() >= 2
            && cross(lower[lower.len() - 2], lower[lower.len() - 1], point) <= 0.0
        {
            lower.pop();
        }
        lower.push(point);
    }
    let mut upper: Vec<(f64, f64)> = Vec::new();
    for &point in points.iter().rev() {
        while upper.len() >= 2
            && cross(upper[upper.len() - 2], upper[upper.len() - 1], point) <= 0.0
        {
            upper.pop();
        }
        upper.push(point);
    }
    let mut hull = lower;
    for item in upper.iter().skip(1).take(upper.len().saturating_sub(2)) {
        hull.push(*item);
    }
    hull
}

fn polygon_area(polygon: &[(f64, f64)]) -> f64 {
    if polygon.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for index in 0..polygon.len() {
        let next = (index + 1) % polygon.len();
        area += polygon[index].0 * polygon[next].1 - polygon[next].0 * polygon[index].1;
    }
    area.abs() * 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn landmark(x: f32, y: f32, z: f32, visibility: f32) -> PoseLandmark {
        PoseLandmark {
            x,
            y,
            z,
            visibility,
        }
    }

    fn standing() -> Vec<PoseLandmark> {
        let mut points = vec![landmark(0.5, 0.5, 0.0, 1.0); LANDMARK_COUNT];
        let set = |points: &mut Vec<PoseLandmark>, index: usize, x: f32, y: f32, z: f32| {
            points[index] = landmark(x, y, z, 1.0);
        };
        set(&mut points, 11, 0.42, 0.3, 0.0);
        set(&mut points, 12, 0.58, 0.3, 0.0);
        set(&mut points, 13, 0.36, 0.45, 0.0);
        set(&mut points, 14, 0.64, 0.45, 0.0);
        set(&mut points, 15, 0.33, 0.6, 0.0);
        set(&mut points, 16, 0.67, 0.6, 0.0);
        set(&mut points, 23, 0.45, 0.55, 0.0);
        set(&mut points, 24, 0.55, 0.55, 0.0);
        set(&mut points, 25, 0.44, 0.72, 0.0);
        set(&mut points, 26, 0.56, 0.72, 0.0);
        set(&mut points, 27, 0.43, 0.9, 0.0);
        set(&mut points, 28, 0.57, 0.9, 0.0);
        points
    }

    #[test]
    fn identical_pose_scores_one() {
        let pose = Pose::from_landmarks(&standing());
        let score = pose.similarity(&pose, &Weights::default());
        assert!((score - 1.0).abs() < 1e-9, "score={score}");
    }

    #[test]
    fn mirrored_pose_still_scores_one() {
        let pose = Pose::from_landmarks(&standing());
        let mirrored = mirror(&pose);
        let score = pose.similarity(&mirrored, &Weights::default());
        assert!((score - 1.0).abs() < 1e-6, "score={score}");
    }

    #[test]
    fn raised_arm_lowers_score() {
        let pose = Pose::from_landmarks(&standing());
        let mut raised = standing();
        raised[13] = landmark(0.36, 0.2, 0.0, 1.0);
        raised[15] = landmark(0.33, 0.1, 0.0, 1.0);
        let raised = Pose::from_landmarks(&raised);
        let score = pose.similarity(&raised, &Weights::default());
        assert!(score < 0.99, "score={score}");
        assert!(score > 0.5, "score={score}");
    }

    // Golden values generated by compiling the original
    // `MediaPipePoseData::similarity` with MSVC, with landmarks cast to f32
    // first so both implementations see identical inputs.
    #[allow(clippy::excessive_precision)]
    #[test]
    fn matches_original_program_golden_values() {
        fn set(points: &mut [PoseLandmark], index: usize, x: f64, y: f64, z: f64, visibility: f64) {
            points[index] = landmark(x as f32, y as f32, z as f32, visibility as f32);
        }

        fn standing() -> Vec<PoseLandmark> {
            let mut points = vec![landmark(0.5, 0.5, 0.0, 1.0); LANDMARK_COUNT];
            for (index, x, y) in [
                (11, 0.42, 0.3),
                (12, 0.58, 0.3),
                (13, 0.36, 0.45),
                (14, 0.64, 0.45),
                (15, 0.33, 0.6),
                (16, 0.67, 0.6),
                (23, 0.45, 0.55),
                (24, 0.55, 0.55),
                (25, 0.44, 0.72),
                (26, 0.56, 0.72),
                (27, 0.43, 0.9),
                (28, 0.57, 0.9),
            ] {
                set(&mut points, index, x, y, 0.0, 1.0);
            }
            points
        }

        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> f64 {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (self.0 >> 33) as f64 / (1_u64 << 31) as f64
            }
        }

        fn random_pose(rng: &mut Lcg, count: usize) -> Vec<PoseLandmark> {
            let mut points = vec![landmark(0.5, 0.5, 0.0, 0.0); LANDMARK_COUNT];
            for index in 0..count.min(LANDMARK_COUNT) {
                let x = rng.next();
                let y = rng.next();
                let z = rng.next() * 0.4 - 0.2;
                let visibility = 0.5 + rng.next() * 0.5;
                set(&mut points, index, x, y, z, visibility);
            }
            points
        }

        let base = standing();

        let mut raised = base.clone();
        set(&mut raised, 13, 0.36, 0.2, 0.0, 1.0);
        set(&mut raised, 15, 0.33, 0.1, 0.0, 1.0);

        let mut translated = vec![landmark(0.0, 0.0, 0.0, 0.0); LANDMARK_COUNT];
        for (index, point) in base.iter().enumerate() {
            set(
                &mut translated,
                index,
                f64::from(point.x) * 0.8 + 0.1,
                f64::from(point.y) * 0.8 + 0.05,
                f64::from(point.z) * 0.8,
                f64::from(point.visibility),
            );
        }

        let mut mirrored_x = vec![landmark(0.0, 0.0, 0.0, 0.0); LANDMARK_COUNT];
        for (index, point) in base.iter().enumerate() {
            set(
                &mut mirrored_x,
                index,
                1.0 - f64::from(point.x),
                f64::from(point.y),
                f64::from(point.z),
                f64::from(point.visibility),
            );
        }

        let mut bent = base.clone();
        set(&mut bent, 14, 0.7, 0.3, 0.0, 1.0);
        set(&mut bent, 16, 0.7, 0.1, 0.0, 1.0);
        set(&mut bent, 26, 0.65, 0.6, 0.0, 1.0);
        set(&mut bent, 28, 0.75, 0.7, 0.0, 1.0);

        let mut missing = base.clone();
        for index in [13, 14, 15, 16, 23, 24, 25, 26, 27, 28] {
            set(&mut missing, index, 0.0, 0.0, 0.0, 0.0);
        }

        let mut rng = Lcg(0x1234_5678_9abc_def0);
        let random_a = random_pose(&mut rng, LANDMARK_COUNT);

        let random_b = random_pose(&mut rng, LANDMARK_COUNT);
        let partial = random_pose(&mut rng, 15);

        let weights = Weights::default();
        let cases = [
            ("standing-self", &base, &base, 1.0_f64),
            ("standing-raised", &base, &raised, 0.905_121_781_955_451_23),
            (
                "standing-translated",
                &base,
                &translated,
                0.999_999_988_939_879_4,
            ),
            (
                "standing-mirrored-x",
                &base,
                &mirrored_x,
                0.824_999_997_368_694_34,
            ),
            ("standing-bent", &base, &bent, 0.721_192_550_101_614_98),
            (
                "standing-missing",
                &base,
                &missing,
                0.099_093_350_224_062_96,
            ),
            (
                "standing-random",
                &base,
                &random_a,
                0.155_409_876_689_238_36,
            ),
            (
                "random-random",
                &random_a,
                &random_b,
                0.173_464_933_499_687_75,
            ),
            (
                "random-partial",
                &random_a,
                &partial,
                0.135_403_018_106_147_02,
            ),
        ];
        for (name, left, right, expected) in cases {
            let left = Pose::from_landmarks(left);
            let right = Pose::from_landmarks(right);
            let score = left.similarity(&right, &weights);
            assert!(
                (score - expected).abs() < 1e-12,
                "{name}: rust={score:.17} cpp={expected:.17}"
            );
        }
    }

    #[test]
    fn missing_landmarks_do_not_panic() {
        let pose = Pose::from_landmarks(&[]);
        let score = pose.similarity(&pose, &Weights::default());
        // 原算法对全零点的 PCA 退化会得到 pcaSim=1，因此结果为 pca 权重占比 0.1；
        // 保留这一行为，避免与旧程序分数不一致。
        assert!(score.is_finite());
        assert!((score - 0.1).abs() < 1e-9, "score={score}");
    }
}
