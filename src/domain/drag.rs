//! Dragging with the real pointer from one point of a window to another.

use crate::domain::actions::At;
use crate::domain::policy::AccessPolicy;
use crate::domain::session::{Error, Session};
use crate::ports::{DragCancel, DragOutcome, DragUndo, Point, Rect};

/// One end of a drag from its tool parameters: `ref`, or `x` with `y`.
/// `end` names the end in the error.
pub fn drag_end(end: &'static str, reference: Option<String>, x: Option<f64>, y: Option<f64>) -> Result<At, Error> {
    match (reference, x, y) {
        (Some(reference), None, None) => Ok(At::Element(reference)),
        (None, Some(x), Some(y)) => Ok(At::Pixel { x, y }),
        _ => Err(Error::BadDragEnd(end)),
    }
}

impl Session {
    /// Presses at `from`, moves the real pointer to `to` and releases there.
    /// Returns a sentence saying how the drag was delivered.
    pub fn drag(&self, policy: &AccessPolicy, window_id: u32, from: &At, to: &At) -> Result<String, Error> {
        if from == to {
            return Err(Error::DragSamePoint);
        }
        self.ensure_running()?;
        let window = self.allowed_window(policy, window_id)?;
        // Both ends are checked before the window is brought forward.
        let outside = |end| move |error| if error == Error::PointOutside { Error::DragOutside(end) } else { error };
        let from_aim = self.aim(&window, Some(from)).map_err(outside("from"))?;
        let to_aim = self.aim(&window, Some(to)).map_err(outside("to"))?;
        self.input()?.activate(window.pid, window.id).map_err(Error::System)?;

        let frame = self.current_frame(&window)?;
        let start = self.aim_point(&window, frame, from_aim)?;
        let end = self.aim_point(&window, frame, to_aim)?;
        // A pixel of an observe from before the window shrank can fall outside it now.
        for (name, point) in [("from", start), ("to", end)] {
            if !inside(frame, point) {
                return Err(Error::DragOutside(name));
            }
        }
        // Two refs can have the same centre.
        if (start.x - end.x).hypot(start.y - end.y) < 1.0 {
            return Err(Error::DragSamePoint);
        }
        let stop = || self.ports.pause.is_paused();
        match self.input()?.drag(window.pid, window.id, start, end, &stop).map_err(Error::System)? {
            DragOutcome::Dropped => {
                self.ports.overlay.point_at(start);
                self.ports.overlay.point_at(end);
                Ok(format!("dragged with the real pointer; {} was brought to the front", window.app_name))
            }
            DragOutcome::NotOnTop => Err(Error::DragNotStarted),
            DragOutcome::OffDisplay => Err(Error::DragOffDisplay),
            DragOutcome::Cancelled { why: DragCancel::Stopped, undo: DragUndo::AtStart } => Err(Error::Paused),
            DragOutcome::Cancelled { why, undo } => Err(Error::DragCancelled {
                why: match why {
                    DragCancel::Covered => {
                        "something came on top of the window on the drag's path, or the window moved or closed"
                    }
                    DragCancel::Stopped => "the user paused agent input with the emergency stop",
                    DragCancel::PointerMoved => "the pointer was moved during the drag",
                },
                undo,
            }),
        }
    }
}

fn inside(frame: Rect, point: Point) -> bool {
    (frame.x..frame.x + frame.width).contains(&point.x) && (frame.y..frame.y + frame.height).contains(&point.y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::testing::{Fake, observed, policy, session_with};

    const SLIDER: Rect = Rect { x: 600.0, y: 300.0, width: 200.0, height: 20.0 };

    fn pixel(x: f64, y: f64) -> At {
        At::Pixel { x, y }
    }

    fn element(reference: &str) -> At {
        At::Element(reference.to_string())
    }

    #[test]
    fn drag_between_two_pixels_maps_both_to_the_screen() {
        let (session, fake) = observed(Fake::default());
        // Half scale, window at (100,50).
        let done = session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 80.0)).unwrap();
        assert_eq!(done, "dragged with the real pointer; com.apple.TextEdit was brought to the front");
        assert_eq!(fake.log(), ["activate pid 1 window 1", "drag 350,170 -> 450,210"]);
        let (start, end) = (Point { x: 350.0, y: 170.0 }, Point { x: 450.0, y: 210.0 });
        assert_eq!(*fake.pointed.lock().unwrap(), [start, end]);
    }

    #[test]
    fn drag_between_refs_uses_live_centres_checked_by_hit_testing() {
        let moved = Rect { x: 300.0, y: 550.0, width: 100.0, height: 40.0 };
        let (session, fake) =
            observed(Fake { slider: Some(SLIDER), button_moved_to: Some(moved), ..Fake::default() });
        session.drag(&policy(), 1, &element("e2"), &element("e3")).unwrap();
        assert_eq!(fake.log(), ["activate pid 1 window 1", "drag 350,570 -> 700,310"]);

        let (session, fake) = observed(Fake { slider: Some(SLIDER), obscured: true, ..Fake::default() });
        assert_eq!(
            session.drag(&policy(), 1, &pixel(10.0, 10.0), &element("e3")),
            Err(Error::Obscured("e3".to_string()))
        );
        assert_eq!(fake.log(), ["activate pid 1 window 1"], "refused before anything was pressed");
    }

    #[test]
    fn an_end_outside_the_window_or_the_same_point_twice_is_refused() {
        let (session, fake) = observed(Fake { slider: Some(SLIDER), ..Fake::default() });
        let p = policy();
        assert_eq!(session.drag(&p, 1, &pixel(1000.0, 60.0), &element("e2")), Err(Error::DragOutside("from")));
        assert_eq!(session.drag(&p, 1, &element("e2"), &pixel(10.0, -1.0)), Err(Error::DragOutside("to")));
        assert_eq!(session.drag(&p, 1, &pixel(10.0, 10.0), &pixel(10.0, 10.0)), Err(Error::DragSamePoint));
        assert_eq!(session.drag(&p, 1, &element("e2"), &element("e2")), Err(Error::DragSamePoint));
        assert!(fake.log().is_empty(), "{:?}", fake.log());
        assert!(fake.pointed.lock().unwrap().is_empty());

        // A pixel on the button's centre and the button itself: known only
        // once positions are read, before anything is pressed.
        assert_eq!(session.drag(&p, 1, &pixel(125.0, 60.0), &element("e2")), Err(Error::DragSamePoint));
        assert_eq!(fake.log(), ["activate pid 1 window 1"]);
    }

    #[test]
    fn a_covered_start_point_presses_nothing() {
        let (session, fake) = observed(Fake { covered: true, ..Fake::default() });
        let refused = session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 60.0));
        assert_eq!(refused, Err(Error::DragNotStarted));
        assert!(refused.unwrap_err().to_string().contains("nothing was pressed"));
        assert_eq!(fake.log(), ["activate pid 1 window 1"]);
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_path_off_every_display_presses_nothing() {
        let (session, _) = observed(Fake { drag_outcome: Some(DragOutcome::OffDisplay), ..Fake::default() });
        let refused = session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 60.0));
        assert_eq!(refused, Err(Error::DragOffDisplay));
        assert!(refused.unwrap_err().to_string().contains("nothing was pressed"));
    }

    #[test]
    fn a_cancelled_drag_says_why_and_where_it_was_released() {
        let cancelled = |why, undo| {
            let outcome = DragOutcome::Cancelled { why, undo };
            let (session, fake) = observed(Fake { drag_outcome: Some(outcome), ..Fake::default() });
            let result = session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 60.0));
            assert!(fake.pointed.lock().unwrap().is_empty(), "nothing was dropped, so no cursor");
            result.unwrap_err()
        };
        let covered = cancelled(DragCancel::Covered, DragUndo::AtStart);
        assert!(matches!(covered, Error::DragCancelled { why, undo: DragUndo::AtStart } if why.contains("came on top")));
        let text = covered.to_string();
        assert!(text.contains("released the button at the start point") && text.contains("not always a clean undo"), "{text}");
        let moved = cancelled(DragCancel::PointerMoved, DragUndo::AtStart);
        assert!(matches!(moved, Error::DragCancelled { why, .. } if why.contains("pointer was moved")));

        // The way back was covered: the agent must not believe it was undone.
        let short = cancelled(DragCancel::Covered, DragUndo::OnPath).to_string();
        assert!(short.contains("way back to its start was covered") && short.contains("dropped there"), "{short}");
        let escaped = cancelled(DragCancel::PointerMoved, DragUndo::Escaped).to_string();
        assert!(escaped.contains("Escape was sent") && escaped.contains("Tell the user"), "{escaped}");
    }

    #[test]
    fn the_emergency_stop_reaches_the_drag_in_progress() {
        let (session, fake) = observed(Fake { pause_once_dragging: true, ..Fake::default() });
        assert_eq!(session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 60.0)), Err(Error::Paused));
        assert_eq!(fake.log(), ["activate pid 1 window 1", "drag 350,170 -> 450,170", "stopped"]);

        // Released elsewhere than the start: said as such, not as a plain pause.
        let (session, _) =
            observed(Fake { pause_once_dragging: true, drag_undo: Some(DragUndo::OnPath), ..Fake::default() });
        let result = session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 60.0));
        assert!(matches!(result, Err(Error::DragCancelled { why, undo: DragUndo::OnPath }) if why.contains("emergency stop")));

        // Not paused: the same callback lets the drag finish.
        let (session, fake) = observed(Fake::default());
        session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 60.0)).unwrap();
        assert!(!fake.log().contains(&"stopped".to_string()));

        // Hit while the window comes forward: nothing is pressed.
        let (session, fake) = observed(Fake { pause_once_activated: true, ..Fake::default() });
        assert_eq!(session.drag(&policy(), 1, &pixel(125.0, 60.0), &pixel(175.0, 60.0)), Err(Error::Paused));
        assert_eq!(fake.log(), ["activate pid 1 window 1"]);
    }

    #[test]
    fn a_pixel_outside_a_window_that_shrank_since_observe_is_refused() {
        // Observed at 2000 pt wide; once in front the window is 1000 wide,
        // so pixel 900 (window x 1800) is past its right edge.
        let (session, fake) = observed(Fake { resized_on_activate: Some((1000.0, 1000.0)), ..Fake::default() });
        assert_eq!(
            session.drag(&policy(), 1, &pixel(900.0, 60.0), &pixel(100.0, 60.0)),
            Err(Error::DragOutside("from"))
        );
        assert_eq!(fake.log(), ["activate pid 1 window 1"], "refused before anything was pressed");
    }

    #[test]
    fn drags_that_cannot_work_touch_nothing() {
        let p = policy();
        let (a, b) = (pixel(10.0, 10.0), pixel(20.0, 10.0));

        let (session, fake) = observed(Fake::default());
        *fake.paused.lock().unwrap() = true;
        assert_eq!(session.drag(&p, 1, &a, &b), Err(Error::Paused));
        assert!(fake.log().is_empty());

        let (session, fake) = observed(Fake::default());
        assert_eq!(session.drag(&p, 2, &a, &b), Err(Error::NotAllowed("com.apple.Notes".to_string())));
        assert_eq!(session.drag(&p, 3, &a, &b), Err(Error::Forbidden("com.apple.Terminal".to_string())));
        assert_eq!(session.drag(&p, 1, &element("e1000"), &b), Err(Error::StaleRef("e1000".to_string())));
        assert_eq!(session.drag(&p, 1, &a, &element("e1000")), Err(Error::StaleRef("e1000".to_string())));
        assert!(fake.log().is_empty(), "{:?}", fake.log());

        let (session, fake) = session_with(Fake { off_screen: vec![1], ..Fake::default() });
        assert_eq!(session.drag(&p, 1, &a, &b), Err(Error::OffScreen(1)));
        let (session, fake_unobserved) = session_with(Fake::default());
        assert_eq!(session.drag(&p, 1, &a, &b), Err(Error::NoView));
        assert!(fake.log().is_empty() && fake_unobserved.log().is_empty());
    }

    #[test]
    fn each_end_is_a_ref_or_a_pixel() {
        assert_eq!(drag_end("from", Some("e2".to_string()), None, None), Ok(element("e2")));
        assert_eq!(drag_end("to", None, Some(1.0), Some(2.0)), Ok(pixel(1.0, 2.0)));
        assert_eq!(drag_end("from", None, None, None), Err(Error::BadDragEnd("from")));
        assert_eq!(drag_end("to", None, Some(1.0), None), Err(Error::BadDragEnd("to")));
        assert_eq!(drag_end("to", Some("e2".to_string()), Some(1.0), Some(2.0)), Err(Error::BadDragEnd("to")));
        assert!(Error::BadDragEnd("from").to_string().contains("`from_ref`"));
    }
}
