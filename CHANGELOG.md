# What's new in RustInput

What changed for you in each version, newest first. RustInput shows the part of its own version once, the first time
its window opens after an update.

Writing an entry: a `- ` line per change people will notice, in plain words, under the version it comes with; code
between backticks. Each change is translated like the texts of the program: `cargo run -p rustinput-i18n --example
update-po` puts it in `po/` (context `changelog`), and the window shows it in the language of the windows.

## 0.54.0

- The connection for mouse, keyboard and clipboard now takes only its modern encryption (AES-GCM): an older and weaker kind it still allowed, never used in practice, is refused. Lan Mouse and older versions of RustInput still connect.

## 0.53.0

- A computer switched off in the list of connections can no longer control this one either (it could before): switched off, neither controls the other, and it does not fill the clipboard of this one or get its sound. Its window says that this computer switched it off.

## 0.52.0

- The sound of every computer in the headphones of one: switch on "Hear the other computers here" in Preferences on the computer with the headphones. The others send it their sound, encrypted, with their speakers silent meanwhile; each one has its volume and mute there.

## 0.51.0

- Linux: "Update now" downloads the new version, checks the signature of its release and installs it in place of this one; it runs after "Restart now". AI agents can do it too (apply_update).

## 0.50.0

- An AI agent (Claude Code, Claude Desktop or any MCP client) can see where the mouse and keyboard are, move them, send files and look for a new version: `claude mcp add rustinput -- rustinput mcp`. It can never authorize a computer, type or read a clipboard.

## 0.49.0

- Português (Portugal): the windows, the menus, what is new and the installer for Windows also speak the Portuguese of Portugal.

## 0.48.0

- After an update, the window shows what is new, once.
- The list of computers shows the version of RustInput each one runs, and says when one is older than this one.

## 0.47.0

- Linux: "Start RustInput with the session" in Preferences starts it without its window when you log in.

## 0.46.0

- `rustinput status`, `rustinput go <computer>` and `rustinput home` from a terminal, and `rustinput status --waybar`
  for a module of Waybar.

## 0.45.0

- Search the copies: above the cards of the Clipboards tab, it looks into the history of every computer at once.

## 0.44.0

- The clipboard only when you ask: switch off "Bring the clipboard along with the pointer" in Preferences, then
  AltGr + right Shift + C takes it to the computer the pointer is on, or brings the one of the computer it came from.

## 0.43.0

- The tray icon shows where the mouse and keyboard are (a blue sign while they are on another computer), and its menu
  moves them to any computer.

## 0.42.0

- Keyboard shortcuts: AltGr + right Shift + 1 … 9 goes to that computer, + 0 back here; the right Shift held for
  3 seconds always brings the pointer back. They are in Preferences.

## 0.41.0

- A computer paired with one of yours is offered to the others, which ask once whether to authorize it too.
- Names of other computers can no longer hide characters that turn the text around.

## 0.40.0

- On this network: the Computers tab finds the other computers running RustInput nearby and adds them with one click.
  Off until you switch on "Show this computer to the others", on each computer.

## 0.39.0

- The transfers window says what did not go or come, and why: links to folders, files that changed or went away while
  they were sent (the rest goes on), names not allowed on the other system.

## 0.38.0

- Large copies that come along with the pointer ask first (over 1 GB, in Preferences).
- A transfer that does not fit on the other computer is refused before it starts, with the sizes.

## 0.37.0

- A transfer cut off by the network, a restart or a computer turned off goes on from the byte where it stopped.

## 0.36.0

- "Clipboards" in the radial menu opens the Clipboards tab.

## 0.35.0

- The history of the clipboard of every computer, in memory only: bring back any copy of any computer.

## 0.34.0

- The Clipboards tab shows what every computer copied, and "Copy here" brings it.
