# look-at-this

An MCP server that lets AI agents see and control the windows of macOS apps
you have allowed: look at a window, click, type, press keys, scroll.

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
- Rust 1.85 or later to build.

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

While paused, every click, key and scroll is refused, a text being typed is cut
short, and `doctor` reports the pause. Looking (`observe`) still works. The
pause applies to every running server and survives restarts: only you lift it.

## Tools

| Tool | What it does |
|---|---|
| `doctor` | Permissions, allowed apps, pause state, and what to fix. |
| `list_windows` | Open windows of allowed apps, with their ids. |
| `observe` | A window's element tree and/or screenshot. `screenshot: "none"` is the cheapest; `"region"` with a `ref` returns a magnified view of one element; `max_edge` limits the image size. |
| `click` | Click an element by `ref`, or a screenshot pixel by `x`,`y`. Options: `button`, `double`, `mode`. |
| `type_text` | Put text into an element by `ref`, or type into whatever has focus. `replace` swaps the whole content. |
| `press_key` | A key or shortcut such as `enter` or `cmd+s`. |
| `scroll` | Scroll by lines over an element, a pixel, or the middle of the window. |

`click` and `type_text` take `mode: "background"`, which uses accessibility
only and fails rather than move the pointer or bring the app forward.

## Safety model

- **Allowlist.** Every call checks the window's app against your list. Windows
  of other apps are not listed, captured, read or acted on.
- **Real clicks are verified.** Before each press the server checks that the
  target window is the topmost thing at that point, across all apps and system
  layers, and for element refs that the element is really what is shown there.
  Otherwise nothing is clicked.
- **Keys reach only the target app.** They are delivered to that process, never
  system-wide, so system shortcuts such as Spotlight or the app switcher cannot
  be triggered.
- **Password fields.** Their values are never read. Typing into them is allowed.
- **Stale refs fail.** A ref from an earlier `observe`, another window or
  another process is refused, never resolved to a different element.

### What it does not protect against

- The allowlist is an ordinary file. An agent that also has shell access can
  edit it directly; the confirmation dialog only guards the `allow` command.
- An allowed app can itself open or control other apps (Finder, for example).
- A press sent through accessibility to a window behind an open modal dialog is
  reported as delivered although the app ignores it. Observe after acting.

## Known limitations

- **Stage Manager.** A background app's window is only a thumbnail there: it
  cannot be captured until the app is brought forward, and its dialogs are not
  listed.
- **Keyboard layout** is read when the server starts. After switching to a
  different Latin layout, restart the server or shortcuts may hit other keys.
- **Typing with real keys** is paced at roughly 150 characters a second so the
  emergency stop can cut it short. Text put in through accessibility is instant.
- **Accessibility text insertion** changes the field's value, but some apps do
  not notice the change; real keys are more reliable there.
- **Not yet verified:** a second display, keyboard layouts other than US and
  Russian, full-screen apps and other Spaces, floating panels that never take
  focus.

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
