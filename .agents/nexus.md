# Nexus Journal — rust-less

## Run: /nexus clone "lessコマンドをRustで実装" (2026-08-08)

### Routing Decision Record
- Input: subcommand `clone` + target "less コマンドを Rust で実装"
- Recipe: clone (subcommand direct dispatch, CLASSIFY skip)
- Mode: AUTORUN_FULL(contract-level gates のみ interactive)
- Chain: research-sweep → capture → spec+arch → builder → parity-verify(loop ≤3)→ ship
- Confidence: high(ターゲットはローカルバイナリ less 668、オラクル取得容易)

### Stack Decision Record (SDR) — locked 2026-08-08
| Layer | Decision | Rationale |
|-------|----------|-----------|
| Runtime | Rust 1.92 / edition 2024 | ユーザ指定 |
| 端末制御 | crossterm | ユーザ選択(推奨案)。raw mode / alternate screen / key events |
| 引数パース | clap | ユーザ選択(実用クレート方針) |
| 検索 | regex クレート | 同上。**parity ceiling**: less 668 は POSIX regex — 後方参照等の方言差は宣言済み許容差 |
| ファイルIO | std(必要なら memmap2) | 実用方針 |
| テスト | cargo test + parity/ の tmux 差分ハーネス | ハーネスは本家と clone の両方を駆動する共有オラクル |
| 配布 | cargo build バイナリ | ローカル |
| License | MIT または Apache-2.0/MIT dual | クリーンルーム成果物 |

宣言済み parity ceilings:
1. regex 方言差(POSIX BRE/ERE vs Rust regex)— 一般的パターンは一致必須、後方参照・特殊 POSIX 構文は許容差
2. crossterm の端末初期化シーケンス差 — 画面「状態」比較で正規化(escape ストリーム自体は比較しない)

### Rights Record — locked 2026-08-08
- Authorization basis: **ブラックボックス挙動再現**(ユーザ宣言)。公開 CLI ツールの観測可能挙動のクリーンルーム再実装。less ソースコードは参照禁止(全エージェントに伝播)
- Capture posture: ローカルバイナリをローカル pty で駆動 — ToS/robots/レート制限 N/A
- PII: fixture は合成データのみ — N/A
- Assets: 端末テキストのみ。`h` ヘルプ画面(本家テキスト逐語)はスコープ外
- No-circumvention: N/A

### Capture Provenance Stamp
- Target: GNU less 668 (POSIX regular expressions build), /usr/bin/less
- OS: macOS Darwin 25.5.0 (arm64)
- Terminal: tmux 3.7b pane, 80x24 固定, TERM=screen(tmux 内), 外側 xterm-256color
- Locale: LANG=C.UTF-8
- Env canonicalization: LESS/LESSOPEN/LESSCLOSE unset, LESSHISTFILE=-
- Capture date: 2026-08-08
- Fixtures: 合成(parity/fixtures/)

### Scope
- Core pager(ユーザ選択): 表示(file/stdin/複数ファイル)、縦ナビ+数値プレフィクス、/? n N 検索+ハイライト、-N/-S/-R、:n/:p、q 系終了+終了コード、プロンプト/メッセージ、リサイズ
- 戦略: incremental-clone。拡張機能は coverage_gaps として明示

### Parity Map(この run の有効次元)
- Visual+Behavioral: tmux セルグリッドダンプ(text + -e 属性付き)の canonicalize 後完全一致
- Feature: declared inventory カバレッジ行列
- Data-API / Asset / Performance: N/A(宣言なし)

### Decision Ledger
- DEC-1: 対話ゲートを1回の AskUserQuestion に統合(Stack 4層 + Rights)— CLI ターゲットで Web 系レイヤーが N/A のため
- DEC-2: capture adapter は tmux(pyte 不在、tmux 3.7b あり)。conformant adapter 4成果物を満たす
- DEC-3: Phase 3 ARCHITECT を Phase 2 spec エージェントに統合(minimum viable chain)。capture 戦略は incremental で確定済みのため Magi 仲裁不要
- DEC-4: comparator は SSIM でなくセルグリッド完全一致(TUI のため最厳密かつ正しい)
- DEC-5: 分類器がサブエージェントへの sandbox-bypass 指示を拒否 → 役割分割(エージェント=成果物作成、ハブ=tmux 実行)。拒否の意図を尊重した再構成
- DEC-6: capture-author の EXTENSIONS.md 11 点を承認(全て追加的)。#6 の scenario 全域 attrs 比較は偽陽性リスクとして記録。APFS 大文字小文字衝突の検出・修正(39→36 に無警告減少していた)は capture-author の成果
- DEC-7: diff は expect_exit を参照せず baseline/candidate の実測 exit code 同士を比較(差分オラクルの純粋性維持)

## Run: /nexus killer (2026-08-08)

### Routing Decision Record
- Input: bare subcommand `killer` → bare-subcommand exception → CLASSIFY GATE で対象を1問確認
- 確定スコープ: rless 本体(ページャ)にとっての差別化
- Chain: Compete ‖ Plea ‖ Flux(claude) ‖ Flux(codex) → Spark → [moat + 反証3名] → Magi → verdict card
- Mode: AUTORUN_FULL(verdict card は契約上 interactive)

### Outcome
NO-GO(確信度 H)。候補 Session Ledger は却下。決定記録 → docs/DECISION-001-session-ledger.md
ユーザー選択: No 分岐(監査可能な決定記録として保存、実装せず)

### Decision Ledger
- DEC-8: codex を Flux と懐疑者2に使用しモデル単一文化を回避。初回 2 回は失敗(サンドボックスのプロセス制限 → 無効化で解決、プロンプト位置引数 → stdin 渡しで解決)。以後 codex は stdin + `-o` artifact + フォアグラウンドで実行すること
- DEC-9: Magi が集約規則の機械適用(refuted 1 / unproven 2 → GO-with-flag)を採らず NO-GO。理由は「survived ラベルの中身が全根拠を個別反証していた」— ラベルでなく内容で重み付けた判断としてハブも支持
- DEC-10: ハブが load-bearing な反証2件(less -o の存在、diff.py の比較実体)を独立に検証。1件目は過大主張を含むことも判明し、その区別を決定記録に明記

### 生き残った空白(将来の再評価の起点)
- 対話セッション2本のセルグリッド差分ツールは市場に不在
- 再生器忠実度の機械検証基盤は rless のみ保有(ただし現状は開発内部資産)
