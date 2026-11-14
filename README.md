# lkjyolo

ChatGPT に、自分の Coder ワークスペースをできるだけ自律的に使わせるための Rust 製実行基盤。
任意のシェル、対話ターミナル、長時間ビルド、sudo、Docker、認証済み GitHub CLI を、既存の Coder OAuth 接続から使います。
CLI・worker・インストーラー・プラグインの ZIP 作成・テストを Rust で実装し、Python ランタイムに依存しません。

```text
ChatGPT / Codex
  └─ lkjyolo MCP: exec_command / write_stdin / list_sessions / read_file / apply_patch / get_result
       └─ Rust gateway + same-origin Coder OAuth
            └─ Coder (workspace auto-start and transport, hidden from the model)
                 └─ Rust CLI / durable worker / SQLite journal
                      ├─ native Rust pipes / separate stdout + stderr / real process status
                      └─ chatgpt-exec-mcp 0.2.1 / interactive PTY / merged output
```

## できること

- 呼び出しが終わってもシェル・REPL・ビルドを維持し、別の呼び出しで続ける。
- 同じ `request_id` と引数の再送は、保存済みの結果を返す。コマンドを二重実行しない。
- Coder 呼び出しが切れても worker が結果を保存し、`result` で取り直せる。
- 通常のコマンドは stdout / stderr を分離し、終了コード・シグナル・実行時間を返す。
- 多量の出力を短く返し、保存上限内の省略ログをストリーム別のバイト範囲で回収する。
- コマンド許可リスト、作業ディレクトリによるアクセス制限、追加の承認プロンプトを設けない。
- `sudo -n`、Docker、既存の `gh` ログインなど、ワークスペース利用者に与えられた権限を利用する。
- 同じ Coder OAuth を使う専用 HTTPS エンドポイントを提供し、モデルには開発用の 6 ツールだけを見せる。

## 他の人に使ってもらう

[利用開始ガイド](docs/getting-started.md)にアカウント申請、各自の環境作成、OAuth接続をまとめています。
0.4.0のgatewayは認証ユーザーごとに所有ワークスペースを選び、実行前に所有者IDを検証します。
配布用ZIPは `target/release/lkjyolo package-plugin --portable` で作成でき、個人用App IDを含みません。
同じオプションに `--endpoint https://your-coder.example/lkjyolo/mcp` を付けると別の配備先に対応します。

## インストール

Linux x86_64。ソースビルドには Rust 1.88 以降と C コンパイラーが必要です。
SQLite をバイナリに組み込み、TLS に rustls を使います。配置後は Python・Rust ツールチェーン・OpenSSL に依存しません。
glibc などの基本 OS ライブラリと、対話端末には別バイナリとして配置する Rust 製実行エンジンを利用します。通常のコマンドは lkjyolo 自身が実行します。

```sh
git clone https://github.com/lkjsxc/lkjyolo.git
cd lkjyolo
cargo build --locked --release -j 4
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

## ChatGPT からの操作

> 0.4.x 以降の方針: lkjyolo は「自分のシェルを持たないクライアント（ChatGPT のチャット等）向け」です。
> Codex など、すでにこのワークスペース内で動くエージェントには不要で、プラグインの skill と MCP 説明もそう明記しています。
> `request_id` は任意になりました（省略するとサーバーが採番して結果に返します）。再送で回復したい呼び出しだけ自分で指定します。
> `exec_command` の結果には、サービス操作・本番反映・破壊的 git・秘密情報ファイルらしいコマンドに対して非ブロックの `warnings` を付けます。
> 旧形式（`request_id` 必須を前提とした説明、「実行環境が未指定なら既定で使う」という自動選択文言）との互換は維持しません。

専用 URL は `https://coder.lkjsxc.com/lkjyolo/mcp`。プラグイン 0.6.1 の内部名・表示名は **lkjyolo**。
シェルを持たないクライアントからの「実装して」「バグを直して」「テストして」のような実行依頼を対象にします。
ローカルのシェルやチェックアウトを持つエージェントは、それを優先して lkjyolo を使いません。
説明だけの依頼では実行しません。ホスト側で接続・有効化が必要で、自動選択はモデルの判断です。

AI 向けのツール説明・スキル手順・選択用の説明は英語に統一しています。
画面の説明と利用ガイドは日本語を維持し、スキルには日本語の依頼例も含めています。
応答にはユーザーの言語を使います。

ChatGPT では専用の **lkjyolo MCP** 接続をプラグインに紐付けます。
ソースは `plugins/lkjyolo`、配布ZIPは `dist/lkjyolo-0.6.1.zip` です。
既存の `lkjyolo MCP` 接続と認証を引き継ぎます。旧 `home-coder` は名前の異なるプラグインなので、
新しいlkjyoloを有効化して6つのツールを確認してから、旧プラグインと旧 **Home Coder MCP** 接続を削除します。
Coder サーバーとワークスペースは引き続き実行基盤として使用します。

| ツール | 用途 |
| --- | --- |
| `exec_command` | コマンド実行、対話シェル、sudo・Docker・GitHub CLI |
| `write_stdin` | 入力、出力回収、15〜35 秒の待機、Ctrl-C |
| `read_file` | ファイル・ディレクトリ・省略ログの読み取り |
| `apply_patch` | 更新前 SHA-256 を確認した作成・全文置換・削除 |
| `list_sessions` | 開始済みセッションの一覧（コマンド・作業ディレクトリ・最後に観測した状態）。`session_id` を失った後の再発見用 |
| `get_result` | 通信断後の結果回収、最近の依頼一覧 |

変更操作とポーリングには一意の `request_id` を付けます。返り値は直接読める JSON で、
Coder の output の中から別の JSON を取り出す必要はありません。

- `queued/running`: `get_result` で依頼の完了を回収する。
- `done`: 応答保存済み。通常コマンドの成功は `finished=true`, `exit_code=0`, `signal=null` で判定する。
- `session_id`: コマンドは継続中。`write_stdin` で続ける。
- `unknown`: 実行された可能性がある。新しい ID で再実行せず、実際の副作用を確認する。
- `stdout_ref` / `stderr_ref`: `path` と `offset` から `end_offset` までを `read_file` で回収する。対話端末は `output_ref.path` を使う。

通常の `exec_command`（`tty=false`）は、標準入力を閉じたネイティブの pipe 実行です。
`stdout`, `stderr`, `exit_code`, `signal`, `duration_ms`, `finished`, `truncated`,
`stdout_truncated`, `stderr_truncated` を返します。シグナル終了では `exit_code=null`、
`signal` は Linux のシグナル番号です。実行中は両方 null です。stdout と stderr 間の
インターリーブ順序は保証しません。`duration_ms` は起動からの単調時計による経過時間で、
子孫が pipe を保持する場合はその出力の完了まで含みます。

`max_output_tokens` は 1 token = 4 bytes の概算予算で、半分ずつ各ストリームに割り当てます。
保存された出力は先頭 1 MiB / stream までで、超過分は破棄して `*_capture_truncated=true` と
`*_ref.total_bytes` に示します。応答だけで省略された部分は `*_ref.path` の `offset` から
`end_offset` まで回収できます。poll は新しく保存された部分を返すため、前回応答の省略部分は
その参照から読んでください。UTF-8 として不正なバイトを含む応答には `*_base64` と
`*_encoding_loss=true` が付きます。生ログと base64 は元のバイトを保持します。

`timeout_ms` は任意（1 ms〜7日、既定の期限なし）で、通常コマンドのプロセスグループに
TERM、250 ms 後も続いていれば KILL を送ります。`write_stdin(chars="\u0003")` による
キャンセルは同じグループに INT、必要なら KILL を送ります。`timed_out` / `cancelled` と
実際の終了ステータスを確認してください。通常の pipe への文字入力は拒否します。
端末入力や REPL が必要なら最初から `tty=true` を使います。

`tty=true` は従来の固定エンジンを利用し、`stream_mode=engine_merged` と
`output` / `output_ref` を返します。PTY の stdout / stderr は分離できないので、
`stdout`, `stderr`, `signal`, `duration_ms` は null です。エンジンの `exit_code` は
シェル形式（シグナルは 128 + 番号）であり、明示的な同じコードとの区別はできません。
端末は入力のエコー、CRLF、ANSI 制御列、端末向けのバッファリングを伴うことがあります。
選択理由と調査結果は [実行バックエンドの設計](docs/execution-backends.md) を参照してください。

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

既定の実行時間制限はありません。ネイティブ pipe 実行は同時 512 件で、終了時に枠を解放し、
未回収の最終出力が枠を占有しません。ログは各 stream 1 MiB、合計 4 GiB 以内です。
新規実行の受付時に7日超の終了済みログ、および容量確保に必要な最古の終了済みログを削除します。
実行中のログは削除しません。期限切れの参照は `available=false`, `capture_status=expired` で示します。
この上限は現在ネイティブ実装の定数です。依頼履歴とセッションのメタデータは削除しません。

対話エンジンは64対話セッション、512継続コマンド、7日間の無操作期限、7日間のログ保持、
別枠で合計4 GiBのログ容量を設定しています。こちらは `engine.json` で変更できます。
設定の反映には対話エンジン再起動が必要です。稼働中の設定は通常作業で変更しません。
保存エラーは `capture_status` / `capture_error`、容量による破棄は `*_capture_truncated` で報告します。

対話エンジンについては、0.3.1 以降、終了済みの最終結果が未回収のまま枠を占有した場合、上限エラーを受けた worker が
既知セッションを空入力で回収し、枠が解放できたときだけ元のコマンドを一度再試行します。
実行中と分かるプロセスと、別の入力・待機処理が操作中のセッションは回収対象から外します。
終了判定にはエンジンの応答を使い、Ctrl-C、プロセス停止、エンジン再起動は行いません。
実行中の処理だけで上限に達している場合は、従来どおり上限エラーを返します。

自動回収した出力・終了コードは SQLite に保存され、元の `session_id` を空入力でポーリングすると
受け取れます。途中の出力を回収した場合も順序を保持し、入力前に空入力での回収を案内します。
省略された出力は `output_ref` から通常の保持期限内に取得できます。
回収の履歴は `auto-collect-...` の依頼として残り、通信結果が不明な回収を自動再送しません。
新しいセッションは `lkj-...` の不透明な ID を返します。エンジン内部の ID が再利用されても、
回収済みセッションへの入力が新しい処理へ転送されることはありません。既存セッションの ID も継続利用できます。

接続断と Coder の短いツール呼び出しをまたいでプロセスを維持します。
**ワークスペース停止・コンテナ再起動ではプロセスは継続しません。**
対話エンジンの再起動は対話セッションを失いますが、独立したネイティブ worker のコマンドには影響しません。
ネイティブ worker が消失した場合は `unknown` / `capture_status=supervisor_lost` とし、
プロセスが残る可能性があるため再実行前に副作用を確認します。
永続ホーム上の依頼履歴と保持対象ログは残ります。次回の対話実行でエンジンを自動起動し、
前世代の既知セッションへの入力は拒否します。`sessions` は最終観測の記録であり、常時の生存保証ではありません。

SQLite による重複防止はこのインストールの同じ `request_id` に対する保証です。
外部システムのトランザクションまで「exactly once」にするものではありません。
送信済みか判定できない状況は `unknown` として残し、勝手に再送しません。
依頼履歴は自動削除しません。これが重複防止の記録でもあるため、保存量を把握して明示的に整理してください。

## 障害の切り分け（実行基盤とは別の診断ツール）

安全性拒否、OAuth/権限エラー、通信障害、ツールエラー、コマンドの非ゼロ終了を
混同しないため、読み取り専用の `lkjyolo-diagnostics` を用意しています。

`cargo build --locked --release -j 4 --bin lkjyolo-diagnostics` で単独ビルドできます。
既存の `lkjyolo` CLI や実行エンジンを更新・再起動する必要はありません。

- `lkjyolo-diagnostics --limit 100`: 直近の最大100件を集計（最大指定1000件）。
- `lkjyolo-diagnostics --request-id <元のID>`: その依頼の保存状態だけを確認。
- `--state <path>` または `LKJYOLO_STATE` で別の既存ジャーナルを指定。

DBは読み取り専用で開き、スキーマ初期化・要求の状態修復・再実行・セッションの出力回収は
しません。引数は取得せず、保存結果は内部で分類に必要な範囲だけ解析し、本文・エラー文字列・
出力は返しません。1 MiB超の保存結果は解析せず、要確認として扱います。
SQLiteがWAL読み取りのために使用する共有メモリ等の通常の補助処理はあり得ます。

ジャーナルに記録がないことは「未実行」や「OpenAIが拒否した」ことを証明しません。
プラットフォーム側の判定は取得できず、集計から安全性ブロック率も計算できません。
プラグイン0.5.1はこの区別と復旧手順を明記しますが、ホストの安全性判定を解除するものではありません。
詳細は [障害切り分けの運用記録](docs/failure-attribution-2026-10-08.md) を参照してください。

## 公開 gateway

配備用設定は `deploy/`、実際の配置と復旧手順は [2026-10-02 の運用記録](docs/deployment-2026-10-02.md) にあります。
`lkjyolo serve --config /absolute/path/config.json` で動作し、HTTP MCP 2025 と 2026-07-28 の discovery に対応します。
Coder backend は同じ OAuth resource の認証を上流 Coder で検証します。`upstream_ip` は DNS の接続先だけを置き換え、TLS の検証は維持します。
独立したローカル検証には `backend=local` と所有者だけが読める `token_file` を使えます。認証なしの実行モードはありません。

## 配置と運用

| パス | 内容 |
| --- | --- |
| `~/.local/bin/lkjyolo` | バージョン固定のネイティブ実行ファイルへのリンク |
| `~/.local/share/lkjyolo/releases/0.3.3/lkjyolo` | Rust バイナリ |
| `~/.local/share/lkjyolo/engines/0.2.1/` | 実行エンジンとライセンス |
| `~/.local/state/lkjyolo/` | モード 0700 の状態ディレクトリ |
| `settings.json` / `engine.json` | 配置・環境・実行エンジン設定 |
| `requests.sqlite3` | 依頼引数・出力・結果、ネイティブ実行状態・配信カーソルと対話セッション対応表・自動回収出力（秘密を含み得るため Git 対象外） |
| `engine.log` / `workers.log` | 診断ログ |
| `outputs/` | 回収可能な省略出力 |
| `mcp.sock` | モード 0600 の Unix socket（ワークスペースのエンジンは TCP 非公開） |

対話実行では daemon の生存と起動ロックを確認します。通常の pipe 実行はエンジンを起動しません。
ワークスペースの systemd や startup script の変更は不要です。公開 gateway は別の systemd サービスで動作します。停止する場合は、進行中の処理が終わったことを確認してから、
`doctor` が返す実行エンジン PID に `TERM` を送ってください。削除・停止を通常のセットアップに混ぜません。
アンインストールでは既存 Coder 接続を維持でき、`~/.local/bin/lkjyolo` と専用配置だけを対象にできます。

## 開発・検証

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -j 4 -- -D warnings
cargo test --locked --all-targets -j 4
LKJYOLO_TEST_ENGINE="$HOME/.local/share/lkjyolo/engines/0.2.1/chatgpt-exec-mcp" \
  cargo test --locked -j 4 --test engine -- --ignored --test-threads=2
cargo build --locked --release -j 4
target/release/lkjyolo package-plugin
```

Linux 実機テストは切断後の回収、同時重複依頼、対話状態、バックグラウンド完了、
Ctrl-C、長い出力の復元、構文エラー、設定再実行でのプロセス維持、旧 Python 版の履歴との互換性を検証します。
ネイティブバックエンドの単体テストと実 HTTP MCP テスト（`tests/native_http.rs`）は通常の `cargo test -j 4` で実行します。
実 HTTP テストは新しいローカルポートと使い捨て状態を使用し、`LKJYOLO_NATIVE_TEST_BINARY` で自分の release ビルドも指定できます。
対話エンジンの統合テストは通常の `cargo test` では `ignored` と表示され、上の `--ignored` 指定で明示的に実行します。
エンジン未指定で統合テストを要求した場合は失敗します。macOS では単体テスト・コンパイル・プラグイン ZIP 作成を確認でき、実行管理は Linux 専用です。
GitHub Actions は同じエンジンのハッシュを検証し、単体・実機テスト、Clippy、リリースビルド、ZIP 作成を実行します。
`Cargo.lock` を保存し、CI と配置用ビルドでは `--locked` を使います。

## 参考・ライセンス

実行エンジンは [CeraCharlesCC/chatgpt-exec-mcp](https://github.com/CeraCharlesCC/chatgpt-exec-mcp)
の `v0.2.1`（Apache-2.0）を対話セッション用にそのまま使用します。
通常コマンドの pipe 実行、出力保存、キャンセルはこのリポジトリの `src/pipes.rs` が実装します。
このプロジェクトは Rust で Coder 連携、永続リクエスト処理、インストール・プラグインを追加するものです。
upstream の原作者や OpenAI による公式サービスではありません。

- [OpenAI: MCP サーバー実装](https://developers.openai.com/plugins/build/mcp-server)
- [OpenAI: OAuth 認証](https://developers.openai.com/plugins/build/auth)
- [OpenAI: 接続と検証](https://developers.openai.com/plugins/deploy/connect-chatgpt)

ライセンスは Apache-2.0。実行エンジンの NOTICE と THIRD_PARTY_NOTICES はバイナリと一緒に保持します。
