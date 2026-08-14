//! Terminal frame scheduling and animation deadlines.
//!
//! State changes invalidate a future frame. They never imply a terminal
//! clear: hard clears are an explicit recovery request carried separately.

use std::time::Duration;

use tokio::time::Instant;

/// Background updates are coalesced to at most thirty terminal frames per
/// second. The rounded-up nanosecond value keeps the actual rate <= 30 FPS.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_334);
pub(crate) const ANIMATION_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FramePlan {
    pub(crate) hard_clear: bool,
}

/// The single owner of dirty state, frame cadence, and explicit hard clears.
pub(crate) struct FrameScheduler {
    dirty: bool,
    hard_clear: bool,
    frame_deadline: Option<Instant>,
    last_frame: Option<Instant>,
    animation_deadline: Option<Instant>,
}

impl FrameScheduler {
    /// Starts dirty so the first panel frame is rendered immediately.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            dirty: true,
            hard_clear: false,
            frame_deadline: Some(now),
            last_frame: None,
            animation_deadline: None,
        }
    }

    /// Invalidates presentation state produced by background work. Repeated
    /// invalidations retain the earliest already-scheduled frame.
    pub(crate) fn invalidate_background(&mut self, now: Instant) {
        let deadline = self
            .last_frame
            .map_or(now, |last| std::cmp::max(now, last + FRAME_INTERVAL));
        self.dirty = true;
        self.frame_deadline = Some(
            self.frame_deadline
                .map_or(deadline, |scheduled| std::cmp::min(scheduled, deadline)),
        );
    }

    /// Terminal control/input events are allowed to request an immediate
    /// frame so editing latency is not coupled to the background frame cap.
    pub(crate) fn invalidate_immediate(&mut self, now: Instant) {
        self.dirty = true;
        self.frame_deadline = Some(now);
    }

    /// Requests the only normal hard-clear path (`Ctrl+L` recovery).
    pub(crate) fn request_hard_redraw(&mut self, now: Instant) {
        self.hard_clear = true;
        self.invalidate_immediate(now);
    }

    pub(crate) fn frame_deadline(&self) -> Option<Instant> {
        self.frame_deadline
    }

    /// Returns one due plan and consumes the current dirty state. A fatal
    /// backend error exits the driver, so no retry state is needed here.
    pub(crate) fn take_frame(&mut self, now: Instant) -> Option<FramePlan> {
        if !self.dirty || self.frame_deadline.is_none_or(|deadline| deadline > now) {
            return None;
        }
        let plan = FramePlan {
            hard_clear: self.hard_clear,
        };
        self.dirty = false;
        self.hard_clear = false;
        self.frame_deadline = None;
        self.last_frame = Some(now);
        Some(plan)
    }

    /// Animation deadlines exist only while an animation is active.
    pub(crate) fn sync_animation(&mut self, active: bool, now: Instant) {
        match (active, self.animation_deadline) {
            (true, None) => self.animation_deadline = Some(now + ANIMATION_INTERVAL),
            (false, Some(_)) => self.animation_deadline = None,
            _ => {}
        }
    }

    pub(crate) fn animation_deadline(&self) -> Option<Instant> {
        self.animation_deadline
    }

    /// Advances a due animation deadline with missed-tick skip semantics.
    pub(crate) fn take_animation_tick(&mut self, now: Instant) -> bool {
        if self
            .animation_deadline
            .is_none_or(|deadline| deadline > now)
        {
            return false;
        }
        self.animation_deadline = Some(now + ANIMATION_INTERVAL);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_has_no_deadline_after_the_initial_frame() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::new(now);

        assert_eq!(
            scheduler.take_frame(now),
            Some(FramePlan { hard_clear: false })
        );
        assert_eq!(scheduler.frame_deadline(), None);
        assert_eq!(scheduler.take_frame(now + Duration::from_secs(60)), None);
    }

    #[test]
    fn background_invalidations_are_capped_at_thirty_fps() {
        let start = Instant::now();
        let mut scheduler = FrameScheduler::new(start);
        let mut frames = usize::from(scheduler.take_frame(start).is_some());

        for millis in 1..=1_000 {
            let now = start + Duration::from_millis(millis);
            scheduler.invalidate_background(now);
            frames += usize::from(scheduler.take_frame(now).is_some());
        }

        assert!(
            frames <= 31,
            "initial frame plus at most 30 background frames"
        );
        assert!(
            frames >= 29,
            "the scheduler should not under-render a steady stream"
        );
    }

    #[test]
    fn input_can_render_immediately_without_requesting_a_clear() {
        let start = Instant::now();
        let mut scheduler = FrameScheduler::new(start);
        scheduler.take_frame(start).expect("initial frame");

        let input_time = start + Duration::from_millis(1);
        scheduler.invalidate_immediate(input_time);
        assert_eq!(
            scheduler.take_frame(input_time),
            Some(FramePlan { hard_clear: false })
        );
    }

    #[test]
    fn only_an_explicit_recovery_plan_carries_hard_clear() {
        let start = Instant::now();
        let mut scheduler = FrameScheduler::new(start);
        scheduler.take_frame(start).expect("initial frame");

        let background = start + FRAME_INTERVAL;
        scheduler.invalidate_background(background);
        assert_eq!(
            scheduler.take_frame(background),
            Some(FramePlan { hard_clear: false })
        );

        let recovery = background + Duration::from_millis(1);
        scheduler.request_hard_redraw(recovery);
        assert_eq!(
            scheduler.take_frame(recovery),
            Some(FramePlan { hard_clear: true })
        );
    }

    #[test]
    fn animation_deadline_exists_only_while_active() {
        let start = Instant::now();
        let mut scheduler = FrameScheduler::new(start);
        assert_eq!(scheduler.animation_deadline(), None);

        scheduler.sync_animation(true, start);
        let deadline = start + ANIMATION_INTERVAL;
        assert_eq!(scheduler.animation_deadline(), Some(deadline));
        assert!(!scheduler.take_animation_tick(deadline - Duration::from_millis(1)));
        assert!(scheduler.take_animation_tick(deadline));

        scheduler.sync_animation(false, deadline);
        assert_eq!(scheduler.animation_deadline(), None);
    }
}
