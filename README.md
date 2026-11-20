# Harp

Harp is a next generation terminal multiplexer.

Harp supports:
- Managing multiple local and remote sessions seamlessly
- Session persistance
- Arbitrary extensions, which are just programs

## Setup & Usage

### Building and Running

Currently harp is only available by running a development build. Clone the repo
and run:

```sh
cargo run --release
```

By default, running `harp` starts a detached background server (`harp --serve
--bind 127.0.0.1:7420`) if one is not already running, and opens a new scope
with two `zsh` terminal projections tiled left-to-right.

### Client-Server Architecture & Encryption

Harp uses a client-server model so scopes persist across client disconnects and
can be served across multiple machines:

**Start a server explicitly:**

```sh
harp --serve --bind 0.0.0.0:7420
```

**Connect to a local or remote server:**

```sh
harp --connect <host>:<port>
```

Notably, the servers support 2 major features:
- Zero-config encryption: All client-server traffic is encrypted with an
  ephemeral X25519 key exchange and ChaCha20-Poly1305 AEAD framing—no accounts,
  passwords, or manual certificates are required.
- Persistence & cleanup: Detaching from a client leaves all scopes and their
  running processes alive on the server. When reconnecting, Harp prompts you to
  resume an existing scope or create a new one. Once all projections in a scope
  exit, the scope is removed; when no scopes remain, the server exits
  automatically.

## How to Use Harp

Harp is a modal terminal multiplexer. The main modes are "Managing" and
"Interacting". To switch into managing mode, press `Ctrl+Space`. Use the
keybinds in the bottom status bar to manage the projections in your current
scope, switch scopes or suspend the view.

There are 2 main abstractions: scopes and projections.

A scope is a collection of "projections". A scope lives on a local
machine or a remote machine. You can switch scopes within a session, or quit
your client while a scope continues in the background.

Projections are individual applications that live in a scope. They may be a
terminal, an editor, or a custom tool that belongs to a workspace.

### Copy & Paste

Harp has its own clipboard keybindings so it works uniformly across scopes and
remote sessions, and so pasting never triggers the terminal emulator's
"sanitize or not" confirmation dialog:

- `Ctrl+Shift+C` — enter copy mode over the focused pane. `Space` anchors a
  selection, arrows/extended keys move and extend it, and `Enter` or
  `Ctrl+Shift+C` copies the selection (or the whole visible screen if nothing
  is selected) to the OS clipboard. `q`/`Esc` quits without copying,
  `Ctrl+Space` jumps back to managing mode.
- `Ctrl+Shift+V` — paste the OS clipboard into the focused terminal. Content
  is sanitized (escape sequences and control codes are stripped, mirroring
  kitty's "sanitize" paste action) before being injected via bracketed paste.

**kitty requirement.** By default kitty handles `Ctrl+Shift+C` (copy selection)
and `Ctrl+Shift+V` (paste) itself and never sends them to harp. To let harp see
them, unbind them in `~/.config/kitty/kitty.conf`:

```text
map ctrl+shift+c no_op
map ctrl+shift+v no_op
```

**Clipboard access.** Copying prefers the terminal emulator's OSC 52
(write-clipboard) support, which kitty allows by default; if you also want
copies to land on the clipboard when harp runs in a terminal without OSC 52
support, install `wl-copy`/`wl-paste` (Wayland), `xclip` or `xsel` (X11),
`pbcopy`/`pbpaste` (macOS) or the `termux-clipboard-*` tools (Android). Pasting
reads the clipboard through those tools (OSC 52 *reads* would themselves show a
confirmation prompt in kitty, so they are not used).

### `.harp` Configuration

Launcher programs are discovered from `.harp` files in the following order
(closest takes precedence by name):

1. `CWD/.harp` and `.harp` files in ancestor directories walking up to `/`.
2. `~/.config/harp/*.harp` (loaded in sorted filename order).

Each line in a `.harp` file defines a named program (`Name = command` or `Name:
command`, with `#` / `//` comments):

```text
Cargo Test = ./test.ysh
ZSH = zsh
Neovim = nvim
lazygit = lazygit
```

Relative paths containing `/` (such as `./test.ysh`) are resolved relative to
the directory containing that `.harp` file, while bare command names are
resolved via `PATH`.

## License

See [`LICENSE`](./LICENSE).
