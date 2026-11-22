---@meta _

---@alias Tvi.KeymapMode "normal" | "command" | "search"

---@class Tvi.KeymapOptions
---@field desc? string キー一覧に表示する操作の説明。

---@class Tvi.KeymapApi
---@field set fun(mode: Tvi.KeymapMode, keys: string, callback: fun(), opts?: Tvi.KeymapOptions) モード別にキーを登録します。

---@class Tvi.UiApi
---@field current_view fun(): Tvi.View 現在の本文ビューを取得します。
---@field notify fun(message: string) 状態欄の通知を更新します。

---@class Tvi.Listener: userdata
---@field close fun(self: Tvi.Listener) ローカルの受信を終了します。複数回呼べます。

---@class Tvi.NetApi
---@field listen_local fun(port: integer, on_line: fun(line: string): boolean?): Tvi.Listener?, string? IPv4ループバックで1行ずつ受信します。失敗時は nil, err を返します。

---@class Tvi.ProjectRootOptions
---@field root_markers? string[] 探すマーカー名の配列。既定は { ".git" } です。

---@class Tvi.Link
---@field id integer 現在の文書内でのリンクID。
---@field label string リンクの表示文字列。
---@field target string リンク先の文字列。

---@class Tvi.View: userdata
---@field path fun(self: Tvi.View): string?, string? 現在の文書のUTF-8絶対パス、または nil, err を返します。
---@field project_root fun(self: Tvi.View, opts?: Tvi.ProjectRootOptions): string?, string? プロジェクトルートの絶対パス、または nil, err を返します。
---@field links fun(self: Tvi.View): Tvi.Link[] 対象のリンクを出現順に返します。
---@field resolve_link fun(self: Tvi.View, id: integer): string?, string? リンク先の絶対パス、または nil, err を返します。存在は確認しません。
---@field open_file fun(self: Tvi.View, path: string): boolean, string? 絶対パスのファイルを開き、true, nil または false, err を返します。
---@field scroll_to_line fun(self: Tvi.View, line: integer) 現在の元ファイルの1始まりの行へ本文をスクロールします。
---@field open_float fun(self: Tvi.View, opts: Tvi.OpenFloatOptions): Tvi.Float フロートを開きます。すでに開いている場合は Lua エラーになります。

---@class Tvi.FloatDisplayOptions
---@field title? string 枠に表示するタイトル。
---@field header? string スクロールしても残る1行。
---@field lines? string[] 本文の行。更新時に指定すると本文全体を置き換えます。
---@field focus_line? integer|false 強調する1始まりの行番号。false で強調を解除します。

---@class Tvi.OpenFloatOptions: Tvi.FloatDisplayOptions
---@field on_key? fun(float: Tvi.Float, key: Tvi.FloatKey) キー入力を受け取るコールバック。

---@class Tvi.FloatKey
---@field name string キー名（例: "Enter"、"C-n"）。
---@field text? string 文字入力に対応する文字列。

---@class Tvi.Float: userdata
---@field update fun(self: Tvi.Float, opts: Tvi.FloatDisplayOptions) 指定した表示項目だけを更新します。
---@field close fun(self: Tvi.Float) フロートを閉じます。

---@class Tvi.ProcessOptions
---@field args? string[] コマンドに渡す引数。
---@field cwd? string 子プロセスの作業ディレクトリ。

---@class Tvi.ProcessResult
---@field stdout string NULを含む標準出力。標準エラーは含みません。
---@field ok boolean 終了成功時に true。
---@field code? integer 終了コード。取得できない場合は nil。

---@alias Tvi.ProcessErrorKind
---| "not_found" # 実行ファイルが見つかりません。
---| "cwd" # 作業ディレクトリを利用できません。
---| "spawn" # その他の実行エラーです。

---@class Tvi.ProcessError
---@field kind Tvi.ProcessErrorKind エラーの種類。
---@field message string エラーの理由。

---@class Tvi.ProcessApi
---@field run fun(program: string, opts: Tvi.ProcessOptions): Tvi.ProcessResult?, Tvi.ProcessError? シェルを介さず同期実行します。実行できない場合は nil, err を返します。

---@alias Tvi.StyleGroup
---| "document.normal"
---| "markdown.heading1"
---| "markdown.heading2"
---| "markdown.heading3"
---| "markdown.heading4"
---| "markdown.heading5"
---| "markdown.heading6"
---| "markdown.link"
---| "markdown.quote"
---| "markdown.code"
---| "search.match"
---| "search.current"
---| "ui.status"
---| "ui.prompt"
---| "float.normal"
---| "float.border"

---@alias Tvi.Color
---| "default"
---| "black"
---| "red"
---| "green"
---| "yellow"
---| "blue"
---| "magenta"
---| "cyan"
---| "white"
---| "bright_black"
---| "bright_red"
---| "bright_green"
---| "bright_yellow"
---| "bright_blue"
---| "bright_magenta"
---| "bright_cyan"
---| "bright_white"
---| string # #RRGGBB 形式の色も指定できます。

---@class Tvi.Style
---@field fg? Tvi.Color 文字色。
---@field bg? Tvi.Color 背景色。
---@field bold? boolean 太字。
---@field italic? boolean 斜体。
---@field underline? boolean 下線。
---@field reverse? boolean 反転。

---@class Tvi.ColorschemeApi
---@field set fun(styles: table<Tvi.StyleGroup, Tvi.Style>) 既定の配色を基準に全体を置き換えます。空テーブルで既定に戻ります。

---@class Tvi.Api
---@field keymap Tvi.KeymapApi
---@field ui Tvi.UiApi
---@field net Tvi.NetApi
---@field process Tvi.ProcessApi
---@field colorscheme Tvi.ColorschemeApi

-- 実際のグローバルは tvi 本体が作成します。
if false then
  ---@type Tvi.Api
  tvi = nil
end
