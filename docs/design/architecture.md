# モジュール構成と依存関係

[目次に戻る](../index.md)

## 対象と前提

[機能仕様](../index.md)のMarkdown閲覧、3つの動作モード、移動・検索を実現するための構成です。
単一crate内で責務を分け、状態遷移と検索処理を端末なしで確認できるようにします。
UIにはratatui、端末制御にはcrossterm、Markdown描画にはtui-markdownを使用します。

## 構成と責務

| モジュール | 責務 |
|---|---|
| `main` | 起動、ファイル読み込み、各部の組み立て、イベントループ |
| `app` | モード、閲覧位置、検索状態、終了要求などの状態と状態遷移 |
| `app::document` | 読み込み済みの文書を保持。ファイルI/Oは行わない |
| `app::search` | 文字列検索、一致位置の管理、次／前の一致の選択 |
| `app::command` | コマンド文字列を解釈し、アプリの操作へ変換 |
| `ui` | 本文、ステータス欄、入力欄、ヘルプの描画 |
| `ui::input` | モードに応じたキー入力をアプリの操作へ変換。連続入力も扱う |
| `ui::layout` | 折り返し、表示行数、文書内位置と画面位置の対応 |
| `markdown` | Markdown描画のインターフェースを定義 |
| `markdown::tui_markdown` | tui-markdownを呼び出すadapter |
| `terminal` | raw mode、代替画面、イベント取得、終了時の端末復元 |

`Mode`や`Action`は、最初から個別ファイルへ分割せず、`app`内に置きます。

## 依存関係

以下の矢印は「依存元 → 依存先」であり、処理順序ではありません。

```text
main → app, ui, markdown::tui_markdown, terminal
ui → app, markdown::MarkdownRenderer
markdown::tui_markdown → markdown::MarkdownRenderer, tui-markdown
```

- `app`は`ui`、`markdown`、`terminal`に依存しません。
- `app`はratatui、crossterm、tui-markdownの型を扱いません。
- `ui`はMarkdownの具象adapterを直接生成・参照しません。
- `main`が具象adapterを生成し、`ui`へ渡します。
- ratatuiへの依存は描画系と端末管理に、crosstermへの依存は端末管理とキー入力の解釈に限定します。

## Markdown adapter

Markdownライブラリを交換しても、アプリの処理やUI全体へ変更が広がらないようにします。
`MarkdownRenderer`を共通のインターフェースとし、次の境界を設けます。

| 項目 | 内容 |
|---|---|
| 入力 | Markdown文字列と表示幅 |
| 出力 | 所有権を持つ`ratatui::Text`、表示幅に依存しない検索用テキスト、両者の位置の対応情報 |
| 隔離するもの | tui-markdownの呼び出し、設定型、表示幅に応じたライブラリ固有の設定 |

tui-markdownのAPIを直接使うのは`markdown::tui_markdown`だけです。
ratatuiは継続利用する前提とし、ratatuiを隠すための独自ASTやスタイル型は設けません。

[Mermaidの図としての描画](../features/markdown.md)は対象外です。
将来、画像として描画する場合は、テキストのみを返す現在のインターフェースの拡張が必要です。

## 主な連携

1. TUIを開始する前に`main`でファイルを読み込み、UTF-8として検証した文書を`app`へ渡します。
2. Markdown adapterの出力を基に、`ui`が表示とレイアウトを準備します。
   検索用テキストと表示領域の情報は、`main`を介して`app`へ渡します。
3. `terminal`から取得したキー入力を`ui::input`でアプリの操作へ変換し、`app`の状態を更新します。
4. 更新後の状態を`ui`が描画します。終了時の端末復元は`terminal`が担当します。

## 検索とリサイズの位置管理

[検索](../features/search.md)の対象を画面レイアウトから分離します。
adapterは同じ文書解釈から描画用と検索用のテキストを生成し、両者の位置の対応を保ちます。
`app::search`は検索用テキストを扱い、画面上の位置への変換とハイライトは`ui`が担当します。

検索位置と閲覧位置は、画面幅に依存しない文書内の位置を基準に保持します。
`ui::layout`が文書内の位置を折り返し後の表示行へ対応付け、
[リサイズ時](../features/navigation.md)には対応を更新して閲覧位置を維持します。

## 端末の復元

[終了仕様](../features/commands.md)に従い、通常終了と処理可能なエラーでの終了では、
`terminal`がraw modeを解除し、代替画面から復帰します。
端末の初期化に一部成功した後でエラーになった場合も、変更済みの状態を復元します。

## 使用ライブラリの参照先

- [ratatui](https://docs.rs/ratatui/latest/ratatui/)
- [crossterm](https://docs.rs/crossterm/latest/crossterm/)
- [tui-markdown](https://docs.rs/tui-markdown/latest/tui_markdown/)
