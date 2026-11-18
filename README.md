# look-at-this

An MCP server that lets AI agents see and control the windows of macOS apps
you have allowed: look at a window, click, type, press keys, scroll, drag.

Use it to let an agent check how an app looks, fill in a form, or test what it
has just built.

## How it works

- **Seeing.** `observe` returns a window's accessibility tree, a screenshot, or
  both. Tree lines carry refs (`e12`) and coordinates in screenshot pixels, so
  the agent can act on an element by name instead of guessing at pixels.
  Screenshots are taken per window, so other apps never appear in them.
- **Acting.** Every action first tries accessibility (press a button, put text
  into a field), which does not move your pointer or change focus. If the
  element cannot do that, the server falls back to real input: it brings the
  app to the front, clicks with the real pointer and puts the pointer back.
  Each result says which way the action was delivered.
- **The agent's cursor.** An orange arrow is drawn where each action lands, so
  you can see what the agent is doing. It is only a picture: clicks pass
  through it and it never appears in the agent's screenshots.

## Requirements

- macOS 14 or later (developed and tested on macOS 26, Apple silicon).
- Rust 1.88 or later to build.

## Install

```sh
cargo build --release
```

Register the binary with your MCP client. For Claude Code:

```sh
claude mcp add look-at-this /path/to/look-at-this/target/release/look-at-this
```

### Permissions

The server needs **Screen & System Audio Recording** and **Accessibility**
(System Settings → Privacy & Security). macOS grants these to the app that
launches the server — your terminal or agent host — not to the binary itself.
The `doctor` tool reports what is missing.

### Allow apps

Nothing is accessible until you allow it:

```sh
look-at-this allow com.apple.TextEdit
```

A system dialog asks you to confirm; the app is added only if you click
**Allow**. The list lives in `~/.config/look-at-this/config.toml`.

Some apps can never be allowed, because controlling them would let an agent
get around its own safeguards or reach secrets: terminals, editors with a
built-in terminal, System Settings, Script Editor, Automator, Shortcuts,
password managers and the agent hosts themselves. The list is in
`src/domain/policy.rs`.

## Emergency stop

Press **ctrl+option+cmd+escape** to pause all agent input. A notice appears in
the middle of the screen; press the same keys to resume.

While paused, every click, key, scroll, drag, menu press and `activate` is refused, a text being typed is cut
short, a drag in progress is cancelled (released back where it started when that is safe), and `doctor` reports the pause. Looking (`observe`, `wait_for`, listing a `menu`) still works. The
pause applies to every running server and survives restarts: only you lift it.

## Tools

| Tool | What it does |
|---|---|
| `doctor` | Permissions, allowed apps, pause state, and what to fix. |
| `list_windows` | Open windows of allowed apps, with their ids, including minimised and hidden ones. Each line ends with the state words that apply: `frontmost`, `thumbnail` (Stage Manager shows only a thumbnail), `off-screen` (minimised, hidden or on another Space). |
| `activate` | Bring a window and its app to the front, un-minimising it if needed. Required before `observe` or any action on a `thumbnail` or `off-screen` window. |
| `observe` | A window's element tree and/or screenshot. `screenshot: "none"` is the cheapest; `"region"` with a `ref` returns a magnified view of one element; `max_edge` limits the image size. |
| `observe` filters | `find` (text in label or value), `role` (e.g. `button`), `interactive` (controls only) and `max_depth` print only part of the tree; conditions combine. Matches of `find`, `role` or `interactive` are listed flat, and a last line tells how many elements are shown; refs of hidden elements still work. |
| `wait_for` | Wait until an element matching `find` and/or `role` appears, or with `gone` until none is left. Returns the matches as `observe` would, with fresh refs; `timeout_ms` defaults to 5000, at most 30000. Refs from earlier observes stop working, even when it times out. |
| `click` | Click an element by `ref`, or a screenshot pixel by `x`,`y`. Options: `button`, `double`, `mode`. |
| `type_text` | Put text into an element by `ref`, or type into whatever has focus. `replace` swaps the whole content. |
| `set_value` | Set a checkbox, switch or radio button to `on` or `off`, a slider or stepper to a number, or a pop-up button to the title of one of its options, through accessibility only. Does nothing if the control already has that value, and says what it holds afterwards. |
| `menu` | List or press items of the app's menu bar through accessibility, without opening any menu on screen. No `path` lists the menus; a `path` such as `Format > Font` lists that menu's items with their shortcut and the words `checked`, `disabled`, `submenu`; a `path` ending at an item, such as `Format > Font > Bold`, presses it. Titles match ignoring case and a trailing `…`; a title containing `>` cannot be named. A press needs `window_id` to be the app's main window (`activate` it first). |
| `press_key` | A key or shortcut such as `enter` or `cmd+s`. |
| `scroll` | Scroll by lines over an element, a pixel, or the middle of the window. |
| `drag` | Press at `from` (`from_ref`, or `from_x`,`from_y`), move the real pointer to `to` (`to_ref`, or `to_x`,`to_y`) and release, inside one window: select text, move a slider or divider, reorder items. Both ends must lie inside the window, on a display, and differ. Takes about 1.1 s. |

`click` and `type_text` take `mode: "background"`, which uses accessibility
only and fails rather than move the pointer or bring the app forward.

## Safety model

- **Allowlist.** Every call checks the window's app against your list. Windows
  of other apps are not listed, captured, read or acted on.
- **Real clicks and drags are verified.** Before each press the server checks
  that the target window is the topmost thing at that point, across all apps
  and system layers, and for element refs that the element is really what is
  shown there. Otherwise nothing is clicked or pressed.
- **A drag is released only where it was verified.** After each step of the
  path and right before the release, the window must still be the topmost
  thing under the pointer, and at the end still at the frame it had at the
  press. If not, or if the emergency stop is hit or the mouse is moved during
  the drag, the drag is cancelled: the pointer retraces its path and releases
  at the start, or, when the way back is covered, at the last point before the
  cover, crossing only points where the window was on top when the cancel began. When it cannot
  step back at all, or the release point gets covered meanwhile, Escape is sent
  to the app first (it cancelled a TextEdit drag-and-drop live, where a release
  without it dropped the text into the window on top). The result says which.
- **Keys reach only the target app.** They are delivered to that process, never
  system-wide, so system shortcuts such as Spotlight or the app switcher cannot
  be triggered.
- **Password fields.** Their values are never read. Typing into them is allowed.
- **System menus through `menu`.** The `menu` tool never lists or presses the
  Apple menu or the Services menu, which reach System Settings, logout and
  shutdown, and other apps. This holds for `menu` only (see below).
- **Stale refs fail.** A ref from before the latest `observe` or `wait_for`,
  from another window or another process is refused, never resolved to a
  different element.

### What it does not protect against

- The allowlist is an ordinary file. An agent that also has shell access can
  edit it directly; the confirmation dialog only guards the `allow` command.
- An allowed app can itself open or control other apps (Finder, for example).
- A press sent through accessibility to a window behind an open modal dialog is
  reported as delivered although the app ignores it. Observe after acting.
- A Services keyboard shortcut sent with `press_key` is handled by the allowed
  app and can start another app.

## Known limitations

- **Stage Manager.** A background app's window is only a thumbnail there, or
  off screen while the strip is hidden: `list_windows` marks it, and it cannot
  be observed or acted on until `activate` brings it forward. Its dialogs are
  not listed.
- **Other Spaces and hidden apps.** `activate` un-minimises a window reliably.
  A window on another Space is listed as `off-screen` but cannot be brought
  forward: apps report only the current Space's windows to accessibility, so
  `activate` refuses and asks for the user. A hidden app's window is best
  effort and may time out.
- **Keyboard layout** is read when the server starts. After switching to a
  different Latin layout, restart the server or shortcuts may hit other keys.
- **Typing with real keys** is paced at roughly 150 characters a second so the
  emergency stop can cut it short. Text put in through accessibility is instant.
- **Accessibility text insertion** changes the field's value, but some apps do
  not notice the change; real keys are more reliable there.
- **`set_value` and combo boxes.** Pop-up buttons work; choosing from a combo
  box's list does not, so put the text into it with `type_text` instead.
- **`menu` state lags.** Apps refresh a menu's checkmarks and enabled items a
  moment after a change (about a second in TextEdit), so a listing right after a
  press may still show the old state. A menu an app fills only when it is opened
  on screen is listed as empty. Shortcuts on keys other than printable ones (such
  as escape or the Globe key) are left out of listings.
- **`drag`** takes about 1.1 s: the button is held 0.2 s before the pointer
  moves, as a person's would be, so a drag that starts inside selected text
  moves that text rather than starting a new selection. Dragging a title bar
  moves the window, so the drag is cancelled. Some elements, such as TextEdit's
  ruler markers, are not found by the element check at their position: drag
  them by `x`,`y`. A cancel is not always a clean undo: a text selection becomes
  a caret at the start, a drawing app may keep the there-and-back stroke, and a
  drag-and-drop is dropped onto its own origin, which most apps ignore. A
  window of another app that comes on top during a drag-and-drop gets the drag
  for one step (about 30 ms) before the cancel, enough to see what is dragged;
  when the cancel is by Escape, the pointer stays over it until the release
  0.1 s later. That Escape also arrives when nothing was being dragged and
  dropped (a slider or a selection drag): the app then treats it as a normal
  Escape and may close a sheet or popover, revert a field being edited or
  leave full screen.
- **A server that dies mid-drag** (a panic ends the process; the host may kill
  it) leaves the button held down until your next click. A cancelled request
  or a closed connection does not cut a drag short: it finishes first (checked
  live).
- **Not yet verified:** a second display, keyboard layouts other than US and
  Russian, full-screen apps and other Spaces, floating panels that never take
  focus, `set_value` on steppers, switches and radio buttons, a `menu` press
  that opens an app-modal dialog, the emergency stop in the middle of a drag
  with a real app, Escape on drag-and-drop in apps other than TextEdit.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The code is split into a pure domain (`src/domain`: policy, refs, coordinates,
the action ladder), the contracts it needs from the system (`src/ports.rs`),
and adapters (`src/adapters`: macOS and MCP). The domain is tested against a
scripted fake of the operating system.

## License

MIT. See [LICENSE](LICENSE).
