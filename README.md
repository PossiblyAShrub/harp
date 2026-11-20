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

### Scrollback

Terminal panes keep up to 1000 rows of scrollback on the main screen. Scroll a
pane with the mouse wheel, or page the focused pane with `PgUp`/`PgDn`; typing
or clicking returns the view to the bottom. Programs on their alternate screen
(`vim`, `less`, `htop`, ...) get wheel and `PgUp`/`PgDn` events passed through
instead, and scroll themselves. Scroll back in a pane and hit `Ctrl+Shift+C`
to select and copy text from history.

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
