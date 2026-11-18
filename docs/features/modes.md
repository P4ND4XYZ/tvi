# 動作モードと入力

[目次に戻る](../index.md)

## 概要

閲覧、コマンド入力、検索語入力を区別するため、NORMAL、COMMAND、SEARCHの3モードを持ちます。
起動時はNORMALモードです。

| モード | 役割 |
|---|---|
| NORMAL | 文書の閲覧、スクロール、検索結果への移動 |
| COMMAND | `:`に続くコマンドの入力 |
| SEARCH | `/`または`?`に続く検索語の入力 |

## モードの切り替え

| 現在のモード | 操作 | 結果 |
|---|---|---|
| NORMAL | `:` | COMMANDへ移り、コマンド入力を開始 |
| NORMAL | `/` | SEARCHへ移り、前方検索の入力を開始 |
| NORMAL | `?` | SEARCHへ移り、後方検索の入力を開始 |
| COMMAND | `Enter` | コマンドを実行し、終了コマンド以外はNORMALへ戻る |
| SEARCH | `Enter` | 検索を確定してNORMALへ戻る |
| COMMAND / SEARCH | `Esc` | 入力をキャンセルしてNORMALへ戻る |

COMMANDとSEARCHでは、文字入力と`Backspace`で入力内容を変更します。
入力中の文字をNORMALモードのキーバインドとして扱いません。
たとえば、検索語に含まれる`q`でアプリが終了することはありません。
