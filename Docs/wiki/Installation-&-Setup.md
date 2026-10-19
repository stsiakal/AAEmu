# Installation & Setup

- Audience: Contributors, players, and testers
- Last verified against: `develop` on October 10, 2026
- Prerequisites: stable Rust (rustup), required AAEmu dependencies/downloads,
  and Docker (Track A) or MySQL 8 (Track B)

AAEmu is written in Rust (it was ported from C#/.NET; no .NET SDK is needed any
more). This page has two setup paths:

1. `Track A (Preferred)`: `cargo xtask dev` local development workflow.
1. `Track B`: Manual setup workflow.

Players who only want to run a server can also use a published release (see
[Running a published build](#running-a-published-build)).

## Track A (Preferred): `cargo xtask dev` workflow

Use this path if you want the fastest contributor onboarding.

### Requirements

1. Install stable Rust with [rustup](https://rustup.rs/).
1. Install Docker (Docker Desktop, or Docker Engine with `docker compose`).
1. Clone [AAEmu](https://github.com/AAEmu/AAEmu) (`develop` branch recommended).
1. Download required files from [Dependencies and Downloads](Dependencies-and-Downloads):
   - `compact.sqlite3`
   - ArcheAge 1.2 client
1. Place `compact.sqlite3` in `rust/crates/aaemu-game/app/Data/`.
1. Set your `game_pak` path.

### Set `game_pak` path

Recommended: put the path in `rust/crates/aaemu-game/app/Config.Local.json`
(`ClientData.Sources`) so it overrides other game config files.

### Launch

```sh
cd rust
cargo xtask dev --release
```

Expected startup sequence:

1. MySQL container starts (`docker compose up -d --wait db`).
1. `aaemu_login` and `aaemu_game` are initialized from `SQL/` on the first start.
1. Login and game servers are built and started (login first).
1. Both servers log to the console; `Ctrl+C` stops them.

Then start the client with `aaemu-launcher` (see
[Starting the client](#starting-the-client)).

For full details, see [Local Development Guide](Local-Development-Guide).

## Track B: Manual setup workflow

Use this path if you do not want to use Docker.

### Manual requirements

1. Install MySQL 8.x.
1. Install stable Rust with [rustup](https://rustup.rs/).
1. Clone [AAEmu](https://github.com/AAEmu/AAEmu) (`develop` branch recommended).
1. Download required files from [Dependencies and Downloads](Dependencies-and-Downloads):
   - `compact.sqlite3`
   - ArcheAge 1.2 client
1. Place `compact.sqlite3` in `rust/crates/aaemu-game/app/Data/`.

### Database setup (manual)

1. Create two schemas in MySQL:
   - `aaemu_login`
   - `aaemu_game`
1. Import:
   - `SQL/aaemu_login.sql`
   - `SQL/aaemu_game.sql`

Later schema changes in `SQL/updates` are applied automatically at startup when
`Connections:AutoApplyUpdates` is `true`.

Do not insert rows into `aaemu_login.game_servers`.
Game server listing is configured via login server configuration
(`GameServers`).

### Login server configuration (manual)

Create or edit `rust/crates/aaemu-login/app/Config.Local.json` and set DB
credentials plus `GameServers`.

Example:

```json
{
  "Connections": {
    "MySQLProvider": {
      "Host": "127.0.0.1",
      "Port": "3306",
      "User": "your_user",
      "Password": "your_password",
      "Database": "aaemu_login"
    }
  },
  "GameServers": [
    {
      "Id": 1,
      "Name": "AAEmu.Game",
      "Host": "127.0.0.1",
      "Port": 1239,
      "Hidden": false
    }
  ]
}
```

### Game server configuration (manual)

Create or edit `rust/crates/aaemu-game/app/Config.Local.json`.
Because `Config.Local.json` is loaded after `Config.json` and
`Configurations/*.json`, it overrides those files.

At minimum, set database and login network values for your machine.

Set `game_pak` source in either:

- `rust/crates/aaemu-game/app/Configurations/ClientData.json`, or
- `rust/crates/aaemu-game/app/Config.Local.json` as an override.

Instead of putting passwords in files you can also use environment variables,
for example `Connections__MySQLProvider__Password=...`.

### Build and run (manual)

1. Build:

```bash
cd rust
cargo build --release
```

1. Start login server: `cargo run --release -p aaemu-login`
   (or `Scripts/start_login.sh`, `Scripts/start AAEmu.Login.bat`).
1. Start game server in a second console: `cargo run --release -p aaemu-game`
   (or `Scripts/start_game.sh`, `Scripts/start AAEmu.Game.bat`).

Start login before game. The servers log to the console; set `RUST_LOG=debug`
for more detail.

## Running a published build

`cargo xtask publish --archive` (from `rust/`) or the archives attached to a
GitHub release contain ready-to-run folders:

```text
aaemu-<version>-<target>/
  AAEmu.Login/   aaemu-login binary, Config.json, ExampleConfig.Local.json
  AAEmu.Game/    aaemu-game binary, Config.json, Configurations/, Data/, ClientData/, Scripts/
  Tools/         aaemu-launcher, world converter, updates-for-transform, test client
  SQL/
```

Put `compact.sqlite3` in `AAEmu.Game/Data/`, create `Config.Local.json` next to
each `Config.json` as described above, and start the binaries from their
folders (login first). Rust does not need to be installed for a published
build.

## Starting the client

The 1.2 client is started by AAEmu's own launcher, `aaemu-launcher`, which
hands it the login ticket (Windows). From a source checkout:

```sh
cd rust
cargo build --release -p aaemu-launcher
cargo run --release -p aaemu-launcher -- --client "C:\ArcheAge\bin32\archeage.exe" --login 127.0.0.1:1237 --user myname
```

In a published build, run `Tools/aaemu-launcher.exe` with the same arguments.

- `--client`: `archeage.exe` in the `bin32` folder of the extracted client.
- `--login`: the login server's public address and port (`1237` by default;
  use the server's LAN/public address when the client runs on another
  machine).
- `--user`: account name. The launcher asks for the password, or use
  `--password PW`, `--password-file FILE` or the `AAEMU_PASSWORD` environment
  variable.

If auto-account creation is enabled (`AutoAccount`; off in the shipped config,
on under `cargo xtask dev`), accounts are created on first login. More options and details: [Client](Client#launcher-aaemu-launcher).
The public AAEmu Launcher also works as an alternative.

## Hosting safely

Before friends or the internet can reach your machine:

- **Expose only three ports**: login public `1237`, game `1239` and stream `1250`.
  Everything else stays on the host (firewall / router: do not forward it).
- **Login internal port `1234`**: the game server registers here with the shared
  `SecretKey`. The shipped login config binds it to `127.0.0.1`; only the registered game
  server (proved by `SecretKey`) is believed, any other peer is dropped. Set a private
  `SecretKey` (same value in the login and game `Config.Local.json`) instead of the
  shipped `test`. If login and game run on different machines, bind `InternalNetwork.Host`
  to the private interface and firewall it to the game host.
- **Web API `1280`**: binds `127.0.0.1`. Every route except `/status` needs the token
  `WebApiNetwork.Token` (set it in `Config.Local.json`) sent as
  `Authorization: Bearer <token>` or `X-Api-Key: <token>`; with no token set those routes are
  refused. Do not bind it to a public address (the token travels in clear text).
- **Accounts**: `AutoAccount` is off in the shipped login config, so unknown names are
  rejected. Turn it on only while your friends register, or create accounts in the
  database. `cargo xtask dev` turns it on for local development.
- **MySQL (3306) and Adminer (8080)**: never forward them. Docker Compose binds MySQL to
  `127.0.0.1` and starts Adminer only with `docker compose --profile tools up`.
- **GM commands**: only seven commands are level 0 (everyone): `help`, `online`,
  `soloparty`, `dungeon` (own dungeon bindings), `collectdominion`, `joinsiege`,
  `leavesiege` (siege participation). Movement, spawn and item commands are level 50 or
  100 (`Configurations/AccessLevels.json`; unlisted commands default to 100). The first
  account and character are created with level 100 (`Account.AccessLevelFirstAccount`);
  lower it for a public server.

## Docker workflow

If you prefer to run the servers themselves in containers, see
[Docker Installation Guide](Docker-Installation-Guide).

## Related

- [Home](Home)
- [Dependencies and Downloads](Dependencies-and-Downloads)
- [Local Development Guide](Local-Development-Guide)
- [Working with the Config.json files and server listings](Working-with-the-Config.json-files-and-server-listings)
- [Mini troubleshoot guide](Mini-troubleshoot-guide)
