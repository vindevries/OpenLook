# OpenLook

An Outlook-style **native** mail client for Ubuntu (and other Linux distros),
written in Rust with GTK4 + libadwaita — the same toolkit as Ubuntu's own
GNOME apps. It connects to a Microsoft 365 mailbox through the Microsoft
Graph API and keeps a local copy so mail is readable **offline**.

![Three-pane layout: folders, message list, reading pane](docs/screenshot.png)

## Features

- Classic Outlook three-pane layout: folder pane, message list, reading pane,
  with draggable dividers whose widths are remembered, a ribbon-style
  command bar and a status bar
- **Folder tree**: folders inside folders, expanded from the pane, each
  remembering where it sits
- **Several mailboxes at once** — each gets its own section in the folder
  pane, its own cache and its own sync, with a shared Favorites section on top
- Message list grouped by date (Today / Yesterday / weekday / Last Week), an
  All / Unread filter and a date sort toggle
- **Conversations**: a thread is one row showing how many messages it holds,
  with an arrow that expands it in place into its individual messages;
  opening the row reads the whole exchange in order; delete, archive and
  mark-read act on the conversation. Toggle it off to see single messages
- Reading pane with contact initials, Reply, Reply all and Forward, and a
  mark on the messages that have already been replied to or forwarded
- **Attachments**: what a message carries is listed under its header and
  opens with whatever the desktop uses for that kind of file. The bytes are
  fetched on the first open and kept, so an attachment opened once opens
  again with no network. Pictures the message draws are shown in the body
  rather than listed as files
- **Compose in the message itself**: a reply or forward opens with the
  original quoted beneath the cursor, exactly as it will be sent. A forward
  keeps the original's own formatting — pictures, tables, layout — and
  carries its attachments along
- Right-click a message for what to do with it; archiving or deleting the
  open message lands on the next one down, the way Outlook does
- Drag a message onto a folder to move it (within the same mailbox)
- **Connectors**: a pane beside Mail and Calendar for things that are not
  mail but read like it — a helpdesk queue, an issue tracker. Each connector
  is a separate program OpenLook talks to; a HubSpot tickets plugin ships
  with it (see [Connectors](#connectors))
- **Calendar**: a month view switched from the rail on the left, merging
  every mailbox's events (colour-coded), with a day panel showing times,
  location and organiser; double-click an appointment to open it. Cached
  like mail, so it reads offline
- **Works offline**: mail is cached in SQLite and message bodies are
  prefetched, so the app opens instantly and stays readable with no network
- **Changes made offline are queued** — read/unread, delete and messages you
  write are applied locally at once and pushed to the server when you
  reconnect (they survive quitting the app)
- Incremental sync using Graph delta queries — the first enumeration is
  bounded to a recent window so a token arrives in one request, after which
  each poll is a single cheap call; local edits are never clobbered by a
  stale sync
- **A desktop notice when mail arrives**, which goes by itself; turn it off
  in Settings
- Read HTML mail (WebKitGTK, JavaScript disabled; links open in your browser)
- Compose, send, reply; mark read/unread; delete; per-folder search
- **Demo mailbox** out of the box, so the whole UI works before you sign in
- Microsoft 365 sign-in with **no app registration needed** (see below);
  tokens are cached in `~/.config/openlook/` (0600) and refreshed silently

## Requirements

A GTK4 desktop and Rust. Ubuntu 22.04 is the oldest supported release: it has
everything at runtime (GTK 4.6, libadwaita 1.1, and WebKitGTK 6.0 from
jammy-updates), as does anything newer.

```bash
# Rust, if you don't have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Recommended: the GTK development files (needs root)
sudo apt install libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev libgraphene-1.0-dev
```

If you can't install those `-dev` packages, the build falls back to
`tools/dev-shim.sh`, which generates pkg-config metadata and linker symlinks
for the GTK/WebKit libraries already installed on the system. The resulting
binary is completely normal — the shim only affects build time.

## Build, run, install

```bash
cargo run                 # run from the repo
cargo test                # offline-behaviour tests (no display needed)
./install.sh              # build release + install launcher and icon

./tools/mkdeb.sh                  # .deb for this machine
./tools/mkdeb.sh --target jammy   # .deb for Ubuntu 22.04, from any newer host
```

`install.sh` sources the shim automatically when the `-dev` packages are
missing. The `.deb` also installs the HubSpot plugin (`openlook-hubspot`)
and its manifest under `/usr/share/openlook/plugins`.

## Signing in

OpenLook holds any number of mailboxes. Add the first one when it asks on
first start, and further ones from the menu → **Add mailbox…**; each appears
as its own section in the folder pane and syncs independently. Menu →
**Remove this mailbox** drops the one whose folder is selected.

On first start OpenLook asks you to add your account, like Outlook does.
Click **Sign in**, enter the short code in your browser, and log in as usual —
your password and two-factor prompt stay with Microsoft. You can also skip and
use the demo mailbox.

**No Azure app registration is required.** OpenLook signs in as Microsoft's
public "Microsoft Graph Command Line Tools" application
(`14d82eec-204b-4c2f-b7e8-296a70dab67e`), the same public client Microsoft's
own Graph PowerShell uses, requesting delegated `User.Read`, `Mail.ReadWrite`
`Mail.Send` and `Calendars.Read` scopes. The first sign-in shows a consent prompt for those
permissions.

If your organization blocks that app, register your own (2 minutes) and paste
its client ID under **Advanced** in the sign-in dialog, or in Settings:

1. <https://portal.azure.com> → Microsoft Entra ID → **App registrations** →
   **New registration**. No redirect URI needed.
2. **Authentication** → **Allow public client flows** → **Yes** → Save.
3. **API permissions** → Microsoft Graph → Delegated: `User.Read`,
   `Mail.ReadWrite`, `Mail.Send`, `Calendars.Read` (grant admin consent if
   required).

## Connectors

The third pane on the rail is for sources that are not mail but read like it:
something with sections to list, items in them, and a body to read. Nothing
about any particular service is compiled into OpenLook — a connector is a
**plugin**, a separate program OpenLook starts and talks to over its standard
input and output, one JSON object per line. A plugin can be written in any
language, updated on its own, and a crash or a hang in it costs a pane rather
than the mail client.

Open the pane and press **Add mailbox…** to pick an installed plugin, give it
whatever it needs to connect, and choose which of its queues to show as
folders. Items are cached and read offline exactly like mail.

Plugins are looked for in, nearest first:

```
~/.config/openlook/plugins/       your own
/usr/share/openlook/plugins/      installed by a package
/usr/local/share/openlook/plugins/
```

The credential a plugin was set up with is kept outside `settings.json`, in
`~/.config/openlook/plugins/<id>/credential` (0600).

Each is a directory holding the program and a `plugin.json` naming it —
`data/plugins/hubspot/plugin.json` is the one that ships. A plugin sitting
beside the running binary is found too, which is what makes one usable
straight from a build directory.

`src/bin/openlook-hubspot.rs` is the bundled example: HubSpot tickets, in a
little over a hundred lines. The protocol it answers is documented at the top
of `src/plugin.rs`.

## How offline works

The UI never talks to the network. It reads only from the local database,
and a background engine reconciles that database with the server:

```
  UI  ──reads──>  SQLite cache  <──writes──  sync engine  <──HTTPS──>  Graph
   └──actions────────┘  (applied at once)         └── outbox replayed when online
```

- **Reading**: folders, message lists and prefetched bodies come from
  `~/.local/share/openlook/<account>.db`. With no network you still see
  everything that has been synced; a message whose body was never downloaded
  says so instead of failing. Attachments already opened once are kept under
  `~/.local/share/openlook/attachments/`.
- **Writing**: every change is applied to the cache immediately, then queued
  in an `outbox` table. The engine drains that queue whenever it can reach
  the server, and retries what it can't. A message composed offline appears
  in Sent Items marked *Queued* until it goes out.
- **Safety**: messages with queued changes are skipped when a sync would
  otherwise overwrite them, so a sync in flight can't undo what you just did.
- **Nothing waits on the network**: filing a message takes its row out of the
  list at once, and the reports a sync makes gather into one redraw, so the
  window keeps answering clicks while a mailbox is enumerating.
- The header bar shows the state: *Syncing…*, *Offline — showing cached
  mail*, *N waiting to sync*, or *Updated 5 min ago*.

## Layout

```
src/
  model.rs      shared types
  db.rs         SQLite cache + outbox (the offline core)
  sync.rs       background engine: delta sync, body prefetch, queue replay
  graph.rs      Microsoft Graph client, with errors classified for retry
  auth.rs       device-code OAuth + token cache
  connector.rs  what a source other than mail has to provide
  plugin.rs     connectors living outside the app, over stdin/stdout
  hubspot.rs    HubSpot REST client, used by the plugin
  demo.rs       demo mailbox seeded into the cache
  ui/           window, compose, calendar, dialogs
  bin/openlook-hubspot.rs   the HubSpot plugin
tests/offline.rs  offline behaviour tests
tools/dev-shim.sh build without the GTK -dev packages
tools/mkdeb.sh    package a .deb, optionally for Ubuntu 22.04
```

## Roadmap

- Attaching files to a message you write (a forward already carries the
  original's)
- Formatting toolbar for the message you write (a forward keeps the
  original's formatting)
- Creating and editing appointments (the calendar is read-only today)
- Full-text search across folders (the cache makes this cheap)
