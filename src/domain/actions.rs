//! Acting on a window: accessibility first, real input as the fallback.

use crate::domain::keys;
use crate::domain::policy::AccessPolicy;
use crate::domain::session::{Error, Session};
use crate::domain::viewport::clamp_to_window;
use crate::ports::{Button, ElementHandle, Key, KeyCombo, Modifiers, Point, Rect, UiAction, WindowInfo};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Accessibility where possible, otherwise real pointer and keyboard.
    Auto,
    /// Accessibility only: never touch the pointer or bring the app forward.
    Background,
}

/// Where in the window to act.
#[derive(Debug, PartialEq)]
pub enum At {
    /// An element ref of the latest observe.
    Element(String),
    /// A pixel of the latest window screenshot.
    Pixel { x: f64, y: f64 },
}

const SELECT_ALL: KeyCombo = KeyCombo {
    modifiers: Modifiers { cmd: true, ctrl: false, alt: false, shift: false },
    key: Key::Char('a'),
};

impl Session {
    /// Returns a sentence saying how the click was delivered.
    pub fn click(
        &self,
        policy: &AccessPolicy,
        window_id: u32,
        at: &At,
        button: Button,
        double: bool,
        mode: Mode,
    ) -> Result<String, Error> {
        let window = self.allowed_window(policy, window_id)?;
        if let At::Element(reference) = at
            && !double
        {
            let handle = self.element(&window, reference)?;
            let (action, done) = match button {
                Button::Left => (UiAction::Press, "pressed"),
                Button::Right => (UiAction::ShowMenu, "opened the context menu of"),
            };
            let cursor = self.cursor_point(&window, handle);
            if self.ports.actions.perform(handle, action).map_err(Error::System)? {
                self.show_cursor(cursor);
                return Ok(format!("{done} {reference} through accessibility; the pointer was not used"));
            }
        }
        let point = self.real_target(&window, Some(at), mode)?;
        self.real_click(&window, point, button, if double { 2 } else { 1 })?;
        Ok(format!(
            "clicked with the real pointer; {} was brought to the front",
            window.app_name
        ))
    }

    /// Types into `reference`, or into whatever has focus when it is `None`.
    pub fn type_text(
        &self,
        policy: &AccessPolicy,
        window_id: u32,
        reference: Option<&str>,
        text: &str,
        replace: bool,
        mode: Mode,
    ) -> Result<String, Error> {
        let window = self.allowed_window(policy, window_id)?;
        let input = &self.ports.input;
        match reference {
            Some(reference) => {
                let handle = self.element(&window, reference)?;
                let cursor = self.cursor_point(&window, handle);
                let inserted = self
                    .ports
                    .actions
                    .insert_text(handle, text, replace)
                    .map_err(Error::System)?;
                if inserted {
                    self.show_cursor(cursor);
                    return Ok(format!(
                        "put the text into {reference} through accessibility; the keyboard was not used"
                    ));
                }
                // Real keys go to the focused element, so focus it with a click.
                let point = self.real_target(&window, Some(&At::Element(reference.to_string())), mode)?;
                self.real_click(&window, point, Button::Left, 1)?;
            }
            None => {
                if mode == Mode::Background {
                    return Err(Error::NeedsRealInput);
                }
                input.activate(window.pid, window.id).map_err(Error::System)?;
            }
        }
        if replace {
            input.press_key(window.pid, SELECT_ALL).map_err(Error::System)?;
        }
        input.type_text(window.pid, text).map_err(Error::System)?;
        Ok(format!(
            "typed with real key events at the cursor; {} was brought to the front",
            window.app_name
        ))
    }

    /// Presses a combination such as `cmd+s` in the window's app.
    pub fn press_key(&self, policy: &AccessPolicy, window_id: u32, keys: &str) -> Result<String, Error> {
        let combo = keys::parse(keys).map_err(Error::BadKeys)?;
        let window = self.allowed_window(policy, window_id)?;
        let input = &self.ports.input;
        input.activate(window.pid, window.id).map_err(Error::System)?;
        input.press_key(window.pid, combo).map_err(Error::System)?;
        Ok(format!("pressed {keys}; {} was brought to the front", window.app_name))
    }

    /// Scrolls by lines at `at`, or at the middle of the window.
    pub fn scroll(
        &self,
        policy: &AccessPolicy,
        window_id: u32,
        at: Option<&At>,
        dx: i32,
        dy: i32,
    ) -> Result<String, Error> {
        let window = self.allowed_window(policy, window_id)?;
        let point = self.real_target(&window, at, Mode::Auto)?;
        if !self.ports.input.scroll(window.id, point, dx, dy).map_err(Error::System)? {
            return Err(Error::Covered);
        }
        self.ports.overlay.point_at(point);
        Ok(format!("scrolled; {} was brought to the front", window.app_name))
    }

    /// Where to show the agent's cursor for an accessibility action on an
    /// element: its centre, if that is where the window is on screen. A window
    /// reduced to a Stage Manager thumbnail still reports its elements at
    /// full-size positions, which would put the cursor over some other app.
    ///
    /// Read before the action: afterwards the app may be busy with what the
    /// action started, and the reply must not wait for it.
    fn cursor_point(&self, window: &WindowInfo, handle: ElementHandle) -> Option<Point> {
        let frame = self.ports.actions.frame(handle).ok()??;
        let centre = Point { x: frame.x + frame.width / 2.0, y: frame.y + frame.height / 2.0 };
        let on_screen = window.frame;
        let inside = (on_screen.x..on_screen.x + on_screen.width).contains(&centre.x)
            && (on_screen.y..on_screen.y + on_screen.height).contains(&centre.y);
        inside.then_some(centre)
    }

    /// The cursor marks delivered actions only, so it is shown afterwards.
    fn show_cursor(&self, point: Option<Point>) {
        if let Some(point) = point {
            self.ports.overlay.point_at(point);
        }
    }

    fn real_click(&self, window: &WindowInfo, point: Point, button: Button, count: u8) -> Result<(), Error> {
        match self.ports.input.click(window.id, point, button, count).map_err(Error::System)? {
            true => {
                self.ports.overlay.point_at(point);
                Ok(())
            }
            // The port re-checks before each press, so a refused double click
            // may have delivered its first click.
            false if count > 1 => Err(Error::DoubleClickCovered),
            false => Err(Error::Covered),
        }
    }

    /// Brings the window forward and returns the screen point for real input.
    /// Whether the window is on top there is the input port's check.
    fn real_target(&self, window: &WindowInfo, at: Option<&At>, mode: Mode) -> Result<Point, Error> {
        if mode == Mode::Background {
            return Err(Error::NeedsRealInput);
        }
        // Whatever can be refused without disturbing the user is checked first.
        let pixel = match at {
            Some(At::Pixel { x, y }) => {
                let view = self.state.lock().unwrap().view;
                let (_, viewport) = view.filter(|(id, _)| *id == window.id).ok_or(Error::NoView)?;
                Some(viewport.window_point(*x, *y).ok_or(Error::PointOutside)?)
            }
            _ => None,
        };
        let element = match at {
            Some(At::Element(reference)) => Some((reference, self.element(window, reference)?)),
            _ => None,
        };
        self.ports.input.activate(window.pid, window.id).map_err(Error::System)?;

        // Positions are read only now: bringing an app forward can move its
        // windows (Stage Manager), and content may have scrolled since observe.
        let frame = self.current_frame(window)?;
        let Some((reference, handle)) = element else {
            let (dx, dy) = pixel.unwrap_or((frame.width / 2.0, frame.height / 2.0));
            return Ok(Point { x: frame.x + dx, y: frame.y + dy });
        };
        // The middle of the part inside the window: a document view can be
        // far taller than the window, with its own middle off-screen.
        let live = self.live_frame(window, reference)?;
        let relative = Rect { x: live.x - frame.x, y: live.y - frame.y, ..live };
        let visible = clamp_to_window(relative, frame.width, frame.height)
            .ok_or_else(|| Error::RefNotVisible(reference.clone()))?;
        let point = Point {
            x: frame.x + visible.x + visible.width / 2.0,
            y: frame.y + visible.y + visible.height / 2.0,
        };
        // Inside the window the element can still be clipped by a scroll view
        // or lie under a popover; then the click would land on something else.
        match self.ports.actions.is_at(handle, point).map_err(Error::System)? {
            true => Ok(point),
            false => Err(Error::Obscured(reference.clone())),
        }
    }

    /// The window's frame now, provided it still belongs to the same process.
    fn current_frame(&self, window: &WindowInfo) -> Result<Rect, Error> {
        self.ports
            .windows
            .windows()
            .map_err(Error::System)?
            .into_iter()
            .find(|w| w.id == window.id && w.pid == window.pid)
            .map(|w| w.frame)
            .ok_or(Error::WindowNotFound(window.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session::{ObserveRequest, Shot};
    use crate::domain::testing::{Fake, policy, session_with};

    const SAVE: &str = "e2";

    /// A session that has observed window 1 at half scale (1000x500 px).
    fn observed(fake: Fake) -> (Session, std::sync::Arc<Fake>) {
        let (session, fake) = session_with(fake);
        let request = ObserveRequest {
            window_id: 1,
            screenshot: Shot::None,
            tree: true,
            reference: None,
            max_edge: 1000,
        };
        session.observe(&policy(), request).unwrap();
        (session, fake)
    }

    fn save() -> At {
        At::Element(SAVE.to_string())
    }

    #[test]
    fn click_prefers_accessibility_and_leaves_the_pointer_alone() {
        let (session, fake) = observed(Fake { accessible: true, ..Fake::default() });
        session.click(&policy(), 1, &save(), Button::Left, false, Mode::Background).unwrap();
        session.click(&policy(), 1, &save(), Button::Right, false, Mode::Auto).unwrap();
        assert_eq!(fake.log(), ["ax Press 7", "ax ShowMenu 7"]);
    }

    #[test]
    fn the_agent_cursor_is_shown_wherever_an_action_lands() {
        let centre = Point { x: 350.0, y: 170.0 };
        let (session, fake) = observed(Fake { accessible: true, ..Fake::default() });
        session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto).unwrap();
        session.type_text(&policy(), 1, Some(SAVE), "hi", false, Mode::Auto).unwrap();
        assert_eq!(*fake.pointed.lock().unwrap(), [centre, centre]);

        let (session, fake) = observed(Fake::default());
        session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto).unwrap();
        session.scroll(&policy(), 1, None, 0, 3).unwrap();
        assert_eq!(*fake.pointed.lock().unwrap(), [centre, Point { x: 1100.0, y: 550.0 }]);

        // An element reported outside the window's on-screen frame (the
        // window is a thumbnail elsewhere) is acted on but not pointed at.
        let far = Rect { x: 5000.0, y: 150.0, width: 100.0, height: 40.0 };
        let (session, fake) =
            observed(Fake { accessible: true, button_moved_to: Some(far), ..Fake::default() });
        session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto).unwrap();
        assert_eq!(fake.log(), ["ax Press 7"]);
        assert!(fake.pointed.lock().unwrap().is_empty());

        // Refused actions show nothing.
        let (session, fake) = observed(Fake::default());
        let _ = session.click(&policy(), 1, &save(), Button::Left, false, Mode::Background);
        let _ = session.click(&policy(), 2, &save(), Button::Left, false, Mode::Auto);
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn click_falls_back_to_the_real_pointer_at_the_element_centre() {
        let (session, fake) = observed(Fake::default());
        session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto).unwrap();
        // A double click has no accessibility equivalent, even on an accessible element.
        session.click(&policy(), 1, &save(), Button::Left, true, Mode::Auto).unwrap();
        assert_eq!(
            fake.log(),
            [
                "activate pid 1 window 1",
                "click 350,170 Left x1",
                "activate pid 1 window 1",
                "click 350,170 Left x2",
            ]
        );
    }

    #[test]
    fn real_click_follows_an_element_that_moved_since_observe() {
        let moved = Rect { x: 300.0, y: 550.0, width: 100.0, height: 40.0 };
        let (session, fake) = observed(Fake { button_moved_to: Some(moved), ..Fake::default() });
        session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto).unwrap();
        assert_eq!(fake.log()[1], "click 350,570 Left x1");
    }

    #[test]
    fn real_click_aims_at_the_visible_part_of_an_oversized_element() {
        // 5000 pt tall, starting 100 pt below the top of the 1000 pt window at y=50.
        let tall = Rect { x: 300.0, y: 150.0, width: 100.0, height: 5000.0 };
        let (session, fake) = observed(Fake { button_moved_to: Some(tall), ..Fake::default() });
        session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto).unwrap();
        // Visible part spans screen y 150..1050, so its middle is 600.
        assert_eq!(fake.log()[1], "click 350,600 Left x1");

        let below = Rect { x: 300.0, y: 2000.0, width: 100.0, height: 40.0 };
        let (session, _) = observed(Fake { button_moved_to: Some(below), ..Fake::default() });
        assert_eq!(
            session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto),
            Err(Error::RefNotVisible(SAVE.to_string()))
        );
    }

    #[test]
    fn background_mode_never_uses_real_input() {
        let (session, fake) = observed(Fake::default());
        let p = policy();
        assert_eq!(
            session.click(&p, 1, &save(), Button::Left, false, Mode::Background),
            Err(Error::NeedsRealInput)
        );
        assert_eq!(
            session.type_text(&p, 1, Some(SAVE), "hi", false, Mode::Background),
            Err(Error::NeedsRealInput)
        );
        assert_eq!(session.type_text(&p, 1, None, "hi", false, Mode::Background), Err(Error::NeedsRealInput));
        assert!(fake.log().is_empty());
    }

    #[test]
    fn pixel_click_maps_screenshot_pixels_to_the_screen() {
        let (session, fake) = observed(Fake::default());
        // Half scale: pixel (125,60) is window point (250,120), window at (100,50).
        let at = At::Pixel { x: 125.0, y: 60.0 };
        session.click(&policy(), 1, &at, Button::Left, false, Mode::Auto).unwrap();
        assert_eq!(fake.log()[1], "click 350,170 Left x1");

        let outside = At::Pixel { x: 1000.0, y: 60.0 };
        assert_eq!(
            session.click(&policy(), 1, &outside, Button::Left, false, Mode::Auto),
            Err(Error::PointOutside)
        );
    }

    #[test]
    fn pixel_click_needs_a_prior_observe_of_that_window() {
        let (session, _) = session_with(Fake::default());
        let at = At::Pixel { x: 10.0, y: 10.0 };
        assert_eq!(session.click(&policy(), 1, &at, Button::Left, false, Mode::Auto), Err(Error::NoView));
    }

    #[test]
    fn real_input_is_refused_when_another_window_is_on_top() {
        let (session, fake) = observed(Fake { covered: true, ..Fake::default() });
        assert_eq!(
            session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto),
            Err(Error::Covered)
        );
        assert_eq!(
            session.click(&policy(), 1, &save(), Button::Left, true, Mode::Auto),
            Err(Error::DoubleClickCovered)
        );
        assert_eq!(session.scroll(&policy(), 1, None, 0, 3), Err(Error::Covered));
        assert!(fake.log().iter().all(|entry| entry.starts_with("activate")), "{:?}", fake.log());
        assert!(fake.pointed.lock().unwrap().is_empty(), "nothing was delivered, so no cursor");
    }

    #[test]
    fn real_click_is_refused_when_the_element_is_not_what_is_shown_there() {
        let (session, fake) = observed(Fake { obscured: true, ..Fake::default() });
        assert_eq!(
            session.click(&policy(), 1, &save(), Button::Left, false, Mode::Auto),
            Err(Error::Obscured(SAVE.to_string()))
        );
        assert_eq!(fake.log(), ["activate pid 1 window 1"]);
    }

    #[test]
    fn requests_that_cannot_work_do_not_bring_the_app_forward() {
        let (session, fake) = observed(Fake::default());
        let outside = At::Pixel { x: 1000.0, y: 60.0 };
        let stale = At::Element("e1000".to_string());
        assert!(session.click(&policy(), 1, &outside, Button::Left, false, Mode::Auto).is_err());
        assert!(session.scroll(&policy(), 1, Some(&stale), 0, 1).is_err());
        assert!(session.press_key(&policy(), 1, "cmd+nope").is_err());
        assert!(fake.log().is_empty(), "{:?}", fake.log());
    }

    #[test]
    fn actions_on_apps_that_are_not_allowed_touch_nothing() {
        let (session, fake) = observed(Fake { accessible: true, ..Fake::default() });
        let p = policy();
        let pixel = At::Pixel { x: 10.0, y: 10.0 };
        assert_eq!(
            session.click(&p, 2, &pixel, Button::Left, false, Mode::Auto),
            Err(Error::NotAllowed("com.apple.Notes".to_string()))
        );
        assert_eq!(
            session.type_text(&p, 3, None, "rm -rf", false, Mode::Auto),
            Err(Error::Forbidden("com.apple.Terminal".to_string()))
        );
        assert!(matches!(session.press_key(&p, 2, "cmd+a"), Err(Error::NotAllowed(_))));
        assert!(matches!(session.scroll(&p, 4, None, 0, 1), Err(Error::Unidentified(_))));
        // A ref of window 1 must not act when another window is named.
        assert!(matches!(
            session.click(&p, 2, &save(), Button::Left, false, Mode::Auto),
            Err(Error::NotAllowed(_))
        ));
        assert!(fake.log().is_empty(), "{:?}", fake.log());
    }

    #[test]
    fn text_goes_through_accessibility_or_is_typed_after_focusing() {
        let (session, fake) = observed(Fake { accessible: true, ..Fake::default() });
        session.type_text(&policy(), 1, Some(SAVE), "hi", true, Mode::Auto).unwrap();
        assert_eq!(fake.log(), ["ax insert 7 \"hi\" replace=true"]);

        let (session, fake) = observed(Fake::default());
        session.type_text(&policy(), 1, Some(SAVE), "hi", true, Mode::Auto).unwrap();
        session.type_text(&policy(), 1, None, "yo", false, Mode::Auto).unwrap();
        assert_eq!(
            fake.log(),
            [
                "activate pid 1 window 1",
                "click 350,170 Left x1",
                "key pid 1 cmd+Char('a')",
                "type pid 1 \"hi\"",
                "activate pid 1 window 1",
                "type pid 1 \"yo\"",
            ]
        );
    }

    #[test]
    fn keys_and_scroll_use_real_input_in_the_target_app() {
        let (session, fake) = observed(Fake::default());
        session.press_key(&policy(), 1, "cmd+shift+s").unwrap();
        session.scroll(&policy(), 1, None, 0, 3).unwrap();
        assert_eq!(
            fake.log(),
            [
                "activate pid 1 window 1",
                "key pid 1 cmd+shift+Char('s')",
                "activate pid 1 window 1",
                // The middle of the 2000x1000 window at (100,50).
                "scroll 1100,550 by 0,3",
            ]
        );
        assert!(matches!(session.press_key(&policy(), 1, "cmd+nope"), Err(Error::BadKeys(_))));
    }

    #[test]
    fn stale_ref_is_refused_before_anything_happens() {
        let (session, fake) = observed(Fake { accessible: true, ..Fake::default() });
        assert_eq!(
            session.click(&policy(), 1, &At::Element("e1000".to_string()), Button::Left, false, Mode::Auto),
            Err(Error::StaleRef("e1000".to_string()))
        );
        assert!(fake.log().is_empty());
    }
}
