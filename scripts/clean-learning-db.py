#!/usr/bin/env python3
"""学習DB(ime-learning.db)の断片汚染を掃除するメンテナンス用スクリプト。

過去に誤変換のまま確定してしまうと、その誤りが学習され「使うほど悪化」する
ことがある（例: かんじ→カン時、たいしょう→タイ賞）。本体には再汚染を防ぐ
ガードが入っているが、ガード導入前に溜まった汚染はこれで一括除去できる。

使い方:
    python scripts/clean-learning-db.py            # プロジェクト直下の ime-learning.db
    python scripts/clean-learning-db.py path/to.db # パス指定
    python scripts/clean-learning-db.py --dry-run  # 削除せず内容だけ表示

削除対象（断片・カタカナ化の誤学習）:
  - ユニグラム: 1文字の読み→別表記（じ→時 等）
  - ユニグラム: 短い読み(≤3)→そのカタカナ化（かん→カン 等）
  - バイグラム/連想: 短い全カタカナ(≤3)の断片を含む行
"""
import sqlite3
import sys
from pathlib import Path


def katakana(s: str) -> str:
    return "".join(chr(ord(c) + 0x60) if 0x3041 <= ord(c) <= 0x3096 else c for c in s)


def is_short_katakana(surf: str) -> bool:
    return (
        bool(surf)
        and 1 <= len(surf) <= 3
        and all(0x30A1 <= ord(c) <= 0x30FA or c == "ー" for c in surf)
    )


def is_polluted_unigram(reading: str, surface: str) -> bool:
    if len(reading) == 1 and reading != surface:
        return True
    if len(reading) <= 3 and surface == katakana(reading):
        return True
    return False


def main() -> int:
    args = [a for a in sys.argv[1:]]
    dry = "--dry-run" in args
    args = [a for a in args if a != "--dry-run"]
    db_path = Path(args[0]) if args else Path("ime-learning.db")
    if not db_path.exists():
        print(f"DBが見つかりません: {db_path}")
        return 1

    con = sqlite3.connect(str(db_path))
    removed = 0

    uni = con.execute("SELECT reading,surface,frequency FROM conversion_history").fetchall()
    for reading, surface, freq in uni:
        if is_polluted_unigram(reading, surface):
            print(f"  [uni] {reading} -> {surface} ({freq})")
            removed += 1
            if not dry:
                con.execute(
                    "DELETE FROM conversion_history WHERE reading=? AND surface=?",
                    (reading, surface),
                )

    for tbl, a, b in [
        ("word_bigram", "prev_surface", "surface"),
        ("word_assoc", "prev_content", "content"),
    ]:
        for x, y, freq in con.execute(f"SELECT {a},{b},frequency FROM {tbl}").fetchall():
            if is_short_katakana(x) or is_short_katakana(y):
                print(f"  [{tbl}] {x} -> {y} ({freq})")
                removed += 1
                if not dry:
                    con.execute(f"DELETE FROM {tbl} WHERE {a}=? AND {b}=?", (x, y))

    if not dry:
        con.commit()
    print(f"\n{'検出' if dry else '削除'}: {removed} 件  ({db_path})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
