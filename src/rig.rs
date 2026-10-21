//! The virtual body: head and controller poses, moves and animations, sampled by the headset's
//! tracking thread. Poses are SteamVR standing space (the default session recenters in Stage).

use alvr_common::{
    DeviceMotion, HAND_LEFT_ID, HAND_RIGHT_ID, HEAD_ID, Pose,
    glam::{EulerRot, Quat, Vec3},
};
use alvr_packets::{FaceData, TrackingData};
use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Hand {
    Left,
    Right,
}

impl Hand {
    pub fn device_id(self) -> u64 {
        match self {
            Hand::Left => *HAND_LEFT_ID,
            Hand::Right => *HAND_RIGHT_ID,
        }
    }
}

/// An absolute pose: meters, degrees; +Y up, -Z forward.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PoseArgs {
    /// x, y, z in meters.
    pub position: [f32; 3],
    /// Degrees, positive turns left (about +Y). Applied yaw, then pitch, then roll.
    pub yaw: Option<f32>,
    /// Degrees, positive looks up (about +X).
    pub pitch: Option<f32>,
    /// Degrees, positive tilts left (about +Z).
    pub roll: Option<f32>,
    /// x, y, z, w; normalized. Instead of yaw/pitch/roll.
    pub quat: Option<[f32; 4]>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Frame {
    #[default]
    Playspace,
    Local,
}

/// A relative move. Rotations are always about the device's own axes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveArgs {
    /// Meters, in `frame`.
    #[serde(default)]
    pub offset: [f32; 3],
    #[serde(default)]
    pub yaw: f32,
    #[serde(default)]
    pub pitch: f32,
    #[serde(default)]
    pub roll: f32,
    /// "playspace": offset along the playspace axes; "local": along the device's own axes.
    #[serde(default)]
    pub frame: Frame,
}

/// Moves smoothly from the current pose through each waypoint in turn, evenly spaced in time.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AnimateArgs {
    /// 1 to 64 poses.
    pub waypoints: Vec<PoseArgs>,
    /// Seconds, more than 0 and at most 600.
    pub duration_s: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum RelativeTo {
    #[default]
    Playspace,
    /// The head pose at the time of the call.
    Head,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ControllerPoseArgs {
    pub hand: Hand,
    #[serde(flatten)]
    pub pose: PoseArgs,
    #[serde(default)]
    pub relative_to: RelativeTo,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveControllerArgs {
    pub hand: Hand,
    #[serde(flatten)]
    pub mv: MoveArgs,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AnimateControllerArgs {
    pub hand: Hand,
    #[serde(flatten)]
    pub anim: AnimateArgs,
    #[serde(default)]
    pub relative_to: RelativeTo,
}

/// ALVR 21 has one flag per hand for both: a hand without a pose is also disconnected
/// (Controller.cpp:187-205). The controller is active when neither given value is false.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ControllerStateArgs {
    pub hand: Hand,
    pub connected: Option<bool>,
    pub tracked: Option<bool>,
}

impl ControllerStateArgs {
    /// The merged flag, or None when neither value is given.
    pub fn active(&self) -> Option<bool> {
        (self.connected.is_some() || self.tracked.is_some()).then(|| self.connected != Some(false) && self.tracked != Some(false))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", content = "args", rename_all = "snake_case")]
pub enum RigAction {
    SetHeadPose(PoseArgs),
    MoveHead(MoveArgs),
    AnimateHead(AnimateArgs),
    SetControllerPose(ControllerPoseArgs),
    MoveController(MoveControllerArgs),
    AnimateController(AnimateControllerArgs),
    SetControllerState(ControllerStateArgs),
}

impl PoseArgs {
    pub fn to_pose(&self) -> Result<Pose, String> {
        let position = Vec3::from_array(self.position);
        if !position.is_finite() {
            return Err("position must be finite".into());
        }
        let euler = [self.yaw, self.pitch, self.roll];
        let orientation = match self.quat {
            Some(_) if euler.iter().any(Option::is_some) => return Err("give yaw/pitch/roll or quat, not both".into()),
            Some(q) => {
                let q = Quat::from_array(q);
                if !q.is_finite() || q.length() < 1e-6 {
                    return Err("quat must be finite and non-zero".into());
                }
                q.normalize()
            }
            None => {
                let [yaw, pitch, roll] = euler.map(|a| a.unwrap_or(0.0));
                euler_quat(yaw, pitch, roll)?
            }
        };
        Ok(Pose { orientation, position })
    }
}

fn euler_quat(yaw: f32, pitch: f32, roll: f32) -> Result<Quat, String> {
    if ![yaw, pitch, roll].iter().all(|a| a.is_finite()) {
        return Err("yaw, pitch and roll must be finite".into());
    }
    Ok(Quat::from_euler(EulerRot::YXZ, yaw.to_radians(), pitch.to_radians(), roll.to_radians()))
}

/// Keys spaced `step_s` apart from `start`; the first key is the pose the animation started at.
struct Animation {
    start: Instant,
    step_s: f32,
    keys: Vec<Pose>,
}

impl Animation {
    /// The motion at `now`, or None once past the last key.
    fn sample(&self, now: Instant) -> Option<DeviceMotion> {
        let (keys, dt) = (&self.keys, self.step_s);
        let s = now.saturating_duration_since(self.start).as_secs_f32() / dt;
        let i = s as usize;
        if i + 1 >= keys.len() {
            return None;
        }
        let u = s - i as f32;
        let p = |k: usize| keys[k].position;
        // Catmull-Rom tangents per second, zero at both ends
        let m = |k: usize| if k == 0 || k + 1 == keys.len() { Vec3::ZERO } else { (p(k + 1) - p(k - 1)) / (2.0 * dt) };
        let (u2, u3) = (u * u, u * u * u);
        let position = (2.0 * u3 - 3.0 * u2 + 1.0) * p(i)
            + (u3 - 2.0 * u2 + u) * dt * m(i)
            + (-2.0 * u3 + 3.0 * u2) * p(i + 1)
            + (u3 - u2) * dt * m(i + 1);
        let linear_velocity = ((6.0 * u2 - 6.0 * u) * p(i)
            + (3.0 * u2 - 4.0 * u + 1.0) * dt * m(i)
            + (-6.0 * u2 + 6.0 * u) * p(i + 1)
            + (3.0 * u2 - 2.0 * u) * dt * m(i + 1))
            / dt;
        // shortcut: piecewise slerp; use squad if apps show jerk at waypoints
        let (q0, q1) = (keys[i].orientation, keys[i + 1].orientation);
        let q1 = if q0.dot(q1) < 0.0 { -q1 } else { q1 };
        let (axis, angle) = (q1 * q0.inverse()).to_axis_angle();
        Some(DeviceMotion {
            pose: Pose { orientation: q0.slerp(q1, u), position },
            linear_velocity,
            angular_velocity: axis * angle / dt,
        })
    }
}

/// A head or controller: a resting pose, or an animation that ends in one.
struct Body {
    pose: Pose,
    animation: Option<Animation>,
}

impl Body {
    fn at(position: Vec3) -> Body {
        Body { pose: Pose { orientation: Quat::IDENTITY, position }, animation: None }
    }

    /// The motion at `now`; an animation past its end settles into its last key.
    fn motion(&mut self, now: Instant) -> DeviceMotion {
        if let Some(m) = self.animation.as_ref().and_then(|a| a.sample(now)) {
            return m;
        }
        if let Some(a) = self.animation.take() {
            self.pose = *a.keys.last().unwrap();
        }
        DeviceMotion { pose: self.pose, ..Default::default() }
    }

    /// Stops any animation where it is now and returns that pose.
    fn settle(&mut self, now: Instant) -> Pose {
        self.pose = self.motion(now).pose;
        self.animation = None;
        self.pose
    }

    fn set(&mut self, pose: Pose, now: Instant) {
        self.settle(now);
        self.pose = pose;
    }

    fn move_by(&mut self, mv: &MoveArgs, now: Instant) -> Result<(), String> {
        let offset = Vec3::from_array(mv.offset);
        if !offset.is_finite() {
            return Err("offset must be finite".into());
        }
        let delta = euler_quat(mv.yaw, mv.pitch, mv.roll)?;
        let pose = self.settle(now);
        let offset = if mv.frame == Frame::Local { pose.orientation * offset } else { offset };
        self.pose = Pose { orientation: (pose.orientation * delta).normalize(), position: pose.position + offset };
        Ok(())
    }

    fn animate(&mut self, anim: &AnimateArgs, origin: Pose, now: Instant) -> Result<Duration, String> {
        if !(anim.duration_s > 0.0 && anim.duration_s <= 600.0) {
            return Err("duration_s must be more than 0 and at most 600".into());
        }
        if !(1..=64).contains(&anim.waypoints.len()) {
            return Err("give 1 to 64 waypoints".into());
        }
        let waypoints = anim.waypoints.iter().map(|w| Ok(origin * w.to_pose()?)).collect::<Result<Vec<_>, String>>()?;
        let keys = [vec![self.settle(now)], waypoints].concat();
        self.animation = Some(Animation { start: now, step_s: anim.duration_s / anim.waypoints.len() as f32, keys });
        Ok(Duration::from_secs_f32(anim.duration_s))
    }
}

pub struct Rig {
    head: Body,
    /// left, right
    controllers: [Body; 2],
    active: [bool; 2],
}

impl Default for Rig {
    fn default() -> Rig {
        Rig::new()
    }
}

impl Rig {
    pub fn new() -> Rig {
        Rig {
            head: Body::at(Vec3::new(0.0, 1.6, 0.0)),
            controllers: [Body::at(Vec3::new(-0.2, 1.2, -0.3)), Body::at(Vec3::new(0.2, 1.2, -0.3))],
            active: [true; 2],
        }
    }

    /// Where a controller pose given `relative_to` lands in the playspace.
    fn origin(&mut self, relative_to: RelativeTo, now: Instant) -> Pose {
        match relative_to {
            RelativeTo::Playspace => Pose::IDENTITY,
            RelativeTo::Head => self.head.motion(now).pose,
        }
    }

    /// Applies `a` and returns how long until it is done: an animation's duration, else zero.
    pub fn apply(&mut self, a: RigAction, now: Instant) -> Result<Duration, String> {
        match a {
            RigAction::SetHeadPose(p) => self.head.set(p.to_pose()?, now),
            RigAction::MoveHead(mv) => self.head.move_by(&mv, now)?,
            RigAction::AnimateHead(anim) => return self.head.animate(&anim, Pose::IDENTITY, now),
            RigAction::SetControllerPose(c) => {
                let pose = self.origin(c.relative_to, now) * c.pose.to_pose()?;
                self.controllers[c.hand as usize].set(pose, now);
            }
            RigAction::MoveController(c) => self.controllers[c.hand as usize].move_by(&c.mv, now)?,
            RigAction::AnimateController(c) => {
                let origin = self.origin(c.relative_to, now);
                return self.controllers[c.hand as usize].animate(&c.anim, origin, now);
            }
            RigAction::SetControllerState(c) => {
                self.active[c.hand as usize] = c.active().ok_or("give connected or tracked")?;
            }
        }
        Ok(Duration::ZERO)
    }

    /// The head plus each active controller at `now`.
    pub fn tracking(&mut self, now: Instant, poll_timestamp: Duration) -> TrackingData {
        let mut device_motions = vec![(*HEAD_ID, self.head.motion(now))];
        for hand in [Hand::Left, Hand::Right] {
            if self.active[hand as usize] {
                device_motions.push((hand.device_id(), self.controllers[hand as usize].motion(now)));
            }
        }
        TrackingData { poll_timestamp, device_motions, hand_skeletons: [None, None], face: FaceData::default(), body: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec3, b: Vec3) -> bool {
        a.distance(b) < 1e-4
    }

    fn pose_at(p: [f32; 3]) -> PoseArgs {
        PoseArgs { position: p, ..Default::default() }
    }

    fn motion(rig: &mut Rig, now: Instant, id: u64) -> Option<DeviceMotion> {
        rig.tracking(now, Duration::ZERO).device_motions.into_iter().find(|(d, _)| *d == id).map(|(_, m)| m)
    }

    #[test]
    fn yaw_90_turns_left() {
        let pose = PoseArgs { yaw: Some(90.0), ..pose_at([0.0; 3]) }.to_pose().unwrap();
        assert!(close(pose.orientation * Vec3::NEG_Z, Vec3::NEG_X));
    }

    #[test]
    fn pose_args_reject_bad_input() {
        let quat = |q| PoseArgs { quat: Some(q), ..pose_at([0.0; 3]) };
        assert!(quat([0.0; 4]).to_pose().is_err());
        assert!(quat([0.0, f32::NAN, 0.0, 1.0]).to_pose().is_err());
        let both = PoseArgs { yaw: Some(10.0), ..quat([0.0, 0.0, 0.0, 1.0]) };
        assert_eq!(both.to_pose().unwrap_err(), "give yaw/pitch/roll or quat, not both");
        assert!(pose_at([f32::NAN, 0.0, 0.0]).to_pose().is_err());
        assert!(PoseArgs { pitch: Some(f32::INFINITY), ..pose_at([0.0; 3]) }.to_pose().is_err());
    }

    #[test]
    fn quat_is_normalized() {
        let pose = PoseArgs { quat: Some([0.0, 0.0, 0.0, 2.0]), ..pose_at([0.0; 3]) }.to_pose().unwrap();
        assert!(pose.orientation.abs_diff_eq(Quat::IDENTITY, 1e-6));
    }

    #[test]
    fn move_local_follows_orientation() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(RigAction::SetHeadPose(PoseArgs { yaw: Some(90.0), ..pose_at([0.0, 1.6, 0.0]) }), now).unwrap();
        let mv = MoveArgs { offset: [0.0, 0.0, -1.0], frame: Frame::Local, ..Default::default() };
        rig.apply(RigAction::MoveHead(mv), now).unwrap();
        assert!(close(motion(&mut rig, now, *HEAD_ID).unwrap().pose.position, Vec3::new(-1.0, 1.6, 0.0)));
        // playspace offsets ignore the orientation; rotations are about the device's own axes
        let mv = MoveArgs { offset: [0.0, 0.0, -1.0], yaw: 90.0, ..Default::default() };
        rig.apply(RigAction::MoveHead(mv), now).unwrap();
        let head = motion(&mut rig, now, *HEAD_ID).unwrap().pose;
        assert!(close(head.position, Vec3::new(-1.0, 1.6, -1.0)));
        assert!(close(head.orientation * Vec3::NEG_Z, Vec3::Z), "two 90° yaws face +Z");
    }

    #[test]
    fn animation_hits_waypoints_and_stops() {
        let (mut rig, t0) = (Rig::new(), Instant::now());
        let (p0, p1, p2) = (Vec3::new(0.0, 1.6, 0.0), Vec3::new(1.0, 1.6, 0.0), Vec3::new(1.0, 1.6, -1.0));
        let anim = AnimateArgs {
            waypoints: vec![PoseArgs { yaw: Some(90.0), ..pose_at(p1.into()) }, pose_at(p2.into())],
            duration_s: 2.0,
        };
        assert_eq!(rig.apply(RigAction::AnimateHead(anim), t0).unwrap(), Duration::from_secs(2));
        let at = |rig: &mut Rig, s: f32| motion(rig, t0 + Duration::from_secs_f32(s), *HEAD_ID).unwrap();

        let half = at(&mut rig, 0.5);
        assert!(half.linear_velocity.length() > 0.1 && half.linear_velocity.dot(p1 - p0) > 0.0, "{half:?}");
        // yaw 0 -> 90 over 1 s: 90°/s about +Y
        assert!(close(half.angular_velocity, Vec3::new(0.0, 90f32.to_radians(), 0.0)), "{half:?}");

        let mid = at(&mut rig, 1.0);
        assert!(close(mid.pose.position, p1), "{mid:?}");
        assert!(mid.linear_velocity.length() > 0.1, "Catmull-Rom keeps moving through a waypoint");

        for s in [2.0, 3.0] {
            let end = at(&mut rig, s);
            assert_eq!(end.pose.position, p2);
            assert_eq!((end.linear_velocity, end.angular_velocity), (Vec3::ZERO, Vec3::ZERO));
        }
    }

    #[test]
    fn animation_rejects_bad_duration() {
        let mut rig = Rig::new();
        let anim = |waypoints: usize, duration_s| {
            RigAction::AnimateHead(AnimateArgs { waypoints: vec![pose_at([0.0, 1.6, 0.0]); waypoints], duration_s })
        };
        for bad in [anim(1, 0.0), anim(1, 600.5), anim(1, f32::NAN), anim(0, 1.0), anim(65, 1.0)] {
            assert!(rig.apply(bad, Instant::now()).is_err());
        }
        assert!(rig.apply(anim(64, 600.0), Instant::now()).is_ok());
    }

    #[test]
    fn controller_relative_to_head() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(RigAction::SetHeadPose(PoseArgs { yaw: Some(90.0), ..pose_at([0.0, 1.6, 0.0]) }), now).unwrap();
        let args = ControllerPoseArgs { hand: Hand::Right, pose: pose_at([0.0, 0.0, -0.5]), relative_to: RelativeTo::Head };
        rig.apply(RigAction::SetControllerPose(args), now).unwrap();
        let right = motion(&mut rig, now, *HAND_RIGHT_ID).unwrap().pose;
        assert!(close(right.position, Vec3::new(-0.5, 1.6, 0.0)), "{right:?}");
        assert!(close(right.orientation * Vec3::NEG_Z, Vec3::NEG_X), "{right:?}");
        let left = motion(&mut rig, now, *HAND_LEFT_ID).unwrap().pose;
        assert!(close(left.position, Vec3::new(-0.2, 1.2, -0.3)), "default left pose: {left:?}");
    }

    #[test]
    fn inactive_controller_sends_no_motion() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        let state = |connected, tracked| RigAction::SetControllerState(ControllerStateArgs { hand: Hand::Right, connected, tracked });
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_some(), "active by default");
        rig.apply(state(Some(false), None), now).unwrap();
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_none());
        assert!(motion(&mut rig, now, *HAND_LEFT_ID).is_some() && motion(&mut rig, now, *HEAD_ID).is_some());
        rig.apply(state(Some(true), None), now).unwrap();
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_some());
        rig.apply(state(None, Some(false)), now).unwrap();
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_none(), "tracked is the same flag");
        assert!(rig.apply(state(None, None), now).is_err());
        let data = rig.tracking(now, Duration::from_millis(7));
        assert_eq!(data.poll_timestamp, Duration::from_millis(7));
        assert!(matches!(data.hand_skeletons, [None, None]) && data.body.is_none());
    }

    #[test]
    fn rig_action_json_shape() {
        let a: RigAction = serde_json::from_str(
            r#"{"action":"set_controller_pose","args":{"hand":"left","position":[0,1,0],"yaw":10,"relative_to":"head"}}"#,
        )
        .unwrap();
        assert!(matches!(a, RigAction::SetControllerPose(ControllerPoseArgs { hand: Hand::Left, relative_to: RelativeTo::Head, .. })));
        let a: RigAction = serde_json::from_str(r#"{"action":"move_controller","args":{"hand":"right","yaw":5}}"#).unwrap();
        assert!(matches!(a, RigAction::MoveController(MoveControllerArgs { mv: MoveArgs { frame: Frame::Playspace, .. }, .. })));
    }
}
