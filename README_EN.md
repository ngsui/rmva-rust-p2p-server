# RMVA Rust P2P Server

> [中文](README.md) | [English](README_EN.md) | [日本語](README_JA.md)

> A multiplayer backend for RPG Maker VX Ace games — fully implemented in Rust: async server / 32-bit Windows client DLL / accounts & economy / CI/CD / cloud production deployment

![Rust](https://img.shields.io/badge/Rust-stable-dea584?logo=rust)
![tokio](https://img.shields.io/badge/runtime-tokio-41a6c6)
![PostgreSQL](https://img.shields.io/badge/DB-PostgreSQL-336791?logo=postgresql)
![CI](https://img.shields.io/badge/CI-GitHub_Actions-2088ff?logo=githubactions)
![License](https://img.shields.io/badge/License-MIT-green)

用惯了RMVA，写着玩的。

A single-player RPG built on a 2011 engine (RPG Maker VX Ace / RGSS3 / Ruby 1.9.2), transformed into an online game with an account system, auction-house economy, and real-time multiplayer — the entire network layer rewritten from scratch in Rust, covering protocol design, server, client DLL, database layer, CI/CD, and production deployment end to end.

## Screenshots

**In-game login system** (press L) — native Win32 login panel; accounts are stored in PostgreSQL with argon2id hashing, and a session token is issued upon login:

![Login system](screenshots/游戏登录系统截图.png)

**In-game auction house** (press P) — three-tab trading UI (market / my listings / sell); all gold and item settlement happens inside server-side transactions, so the client cannot forge balances:

![Auction house](screenshots/游戏拍卖行页面截图.png)

**Cloud production deployment** — running as a systemd service on a Tencent Cloud Lighthouse server (Debian 12); journald shows the full chain of player connections, registrations, and auction operations:

![Tencent Cloud console](screenshots/腾讯云的控制台.png)

## Architecture

```
RPG Maker VX Ace (RGSS3 / Ruby 1.9.2)
        │  Win32API · stdcall
        ▼
rgss3_rust_net.dll            net-win  · 32-bit cdylib
  ├─ 1 background network thread + mutex-guarded bidirectional queues (main thread never blocks)
  ├─ Length-prefixed framing: sticky/partial packets fully absorbed inside the DLL
  ├─ Native Win32 UI thread: login panel / 3-page auction house / chat input bar (with IME)
  └─ Async latency probe (TCP handshake RTT, independent of the main connection)
        │  TCP · dual-protocol auto-detection
        │  (new clients: length-prefixed frames / legacy clients: newline-delimited JSON)
        ▼
p2p_server                    server   · tokio async runtime
  ├─ Room management / player state / battle command sync
  ├─ Account system: argon2id password hashing + session tokens
  ├─ Server-authoritative auction house: transactions + row locks (prevents double-spend / negative balance)
  ├─ Anti-cheat: per-connection 30 msg/s rate limit, repeat offenders auto-kicked
  └─ Database connection pool (deadpool-postgres, graceful degradation when DB is down)
        ▼
PostgreSQL
```

## Workspace Layout

| Crate | Role | Size |
|---|---|---|
| [`net-core`](net-core) | Zero-dependency protocol library: length-prefixed frame encoding/decoding | ~100 LOC |
| [`net-win`](net-win) | 32-bit Windows DLL (cdylib) called from RGSS3 via Win32API; hand-written Win32 FFI, zero third-party deps | ~3200 LOC |
| [`server`](server) | tokio async game server: room sync / accounts / auction house / rate limiting | ~1600 LOC |
| [`test-harness`](test-harness) | Local test program that simulates RGSS3 calling the DLL | ~200 LOC |

## Highlights

**Protocol Design**
- Length-prefixed framing: implemented once in the dependency-free `net-core`, reused by both read and write sides; sticky/partial packets are absorbed inside the DLL so the upper layer (Ruby) always receives complete frames
- Dual-protocol compatibility: the server detects new clients (frame protocol) vs legacy clients (newline JSON) from the first byte, enabling zero-downtime upgrades

**Security & Anti-Cheat**
- argon2id password hashing (random salt, constant-time verification), session tokens issued on login
- Server-authoritative economy: auction-house gold and items are settled exclusively inside server-side transactions (`SELECT ... FOR UPDATE` row locks) — clients cannot forge balances
- Fixed-window rate limit of 30 msg/s per connection; repeated violations trigger automatic disconnect, defending against message flooding

**32-bit DLL & Win32 FFI**
- Target environment is RGSS3 (Ruby 1.9) on 32-bit Game.exe: exports stdcall interfaces via `#[no_mangle] extern "system"`
- All Win32 constants / structs / messages hand-declared against official headers — **no windows-rs or other binding crates**; the DLL depends only on std + net-core
- Native UI (login / auction house / CJK input) created on dedicated threads, keeping message loops decoupled from the game's main thread
- Statically linked CRT (`+crt-static`) — players never need the VC++ runtime installed

**Engineering**
- GitHub Actions: pushing a `v*` tag cross-compiles a musl static binary and publishes a Release that runs on any x86_64 Linux with zero dependencies
- Production deployment: Debian 12 + systemd (auto-start on boot / auto-restart on crash / journald logs), running on Tencent Cloud
- End-to-end tests (`server/src/test_e2e.rs`, 15 cases covering register/login/listing/purchase/cancel/concurrency) plus a DLL-side test harness

## Getting Started

**Option 1: Download prebuilt binaries (recommended)**

Grab the compiled artifacts from [Releases](https://github.com/ngsui/rmva-rust-p2p-server/releases) — no Rust toolchain required:

- `p2p_server-linux-x86_64` — Linux server (musl static, runs on any x86_64 distro with zero dependencies)
- `p2p_server-win-x86_64.zip` — Windows server (unzip and run, easiest for local testing)
- `rgss3_rust_net_win32.zip` — 32-bit Windows client DLL (unzip into your game's `System/` folder)

The server needs PostgreSQL. Create the database, then run:

```bash
# 1. Create the database
psql -U postgres -c "CREATE USER rmva WITH PASSWORD 'xxx';"
psql -U postgres -c "CREATE DATABASE rmva_p2p OWNER rmva;"

# 2. Configure the connection (schema is auto-initialized on startup)
export DATABASE_URL='postgres://rmva:xxx@127.0.0.1:5432/rmva_p2p'

# 3. Run
chmod +x p2p_server-linux-x86_64 && ./p2p_server-linux-x86_64
```

**Option 2: Build from source**

Requirements: Rust stable, PostgreSQL.

```bash
# Server
export DATABASE_URL='postgres://rmva:xxx@127.0.0.1:5432/rmva_p2p'
cargo run -p server --release

# DLL (32-bit Windows)
rustup target add i686-pc-windows-msvc
cargo build -p net-win --release --target i686-pc-windows-msvc
# Output: target/i686-pc-windows-msvc/release/rgss3_rust_net.dll
```

## Testing

```bash
# Start the server with any DATABASE_URL first, then in another terminal:
cargo run -p server --bin test_e2e     # 15 end-to-end cases (concurrent purchase double-spend, etc.)
cargo run -p test-harness              # Simulates the full RGSS3 -> DLL flow locally
```

## Deployment (Linux)

```bash
# Download the musl static binary from Releases, or cross-compile it yourself
wget https://github.com/ngsui/rmva-rust-p2p-server/releases/latest/download/p2p_server-linux-x86_64
chmod +x p2p_server-linux-x86_64
```

Example systemd unit:

```ini
[Unit]
Description=RMVA P2P Server
After=network-online.target postgresql.service

[Service]
Environment=DATABASE_URL=postgres://rmva:xxx@127.0.0.1:5432/rmva_p2p
ExecStart=/opt/p2p/p2p_server
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

Database credentials always go through the `DATABASE_URL` environment variable — no credentials exist in the code or the repository.

## License

[MIT](LICENSE)
