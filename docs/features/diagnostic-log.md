# 診断ログ

[目次に戻る](../index.md)

## 概要

文書の読み込み、PDF・画像・Mermaidの表示、Lua、端末で起きた問題を調べるための機能です。[状態欄](status-help.md)には短い通知を表示し、原因の調査に必要な情報はログファイルに保存します。

## 保存先と保持

| OS | 書き込み中のログ |
|---|---|
| Windows | `%LOCALAPPDATA%\tvi\logs\tvi.log` |
| Linux | `$XDG_STATE_HOME/tvi/logs/tvi.log`。未設定・空・相対パスの場合は `~/.local/state/tvi/logs/tvi.log` |

- ログは最初に記録するときに作成し、終了後も保持します。文書の隣やTUI表示中の標準エラーにはログを書きません。
- UTCの日付が変わって最初に記録するとき、または次の記録で1ファイルの上限（1 MiB）を超えるときに、新しいファイルに切り替えます。起動や終了だけでは切り替えません。
- 書き込み中の1本と過去5世代を保持します（合計最大6 MiB）。切り替え時に古い世代から削除します。保持単位は日数ではなくファイル数です。
- 保存や切り替えに失敗しても閲覧を続け、状態欄に「ログを保存できません」と1回の起動につき一度だけ通知します。複数のtviが同時に動いても、ログの書き込み待ちで閲覧を止めません。記録待ちの上限は64件で、空きがなければ記録を見送ります。終了時の待機は最大150ミリ秒で、未保存の記録が残ることがあります。

## 記録レベルの指定

既定では、閲覧を続けられる問題を `WARN`、起動や閲覧を続けられない問題を `ERROR` として記録します。詳しく調べる場合は、tviを起動するシェルで環境変数 `TVI_LOG` を `debug` に設定します。コマンドライン引数ではありません。未指定・空・不明な値の場合は既定のまま起動します。

PowerShellでは、設定を削除するまで現在のシェルに残ります。

```powershell
$env:TVI_LOG = 'debug'
tvi README.md
Remove-Item Env:TVI_LOG
```

Bashでは、この書き方なら1回の起動にだけ適用されます。

```bash
TVI_LOG=debug tvi README.md
```

## 記録形式

UTF-8のJSON Lines（1行に1件のJSON）で保存します。全レコードに `ts`（UTCの日時、ミリ秒付き）、`level`（`ERROR` / `WARN` / `DEBUG`）、`event`（イベント名）、`pid`（プロセスID）を付けます。該当しない項目は省略します。文書名や設定ファイル名はファイル名だけとし、`line` はそのファイルの行番号（1始まり）です。

```json
{"ts":"2026-08-24T09:41:23.184Z","level":"WARN","event":"mermaid.convert_failed","pid":1234,"document":"architecture.md","line":42,"stage":"convert","reason":"nonzero_exit","exit_code":1,"elapsed_ms":812,"fallback":"code"}
```

1件の上限は4 KiBです。長い項目は短縮し、有効なJSONを1行で残します。Markdown・Mermaidの本文、検索語、入力キー、環境変数の値、絶対パス、外部コマンドの生の出力やLuaの未加工のエラー文字列は、デバッグ時も保存しません。

## 出力するイベント

| イベント | 記録する条件と内容（共通項目以外） |
|---|---|
| `document.open_failed` | 起動時やLuaから文書を開けなかったとき、リンク先を解決できなかったとき、または自動再読み込みの失敗が続いたとき。`document`、`trigger`（`startup` / `open_file` / `link` / `reload`）、`stage` と `reason`（例：`read` / `not_found`、`decode` / `invalid_utf8`）。 |
| `pdf.page_failed` | 表示中のPDFでページ送りや画面幅への合わせ直しが失敗したとき。`document`、`page`（1始まり）、`trigger`（`turn` / `resize`）、`stage`、`reason`、取得できた場合は `os_code`。同じ要求では1件だけ記録し、無効になった要求の結果は記録しません。PDFを初めて開く際の失敗は `document.open_failed` に記録します。 |
| `document.watch_failed` | 表示中の文書を監視できなかったとき。`document` と `stage`（`register` / `receive`）。同じ文書の監視中に繰り返しません。 |
| `image.load_failed` | ローカル画像を読み込めなかったとき。`document`、`image_index`（元のMarkdown上の出現番号。生成した図は数えない）、`stage`、`reason`（例：ファイルなし、非対応形式、破損、サイズ上限）。再描画では繰り返しません。 |
| `mermaid.launch_failed` | `mmdc` を検出したが起動できなかったとき。`document`、`reason`（例：`permission_denied`）、取得できた場合は `os_code`、`fallback: "code"`。文書を開く操作につき1件にまとめます。 |
| `mermaid.convert_failed` | 変換の異常終了や、出力PNGの欠落・破損・サイズ超過。`document`、ブロックの開始 `line`、`stage`（例：`convert` / `validate_png`）、`reason`、`elapsed_ms`、`fallback: "code"`。異常終了時は、取得できた場合のみ `exit_code` を記録します。標準出力・標準エラーは記録しません。 |
| `mermaid.timeout` | 変換の時間上限に達したとき。`document`、`scope`（`diagram` / `document`）、`limit_ms`。図単位ならブロックの開始 `line`、文書全体なら未変換件数 `remaining`。 |
| `mermaid.cache_failed` | キャッシュの読み込み・検証・保存・古いPNGの削除に失敗したとき。`operation`、`reason`、`action`（例：`regenerate` / `display_without_cache`）。通常のキャッシュミスや容量調整は記録しません。 |
| `lua.failed` | Lua設定の読み込みやコールバックが失敗したとき。特定できた `source` と `line`、`stage`（設定・キーマップ・フロート）、`reason`（例：`syntax_error`）、`action`（例：`use_defaults`）。 |
| `terminal.failed` | 端末の初期化・入力・描画・復元が失敗したとき。`operation`（例：`draw`）、選択済みなら `image_mode`、`reason`、取得できた場合は `os_code`、継続・終了を示す `action`。 |

`mmdc` や `init.lua` が見つからないこと、端末がKitty非対応であること自体は警告・エラーとして記録しません。ただし、それがPDFを開けない理由であれば `document.open_failed` を記録します。`TVI_LOG=debug` の場合は以下も記録します。

| イベント | 記録内容 |
|---|---|
| `tvi.start` | 起動時のtviの `version`。 |
| `mermaid.detect` | `mmdc` の検出結果 `available` と、それに応じた `action`（例：`show_code`）。 |
| `mermaid.summary` | `document`、`cache_hits`（キャッシュ利用件数）、`converted`（変換成功件数）、`fallbacks`（コード表示に戻した件数）、`elapsed_ms`（所要時間）。 |
| `terminal.image_mode` | 選んだ画像表示方式 `mode` と、その理由 `reason`（例：`kitty_not_detected`）。 |
