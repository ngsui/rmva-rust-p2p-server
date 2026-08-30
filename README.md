# RMVA Rust P2P Server

> [中文](README.md) | [English](README_EN.md) | [日本語](README_JA.md)

> 为 RPG Maker VX Ace 游戏打造的联机后端 — Rust 全栈实现：异步服务器 / 32 位 Windows 客户端 DLL / 账号与经济系统 / CI/CD / 云端生产部署

![Rust](https://img.shields.io/badge/Rust-stable-dea584?logo=rust)
![tokio](https://img.shields.io/badge/runtime-tokio-41a6c6)
![PostgreSQL](https://img.shields.io/badge/DB-PostgreSQL-336791?logo=postgresql)
![CI](https://img.shields.io/badge/CI-GitHub_Actions-2088ff?logo=githubactions)
![License](https://img.shields.io/badge/License-MIT-green)

把一款 2011 年引擎（RPG Maker VX Ace / RGSS3 / Ruby 1.9.2）的单机 RPG 改造成支持账号体系、拍卖行经济系统与实时联机的网络游戏——网络层全部用 Rust 从零重写，独立完成协议设计、服务端、客户端 DLL、数据库层、CI/CD 与生产部署的全链路。

## 架构

```
RPG Maker VX Ace（RGSS3 / Ruby 1.9.2）
        │  Win32API · stdcall
        ▼
rgss3_rust_net.dll            net-win  · 32 位 cdylib
  ├─ 1 后台网络线程 + 互斥锁双向队列（主线程零阻塞）
  ├─ 长度前缀分帧：粘包 / 半包全部在 DLL 内消化
  ├─ Win32 原生 UI 线程：登录面板 / 拍卖行三页 / 聊天输入条（含 IME）
  └─ 异步延迟探测（TCP 握手 RTT，不占用主连接）
        │  TCP · 双协议自动识别
        │  （新客户端：长度前缀帧 / 旧客户端：换行 JSON）
        ▼
p2p_server                    server   · tokio 异步运行时
  ├─ 房间管理 / 玩家状态 / 战斗指令同步
  ├─ 账号系统：argon2id 密码哈希 + 会话 token
  ├─ 服务器权威拍卖行：事务 + 行锁（防并发双花/负余额）
  ├─ 防作弊：每连接 30 msg/s 限速，连续违规自动踢线
  └─ 数据库连接池（deadpool-postgres，DB 不可用时优雅降级）
        ▼
PostgreSQL
```

## Workspace 布局

| crate | 职责 | 规模 |
|---|---|---|
| [`net-core`](net-core) | 零依赖协议库：长度前缀分帧编解码 | ~100 行 |
| [`net-win`](net-win) | 32 位 Windows DLL（cdylib），供 RGSS3 经 Win32API 调用；手写 Win32 FFI，零第三方依赖 | ~3200 行 |
| [`server`](server) | tokio 异步游戏服务器：房间同步 / 账号 / 拍卖行 / 限速 | ~1600 行 |
| [`test-harness`](test-harness) | 模拟 RGSS3 调用 DLL 的本机测试程序 | ~200 行 |

## 技术亮点

**协议设计**
- 长度前缀帧协议：`net-core` 零依赖实现，读写两侧复用，粘包/半包在 DLL 内消化，上层（Ruby）拿到的永远是完整帧
- 双协议兼容：服务器通过首字节自动识别新客户端（帧协议）与旧客户端（换行 JSON），平滑升级不停服

**安全与防作弊**
- argon2id 密码哈希（随机盐 + 常量时间校验），登录后签发会话 token
- 服务器权威经济：拍卖行的金币与物品全部在服务端事务内结算（`SELECT ... FOR UPDATE` 行锁），客户端无法伪造余额
- 每连接固定窗口限速 30 msg/s，连续违规自动断开，防消息洪泛

**32 位 DLL 与 Win32 FFI**
- 目标环境为 RPG Maker VX Ace / RGSS3（Ruby 1.9.2）+ 32 位 Game.exe：通过 `#[no_mangle] extern "system"` 导出 stdcall 接口
- 全部 Win32 常量 / 结构体 / 消息对照官方头文件手工声明，**不引入 windows-rs 等绑定库**——DLL 仅依赖标准库 + net-core
- 原生 UI（登录 / 拍卖行 / 中文输入）在独立线程创建，消息循环与游戏主线程解耦
- 静态链接 CRT（`+crt-static`），玩家机器无需安装 VC++ 运行库

**工程化**
- GitHub Actions：打 `v*` tag 自动交叉编译 musl 静态二进制并发布 Release，任何 x86_64 Linux 零依赖直跑
- 生产部署：Debian 12 + systemd（开机自启 / 崩溃自动重启 / journald 日志），运行于腾讯云
- 端到端测试（`server/src/test_e2e.rs`，15 个用例覆盖注册/登录/挂单/购买/下架/并发）+ DLL 侧 test-harness

## 快速开始

要求：Rust stable、PostgreSQL。

```bash
# 1. 建库
psql -U postgres -c "CREATE USER rmva WITH PASSWORD 'xxx';"
psql -U postgres -c "CREATE DATABASE rmva_p2p OWNER rmva;"

# 2. 配置连接（表结构启动时自动初始化）
export DATABASE_URL='postgres://rmva:xxx@127.0.0.1:5432/rmva_p2p'

# 3. 运行
cargo run -p server --release
```

DLL（Windows 32 位）：

```bash
rustup target add i686-pc-windows-msvc
cargo build -p net-win --release --target i686-pc-windows-msvc
# 产物：target/i686-pc-windows-msvc/release/rgss3_rust_net.dll
```

## 测试

```bash
# 先以任意 DATABASE_URL 启动服务器，另开终端：
cargo run -p server --bin test_e2e     # 15 个端到端用例（并发购买双花等）
cargo run -p test-harness              # 本机模拟 RGSS3 调 DLL 全流程
```

## 部署（Linux）

```bash
# Release 下载 musl 静态二进制，或自行交叉编译
wget https://github.com/ngsui/rmva-rust-p2p-server/releases/latest/download/p2p_server-linux-x86_64
chmod +x p2p_server-linux-x86_64
```

systemd 单元示例：

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

数据库凭据一律走 `DATABASE_URL` 环境变量，代码与仓库中不含任何凭据。

## License

[MIT](LICENSE)
