# lkjyolo を使う

lkjyolo は、ChatGPT / Codex から自分専用の永続 Linux 環境で開発するためのサービスです。
利用者ごとにログイン、ワークスペース、ファイル、実行履歴を分けます。

## 初めて使う人

1. [アカウント申請](https://auth.lkjsxc.com/if/flow/account-request/)で登録します。
   既存アカウントがあれば再申請は不要です。新規申請は管理者の承認後に利用できます。
2. [Coder](https://coder.lkjsxc.com) に自分のアカウントでログインします。
3. 環境がなければ Templates から `tiered-workspace` を選び、名前を **lkjyolo** にして作成します。
   標準の利用枠は 2 CPU / 2 GiB / 20 GiB で、実際の上限は自分に割り当てられたティアに従います。
   既に自分の環境が1つだけある場合は、その環境を使えます。
4. ChatGPT の Plugins → ＋ → Add custom MCP server で次の接続を追加します。
   このメニューを使えるアカウント・組織設定が必要です。

   - 名前: **lkjyolo**
   - Server URL: **https://coder.lkjsxc.com/lkjyolo/mcp**
   - 認証: **OAuth**（自動登録 / Dynamic Client Registration）
   - スコープの入力が必要な場合: **coder:all**

5. 自分の Coder アカウントで接続を許可し、作成したプラグインをインストールします。
6. Work のチャットで `@lkjyolo` を選び、「作業用フォルダーを確認して」と依頼します。
   新しい Linux x86_64 環境では、初回利用時に必要な実行基盤をチェックサム検証付きで導入します。

Codex などプラグインのローカル導入を使う場合は、案内ページの配布用ZIPを利用できます。
他人の接続設定、アクセストークン、GitHubログインを共有する必要はありません。
GitHubなど外部サービスは各自が自分の環境で認証してください。

## 実行先の選び方

- 管理者があなたの **CoderユーザーID** に実行先を設定していれば、その環境を使います。
- それ以外は、自分の `lkjyolo` という名前の環境を使います。
- その名前がなく、自分の環境が1つだけなら、それを使います。
- 複数あって決められない場合は実行しません。使いたい環境の名前を `lkjyolo` にするか管理者へ相談してください。

選んだ環境の所有者IDを実行直前に確認します。Coder管理者であっても他人の環境へ自動転送しません。
環境を削除・作り直したり実行先を変更したりすると、以前の環境にある履歴・セッションは引き継がれません。
停止・再起動で実行中プロセスは終了しますが、永続ディスク上のファイルと保存済み履歴は残ります。

## よくある問題

- **401 / 接続できない**: 承認済みの自分のアカウントでCoderへログインできるか確認し、OAuthを再接続します。
- **No workspace exists**: Coderで自分の環境を作成します。停止中の既存環境は、利用枠の範囲でCoderが起動します。
- **Workspace selection is ambiguous**: 実行先の選び方に従って環境を明確にします。
- **初回導入に失敗**: 環境内のHTTPS通信、空き容量、curl、sha256sum、Linux x86_64を確認します。
- **長い処理が途中に見える**: `session_id` が返った処理は継続中です。出力回収を依頼してください。

## 自分でホストする場合

リポジトリのREADMEに従ってLinuxでビルドし、Coderと同じHTTPS originでgatewayを動かします。
`deploy/coder-gateway.json` の `public_origin`、`upstream_ip` と所有者の個別設定を自分の環境に合わせます。
全ての有効なCoder利用者に各自の環境を提供する設定は次の形です。

```json
{
  "listen": "127.0.0.1:8787",
  "public_origin": "https://coder.example.com",
  "backend": "coder",
  "owned_workspaces": { "preferred_name": "lkjyolo", "overrides": {} }
}
```

初回自動導入を有効にする場合は、自分のoriginに公開したLinuxバイナリのURLとSHA-256を
`bootstrap: {"url": "...", "sha256": "..."}` に設定します。別origin・未固定のダイジェストは拒否されます。
未設定の場合は、各ワークスペースへ通常の `lkjyolo install` で導入します。
既存の `owner_id` + `workspace` 設定による1ユーザー限定の運用も引き続き使えます。
両方式を混ぜた設定や、どちらも指定しない設定は起動時に拒否されます。

```sh
target/release/lkjyolo package-plugin --portable \
  --endpoint https://coder.example.com/lkjyolo/mcp
```

このZIPには作成者個人のApp IDを含みません。各利用者が自分でOAuth接続します。
配布用ZIPの作成は、OpenAIの公開ディレクトリへの審査・掲載を意味しません。

## 案内URL

案内ページは [lkjyolo.lkjsxc.com](https://lkjyolo.lkjsxc.com/) です。旧案内URLは新URLに転送します。
OAuthとMCP接続先は引き続き `https://coder.lkjsxc.com/lkjyolo/mcp` を使うため、既存の接続は再設定不要です。
