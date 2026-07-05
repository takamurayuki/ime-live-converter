# Rust製 自動かな漢字変換・誤字補正入力支援ツール 要件定義書

## 1. 概要

本プロジェクトでは、Windows上でmacOSのライブ変換に近い入力体験を実現するため、Rustを用いて日本語入力支援ツールを開発する。

主な目的は、ひらがな入力中に自然なタイミングで自動的にかな漢字変換を行い、必要に応じて誤字修正・予測入力・ユーザー辞書学習を組み合わせることで、入力速度と変換精度を向上させることである。

最終的にはWindowsのIMEとして動作させることを目指すが、初期段階ではIME本体を作り込まず、Rust製の変換エンジンを独立モジュールとして実装し、CLIまたは簡易UIから検証する。

---

## 2. 開発方針

### 2.1 基本方針

本プロジェクトは、以下の順序で段階的に開発する。

1. Rustで変換エンジンを独立実装する
2. CLIでかな漢字変換・誤字補正・候補生成を検証する
3. 小規模UIまたはTauriアプリで仮変換の体験を確認する
4. ユーザー辞書・変換履歴・学習機能を追加する
5. Windows TSF / IME統合を行う

初期段階からWindows IME本体を実装すると難易度が高いため、まずは変換精度を担保するコアエンジンを作る。

---

## 3. 目的

### 3.1 解決したい課題

- Windows標準IMEではmacOSのライブ変換に近い自動変換体験が弱い
- 入力後にスペースキーで変換する操作を減らしたい
- よく使う単語・技術用語・固有名詞を高精度に変換したい
- ひらがな入力時の軽微な誤字を自動補正したい
- 入力履歴を学習して、自分に最適化された変換を行いたい

### 3.2 目指す体験

```txt
きょうはらすとでにほんごにゅうりょくつーるをつくります
↓
今日はRustで日本語入力ツールを作ります
```

また、誤字が含まれる場合も補正候補を出す。

```txt
きょおはいいてんきです
↓
今日はいい天気です
```

---

## 4. スコープ

### 4.1 初期スコープ

初期開発では以下を対象とする。

- ひらがな入力からのかな漢字変換
- 辞書ベースの候補生成
- Viterbiによる最適変換候補の探索
- ユーザー辞書の登録
- 変換履歴の保存
- 簡易的な誤字補正
- 自動仮変換の判定ロジック
- CLIによる動作確認

### 4.2 中期スコープ

- Tauriまたは簡易デスクトップUIでの入力検証
- 変換候補一覧の表示
- Space / Enter / Esc / Ctrl+Backspaceなどの操作対応
- ユーザー履歴による候補順位の調整
- 技術用語・固有名詞辞書の追加
- 文脈スコアリング

### 4.3 長期スコープ

- Windows TSFを用いたIME化
- 全アプリケーション上での入力補助
- アプリごとの文脈学習
- 軽量言語モデルによる候補rerank
- クラウド同期またはローカル専用学習データ管理
- Mozcなど既存エンジンとの連携検討

### 4.4 対象外

初期段階では以下は対象外とする。

- 完全なWindows IME統合
- macOS / Linux対応
- 音声入力
- 手書き入力
- 大規模LLMを毎キー入力ごとに呼び出す構成
- クラウド前提の変換処理

---

## 5. システム全体構成

```txt
[Keyboard Input]
      ↓
[Input Controller]
      ↓
[Composition Manager]
      ↓
[Typo Correction Engine]
      ↓
[Kana-Kanji Converter]
      ↓
[Candidate Generator]
      ↓
[Viterbi Search]
      ↓
[Context Scorer]
      ↓
[User Learning Engine]
      ↓
[Candidate Result]
      ↓
[Provisional Conversion UI]
```

---

## 6. アーキテクチャ方針

### 6.1 レイヤー構成

```txt
presentation/
  CLI / UI / TSF Bridge

application/
  入力制御
  自動変換判定
  候補選択
  確定・取消制御

domain/
  かな漢字変換
  誤字補正
  候補生成
  スコアリング
  ユーザー学習

infrastructure/
  辞書読み込み
  SQLite保存
  設定ファイル管理
  OS API連携
```

### 6.2 重要な設計方針

- IME依存部分と変換エンジンを分離する
- 変換エンジンはCLI・Tauri・TSFのどこからでも呼べるようにする
- 候補生成と候補順位付けを分離する
- 誤字補正はかな漢字変換とは別レイヤーにする
- 自動変換は即確定ではなく仮変換にする
- ユーザー辞書・履歴・ドメイン辞書を後から追加できる設計にする

---

## 7. 機能要件

## 7.1 かな漢字変換機能

### 概要

ひらがな文字列を受け取り、自然な日本語の変換候補を生成する。

### 入力例

```txt
きょうはいいてんきです
```

### 出力例

```txt
1. 今日はいい天気です
2. 今日は良い天気です
3. 今日はい良い天気です
```

### 要件

- ひらがな読みから単語候補を取得できること
- 複数単語に分割できること
- 品詞情報を持てること
- 単語コストを持てること
- 接続コストを利用できること
- Viterbiにより最適経路を探索できること
- N-best候補を返せること

---

## 7.2 候補生成機能

### 概要

読み文字列に対して、変換候補を複数生成する。

### 要件

- 辞書から読み一致する単語を検索できること
- 前方一致・完全一致の両方に対応すること
- 未知語に対してはひらがな候補を残すこと
- 技術用語・英単語・固有名詞を候補に含められること
- ユーザー辞書の候補を優先できること

### 候補データ例

```rust
pub struct Candidate {
    pub text: String,
    pub reading: String,
    pub score: f32,
    pub kind: CandidateKind,
}

pub enum CandidateKind {
    KanjiConversion,
    Prediction,
    TypoCorrection,
    UserDictionary,
    RawKana,
}
```

---

## 7.3 Viterbi変換機能

### 概要

読み文字列に対して、候補単語の組み合わせをラティスとして構築し、最小コスト経路を探索する。

### 要件

- 入力文字列を位置ごとに分割できること
- 各位置に候補単語ノードを配置できること
- 単語コストを加味できること
- 接続コストを加味できること
- 最小コストの候補列を取得できること
- 複数候補を取得できること

### コスト計算例

```txt
total_cost = word_cost + connection_cost + context_penalty - user_frequency_bonus
```

---

## 7.4 自動仮変換機能

### 概要

入力中のひらがなを自動的に仮変換する。

### 要件

- 入力停止から150〜300ms程度で仮変換を実行できること
- 句読点入力時に仮変換できること
- 「です」「ます」「けど」など文節境界らしい入力で仮変換できること
- 仮変換中に追加入力された場合は再変換できること
- ユーザーが明示確定するまで内部的には確定扱いにしないこと

### 状態管理例

```rust
enum CompositionState {
    RawKana(String),
    Provisional {
        reading: String,
        converted: String,
        candidates: Vec<Candidate>,
    },
    Committed(String),
}
```

### 自動変換判定例

```rust
fn should_auto_convert(input: &str, elapsed_ms: u64) -> bool {
    if elapsed_ms >= 250 {
        return true;
    }

    input.ends_with('。')
        || input.ends_with('、')
        || input.ends_with("です")
        || input.ends_with("ます")
        || input.ends_with("けど")
}
```

---

## 7.5 誤字補正機能

### 概要

ひらがな入力中の軽微な誤字を補正し、変換候補として提示する。

### 対象例

```txt
きょお → きょう
こんにちわ → こんにちは
おねがいしｍす → おねがいします
ありがとお → ありがとう
```

### 要件

- ルールベースの補正を行えること
- 編集距離による近似候補を生成できること
- キーボード隣接ミスを補正候補に含められること
- 補正候補は通常変換候補より強制的に優先しないこと
- 文脈上自然な場合のみ上位に出すこと

### 実装方針

```txt
入力
  ↓
正規化
  ↓
誤字候補生成
  ↓
かな漢字変換
  ↓
スコアリング
```

---

## 7.6 ユーザー辞書機能

### 概要

ユーザーが任意の読みと変換後文字列を登録できる。

### 例

```txt
らすと → Rust
ねくすと → Next.js
れいやーえっくす → LayerX
```

### 要件

- 読みと表記を登録できること
- 品詞またはカテゴリを設定できること
- 登録候補を通常辞書より優先できること
- 削除・更新できること
- インポート・エクスポートできること

---

## 7.7 変換履歴学習機能

### 概要

ユーザーが選択・確定した変換候補を履歴として保存し、次回以降の候補順位に反映する。

### 要件

- 確定した単語・文節を保存できること
- 使用回数を保存できること
- 最終使用日時を保存できること
- アプリケーション別の履歴を保存できる余地を残すこと
- 使用頻度が高い候補を上位に出せること

### データ例

```sql
CREATE TABLE conversion_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    reading TEXT NOT NULL,
    surface TEXT NOT NULL,
    frequency INTEGER NOT NULL DEFAULT 1,
    last_used_at TEXT NOT NULL,
    app_name TEXT
);
```

---

## 7.8 予測入力機能

### 概要

入力途中の文字列から、続きの候補を提示する。

### 例

```txt
おつ
↓
お疲れ様です
お疲れさまです
お疲れ様でした
```

### 要件

- 前方一致で候補を検索できること
- 履歴頻度に応じて候補順位を変えられること
- 長文定型文を候補として出せること
- ユーザーが不要な候補を非表示にできる余地を残すこと

---

## 7.9 操作機能

### 想定キー操作

| 操作 | 挙動 |
|---|---|
| Space | 次候補に切り替え |
| Shift + Space | 前候補に戻る |
| Enter | 現在の候補を確定 |
| Esc | 仮変換をキャンセルしてひらがなに戻す |
| Ctrl + Backspace | 直前の変換を入力前状態に戻す |
| Tab | 予測候補を選択 |

---

## 8. 非機能要件

## 8.1 性能要件

- 通常入力時の変換応答は50ms以内を目標とする
- 自動仮変換は150〜300msの入力停止後に実行する
- 1回の候補生成で返す候補は初期表示10件程度とする
- 辞書検索は高速化のためTrieまたはFSTの利用を検討する
- 大規模LLMをリアルタイム入力のメイン経路に入れない

## 8.2 精度要件

- 一般的な日常文で第一候補が自然な変換になること
- 技術用語はユーザー辞書により高精度化できること
- 誤字補正は過剰に介入しないこと
- 誤変換時にすぐ元に戻せること

## 8.3 可用性

- 変換エンジンが失敗しても入力自体は継続できること
- 未知語はひらがなのまま候補に残すこと
- 辞書読み込み失敗時は最低限の入力が可能であること

## 8.4 保守性

- 変換エンジンとOS統合部分を分離する
- 辞書形式を明確にする
- テストしやすい純粋関数を増やす
- 各機能をtraitで差し替え可能にする

## 8.5 セキュリティ・プライバシー

- 変換履歴は原則ローカル保存とする
- クラウド送信は明示的にユーザーが有効化した場合のみ行う
- パスワード入力欄では学習・変換支援を無効化できる設計にする
- 入力履歴の削除機能を用意する

---

## 9. データ設計

## 9.1 システム辞書

```csv
reading,surface,pos,cost
きょう,今日,noun,100
てんき,天気,noun,100
いい,良い,adjective,120
らすと,Rust,noun,80
```

## 9.2 ユーザー辞書

```sql
CREATE TABLE user_dictionary (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    reading TEXT NOT NULL,
    surface TEXT NOT NULL,
    pos TEXT,
    cost INTEGER DEFAULT 50,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

## 9.3 変換履歴

```sql
CREATE TABLE conversion_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    reading TEXT NOT NULL,
    surface TEXT NOT NULL,
    frequency INTEGER NOT NULL DEFAULT 1,
    last_used_at TEXT NOT NULL,
    app_name TEXT
);
```

## 9.4 誤字補正ルール

```json
[
  {
    "from": "きょお",
    "to": "きょう",
    "weight": 0.8
  },
  {
    "from": "こんにちわ",
    "to": "こんにちは",
    "weight": 0.9
  }
]
```

---

## 10. Rustモジュール構成案

```txt
src/
  main.rs
  lib.rs

  application/
    mod.rs
    input_controller.rs
    auto_convert.rs
    composition_manager.rs

  domain/
    mod.rs
    candidate.rs
    converter.rs
    dictionary.rs
    lattice.rs
    viterbi.rs
    typo.rs
    scorer.rs
    learning.rs

  infrastructure/
    mod.rs
    dictionary_loader.rs
    sqlite_history_repository.rs
    user_dictionary_repository.rs
    config_loader.rs

  presentation/
    mod.rs
    cli.rs

  tests/
```

---

## 11. 主要trait設計案

## 11.1 Converter

```rust
pub trait Converter {
    fn convert(&self, ctx: InputContext) -> Vec<Candidate>;
}
```

## 11.2 Dictionary

```rust
pub trait Dictionary {
    fn lookup_exact(&self, reading: &str) -> Vec<WordEntry>;
    fn lookup_prefix(&self, reading: &str) -> Vec<WordEntry>;
}
```

## 11.3 TypoCorrector

```rust
pub trait TypoCorrector {
    fn correct(&self, input: &str) -> Vec<TypoCandidate>;
}
```

## 11.4 Scorer

```rust
pub trait Scorer {
    fn score(&self, candidate: &Candidate, ctx: &InputContext) -> f32;
}
```

## 11.5 LearningRepository

```rust
pub trait LearningRepository {
    fn record_commit(&self, reading: &str, surface: &str);
    fn find_frequency(&self, reading: &str, surface: &str) -> u32;
}
```

---

## 12. 変換処理フロー

```txt
1. ユーザーがひらがなを入力する
2. InputControllerが入力文字列を保持する
3. AutoConvertが変換タイミングを判定する
4. TypoCorrectorが誤字補正候補を生成する
5. Dictionaryが読み候補を取得する
6. LatticeBuilderが候補ラティスを構築する
7. Viterbiが最小コスト経路を探索する
8. Scorerが候補を再スコアリングする
9. LearningEngineが履歴スコアを加算する
10. CandidateResultを返す
11. CompositionManagerが仮変換状態にする
12. ユーザーが確定・取消・候補変更を行う
```

---

## 13. MVP要件

最初のMVPでは以下を満たせばよい。

### 必須

- CLIでひらがなを入力できる
- 辞書CSVを読み込める
- 読みに一致する候補を取得できる
- Viterbiで文全体を変換できる
- 第一候補を表示できる
- ユーザー辞書をSQLiteに保存できる
- 変換履歴を保存できる
- 簡単な誤字補正ができる

### MVP入力例

```txt
input: きょうはらすとをべんきょうします
```

### MVP出力例

```txt
1. 今日はRustを勉強します
2. 今日春ストを勉強します
3. 今日はらすとを勉強します
```

---

## 14. 将来的な高度化案

## 14.1 軽量LMによるrerank

候補生成は辞書とViterbiで高速に行い、上位10件程度を小型言語モデルで再順位付けする。

```txt
候補生成: 辞書 + Viterbi
候補選択: 文脈スコア + 履歴 + 軽量LM
```

リアルタイム入力では速度が重要なため、LLMは毎キー入力ではなく、一定時間停止後または文節単位で利用する。

## 14.2 Mozc連携

将来的に精度を大きく上げたい場合は、Mozcの辞書・変換ロジックを参考にする、または連携する。

ただしMozcはC++ベースであり、Rustから扱うにはFFIやプロセス分離が必要になる可能性がある。

## 14.3 アプリ別学習

エディタ、ブラウザ、チャット、メールなど、アプリごとに候補順位を変える。

例：

```txt
VS Code上: らすと → Rust
チャット上: らすと → ラスト
```

---

## 15. テスト方針

## 15.1 単体テスト

- 辞書検索
- 誤字補正
- Viterbi探索
- スコア計算
- 自動変換判定
- 履歴保存

## 15.2 結合テスト

- 入力文字列から候補生成まで
- 誤字補正込みの変換
- ユーザー辞書込みの変換
- 履歴学習後の候補順位変化

## 15.3 評価データ

以下のようなテスト文を用意する。

```txt
きょうはいいてんきです
おつかれさまです
らすとでつーるをつくります
ねくすとじぇーえすをべんきょうしています
こんにちわよろしくおねがいします
```

---

## 16. 成功条件

### MVP成功条件

- CLI上で自然なかな漢字変換ができる
- ユーザー辞書の登録語が第一候補に出る
- 代表的な誤字を補正候補として出せる
- 変換履歴によって候補順位が変わる

### 実用版成功条件

- 入力中の仮変換がストレスなく表示される
- 誤変換時にすぐ戻せる
- 技術用語・固有名詞が高精度に変換される
- Windows上の実アプリで入力補助として利用できる

---

## 17. 開発ロードマップ

## Phase 1: 変換エンジンMVP

- 辞書CSV作成
- Dictionary trait実装
- Candidate構造体作成
- 入力文字列から候補取得
- CLI出力

## Phase 2: Viterbi実装

- WordEntry定義
- Lattice構築
- コスト計算
- 最小コスト経路探索
- N-best候補生成

## Phase 3: 誤字補正

- ルールベース補正
- 編集距離補正
- キーボード隣接補正
- 誤字候補のスコアリング

## Phase 4: 学習機能

- SQLite導入
- ユーザー辞書保存
- 変換履歴保存
- 使用頻度によるスコア調整

## Phase 5: 自動仮変換UI

- 入力停止時間の監視
- 仮変換状態管理
- 候補切り替え
- 確定・取消操作

## Phase 6: Windows統合

- TSF調査
- RustからCOM連携
- Text Service実装
- 実アプリ上での入力テスト

---

## 18. 技術選定

| 領域 | 技術 |
|---|---|
| 言語 | Rust |
| DB | SQLite |
| 辞書形式 | CSV / JSON / SQLite |
| CLI | clap / standard input |
| UI検証 | Tauri / egui / 独自ミニUI |
| Windows統合 | TSF / windows-rs |
| 形態素解析補助 | Sudachi検討 |
| 高精度化 | Mozc参考 / 軽量LM rerank |

---

## 19. リスク

## 19.1 技術リスク

- Windows TSF実装の難易度が高い
- 日本語変換精度を完全自作で高めるのは難しい
- 辞書データの整備に時間がかかる
- リアルタイム性と精度の両立が難しい

## 19.2 対策

- まずIME化せず変換エンジンを独立開発する
- 初期辞書は小さく始める
- 技術用語はユーザー辞書で補完する
- 候補生成と候補rerankを分離する
- Mozc連携を将来の選択肢として残す

---

## 20. 実装で最も重要なポイント

本プロジェクトで最も重要なのは、以下の3点である。

1. 自動変換を即確定にしないこと
2. 変換エンジンとWindows統合部分を分離すること
3. 辞書・履歴・文脈スコアリングで段階的に精度を上げること

特に、自動変換は「勝手に確定」ではなく「自然に仮変換する」設計にする。

これにより、macOSのライブ変換に近い快適さを目指しつつ、誤変換時のストレスを抑えられる。

---

## 21. 次に着手するタスク

最初に実装するべきタスクは以下である。

```txt
Day 1:
- Rustプロジェクト作成
- Candidate / WordEntry / InputContextを定義
- CSV辞書を読み込むDictionaryを実装
- 入力ひらがなに完全一致する候補を返すCLIを作成
```

次に、前方一致検索、ラティス構築、Viterbi探索へ進む。

---

## 22. 最終目標

最終的には、以下のような入力体験を実現する。

```txt
ユーザー入力:
きょうはらすとでにほんごにゅうりょくをこうそくかします

仮変換:
今日はRustで日本語入力を高速化します

確定:
Enterで確定

取消:
EscまたはCtrl+Backspaceでひらがなに戻す
```

この状態を実現できれば、Windows上でもmacOSのライブ変換に近い、かつユーザーごとに最適化された高速入力体験を提供できる。
