# RMVA Rust P2P Server

> [中文](README.md) | [English](README_EN.md) | [日本語](README_JA.md)

> RPG Maker VX Ace 向けマルチプレイバックエンド — Rust 全自作：非同期サーバー / 32ビット Windows クライアント DLL / アカウント&エコノミー / CI/CD / クラウド本番運用

![Rust](https://img.shields.io/badge/Rust-stable-dea584?logo=rust)
![tokio](https://img.shields.io/badge/runtime-tokio-41a6c6)
![PostgreSQL](https://img.shields.io/badge/DB-PostgreSQL-336791?logo=postgresql)
![CI](https://img.shields.io/badge/CI-GitHub_Actions-2088ff?logo=githubactions)
![License](https://img.shields.io/badge/License-MIT-green)

用惯了RMVA，写着玩的。

2011年製エンジン（RPG Maker VX Ace / RGSS3 / Ruby 1.9.2）のシングルプレイRPGを、アカウントシステム・オークションハウス経済・リアルタイムマルチプレイ対応のオンラインゲームへと改造。ネットワーク層をRustでゼロから書き直し、プロトコル設計・サーバー・クライアントDLL・DB層・CI/CD・本番デプロイまで全リンクを一人で完結させた。

## アーキテクチャ

```
RPG Maker VX Ace (RGSS3 / Ruby 1.9.2)
        │  Win32API · stdcall
        ▼
rgss3_rust_net.dll            net-win  · 32ビット cdylib
  ├─ バックグラウンド通信スレッド1本 + ミューテックス付き双方向キュー（メインスレッドは一切ブロックしない）
  ├─ 長さプレフィックスフレーミング：パケットの分割・結合をDLL内部で完全に吸収
  ├─ Win32 ネイティブUIスレッド：ログインパネル / 3ページ構成オークションハウス / チャット入力バー（IME対応）
  └─ 非同期レイテンシ測定（TCPハンドシェイクRTT、メイン接続に非依存）
        │  TCP · デュアルプロトコル自動判定
        │  （新クライアント：長さプレフィックスフレーム / 旧クライアント：改行区切りJSON）
        ▼
p2p_server                    server   · tokio 非同期ランタイム
  ├─ ルーム管理 / プレイヤー状態 / 戦闘コマンド同期
  ├─ アカウントシステム：argon2id パスワードハッシュ + セッショントークン
  ├─ サーバー権威型オークションハウス：トランザクション + 行ロック（二重消費・残高マイナスを防止）
  ├─ チート対策：接続ごとに 30 msg/s のレート制限、常習違反は自動キック
  └─ DB接続プール（deadpool-postgres、DBダウン時はグレースフル劣化）
        ▼
PostgreSQL
```

## Workspace 構成

| Crate | 役割 | 規模 |
|---|---|---|
| [`net-core`](net-core) | 依存ゼロのプロトコルライブラリ：長さプレフィックスフレームの符号化/復号 | 約100行 |
| [`net-win`](net-win) | 32ビット Windows DLL（cdylib）。RGSS3からWin32API経由で呼ばれる。Win32 FFIは手書き、サードパーティ依存ゼロ | 約3200行 |
| [`server`](server) | tokio 非同期ゲームサーバー：ルーム同期 / アカウント / オークション / レート制限 | 約1600行 |
| [`test-harness`](test-harness) | RGSS3のDLL呼び出しをシミュレートするローカルテストプログラム | 約200行 |

## 技術ハイライト

**プロトコル設計**
- 長さプレフィックスフレーミング：依存ゼロの `net-core` に一度だけ実装し、送受信両側で再利用。パケットの分割・結合はDLL内で吸収され、上位層（Ruby）は常に完全なフレームのみを受け取る
- デュアルプロトコル互換：サーバーが先頭バイトで新クライアント（フレーム）と旧クライアント（改行JSON）を自動判別し、ダウンタイムゼロでアップグレード可能

**セキュリティ & チート対策**
- argon2id パスワードハッシュ（ランダムソルト + 定数時間検証）、ログイン後にセッショントークン発行
- サーバー権威型経済：オークションの所持金とアイテムはサーバー側トランザクション（`SELECT ... FOR UPDATE` 行ロック）内でのみ決済 — クライアントは残高を偽造できない
- 接続ごとの固定ウィンドウ・レート制限 30 msg/s。反復違反で自動切断し、メッセージ洪水を防御

**32ビット DLL & Win32 FFI**
- ターゲット環境はRGSS3（Ruby 1.9）+ 32ビット Game.exe：`#[no_mangle] extern "system"` でstdcallインターフェースをエクスポート
- Win32定数 / 構造体 / メッセージはすべて公式ヘッダーと照合して手書き宣言 — **windows-rs 等のバインディングクレート不使用**。DLLが依存するのは std + net-core のみ
- ネイティブUI（ログイン / オークション / 日本語入力）は専用スレッドで生成し、メッセージループをゲームのメインスレッドから分離
- CRT静的リンク（`+crt-static`）— プレイヤー環境にVC++ランタイムのインストールが不要

**エンジニアリング**
- GitHub Actions：`v*` タグのpushでmusl静的バイナリをクロスコンパイルしRelease公開。任意のx86_64 Linuxで依存ゼロ実行
- 本番運用：Debian 12 + systemd（起動時自動開始 / クラッシュ時自動再起動 / journaldログ）、Tencent Cloud上で稼働
- エンドツーエンドテスト（`server/src/test_e2e.rs`、登録/ログイン/出品/購入/キャンセル/同時実行をカバーする15ケース）+ DLL側テストハーネス

## 始め方

**方法1：ビルド済みバイナリをダウンロード（推奨）**

[Releases](https://github.com/ngsui/rmva-rust-p2p-server/releases) からコンパイル済みの成果物をダウンロード。Rust環境は不要：

- `p2p_server-linux-x86_64` — Linuxサーバー（musl静的リンク、任意のx86_64ディストリで依存ゼロ実行）
- `p2p_server-win-x86_64.zip` — Windowsサーバー（解凍してすぐ実行、ローカルテストに最適）
- `rgss3_rust_net_win32.zip` — 32ビット WindowsクライアントDLL（解凍してゲームの `System/` フォルダへ）

サーバーにはPostgreSQLが必要。DBを作成して実行：

```bash
# 1. データベース作成
psql -U postgres -c "CREATE USER rmva WITH PASSWORD 'xxx';"
psql -U postgres -c "CREATE DATABASE rmva_p2p OWNER rmva;"

# 2. 接続設定（スキーマは起動時に自動初期化）
export DATABASE_URL='postgres://rmva:xxx@127.0.0.1:5432/rmva_p2p'

# 3. 実行
chmod +x p2p_server-linux-x86_64 && ./p2p_server-linux-x86_64
```

**方法2：ソースからビルド**

必要環境：Rust stable、PostgreSQL。

```bash
# サーバー
export DATABASE_URL='postgres://rmva:xxx@127.0.0.1:5432/rmva_p2p'
cargo run -p server --release

# DLL（32ビット Windows）
rustup target add i686-pc-windows-msvc
cargo build -p net-win --release --target i686-pc-windows-msvc
# 成果物：target/i686-pc-windows-msvc/release/rgss3_rust_net.dll
```

## テスト

```bash
# 任意のDATABASE_URLでサーバーを起動してから、別ターミナルで：
cargo run -p server --bin test_e2e     # 15個のE2Eケース（同時購入の二重消費など）
cargo run -p test-harness              # RGSS3 -> DLL の全フローをローカルシミュレート
```

## デプロイ（Linux）

```bash
# Releaseからmusl静的バイナリをダウンロード、または自前クロスコンパイル
wget https://github.com/ngsui/rmva-rust-p2p-server/releases/latest/download/p2p_server-linux-x86_64
chmod +x p2p_server-linux-x86_64
```

systemdユニット例：

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

DBクレデンシャルは常に `DATABASE_URL` 環境変数経由 — コードとリポジトリに認証情報は存在しない。

## License

[MIT](LICENSE)
