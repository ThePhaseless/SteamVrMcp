//! The hand model: finger curls and gestures turned into 26 joints in OpenXR `XR_EXT_hand_tracking`
//! order, which ALVR converts to SteamVR's 31 bones (server_openvr/src/tracking.rs).

use crate::rig::Hand;
use alvr_common::{
    Pose,
    glam::{Quat, Vec3},
};
use rmcp::schemars::JsonSchema;
use serde::Deserialize;

/// 0 is straight, 1 fully curled; splay 0 is fingers together, 1 spread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(default)]
#[schemars(crate = "rmcp::schemars")]
pub struct Curls {
    pub thumb: f32,
    pub index: f32,
    pub middle: f32,
    pub ring: f32,
    pub pinky: f32,
    pub splay: f32,
}

impl Curls {
    pub fn check(&self) -> Result<(), String> {
        let c = self;
        for (name, v) in [("thumb", c.thumb), ("index", c.index), ("middle", c.middle), ("ring", c.ring), ("pinky", c.pinky), ("splay", c.splay)] {
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("{name} must be within 0 and 1, got {v}"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Gesture {
    Open,
    Fist,
    Point,
    Pinch,
    ThumbsUp,
}

/// The curls of each gesture; pinch is tuned so the thumb and index tips meet within ALVR's click distance.
pub fn gesture_curls(g: Gesture) -> Curls {
    let [thumb, index, middle, ring, pinky] = match g {
        Gesture::Open => [0.0; 5],
        Gesture::Fist => [0.6, 1.0, 1.0, 1.0, 1.0],
        Gesture::Point => [0.6, 0.0, 1.0, 1.0, 1.0],
        Gesture::ThumbsUp => [0.0, 1.0, 1.0, 1.0, 1.0],
        Gesture::Pinch => [PINCH_THUMB, PINCH_INDEX, 0.0, 0.0, 0.0],
    };
    Curls { thumb, index, middle, ring, pinky, splay: 0.0 }
}

// Right hand, palm frame (OpenXR joint axes: -Z along the bone toward the tip, +Y the back of
// the hand). Palm identity is palm down, fingers forward; the thumb is on the -X side.
const WRIST_Z: f32 = 0.04;
const METACARPAL_Z: f32 = 0.03;
/// Index, middle, ring, pinky: base x, metacarpal, proximal, intermediate and distal lengths in meters.
const FINGERS: [(f32, [f32; 4]); 4] = [
    (-0.025, [0.075, 0.040, 0.024, 0.022]),
    (-0.005, [0.077, 0.045, 0.028, 0.024]),
    (0.015, [0.074, 0.042, 0.027, 0.023]),
    (0.033, [0.068, 0.033, 0.019, 0.021]),
];
/// Proximal, intermediate and distal turn at curl 1, in degrees, about each joint's +X (toward the palm).
const FINGER_CURL_DEG: [f32; 3] = [80.0, 100.0, 70.0];
/// Proximal turn about its own Y per step away from the middle finger at splay 1.
const SPLAY_DEG: f32 = 10.0;
const THUMB_BASE: Vec3 = Vec3::new(-0.02, -0.01, 0.025);
/// Thumb metacarpal, proximal and distal lengths.
const THUMB_LENGTHS: [f32; 3] = [0.045, 0.032, 0.028];
/// The open thumb's orientation: yaw toward -X, then roll so its palm side (-Y) faces the index.
const THUMB_YAW_DEG: f32 = 70.0;
const THUMB_ROLL_DEG: f32 = 50.0;
/// Metacarpal, proximal and distal turn at curl 1, about each joint's +X, toward the index base.
const THUMB_CURL_DEG: [f32; 3] = [40.0, 50.0, 40.0];
// Thumb yaw, roll and curl angles and these two were tuned together by a grid search: the tips
// then meet 0.8 mm apart, well inside ALVR's 14 mm click distance.
const PINCH_THUMB: f32 = 0.9;
const PINCH_INDEX: f32 = 0.6;

/// Turns of `deg × curl` about +X, the curl direction.
fn curl(deg: f32, c: f32) -> Quat {
    Quat::from_rotation_x(-(deg * c).to_radians())
}

/// Fills `out` (one joint per bone plus the tip) from `base`: each bone turns relative to its
/// parent and runs its length along its own -Z; the tip keeps the last bone's orientation.
fn digit(base: Pose, bones: &[(Quat, f32)], out: &mut [Pose]) {
    let mut pose = base;
    for (&(turn, length), joint) in bones.iter().zip(out.iter_mut()) {
        pose.orientation *= turn;
        *joint = pose;
        pose.position += pose.orientation * Vec3::new(0.0, 0.0, -length);
    }
    out[bones.len()] = pose;
}

/// The 26 joints in the playspace, in OpenXR XR_EXT_hand_tracking order, for a palm at `palm`.
pub fn skeleton(hand: Hand, palm: Pose, c: &Curls) -> [Pose; 26] {
    let mut j = [Pose::IDENTITY; 26];
    j[1].position = Vec3::new(0.0, 0.0, WRIST_Z);
    let thumb = Pose {
        orientation: Quat::from_rotation_y(THUMB_YAW_DEG.to_radians()) * Quat::from_rotation_z(THUMB_ROLL_DEG.to_radians()),
        position: THUMB_BASE,
    };
    let bones = [0, 1, 2].map(|k| (curl(THUMB_CURL_DEG[k], c.thumb), THUMB_LENGTHS[k]));
    digit(thumb, &bones, &mut j[2..6]);
    for (f, ((x, [metacarpal, l1, l2, l3]), c_f)) in FINGERS.into_iter().zip([c.index, c.middle, c.ring, c.pinky]).enumerate() {
        let splay = Quat::from_rotation_y((1.0 - f as f32) * (SPLAY_DEG * c.splay).to_radians());
        let [a1, a2, a3] = FINGER_CURL_DEG;
        let bones = [(Quat::IDENTITY, metacarpal), (splay * curl(a1, c_f), l1), (curl(a2, c_f), l2), (curl(a3, c_f), l3)];
        let base = Pose { orientation: Quat::IDENTITY, position: Vec3::new(x, 0.0, METACARPAL_Z) };
        digit(base, &bones, &mut j[6 + 5 * f..11 + 5 * f]);
    }
    j.map(|p| {
        let (o, x) = (p.orientation, p.position);
        let p = match hand {
            Hand::Right => p,
            // mirror across the YZ plane
            Hand::Left => Pose { orientation: Quat::from_xyzw(o.x, -o.y, -o.z, o.w), position: Vec3::new(-x.x, x.y, x.z) },
        };
        palm * p
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ALVR's local_pose (server_openvr/src/tracking.rs:168-189), orientation only: the child
    /// relative to its parent, in SteamVR's hand-specific bone basis.
    fn local(hand: Hand, parent: Pose, child: Pose) -> Quat {
        let o = parent.orientation.conjugate() * child.orientation;
        match hand {
            Hand::Left => Quat::from_xyzw(-o.z, -o.y, -o.x, o.w),
            Hand::Right => Quat::from_xyzw(o.z, o.y, -o.x, o.w),
        }
    }

    /// ALVR's curl readout for index (1) to pinky (4), Controller.cpp:316-330: 0.67 × the sum of
    /// the bone rotations' z over proximal, intermediate and distal.
    fn readout(hand: Hand, gj: &[Pose; 26], finger: usize) -> f32 {
        let metacarpal = 1 + 5 * finger;
        0.67 * (metacarpal + 1..=metacarpal + 3).map(|j| local(hand, gj[j - 1], gj[j]).z).sum::<f32>()
    }

    /// ALVR sends no thumb curl component, so the thumb is checked by how far its proximal and
    /// distal joints turn relative to their parents, in degrees.
    fn thumb_bend(gj: &[Pose; 26]) -> f32 {
        (3..=4).map(|j| gj[j - 1].orientation.angle_between(gj[j].orientation).to_degrees()).sum()
    }

    /// Thumb, index, middle, ring, pinky.
    fn curls(c: [f32; 5]) -> Curls {
        Curls { thumb: c[0], index: c[1], middle: c[2], ring: c[3], pinky: c[4], splay: 0.0 }
    }

    #[test]
    fn open_hand_is_flat() {
        let gj = skeleton(Hand::Right, Pose::IDENTITY, &Curls::default());
        let tip = gj[10].position - gj[0].position;
        assert!((0.12..=0.20).contains(&tip.length()) && tip.z < -0.1, "index tip {tip}");
    }

    #[test]
    fn curls_read_back_per_finger() {
        for hand in [Hand::Left, Hand::Right] {
            let gj = skeleton(hand, Pose::IDENTITY, &Curls::default());
            for f in 1..=4 {
                assert!(readout(hand, &gj, f) <= 0.05, "{hand:?} open finger {f}: {}", readout(hand, &gj, f));
            }
            assert!(thumb_bend(&gj) <= 1.0, "{hand:?} open thumb bends {}", thumb_bend(&gj));
            for curled in 0..5 {
                let mut c = [0.0; 5];
                c[curled] = 1.0;
                let gj = skeleton(hand, Pose::IDENTITY, &curls(c));
                for f in 1..=4 {
                    let r = readout(hand, &gj, f);
                    assert!(if f == curled { r >= 0.9 } else { r <= 0.05 }, "{hand:?} curl {curled}: finger {f} reads {r}");
                }
                let bend = thumb_bend(&gj);
                assert!(if curled == 0 { bend >= 60.0 } else { bend <= 1.0 }, "{hand:?} curl {curled}: thumb bends {bend}°");
            }
        }
    }

    #[test]
    fn curls_are_monotonic() {
        let read = |v| readout(Hand::Right, &skeleton(Hand::Right, Pose::IDENTITY, &curls([0.0, v, 0.0, 0.0, 0.0])), 1);
        let r = [0.25, 0.5, 0.75, 1.0].map(read);
        assert!(r.windows(2).all(|w| w[0] < w[1]), "{r:?}");
    }

    #[test]
    fn pinch_meets_alvr_threshold() {
        for hand in [Hand::Left, Hand::Right] {
            let gj = skeleton(hand, Pose::IDENTITY, &gesture_curls(Gesture::Pinch));
            // ALVR's pinch click: pinch_touch_distance 0 + thumb 0.0075 + index 0.0065 (hand_gestures.rs:113-121)
            let d = gj[5].position.distance(gj[10].position);
            assert!(d < 0.014, "{hand:?} thumb tip to index tip {d}");
        }
    }

    #[test]
    fn fist_tips_near_palm() {
        let gj = skeleton(Hand::Right, Pose::IDENTITY, &gesture_curls(Gesture::Fist));
        for tip in [10, 15, 20, 25] {
            let d = gj[tip].position.distance(gj[0].position);
            assert!(d < 0.06, "tip {tip} is {d} m from the palm");
        }
    }

    #[test]
    fn left_mirrors_right() {
        let c = Curls { thumb: 0.3, index: 0.7, middle: 0.2, ring: 0.9, pinky: 0.5, splay: 0.6 };
        let (left, right) = (skeleton(Hand::Left, Pose::IDENTITY, &c), skeleton(Hand::Right, Pose::IDENTITY, &c));
        for (i, (l, r)) in left.iter().zip(&right).enumerate() {
            let (p, o) = (r.position, r.orientation);
            assert!(l.position.abs_diff_eq(Vec3::new(-p.x, p.y, p.z), 1e-6), "joint {i}: {l:?} vs {r:?}");
            assert!(l.orientation.abs_diff_eq(Quat::from_xyzw(o.x, -o.y, -o.z, o.w), 1e-6), "joint {i}: {l:?} vs {r:?}");
        }
    }

    #[test]
    fn skeleton_follows_palm() {
        let palm = Pose { orientation: Quat::from_rotation_y(1.0), position: Vec3::new(0.2, 1.2, -0.3) };
        let c = gesture_curls(Gesture::Point);
        let (posed, local) = (skeleton(Hand::Right, palm, &c), skeleton(Hand::Right, Pose::IDENTITY, &c));
        assert!(posed[0].position.abs_diff_eq(palm.position, 1e-6));
        assert!(posed[10].position.abs_diff_eq(palm.position + palm.orientation * local[10].position, 1e-6));
    }
}
