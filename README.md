# lkjyolo

ChatGPT に、自分の Coder ワークスペースをできるだけ自律的に使わせるための Rust 製実行基盤。
任意のシェル、対話ターミナル、長時間ビルド、sudo、Docker、認証済み GitHub CLI を、既存の Coder OAuth 接続から使います。
CLI・worker・インストーラー・プラグインの ZIP 作成・テストを Rust で実装し、Python ランタイムに依存しません。

```text
ChatGPT / Codex
  └─ lkjyolo MCP: exec_command / write_stdin / read_file / apply_patch / get_result
       └─ Rust gateway + same-origin Coder OAuth
            └─ Coder (workspace auto-start and transport, hidden from the model)
                 └─ Rust CLI / durable worker / SQLite journal
                      └─ chatgpt-exec-mcp 0.2.1 / PTY / persistent output
```

## できること

- 呼び出しが終わってもシェル・REPL・ビルドを維持し、別の呼び出しで続ける。
- 同じ `request_id` と引数の再送は、保存済みの結果を返す。コマンドを二重実行しない。
- Coder 呼び出しが切れても worker が結果を保存し、`result` で取り直せる。
- 多量の出力を短く返し、省略されたログをバイト範囲で回収する。
- コマンド許可リスト、作業ディレクトリによるアクセス制限、追加の承認プロンプトを設けない。
- `sudo -n`、Docker、既存の `gh` ログインなど、ワークスペース利用者に与えられた権限を利用する。
- 同じ Coder OAuth を使う専用 HTTPS エンドポイントを提供し、モデルには開発用の 5 ツールだけを見せる。

## インストール

Linux x86_64。ソースビルドには Rust 1.88 以降と C コンパイラーが必要です。
SQLite をバイナリに組み込み、TLS に rustls を使います。配置後は Python・Rust ツールチェーン・OpenSSL に依存しません。
glibc などの基本 OS ライブラリと、別バイナリとして配置する Rust 製実行エンジンを利用します。

```sh
git clone https://github.com/lkjsxc/lkjyolo.git
cd lkjyolo
cargo build --locked --release
target/release/lkjyolo install --workspace "$HOME/workspace"
~/.local/bin/lkjyolo doctor
~/.local/bin/lkjyolo exec --request-id first-check 'pwd; id; sudo -n id -u'
```

インストーラーは upstream の `v0.2.1` バイナリをダウンロードし、SHA-256
`790fc21733c51373d9967931a834325b2e9f49581e3e9187cf7fb54cb62435f6` を検証します。
`latest` を実行時に追従しません。設定の再実行で既存の稼働プロセスを再起動しません。

ARM Linux は upstream と lkjyolo をソースビルドし、
`target/release/lkjyolo setup --engine /path/to/chatgpt-exec-mcp --workspace /path/to/workspace --sha256 <検証した値>`
で設定できます。このインストール経路は今回の実機検証対象外です。

### Python 版 0.1.0 からの移行

同じ `install` コマンドで `~/.local/bin/lkjyolo` を Rust 版にアトミックに切り替えます。
SQLite のスキーマ、依頼 ID、CLI 引数、結果 JSON、設定ファイル、PID の識別方式、Unix socket を引き継ぎます。
既存の実行エンジンは再起動せず、旧版で開始した対話セッションを Rust 版から継続できます。
Python が保存した JSON の表記差があっても、同じツール・引数の再送は保存済みの結果を返します。
旧版の zipapp と状態データは削除しません。インストール結果の `previous_target` が切替前の配置先です。
必要なら、その既知の配置先へ CLI のシンボリックリンクを戻してロールバックできます。

## ChatGPT からの操作

専用 URL は `https://coder.lkjsxc.com/lkjyolo/mcp`。プラグイン 0.4.1 の表示名は **lkjyolo**。
「実装して」「バグを直して」「テストして」のような実行依頼を自動選択の対象にします。
実行環境が未指定なら既定の Linux 環境を使い、指定済みの環境やローカルプロジェクトを優先します。
説明だけの依頼では実行しません。ホスト側で接続・有効化が必要で、自動選択はモデルの判断です。

AI 向けのツール説明・スキル手順・選択用の説明は英語に統一しています。
画面の説明と利用ガイドは日本語を維持し、スキルには日本語の依頼例も含めています。
応答にはユーザーの言語を使います。

| ツール | 用途 |
| --- | --- |
| `exec_command` | コマンド実行、対話シェル、sudo・Docker・GitHub CLI |
| `write_stdin` | 入力、出力回収、15〜35 秒の待機、Ctrl-C |
| `read_file` | ファイル・ディレクトリ・省略ログの読み取り |
| `apply_patch` | 更新前 SHA-256 を確認した作成・全文置換・削除 |
| `get_result` | 通信断後の結果回収、最近の依頼一覧 |

変更操作とポーリングには一意の `request_id` を付けます。返り値は直接読める JSON で、
Coder の output の中から別の JSON を取り出す必要はありません。

- `queued/running`: `get_result` で依頼の完了を回収する。
- `done`: 応答保存済み。コマンドの成功判定には `exit_code` を使う。
- `session_id`: コマンドは継続中。`write_stdin` で続ける。
- `unknown`: 実行された可能性がある。新しい ID で再実行せず、実際の副作用を確認する。
- `output_ref.path`: `read_file` で省略ログを必要な範囲だけ回収する。

同じ ID と同じ引数の再送は保存結果を返します。別引数の同じ ID は拒否します。
ツール引数は合計 64 KiB、読み取りは最大 256 KiB、編集の SHA-256 は 16 MiB 以下のファイルに対応します。
大きい処理は環境内のコマンドで行えます。ファイル編集は各ファイル単位で atomic rename します。
バッチ全体のトランザクションや、外部エディターとの同時編集の排他は提供しません。

CLI の `exec/start/input/poll/wait/interrupt/result/requests/sessions/log` も引き続き利用できます。

## 権限と持続性

実行プロセスは Coder の `coder` ユーザーで動きます。必要な処理では `sudo -n ...` をそのまま指定できます。
既存のホーム以下の認証設定は利用できます。親プロセスの全環境変数を暗黙に継承せず、
`HOME/USER/PATH/LANG/TERM` を明示しているため、追加の環境変数はコマンド内、設定、既存の認証ストアで与えます。
シェルは `/bin/bash -c` であり、ログインプロファイルは自動読込しません。

ChatGPT のアプリ権限はプラグイン定義と別です。lkjyolo 接続の「すべての操作を許可」を選ぶと、
この接続の読み取り・実行時の確認を抑えられます。モデルやホストが課す規則をプラグインが解除する機能はありません。
任意コマンド実行を読み取り専用と偽るようなメタデータ変更もしません。

コマンドに実行時間の上限は加えません。64 対話セッション、128 継続コマンド、7 日間のセッション無操作期限、
7 日間の省略ログ保持、合計 4 GiB のログ容量を設定しています。上限は `~/.local/state/lkjyolo/engine.json` で変更できます。
設定の反映にはエンジン再起動が必要です。ログ保存限界は `capture_status` / `capture_error` で報告されます。

接続断と Coder の短いツール呼び出しをまたいでプロセスを維持します。
**ワークスペース停止・コンテナ再起動・実行エンジン再起動ではプロセスは継続しません。**
永続ホーム上の依頼履歴と保持対象ログは残ります。次回の実行でエンジンを自動起動し、
前世代の既知セッションへの入力は拒否します。`sessions` は最終観測の記録であり、常時の生存保証ではありません。

SQLite による重複防止はこのインストールの同じ `request_id` に対する保証です。
外部システムのトランザクションまで「exactly once」にするものではありません。
送信済みか判定できない状況は `unknown` として残し、勝手に再送しません。
依頼履歴は自動削除しません。これが重複防止の記録でもあるため、保存量を把握して明示的に整理してください。

## 公開 gateway

配備用設定は `deploy/`、実際の配置と復旧手順は [2026-10-02 の運用記録](docs/deployment-2026-10-02.md) にあります。
`lkjyolo serve --config /absolute/path/config.json` で動作し、HTTP MCP 2025 と 2026-07-28 の discovery に対応します。
Coder backend は同じ OAuth resource の認証を上流 Coder で検証します。`upstream_ip` は DNS の接続先だけを置き換え、TLS の検証は維持します。
独立したローカル検証には `backend=local` と所有者だけが読める `token_file` を使えます。認証なしの実行モードはありません。

## 配置と運用

| パス | 内容 |
| --- | --- |
| `~/.local/bin/lkjyolo` | バージョン固定のネイティブ実行ファイルへのリンク |
| `~/.local/share/lkjyolo/releases/0.3.0/lkjyolo` | Rust バイナリ |
| `~/.local/share/lkjyolo/engines/0.2.1/` | 実行エンジンとライセンス |
| `~/.local/state/lkjyolo/` | モード 0700 の状態ディレクトリ |
| `settings.json` / `engine.json` | 配置・環境・実行エンジン設定 |
| `requests.sqlite3` | 依頼引数・出力・結果（秘密を含み得るため Git 対象外） |
| `engine.log` / `workers.log` | 診断ログ |
| `outputs/` | 回収可能な省略出力 |
| `mcp.sock` | モード 0600 の Unix socket（ワークスペースのエンジンは TCP 非公開） |

依頼を始めるたびに daemon の生存と起動ロックを確認します。
ワークスペースの systemd や startup script の変更は不要です。公開 gateway は別の systemd サービスで動作します。停止する場合は、進行中の処理が終わったことを確認してから、
`doctor` が返す実行エンジン PID に `TERM` を送ってください。削除・停止を通常のセットアップに混ぜません。
アンインストールでは既存 Coder 接続を維持でき、`~/.local/bin/lkjyolo` と専用配置だけを対象にできます。

## 開発・検証

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
LKJYOLO_TEST_ENGINE="$HOME/.local/share/lkjyolo/engines/0.2.1/chatgpt-exec-mcp" \
  cargo test --locked --test engine -- --ignored --test-threads=2
cargo build --locked --release
target/release/lkjyolo package-plugin
```

Linux 実機テストは切断後の回収、同時重複依頼、対話状態、バックグラウンド完了、
Ctrl-C、長い出力の復元、構文エラー、設定再実行でのプロセス維持、旧 Python 版の履歴との互換性を検証します。
統合テストは通常の `cargo test` では `ignored` と表示され、上の `--ignored` 指定で明示的に実行します。
エンジン未指定で統合テストを要求した場合は失敗します。macOS では単体テスト・コンパイル・プラグイン ZIP 作成を確認でき、実行管理は Linux 専用です。
GitHub Actions は同じエンジンのハッシュを検証し、単体・実機テスト、Clippy、リリースビルド、ZIP 作成を実行します。
`Cargo.lock` を保存し、CI と配置用ビルドでは `--locked` を使います。

## 参考・ライセンス

実行エンジンは [CeraCharlesCC/chatgpt-exec-mcp](https://github.com/CeraCharlesCC/chatgpt-exec-mcp)
の `v0.2.1`（Apache-2.0）をそのまま使用します。
このプロジェクトは Rust で Coder 連携、永続リクエスト処理、インストール・プラグインを追加するものです。
upstream の原作者や OpenAI による公式サービスではありません。

- [OpenAI: MCP サーバー実装](https://developers.openai.com/plugins/build/mcp-server)
- [OpenAI: OAuth 認証](https://developers.openai.com/plugins/build/auth)
- [OpenAI: 接続と検証](https://developers.openai.com/plugins/deploy/connect-chatgpt)

ライセンスは Apache-2.0。実行エンジンの NOTICE と THIRD_PARTY_NOTICES はバイナリと一緒に保持します。
