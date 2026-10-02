# lkjyolo

ChatGPT に、自分の Coder ワークスペースをできるだけ自律的に使わせるための Rust 製実行基盤。
任意のシェル、対話ターミナル、長時間ビルド、sudo、Docker、認証済み GitHub CLI を、既存の Coder OAuth 接続から使います。
CLI・worker・インストーラー・プラグインの ZIP 作成・テストを Rust で実装し、Python ランタイムに依存しません。

```text
ChatGPT / Codex
  └─ Home Coder MCP（OAuth・アプリ権限・Coder RBAC）
       └─ coder_workspace_bash
            └─ lkjyolo native CLI (Rust)
                 ├─ SQLite: 依頼 ID・実行結果・状態
                 ├─ 独立 worker: 呼び出し元が切れても結果を保存
                 └─ private Unix socket
                      └─ chatgpt-exec-mcp 0.2.1
                           ├─ shell / PTY / stdin / Ctrl-C
                           └─ セッション・省略出力のログ
```

## できること

- 呼び出しが終わってもシェル・REPL・ビルドを維持し、別の呼び出しで続ける。
- 同じ `request_id` と引数の再送は、保存済みの結果を返す。コマンドを二重実行しない。
- Coder 呼び出しが切れても worker が結果を保存し、`result` で取り直せる。
- 多量の出力を短く返し、省略されたログをバイト範囲で回収する。
- コマンド許可リスト、作業ディレクトリによるアクセス制限、追加の承認プロンプトを設けない。
- `sudo -n`、Docker、既存の `gh` ログインなど、ワークスペース利用者に与えられた権限を利用する。
- 新しい公開ポートや別の認証情報を用意せず、既に接続済みの Home Coder を拡張する。

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

同じ `install` コマンドで `~/.local/bin/lkjyolo` を Rust 版 0.2.0 にアトミックに切り替えます。
SQLite のスキーマ、依頼 ID、CLI 引数、結果 JSON、設定ファイル、PID の識別方式、Unix socket を引き継ぎます。
既存の実行エンジンは再起動せず、旧版で開始した対話セッションを Rust 版から継続できます。
Python が保存した JSON の表記差があっても、同じツール・引数の再送は保存済みの結果を返します。
旧版の zipapp と状態データは削除しません。インストール結果の `previous_target` が切替前の配置先です。
必要なら、その既知の配置先へ CLI のシンボリックリンクを戻してロールバックできます。

## ChatGPT からの操作

プラグイン 0.3.0 は「実装して」「バグを直して」「このリポジトリをテストして」などの
開発依頼を自動選択の対象にします。実行環境が未指定なら lkjyolo を使い、指定済みの別環境や
ローカルリポジトリがあればそちらを優先します。説明やコード例だけの依頼では実行しません。
スキルには `allow_implicit_invocation: true` と Home Coder MCP の依存関係を同梱しています。
ChatGPT 側でこのプラグインが有効で、Coder の接続が利用可能である必要があります。
自動選択はモデルの判断なので全依頼での呼び出しを保証する設定ではありません。
反映前の会話や自動選択されなかった場合は `@lkjyolo · Home Coder` を明示して使えます。

`plugins/home-coder` のスキルを使います。最初に本人・ワークスペース一覧を確認し、対象を確定します。
インストール先の既定は `lkjsxc/tomato-ocelot-73` ですが、接続時の実在確認を省略しません。

`coder_workspace_bash` の例:

```json
{
  "workspace": "lkjsxc/tomato-ocelot-73",
  "command": "/home/coder/.local/bin/lkjyolo exec --request-id project-build-001 --cwd /home/coder/workspace/my-project 'npm test'",
  "timeout_ms": 60000
}
```

`lkjyolo` の JSON は Coder の `output` 文字列内に返ります。結果の見方:

| 値 | 意味 |
| --- | --- |
| `status=queued/running` | 依頼処理中。`result <request_id> --wait 45` で取得する |
| `status=done` | MCP 応答を保存済み。コマンド成功とは限らない |
| `result.structuredContent.session_id` | プロセス継続中。`wait` / `input` / `poll` に渡す |
| `result.structuredContent.exit_code` | コマンド終了コード。0 以外も正確に返す |
| `status=error` | 入力・起動・MCP エラー。`result` 内の理由を確認する |
| `status=unknown` | 送信後に結果を確認できなかった。再実行せず履歴・ログ・副作用を調べる |
| `output_ref` | 切り詰められた出力の保存場所・範囲・保存状態 |

新しい作業には新しい依頼 ID を使い、通信が切れた依頼の回収には同じ ID を使います。
同じ ID で異なる引数を送るとエラーになります。入力送信も依頼 ID を指定すれば二重送信を防げます。

```sh
# すぐにセッションを返して、後から待機する
lkjyolo exec --request-id test-001 --yield-ms 100 --cwd /path/to/project 'npm test'
lkjyolo wait --request-id test-wait-001 <session_id> --seconds 35

# 対話シェル。export / cd / REPL の状態が次の呼び出しに残る
lkjyolo start --request-id repl-001 'python3 -q'
lkjyolo call write_stdin --request-id repl-input-001 --json '{"session_id":"<session_id>","chars":"print(6 * 7)\n"}'
lkjyolo interrupt --request-id interrupt-001 <session_id>

# 保存結果・実行履歴・セッションの確認
lkjyolo result test-001 --wait 45
lkjyolo requests --limit 30
lkjyolo sessions
lkjyolo log /absolute/path/from/output_ref --offset 0 --limit 16384

# 複雑なクオートは Coder の専用ファイルツールで JSON を置いて渡す
lkjyolo call exec_command --request-id task-001 --json @/path/to/arguments.json
```

ファイルの通常の読み書きは既存 Coder ファイルツールを使います。`log` は実行エンジンの保存ログ専用です。
CLI は完了した子コマンドの終了コードを JSON に保持します。CLI 自身の終了コード 0 は「応答取得成功」であり、子コマンド成功の判定には `exit_code` を使います。

## 権限と持続性

実行プロセスは Coder の `coder` ユーザーで動きます。必要な処理では `sudo -n ...` をそのまま指定できます。
既存のホーム以下の認証設定は利用できます。親プロセスの全環境変数を暗黙に継承せず、
`HOME/USER/PATH/LANG/TERM` を明示しているため、追加の環境変数はコマンド内、設定、既存の認証ストアで与えます。
シェルは `/bin/bash -c` であり、ログインプロファイルは自動読込しません。

ChatGPT のアプリ権限はプラグイン定義と別です。Home Coder MCP の「すべての操作を許可」を選ぶと、
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

## 配置と運用

| パス | 内容 |
| --- | --- |
| `~/.local/bin/lkjyolo` | バージョン固定のネイティブ実行ファイルへのリンク |
| `~/.local/share/lkjyolo/releases/0.2.0/lkjyolo` | Rust バイナリ |
| `~/.local/share/lkjyolo/engines/0.2.1/` | 実行エンジンとライセンス |
| `~/.local/state/lkjyolo/` | モード 0700 の状態ディレクトリ |
| `settings.json` / `engine.json` | 配置・環境・実行エンジン設定 |
| `requests.sqlite3` | 依頼引数・出力・結果（秘密を含み得るため Git 対象外） |
| `engine.log` / `workers.log` | 診断ログ |
| `outputs/` | 回収可能な省略出力 |
| `mcp.sock` | モード 0600 の Unix socket、公開 TCP listener なし |

依頼を始めるたびに daemon の生存と起動ロックを確認します。
systemd や workspace startup script の変更は不要です。停止する場合は、進行中の処理が終わったことを確認してから、
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
