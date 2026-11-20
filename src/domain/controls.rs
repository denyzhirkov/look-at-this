//! Setting the state of a control, such as a checkbox, slider or pop-up
//! button, through accessibility alone.

use std::cell::Cell;

use crate::domain::menu::{Found, find_title};
use crate::domain::policy::AccessPolicy;
use crate::domain::session::{Error, Session};
use crate::domain::snapshot::one_line;
use crate::ports::{ControlState, ElementHandle, LiveValue, MenuChoice, MenuOption, UiAction};

const TOGGLE_ACCEPTS: &str = "\"on\" or \"off\"";
const RADIO_ACCEPTS: &str =
    "only \"on\": a radio button is turned off by turning on another one of its group";
const NUMBER_ACCEPTS: &str = "a number";
const OPTION_ACCEPTS: &str = "the title of one of its options";

/// What kind of control an element is, which decides what a value means.
#[derive(Debug, PartialEq)]
enum Kind {
    Toggle { radio: bool },
    /// Holds the current number.
    Range(f64),
    Popup,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Toggle {
    On,
    Off,
    Mixed,
}

impl Toggle {
    fn word(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Mixed => "mixed",
        }
    }
}

/// What a set did: whether anything was delivered, and a sentence saying so.
type Outcome = (bool, String);

impl Session {
    /// Sets the control `reference` to `value`, leaving it alone if it
    /// already has that value. Returns a sentence saying what happened.
    pub fn set_value(
        &self,
        policy: &AccessPolicy,
        window_id: u32,
        reference: &str,
        value: &str,
    ) -> Result<String, Error> {
        self.ensure_running()?;
        let window = self.allowed_window(policy, window_id)?;
        let handle = self.element(&window, reference)?;
        // The tree's value may be out of date; act only on what is there now.
        let state = self.ports.actions.control(handle).map_err(Error::System)?;
        let kind = kind(&state).map_err(|why| not_settable(reference, &state, why))?;
        if !state.enabled {
            return Err(Error::Disabled(reference.to_string()));
        }
        let cursor = self.cursor_point(&window, handle);
        let (delivered, done) = match kind {
            Kind::Toggle { radio } => self.set_toggle(handle, reference, &state, radio, value)?,
            Kind::Range(now) => self.set_range(handle, reference, &state, now, value)?,
            Kind::Popup => self.set_popup(handle, reference, &state, value)?,
        };
        if delivered {
            self.show_cursor(cursor);
        }
        Ok(done)
    }

    fn set_toggle(
        &self,
        handle: ElementHandle,
        reference: &str,
        state: &ControlState,
        radio: bool,
        value: &str,
    ) -> Result<Outcome, Error> {
        let accepted = if radio { RADIO_ACCEPTS } else { TOGGLE_ACCEPTS };
        let want = match value.trim().to_lowercase().as_str() {
            "on" => Toggle::On,
            "off" if !radio => Toggle::Off,
            _ => return Err(Error::BadValue { reference: reference.to_string(), accepted }),
        };
        let unreported = "it does not report whether it is on or off; use click to press it";
        let now = toggle(&state.value).ok_or_else(|| not_settable(reference, state, unreported))?;
        if now == want {
            return Ok((false, format!("{reference} is already {}; nothing was changed", want.word())));
        }
        if !self.ports.actions.perform(handle, UiAction::Press).map_err(Error::System)? {
            let why = "it does not respond to an accessibility press; use click";
            return Err(not_settable(reference, state, why));
        }
        let pressed = format!("pressed {reference} through accessibility");
        let Some(after) = self.read_back(handle) else { return Ok((true, unread(pressed))) };
        let done = match toggle(&after.value) {
            Some(after) if after == want => {
                format!("turned {reference} {} through accessibility; the pointer was not used", want.word())
            }
            Some(after) if after == now => format!("{pressed}, but it is still {}", now.word()),
            Some(after) => format!("{pressed}, but it is {} now, not {}", after.word(), want.word()),
            None => format!("{pressed}, but it no longer reports whether it is on or off; observe to check"),
        };
        Ok((true, done))
    }

    fn set_range(
        &self,
        handle: ElementHandle,
        reference: &str,
        state: &ControlState,
        now: f64,
        value: &str,
    ) -> Result<Outcome, Error> {
        let want = value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .ok_or_else(|| Error::BadValue { reference: reference.to_string(), accepted: NUMBER_ACCEPTS })?;
        if state.min.is_some_and(|min| want < min) || state.max.is_some_and(|max| want > max) {
            return Err(Error::OutOfRange {
                reference: reference.to_string(),
                value: want,
                min: state.min,
                max: state.max,
            });
        }
        if number(now) == number(want) {
            return Ok((false, format!("{reference} is already {}; nothing was changed", number(want))));
        }
        self.ports.actions.set_number(handle, want).map_err(Error::System)?;
        let set = format!("set {reference} to {} through accessibility", number(want));
        let Some(after) = self.read_back(handle) else { return Ok((true, unread(set))) };
        let done = match after.value {
            LiveValue::Number(after) if number(after) == number(want) => set,
            LiveValue::Number(after) => format!("{set}; it now reads {}", number(after)),
            _ => format!("{set}, but it no longer reports a number; observe to check"),
        };
        Ok((true, done))
    }

    fn set_popup(
        &self,
        handle: ElementHandle,
        reference: &str,
        state: &ControlState,
        value: &str,
    ) -> Result<Outcome, Error> {
        let wanted = value.trim();
        if wanted.is_empty() {
            return Err(Error::BadValue { reference: reference.to_string(), accepted: OPTION_ACCEPTS });
        }
        // Only an exact match: a looser one may belong to another option, such
        // as "bold" next to the "Bold" shown.
        if let LiveValue::Text(now) = &state.value
            && now.trim() == wanted
        {
            let now = one_line(now.trim());
            return Ok((false, format!("{reference} already shows \"{now}\"; nothing was changed")));
        }
        let paused = Cell::new(false);
        let pick = |options: &[MenuOption]| {
            // The menu is open by now; the user may have hit the stop meanwhile.
            if self.ports.pause.is_paused() {
                paused.set(true);
                return None;
            }
            match match_option(options, wanted) {
                Found::One(i) if options[i].enabled => Some(i),
                _ => None,
            }
        };
        match self.ports.actions.choose_option(handle, &pick).map_err(Error::System)? {
            MenuChoice::Cancelled(_) if paused.get() => Err(Error::Paused),
            MenuChoice::Pressed => {
                let chose = format!("chose \"{}\" in {reference} through accessibility", one_line(wanted));
                let Some(after) = self.read_back(handle) else { return Ok((true, unread(chose))) };
                let done = match after.value {
                    LiveValue::Text(after) if find_title(&[after.as_str()], wanted) == Found::One(0) => {
                        format!("chose \"{}\" in {reference} through accessibility", one_line(after.trim()))
                    }
                    LiveValue::Text(after) => format!("{chose}, but it now shows \"{}\"", one_line(after.trim())),
                    _ => format!("{chose}; observe to check what it shows"),
                };
                Ok((true, done))
            }
            MenuChoice::Cancelled(options) => Err(Error::NoSuchOption {
                reference: reference.to_string(),
                value: one_line(wanted),
                disabled: matches!(match_option(&options, wanted), Found::One(i) if !options[i].enabled),
                options: options
                    .iter()
                    .map(|o| one_line(o.title.trim()))
                    .filter(|title| !title.is_empty())
                    .collect(),
            }),
        }
    }

    /// The control's state after a delivered action; `None` if it cannot be
    /// read back, which does not undo the action.
    fn read_back(&self, handle: ElementHandle) -> Option<ControlState> {
        self.ports.actions.control(handle).ok()
    }
}

/// Reports a delivered action whose result could not be read.
fn unread(done: String) -> String {
    format!("{done}, but it could not be read back afterwards; observe to check")
}

/// The kind of control, or why set_value does not work on it.
fn kind(state: &ControlState) -> Result<Kind, &'static str> {
    if state.value == LiveValue::Secret {
        return Err("it is a secure (password) field, whose value is never read or written");
    }
    match state.role.as_str() {
        "AXCheckBox" | "AXSwitch" => Ok(Kind::Toggle { radio: false }),
        "AXRadioButton" => Ok(Kind::Toggle { radio: true }),
        "AXPopUpButton" => Ok(Kind::Popup),
        "AXComboBox" => Err("choosing from a combo box's list is not supported; put the text into it with type_text"),
        "AXTextField" | "AXTextArea" => Err("use type_text to change its text"),
        "AXButton" | "AXMenuButton" | "AXMenuItem" | "AXLink" | "AXDisclosureTriangle" => {
            Err("use click to press it")
        }
        role => match state.value {
            LiveValue::Number(now) if state.settable => Ok(Kind::Range(now)),
            _ if matches!(role, "AXSlider" | "AXIncrementor") => {
                Err("its value cannot be set as a number through accessibility; try click instead")
            }
            _ => Err("it works on checkboxes, switches, radio buttons, sliders, steppers and pop-up buttons"),
        },
    }
}

fn not_settable(reference: &str, state: &ControlState, why: &'static str) -> Error {
    Error::NotSettable { reference: reference.to_string(), role: state.role.clone(), why }
}

/// A checkbox reports 0 for off, 1 for on and 2 for mixed.
fn toggle(value: &LiveValue) -> Option<Toggle> {
    match value {
        LiveValue::Number(n) if *n == 0.0 => Some(Toggle::Off),
        LiveValue::Number(n) if *n == 1.0 => Some(Toggle::On),
        LiveValue::Number(_) => Some(Toggle::Mixed),
        _ => None,
    }
}

/// The option titled `wanted`, matched as menu titles are. Options with the
/// same title count as one, since a pop-up may repeat an option (a font
/// under "Recently Used" and again in the full list): the first enabled of
/// them stands for all.
fn match_option(options: &[MenuOption], wanted: &str) -> Found {
    let mut distinct: Vec<usize> = Vec::new();
    for (i, option) in options.iter().enumerate() {
        match distinct.iter_mut().find(|j| options[**j].title.trim() == option.title.trim()) {
            Some(j) if !options[*j].enabled && option.enabled => *j = i,
            Some(_) => {}
            None => distinct.push(i),
        }
    }
    let titles: Vec<&str> = distinct.iter().map(|&i| options[i].title.as_str()).collect();
    match find_title(&titles, wanted) {
        Found::One(i) => Found::One(distinct[i]),
        other => other,
    }
}

/// A number as trees show it: at most three decimals, since controls often
/// hold float32 values such as 0.18518517911434174.
pub(super) fn number(n: f64) -> String {
    ((n * 1000.0).round() / 1000.0).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::testing::{Fake, observed, policy, session_with};
    use crate::ports::Point;

    const CONTROL: &str = "e2";
    const CENTRE: Point = Point { x: 350.0, y: 170.0 };

    fn state(role: &str, value: LiveValue) -> ControlState {
        ControlState { role: role.to_string(), enabled: true, value, settable: false, min: None, max: None }
    }

    fn slider(value: f64) -> ControlState {
        ControlState { settable: true, min: Some(0.0), max: Some(100.0), ..state("AXSlider", LiveValue::Number(value)) }
    }

    fn option(title: &str, enabled: bool) -> MenuOption {
        MenuOption { title: title.to_string(), enabled }
    }

    /// A session that has observed the control, which reports `control`.
    fn with(control: ControlState, fake: Fake) -> (Session, std::sync::Arc<Fake>) {
        observed(Fake { accessible: true, control: std::sync::Mutex::new(Some(control)), ..fake })
    }

    fn live(fake: &Fake) -> LiveValue {
        fake.control.lock().unwrap().as_ref().unwrap().value.clone()
    }

    fn popup() -> (Session, std::sync::Arc<Fake>) {
        popup_with(Fake::default())
    }

    /// A pop-up button showing "Plain Text", over `fake`.
    fn popup_with(fake: Fake) -> (Session, std::sync::Arc<Fake>) {
        let options = vec![
            option("Rich Text", true),
            option("Plain Text", true),
            option("", false),
            option("Markdown", false),
        ];
        with(state("AXPopUpButton", LiveValue::Text("Plain Text".to_string())), Fake { options, ..fake })
    }

    #[test]
    fn a_toggle_already_in_the_wanted_state_is_not_pressed() {
        let (session, fake) = with(state("AXCheckBox", LiveValue::Number(1.0)), Fake::default());
        let done = session.set_value(&policy(), 1, CONTROL, " On ").unwrap();
        assert_eq!(done, "e2 is already on; nothing was changed");
        assert!(fake.log().is_empty(), "{:?}", fake.log());
        assert!(fake.pointed.lock().unwrap().is_empty(), "nothing was delivered, so no cursor");
    }

    #[test]
    fn a_toggle_is_pressed_when_different_and_the_new_state_reported() {
        let (session, fake) = with(state("AXSwitch", LiveValue::Number(0.0)), Fake::default());
        let done = session.set_value(&policy(), 1, CONTROL, "on").unwrap();
        assert_eq!(done, "turned e2 on through accessibility; the pointer was not used");
        assert_eq!(fake.log(), ["ax Press 107"]);
        assert_eq!(live(&fake), LiveValue::Number(1.0));
        assert_eq!(*fake.pointed.lock().unwrap(), [CENTRE]);

        // A mixed checkbox matches neither state; this one turns on when pressed.
        let (session, fake) = with(state("AXCheckBox", LiveValue::Number(2.0)), Fake::default());
        let done = session.set_value(&policy(), 1, CONTROL, "off").unwrap();
        assert_eq!(done, "pressed e2 through accessibility, but it is on now, not off");
        assert_eq!(fake.log(), ["ax Press 107"]);

        let (session, fake) = with(state("AXRadioButton", LiveValue::Number(0.0)), Fake::default());
        session.set_value(&policy(), 1, CONTROL, "on").unwrap();
        assert_eq!(fake.log(), ["ax Press 107"]);
    }

    #[test]
    fn values_that_do_not_fit_the_control_are_refused() {
        let bad = |accepted| Err(Error::BadValue { reference: CONTROL.to_string(), accepted });
        let (session, fake) = with(state("AXCheckBox", LiveValue::Number(0.0)), Fake::default());
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "banana"), bad(TOGGLE_ACCEPTS));
        let (session, radio) = with(state("AXRadioButton", LiveValue::Number(1.0)), Fake::default());
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "off"), bad(RADIO_ACCEPTS));
        let (session, range) = with(slider(10.0), Fake::default());
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "ten"), bad(NUMBER_ACCEPTS));
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "NaN"), bad(NUMBER_ACCEPTS));
        let (session, menu) = popup();
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "  "), bad(OPTION_ACCEPTS));
        for fake in [fake, radio, range, menu] {
            assert!(fake.log().is_empty(), "{:?}", fake.log());
        }
    }

    #[test]
    fn a_number_is_set_and_the_value_it_snapped_to_reported() {
        let (session, fake) = with(slider(10.0), Fake { step: Some(5.0), ..Fake::default() });
        let done = session.set_value(&policy(), 1, CONTROL, "42").unwrap();
        assert_eq!(done, "set e2 to 42 through accessibility; it now reads 40");
        assert_eq!(fake.log(), ["ax set 107 42"]);
        assert_eq!(*fake.pointed.lock().unwrap(), [CENTRE]);

        let done = session.set_value(&policy(), 1, CONTROL, "40").unwrap();
        assert_eq!(done, "e2 is already 40; nothing was changed");
        assert_eq!(fake.log(), ["ax set 107 42"]);

        // Any element with a settable number counts, whatever its role.
        let level = ControlState { settable: true, ..state("AXLevelIndicator", LiveValue::Number(1.0)) };
        let (session, fake) = with(level, Fake::default());
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "2.5").unwrap(), "set e2 to 2.5 through accessibility");
        assert_eq!(fake.log(), ["ax set 107 2.5"]);
    }

    #[test]
    fn a_number_out_of_range_is_refused() {
        let (session, fake) = with(slider(10.0), Fake::default());
        let refused = session.set_value(&policy(), 1, CONTROL, "150");
        assert_eq!(
            refused,
            Err(Error::OutOfRange { reference: CONTROL.to_string(), value: 150.0, min: Some(0.0), max: Some(100.0) })
        );
        assert_eq!(refused.unwrap_err().to_string(), "150 is outside the range of e2: 0 to 100");
        assert!(session.set_value(&policy(), 1, CONTROL, "-1").is_err());
        // Shown as given: rounding would print a value inside the range.
        let refused = session.set_value(&policy(), 1, CONTROL, "100.0004").unwrap_err();
        assert_eq!(refused.to_string(), "100.0004 is outside the range of e2: 0 to 100");
        assert!(fake.log().is_empty(), "{:?}", fake.log());
    }

    #[test]
    fn a_popup_option_is_chosen_by_its_title() {
        let (session, fake) = popup();
        let done = session.set_value(&policy(), 1, CONTROL, "rich text").unwrap();
        assert_eq!(done, "chose \"Rich Text\" in e2 through accessibility");
        assert_eq!(fake.log(), ["ax open menu 107", "ax choose 107 Rich Text"]);
        assert_eq!(*fake.pointed.lock().unwrap(), [CENTRE]);

        let done = session.set_value(&policy(), 1, CONTROL, "Rich Text").unwrap();
        assert_eq!(done, "e2 already shows \"Rich Text\"; nothing was changed");
        assert_eq!(fake.log().len(), 2);
    }

    #[test]
    fn only_an_exact_title_of_what_a_popup_shows_leaves_it_alone() {
        let (session, fake) = popup();
        let done = session.set_value(&policy(), 1, CONTROL, " Plain Text ").unwrap();
        assert_eq!(done, "e2 already shows \"Plain Text\"; nothing was changed");
        assert!(fake.log().is_empty(), "{:?}", fake.log());
        assert!(fake.pointed.lock().unwrap().is_empty());

        // Another case may name another option, so it is looked up in the menu.
        let options = vec![option("Bold", true), option("bold", true)];
        let shows_bold = state("AXPopUpButton", LiveValue::Text("Bold".to_string()));
        let (session, fake) = with(shows_bold, Fake { options, ..Fake::default() });
        let done = session.set_value(&policy(), 1, CONTROL, "bold").unwrap();
        assert_eq!(done, "chose \"bold\" in e2 through accessibility");
        assert_eq!(fake.log(), ["ax open menu 107", "ax choose 107 bold"]);
    }

    #[test]
    fn an_option_a_popup_lists_twice_is_one_option() {
        // A font pop-up: a header that cannot be chosen, then the font again.
        let options = vec![option("Menlo", false), option("Times", true), option("Menlo", true), option("Menlo", true)];
        assert_eq!(match_option(&options, "menlo"), Found::One(2), "the first that can be chosen");
        assert_eq!(match_option(&options, "Times"), Found::One(1));
        let options = vec![option("Bold", true), option("Bold", true), option("bold", true)];
        assert_eq!(match_option(&options, "BOLD"), Found::Ambiguous, "different titles stay different");
    }

    #[test]
    fn the_emergency_stop_hit_while_the_menu_opens_closes_it_unchosen() {
        let (session, fake) = popup_with(Fake { pause_once_menu_open: true, ..Fake::default() });
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "Rich Text"), Err(Error::Paused));
        assert_eq!(fake.log(), ["ax open menu 107", "ax cancel menu 107"]);
        assert_eq!(live(&fake), LiveValue::Text("Plain Text".to_string()));
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_delivered_action_whose_result_cannot_be_read_back_still_succeeds() {
        let once = || Fake { control_read_once: true, step: Some(5.0), ..Fake::default() };
        let unread = ", but it could not be read back afterwards; observe to check";

        let (session, fake) = with(state("AXCheckBox", LiveValue::Number(0.0)), once());
        let done = session.set_value(&policy(), 1, CONTROL, "on").unwrap();
        assert_eq!(done, format!("pressed e2 through accessibility{unread}"));
        assert_eq!(fake.log(), ["ax Press 107"]);
        assert_eq!(*fake.pointed.lock().unwrap(), [CENTRE]);

        let (session, fake) = with(slider(10.0), once());
        let done = session.set_value(&policy(), 1, CONTROL, "42").unwrap();
        assert_eq!(done, format!("set e2 to 42 through accessibility{unread}"));
        assert_eq!(*fake.pointed.lock().unwrap(), [CENTRE]);

        let (session, fake) = popup_with(once());
        let done = session.set_value(&policy(), 1, CONTROL, "Rich Text").unwrap();
        assert_eq!(done, format!("chose \"Rich Text\" in e2 through accessibility{unread}"));
        assert_eq!(*fake.pointed.lock().unwrap(), [CENTRE]);
    }

    #[test]
    fn a_missing_or_disabled_option_closes_the_menu_and_lists_the_options() {
        let (session, fake) = popup();
        let options: Vec<String> = ["Rich Text", "Plain Text", "Markdown"].map(String::from).to_vec();
        let refused = session.set_value(&policy(), 1, CONTROL, "Comic Sans");
        assert_eq!(
            refused,
            Err(Error::NoSuchOption {
                reference: CONTROL.to_string(),
                value: "Comic Sans".to_string(),
                options: options.clone(),
                disabled: false
            })
        );
        assert_eq!(
            refused.unwrap_err().to_string(),
            "e2 has no option \"Comic Sans\"; its menu was closed again. Options: \"Rich Text\", \
             \"Plain Text\", \"Markdown\""
        );
        assert_eq!(fake.log(), ["ax open menu 107", "ax cancel menu 107"]);
        assert_eq!(live(&fake), LiveValue::Text("Plain Text".to_string()));
        assert!(fake.pointed.lock().unwrap().is_empty());

        assert!(matches!(
            session.set_value(&policy(), 1, CONTROL, "markdown"),
            Err(Error::NoSuchOption { disabled: true, .. })
        ));
        assert_eq!(live(&fake), LiveValue::Text("Plain Text".to_string()));
    }

    #[test]
    fn elements_that_are_not_settable_controls_are_refused_naming_the_right_tool() {
        let why = |control: ControlState| {
            let (session, fake) = with(control, Fake::default());
            let refused = session.set_value(&policy(), 1, CONTROL, "on").unwrap_err();
            assert!(fake.log().is_empty(), "{:?}", fake.log());
            refused.to_string()
        };
        let text = why(state("AXTextField", LiveValue::Text("hi".to_string())));
        assert!(text.starts_with("e2 (AXTextField) cannot be set") && text.contains("type_text"), "{text}");
        assert!(why(state("AXButton", LiveValue::None)).contains("use click"));
        assert!(why(state("AXComboBox", LiveValue::Text("a".to_string()))).contains("type_text"));
        assert!(why(state("AXTextField", LiveValue::Secret)).contains("secure"));
        // Even a role set_value knows is refused when its value is secret.
        assert!(why(state("AXCheckBox", LiveValue::Secret)).contains("secure"));
        assert!(why(state("AXStaticText", LiveValue::Text("x".to_string()))).contains("checkboxes"));
        assert!(why(state("AXSlider", LiveValue::Number(1.0))).contains("cannot be set as a number"));
        assert!(why(state("AXCheckBox", LiveValue::None)).contains("does not report"));

        let disabled = ControlState { enabled: false, ..state("AXCheckBox", LiveValue::Number(0.0)) };
        let (session, fake) = with(disabled, Fake::default());
        assert_eq!(session.set_value(&policy(), 1, CONTROL, "on"), Err(Error::Disabled(CONTROL.to_string())));
        assert!(fake.log().is_empty());
    }

    #[test]
    fn a_toggle_that_cannot_be_pressed_is_refused() {
        let (session, fake) = observed(Fake {
            control: std::sync::Mutex::new(Some(state("AXCheckBox", LiveValue::Number(0.0)))),
            ..Fake::default()
        });
        let refused = session.set_value(&policy(), 1, CONTROL, "on").unwrap_err();
        assert!(refused.to_string().contains("use click"), "{refused}");
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn paused_not_allowed_off_screen_and_stale_requests_touch_nothing() {
        let (session, fake) = with(state("AXCheckBox", LiveValue::Number(0.0)), Fake::default());
        let p = policy();
        assert_eq!(session.set_value(&p, 2, CONTROL, "on"), Err(Error::NotAllowed("com.apple.Notes".to_string())));
        assert_eq!(session.set_value(&p, 3, CONTROL, "on"), Err(Error::Forbidden("com.apple.Terminal".to_string())));
        assert_eq!(session.set_value(&p, 1, "e1000", "on"), Err(Error::StaleRef("e1000".to_string())));
        *fake.paused.lock().unwrap() = true;
        assert_eq!(session.set_value(&p, 1, CONTROL, "on"), Err(Error::Paused));

        let (session, off) = session_with(Fake { accessible: true, off_screen: vec![1], ..Fake::default() });
        assert_eq!(session.set_value(&p, 1, CONTROL, "on"), Err(Error::OffScreen(1)));

        for fake in [fake, off] {
            assert!(fake.log().is_empty(), "{:?}", fake.log());
            assert!(fake.pointed.lock().unwrap().is_empty());
        }
    }
}
