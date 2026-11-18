//! The app's menu bar: listing its menus and pressing their items through
//! accessibility alone, without opening any menu on screen.
//!
//! The Apple and Services menus are never listed or pressed: they reach
//! System Settings, logout and shutdown, and other apps, which the access
//! policy keeps out of the agent's reach.

use std::fmt::Write;

use crate::domain::policy::AccessPolicy;
use crate::domain::session::{Error, Session};
use crate::domain::snapshot::one_line;
use crate::ports::{MenuItem, MenuKind, MenuStep, Modifiers, Point, WindowInfo};

/// Where a wanted title is among a list of titles.
#[derive(Debug, PartialEq)]
pub(super) enum Found {
    One(usize),
    Missing,
    /// Several titles match equally well.
    Ambiguous,
}

/// The title `wanted` among `titles`, both trimmed: the one equal to it,
/// else the one that differs only by a trailing ellipsis ("Save…" or
/// "Save..."), else the one that also differs in case. Empty titles
/// (separators) never match.
pub(super) fn find_title(titles: &[&str], wanted: &str) -> Found {
    let wanted = wanted.trim();
    let bare_wanted = bare(wanted);
    let lower_wanted = bare_wanted.to_lowercase();
    let tiers: [&dyn Fn(&str) -> bool; 3] = [
        &|title| title == wanted,
        &|title| !bare_wanted.is_empty() && bare(title) == bare_wanted,
        &|title| !bare_wanted.is_empty() && bare(title).to_lowercase() == lower_wanted,
    ];
    for matches in tiers {
        let mut found = titles
            .iter()
            .map(|title| title.trim())
            .enumerate()
            .filter(|(_, title)| !title.is_empty() && matches(title));
        match (found.next(), found.next()) {
            (Some((only, _)), None) => return Found::One(only),
            (Some(_), Some(_)) => return Found::Ambiguous,
            _ => {}
        }
    }
    Found::Missing
}

/// A title without its trailing ellipsis.
fn bare(title: &str) -> &str {
    title.strip_suffix('…').or_else(|| title.strip_suffix("...")).unwrap_or(title).trim_end()
}

impl Session {
    /// Without `path`, lists the menu bar of the window's app. With a path
    /// of titles such as `Format > Font > Bold`, lists the menu it leads to,
    /// or presses the item it ends at. Returns the listing, or a sentence
    /// saying what was pressed.
    pub fn menu(&self, policy: &AccessPolicy, window_id: u32, path: Option<&str>) -> Result<String, Error> {
        let titles = parse(path)?;
        // Listing is looking, like observe, so the emergency stop is checked
        // only once the path turns out to end at an item to press.
        let window = self.allowed_window(policy, window_id)?;
        let mut steps = Vec::new();
        let mut chosen: Vec<MenuItem> = Vec::new();
        let mut items = self.menu_items(&window, &steps, &chosen)?;
        for (depth, wanted) in titles.iter().enumerate() {
            let index = choose(&items, wanted, &chosen)?;
            let item = items.swap_remove(index);
            steps.push(MenuStep { index, title: item.title.clone() });
            // A menu-bar title is never pressed: that opens its menu on screen.
            let opens = item.has_submenu || depth == 0;
            chosen.push(item);
            if !opens {
                return match depth + 1 == titles.len() {
                    true => self.press_menu_item(&window, &steps, &chosen),
                    false => Err(Error::NoSubmenu(path_text(&chosen))),
                };
            }
            items = self.menu_items(&window, &steps, &chosen)?;
        }
        Ok(listing(&window, &chosen, &items))
    }

    /// The items of the menu `steps` lead to, unless it may be the Services menu.
    fn menu_items(&self, window: &WindowInfo, steps: &[MenuStep], chosen: &[MenuItem]) -> Result<Vec<MenuItem>, Error> {
        let items = self
            .ports
            .actions
            .menu_items(window.pid, steps)
            .map_err(Error::System)?
            .ok_or_else(|| Error::MenuChanged(path_text(chosen)))?;
        let Some(parent) = chosen.last() else { return Ok(items) };
        // Its items tell what its parent did not when it was read.
        if items.iter().any(|item| item.kind == MenuKind::ServicesItem) {
            return Err(Error::ForbiddenMenu { title: title(parent), apple: false });
        }
        // The application menu is the second menu-bar title. An empty submenu
        // of it may be the Services menu not filled yet, which only its title,
        // in the user's language, would tell.
        if items.is_empty() && steps.len() == 2 && steps[0].index == 1 {
            return Err(Error::UnloadedMenu(path_text(chosen)));
        }
        Ok(items)
    }

    fn press_menu_item(&self, window: &WindowInfo, steps: &[MenuStep], chosen: &[MenuItem]) -> Result<String, Error> {
        // A person could not reach an item inside a disabled one either.
        if let Some(disabled) = chosen.iter().position(|item| !item.enabled) {
            return Err(Error::MenuItemDisabled(path_text(&chosen[..=disabled])));
        }
        if !self.ports.tree.is_main_window(window.pid, window.id).map_err(Error::System)? {
            return Err(Error::NotMainWindow(window.id));
        }
        // Right before the press: the menus were read meanwhile, which takes
        // time, and that is when a user reaches for the stop.
        self.ensure_running()?;
        let path = path_text(chosen);
        if !self.ports.actions.press_menu_item(window.pid, steps).map_err(Error::System)? {
            return Err(Error::MenuChanged(path));
        }
        let title = chosen[0].frame.map(|f| Point { x: f.x + f.width / 2.0, y: f.y + f.height / 2.0 });
        self.show_cursor(title);
        Ok(format!(
            "pressed {path} in the menu bar of {} through accessibility; the pointer was not used",
            one_line(&window.app_name)
        ))
    }
}

/// The titles of a path such as `Format > Font > Bold`, trimmed; none for
/// an absent or blank path.
fn parse(path: Option<&str>) -> Result<Vec<&str>, Error> {
    let Some(path) = path.filter(|path| !path.trim().is_empty()) else { return Ok(Vec::new()) };
    let titles: Vec<&str> = path.split('>').map(str::trim).collect();
    match titles.iter().any(|title| title.is_empty()) {
        true => Err(Error::BadMenuPath),
        false => Ok(titles),
    }
}

/// The item of `items` titled `wanted`, unless it is or belongs to a
/// forbidden menu.
/// `above` are the items chosen on the way to `items`.
fn choose(items: &[MenuItem], wanted: &str, above: &[MenuItem]) -> Result<usize, Error> {
    let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
    match find_title(&titles, wanted) {
        Found::One(index) => match items[index].kind {
            MenuKind::App => Ok(index),
            kind => Err(Error::ForbiddenMenu { title: title(&items[index]), apple: kind == MenuKind::Apple }),
        },
        Found::Missing => Err(Error::NoMenuItem {
            menu: path_text(above),
            title: one_line(wanted),
            available: shown(items).map(title).collect(),
        }),
        Found::Ambiguous => Err(Error::AmbiguousMenuItem { menu: path_text(above), title: one_line(wanted) }),
    }
}

/// The items an agent gets to see: no separators, no forbidden menus.
fn shown(items: &[MenuItem]) -> impl Iterator<Item = &MenuItem> {
    items.iter().filter(|item| item.kind == MenuKind::App && !item.title.trim().is_empty())
}

fn title(item: &MenuItem) -> String {
    one_line(item.title.trim())
}

/// `Format > Font`: the titles of the chosen items.
fn path_text(chosen: &[MenuItem]) -> String {
    chosen.iter().map(title).collect::<Vec<_>>().join(" > ")
}

/// A header naming the menu, then one line per item: its title, its
/// shortcut, and the words `checked`, `disabled` and `submenu` that apply.
fn listing(window: &WindowInfo, chosen: &[MenuItem], items: &[MenuItem]) -> String {
    let app = one_line(&window.app_name);
    let mut text = match chosen.is_empty() {
        true => format!("menu bar of {app}:\n"),
        false => format!("{} ({app}):\n", path_text(chosen)),
    };
    let mut empty = true;
    for item in shown(items) {
        empty = false;
        let _ = write!(text, "\"{}\"", title(item));
        if let Some(keys) = item.shortcut {
            let _ = write!(text, " {}", shortcut(keys));
        }
        for (applies, word) in [(item.checked, "checked"), (!item.enabled, "disabled"), (item.has_submenu, "submenu")] {
            if applies {
                let _ = write!(text, " {word}");
            }
        }
        text.push('\n');
    }
    if empty {
        text.push_str(
            "(no items; the app may fill this menu only when it is opened on screen, which this tool does not do)\n",
        );
    }
    text
}

/// `cmd+shift+s`: a shortcut written the way press_key takes it.
fn shortcut((modifiers, key): (Modifiers, char)) -> String {
    let m = modifiers;
    let mut keys: String = [(m.cmd, "cmd+"), (m.ctrl, "ctrl+"), (m.alt, "alt+"), (m.shift, "shift+")]
        .iter()
        .filter_map(|(on, name)| on.then_some(*name))
        .collect();
    match key {
        '+' => keys.push_str("plus"),
        key => keys.push(key),
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::keys;
    use crate::domain::testing::{Fake, FakeMenu, policy, session_with};
    use crate::ports::{Key, KeyCombo, Rect};

    const CMD: Modifiers = Modifiers { cmd: true, ctrl: false, alt: false, shift: false };

    fn item(title: &str) -> MenuItem {
        MenuItem {
            title: title.to_string(),
            enabled: true,
            checked: false,
            has_submenu: false,
            shortcut: None,
            kind: MenuKind::App,
            frame: None,
        }
    }

    fn leaf(item: MenuItem) -> FakeMenu {
        FakeMenu { item, items: Vec::new() }
    }

    fn menu(item: MenuItem, items: Vec<FakeMenu>) -> FakeMenu {
        FakeMenu { item: MenuItem { has_submenu: true, ..item }, items }
    }

    /// A menu-bar title 40 points wide at `x`.
    fn top(title: &str, x: f64, kind: MenuKind, items: Vec<FakeMenu>) -> FakeMenu {
        let frame = Some(Rect { x, y: 0.0, width: 40.0, height: 24.0 });
        menu(MenuItem { kind, frame, ..item(title) }, items)
    }

    fn menu_bar() -> Vec<FakeMenu> {
        let shift = Modifiers { shift: true, ..CMD };
        let no_cmd = Modifiers { ctrl: true, alt: true, ..Modifiers::default() };
        vec![
            top("Apple", 10.0, MenuKind::Apple, vec![leaf(item("About This Mac")), leaf(item("Shut Down…"))]),
            top(
                "TextEdit",
                50.0,
                MenuKind::App,
                vec![
                    leaf(item("About TextEdit")),
                    leaf(item("")),
                    menu(MenuItem { kind: MenuKind::Services, ..item("Services") }, vec![leaf(item("Activity Monitor"))]),
                    // The Services menu when its parent was not recognised: by
                    // its items, a section header and a service.
                    menu(
                        item("Tools"),
                        vec![
                            leaf(MenuItem { enabled: false, ..item("Development") }),
                            leaf(MenuItem { kind: MenuKind::ServicesItem, ..item("Open man Page in Terminal") }),
                        ],
                    ),
                    // Possibly the Services menu before the app fills it.
                    menu(item("Recent Items"), Vec::new()),
                    leaf(item("")),
                ],
            ),
            top(
                "File",
                90.0,
                MenuKind::App,
                vec![
                    leaf(MenuItem { shortcut: Some((CMD, 's')), ..item("Save…") }),
                    leaf(MenuItem { shortcut: Some((shift, 's')), ..item("Save As...") }),
                ],
            ),
            top(
                "Format",
                130.0,
                MenuKind::App,
                vec![
                    menu(
                        item("Font"),
                        vec![
                            leaf(MenuItem { shortcut: Some((CMD, 'b')), ..item("Bold") }),
                            leaf(MenuItem { checked: true, shortcut: Some((no_cmd, '+')), ..item("Bigger") }),
                            leaf(item("Outline")),
                            leaf(item("outline")),
                            menu(MenuItem { enabled: false, ..item("Highlight") }, vec![leaf(item("Accent"))]),
                        ],
                    ),
                    leaf(item("")),
                    leaf(MenuItem { enabled: false, ..item("Make Plain Text") }),
                    leaf(MenuItem { checked: true, ..item("Show Ruler") }),
                    menu(item("Writing Tools"), Vec::new()),
                ],
            ),
            // An app that fills a menu only when it is opened.
            top("Help", 170.0, MenuKind::App, Vec::new()),
        ]
    }

    fn with(fake: Fake) -> (Session, std::sync::Arc<Fake>) {
        session_with(Fake { menu_bar: menu_bar(), ..fake })
    }

    fn presses(fake: &Fake) -> Vec<String> {
        fake.log().into_iter().filter(|entry| entry.starts_with("menu press")).collect()
    }

    #[test]
    fn the_menu_bar_is_listed_without_the_apple_menu() {
        let (session, fake) = with(Fake::default());
        let listed = session.menu(&policy(), 1, None).unwrap();
        assert_eq!(
            listed,
            "menu bar of com.apple.TextEdit:\n\
             \"TextEdit\" submenu\n\
             \"File\" submenu\n\
             \"Format\" submenu\n\
             \"Help\" submenu\n"
        );
        assert_eq!(session.menu(&policy(), 1, Some("  ")).unwrap(), listed);
        assert_eq!(fake.log(), ["menu read menu bar", "menu read menu bar"]);
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_submenu_is_listed_with_shortcuts_and_marks_but_no_separators() {
        let (session, fake) = with(Fake::default());
        assert_eq!(
            session.menu(&policy(), 1, Some("Format > Font")).unwrap(),
            "Format > Font (com.apple.TextEdit):\n\
             \"Bold\" cmd+b\n\
             \"Bigger\" ctrl+alt+plus checked\n\
             \"Outline\"\n\
             \"outline\"\n\
             \"Highlight\" disabled submenu\n"
        );
        assert_eq!(
            session.menu(&policy(), 1, Some("Format")).unwrap(),
            "Format (com.apple.TextEdit):\n\
             \"Font\" submenu\n\
             \"Make Plain Text\" disabled\n\
             \"Show Ruler\" checked\n\
             \"Writing Tools\" submenu\n"
        );
        assert_eq!(
            session.menu(&policy(), 1, Some("File")).unwrap(),
            "File (com.apple.TextEdit):\n\"Save…\" cmd+s\n\"Save As...\" cmd+shift+s\n"
        );
        // The Services menu is left out of the app menu.
        assert_eq!(
            session.menu(&policy(), 1, Some("TextEdit")).unwrap(),
            "TextEdit (com.apple.TextEdit):\n\"About TextEdit\"\n\"Tools\" submenu\n\"Recent Items\" submenu\n"
        );
        // An empty submenu outside the application menu is listed as empty.
        assert!(session.menu(&policy(), 1, Some("Format > Writing Tools")).unwrap().contains("(no items;"));
        // A menu-bar title is listed even when it has no items to show.
        assert_eq!(
            session.menu(&policy(), 1, Some("Help")).unwrap(),
            "Help (com.apple.TextEdit):\n(no items; the app may fill this menu only when it is opened on \
             screen, which this tool does not do)\n"
        );
        assert!(presses(&fake).is_empty());
    }

    #[test]
    fn a_nested_item_is_pressed_and_the_cursor_shown_at_its_menu_title() {
        let (session, fake) = with(Fake::default());
        let done = session.menu(&policy(), 1, Some("Format > Font > Bold")).unwrap();
        assert_eq!(
            done,
            "pressed Format > Font > Bold in the menu bar of com.apple.TextEdit through accessibility; \
             the pointer was not used"
        );
        assert_eq!(
            fake.log(),
            ["menu read menu bar", "menu read Format", "menu read Format > Font", "menu press Format > Font > Bold"]
        );
        assert_eq!(*fake.pointed.lock().unwrap(), [Point { x: 150.0, y: 12.0 }]);
    }

    #[test]
    fn titles_match_ignoring_case_and_a_trailing_ellipsis() {
        let (session, fake) = with(Fake::default());
        session.menu(&policy(), 1, Some(" format>FONT >  bold ")).unwrap();
        session.menu(&policy(), 1, Some("File > Save")).unwrap();
        session.menu(&policy(), 1, Some("file > save as")).unwrap();
        session.menu(&policy(), 1, Some("File > Save As…")).unwrap();
        assert_eq!(
            presses(&fake),
            [
                "menu press Format > Font > Bold",
                "menu press File > Save…",
                "menu press File > Save As...",
                "menu press File > Save As..."
            ]
        );
    }

    #[test]
    fn an_ambiguous_title_is_refused_unless_it_matches_one_exactly() {
        let (session, fake) = with(Fake::default());
        let refused = session.menu(&policy(), 1, Some("Format > Font > OUTLINE"));
        assert_eq!(
            refused,
            Err(Error::AmbiguousMenuItem { menu: "Format > Font".to_string(), title: "OUTLINE".to_string() })
        );
        assert!(refused.unwrap_err().to_string().contains("exactly as listed"));
        assert!(presses(&fake).is_empty());
        session.menu(&policy(), 1, Some("Format > Font > outline")).unwrap();
        assert_eq!(presses(&fake), ["menu press Format > Font > outline"]);
    }

    #[test]
    fn a_missing_title_lists_what_that_level_has() {
        let (session, fake) = with(Fake::default());
        let refused = session.menu(&policy(), 1, Some("Formt > Bold"));
        assert_eq!(
            refused,
            Err(Error::NoMenuItem {
                menu: String::new(),
                title: "Formt".to_string(),
                available: ["TextEdit", "File", "Format", "Help"].map(String::from).to_vec(),
            })
        );
        assert_eq!(
            refused.unwrap_err().to_string(),
            "the menu bar has no menu \"Formt\". Menus: \"TextEdit\", \"File\", \"Format\", \"Help\""
        );
        assert_eq!(
            session.menu(&policy(), 1, Some("Format > Bold")).unwrap_err().to_string(),
            "Format has no item \"Bold\". Items: \"Font\", \"Make Plain Text\", \"Show Ruler\", \"Writing Tools\""
        );
        assert_eq!(
            session.menu(&policy(), 1, Some("TextEdit > Quit")).unwrap_err(),
            Error::NoMenuItem {
                menu: "TextEdit".to_string(),
                title: "Quit".to_string(),
                available: ["About TextEdit", "Tools", "Recent Items"].map(String::from).to_vec(),
            }
        );
        assert!(
            session.menu(&policy(), 1, Some("Help > Search")).unwrap_err().to_string().contains("Items: none;")
        );
        assert!(presses(&fake).is_empty());
    }

    #[test]
    fn a_disabled_item_or_one_inside_a_disabled_item_is_not_pressed() {
        let (session, fake) = with(Fake::default());
        let refused = session.menu(&policy(), 1, Some("Format > Make Plain Text"));
        assert_eq!(refused, Err(Error::MenuItemDisabled("Format > Make Plain Text".to_string())));
        assert_eq!(
            session.menu(&policy(), 1, Some("Format > Font > Highlight > Accent")),
            Err(Error::MenuItemDisabled("Format > Font > Highlight".to_string()))
        );
        assert!(presses(&fake).is_empty());
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn the_apple_and_services_menus_are_refused_by_name() {
        let (session, fake) = with(Fake::default());
        let apple = Err(Error::ForbiddenMenu { title: "Apple".to_string(), apple: true });
        assert_eq!(session.menu(&policy(), 1, Some("Apple")), apple);
        assert_eq!(session.menu(&policy(), 1, Some("apple > Shut Down")), apple);
        let services = Err(Error::ForbiddenMenu { title: "Services".to_string(), apple: false });
        assert_eq!(session.menu(&policy(), 1, Some("TextEdit > Services")), services);
        assert_eq!(session.menu(&policy(), 1, Some("TextEdit > services > Activity Monitor")), services);
        assert!(session.menu(&policy(), 1, Some("Apple")).unwrap_err().to_string().contains("System Settings"));
        // Nothing inside them was even read.
        assert!(fake.log().iter().all(|entry| entry == "menu read menu bar" || entry == "menu read TextEdit"));
    }

    #[test]
    fn a_menu_holding_services_items_is_refused_whatever_its_parent_was_taken_for() {
        let (session, fake) = with(Fake::default());
        let tools = Err(Error::ForbiddenMenu { title: "Tools".to_string(), apple: false });
        assert_eq!(session.menu(&policy(), 1, Some("TextEdit > Tools")), tools);
        assert_eq!(session.menu(&policy(), 1, Some("TextEdit > Tools > Development")), tools);
        assert_eq!(session.menu(&policy(), 1, Some("TextEdit > Tools > Open man Page in Terminal")), tools);
        assert!(presses(&fake).is_empty());
    }

    #[test]
    fn an_empty_submenu_of_the_application_menu_is_never_listed_or_walked_through() {
        let (session, fake) = with(Fake::default());
        let unloaded = Err(Error::UnloadedMenu("TextEdit > Recent Items".to_string()));
        assert_eq!(session.menu(&policy(), 1, Some("TextEdit > Recent Items")), unloaded);
        assert_eq!(session.menu(&policy(), 1, Some("TextEdit > Recent Items > Notes.rtf")), unloaded);
        let refused = session.menu(&policy(), 1, Some("TextEdit > Recent Items")).unwrap_err();
        assert!(refused.to_string().contains("Services menu"), "{refused}");
        assert!(presses(&fake).is_empty());
    }

    #[test]
    fn an_item_that_became_a_services_item_before_the_press_is_not_pressed() {
        let (session, fake) = with(Fake { kind_at_press: Some(MenuKind::ServicesItem), ..Fake::default() });
        let refused = session.menu(&policy(), 1, Some("Format > Font > Bold"));
        assert_eq!(refused, Err(Error::MenuChanged("Format > Font > Bold".to_string())));
        assert!(presses(&fake).is_empty());
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_press_needs_the_main_window_but_a_listing_does_not() {
        let (session, fake) = with(Fake { not_main: true, ..Fake::default() });
        let refused = session.menu(&policy(), 1, Some("Format > Font > Bold"));
        assert_eq!(refused, Err(Error::NotMainWindow(1)));
        assert!(refused.unwrap_err().to_string().contains("activate"));
        assert!(session.menu(&policy(), 1, Some("Format > Font")).is_ok());
        assert!(presses(&fake).is_empty());
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn the_emergency_stop_refuses_a_press_even_when_hit_during_the_walk_but_not_a_listing() {
        let (session, fake) = with(Fake { paused: std::sync::Mutex::new(true), ..Fake::default() });
        assert_eq!(session.menu(&policy(), 1, Some("Format > Font > Bold")), Err(Error::Paused));
        assert!(session.menu(&policy(), 1, Some("Format > Font")).is_ok());
        assert!(session.menu(&policy(), 1, None).is_ok());
        assert!(presses(&fake).is_empty());

        let (session, fake) = with(Fake { pause_once_menu_read: true, ..Fake::default() });
        assert_eq!(session.menu(&policy(), 1, Some("Format > Font > Bold")), Err(Error::Paused));
        assert_eq!(fake.log().len(), 3, "the path was walked: {:?}", fake.log());
        assert!(presses(&fake).is_empty());
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn windows_that_may_not_be_acted_on_touch_nothing() {
        let (session, fake) = with(Fake::default());
        let p = policy();
        assert_eq!(session.menu(&p, 2, None), Err(Error::NotAllowed("com.apple.Notes".to_string())));
        assert_eq!(session.menu(&p, 3, Some("Shell > New Window")), Err(Error::Forbidden("com.apple.Terminal".to_string())));
        assert_eq!(session.menu(&p, 9, None), Err(Error::WindowNotFound(9)));
        let (session, off) = with(Fake { off_screen: vec![1], ..Fake::default() });
        assert_eq!(session.menu(&p, 1, None), Err(Error::OffScreen(1)));
        assert_eq!(session.menu(&p, 1, Some("Format > Font > Bold")), Err(Error::OffScreen(1)));
        for fake in [fake, off] {
            assert!(fake.log().is_empty(), "{:?}", fake.log());
            assert!(fake.pointed.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn an_item_renamed_between_finding_and_pressing_is_not_pressed() {
        let (session, fake) = with(Fake { renamed_at_press: Some("Bolder".to_string()), ..Fake::default() });
        let refused = session.menu(&policy(), 1, Some("Format > Font > Bold"));
        assert_eq!(refused, Err(Error::MenuChanged("Format > Font > Bold".to_string())));
        assert!(refused.unwrap_err().to_string().contains("nothing was pressed"));
        assert!(presses(&fake).is_empty());
        assert!(fake.pointed.lock().unwrap().is_empty());
    }

    #[test]
    fn malformed_paths_and_paths_past_a_plain_item_are_refused() {
        let (session, fake) = with(Fake::default());
        assert_eq!(session.menu(&policy(), 1, Some("Format > > Bold")), Err(Error::BadMenuPath));
        assert_eq!(session.menu(&policy(), 1, Some(">")), Err(Error::BadMenuPath));
        assert!(fake.log().is_empty());
        assert_eq!(
            session.menu(&policy(), 1, Some("Format > Show Ruler > On")),
            Err(Error::NoSubmenu("Format > Show Ruler".to_string()))
        );
        assert!(presses(&fake).is_empty());
    }

    #[test]
    fn shortcuts_are_written_as_press_key_reads_them() {
        let all = Modifiers { cmd: true, ctrl: true, alt: true, shift: true };
        for (modifiers, key) in [(CMD, 'b'), (all, '+'), (Modifiers::default(), '['), (CMD, ',')] {
            let parsed = keys::parse(&shortcut((modifiers, key))).unwrap();
            assert_eq!(parsed, KeyCombo { modifiers, key: Key::Char(key) });
        }
        assert_eq!(shortcut((all, 's')), "cmd+ctrl+alt+shift+s");
    }

    #[test]
    fn titles_match_exactly_first_then_without_ellipsis_then_ignoring_case() {
        assert_eq!(find_title(&["Bold", "bold", "Italic"], "bold"), Found::One(1));
        assert_eq!(find_title(&["Bold", "bold", "Italic"], "BOLD"), Found::Ambiguous);
        assert_eq!(find_title(&["Bold", "bold", "Italic"], "italic"), Found::One(2));
        assert_eq!(find_title(&["  Plain  "], "Plain"), Found::One(0));
        assert_eq!(find_title(&["Save…", "Save As..."], "save as"), Found::One(1));
        assert_eq!(find_title(&["Save", "Save…"], "Save"), Found::One(0));
        assert_eq!(find_title(&["Save", "Save…"], "Save..."), Found::Ambiguous, "three dots are not \"…\"");
        assert_eq!(find_title(&["Default", "Default"], "Default"), Found::Ambiguous);
        assert_eq!(find_title(&["", "Font"], ""), Found::Missing);
        assert_eq!(find_title(&["", "Font"], "..."), Found::Missing);
        assert_eq!(find_title(&["Font"], "Fonts"), Found::Missing);
    }
}
