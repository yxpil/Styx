#!/usr/bin/env python3
"""核对 `tags.rs` 里的归组词表，在真实 WD14 词表上到底有没有用。

## 存在的理由

归组词表是**猜**出来的。猜错的代价不是报错，而是静默地把标签分错组——
`hair_style_7` 因为词表里多了一个 `style` 就被分进"画风"，看起来毫无异常。
所以每加一批关键词，都要拿真实的 9083 条词表核一遍。这个脚本就是干这个的。

## 为什么直接解析 `tags.rs` 而不是把词表抄一份过来

因为抄一份就会**失同步**。第一次写这个脚本时我把七组关键词手抄进了 Python，
结果 `tags.rs` 改了两轮之后，脚本里还是旧的——它开始报告一些早已修好的问题，
也漏掉新加的词。检查脚本和被检查的代码不一致时，脚本的结论比没有结论更糟。
现在词表只有一个来源，就是 `tags.rs`。

## 两个不同的判据，别混

- **「这个关键词本身是不是一个标签」** —— 基本不重要。`hair` 不是 moat 的
  标签，但它是 `long_hair` / `white_hair` 的词头，删掉它整组就塌了。
- **「这个关键词能命中多少个真实标签」** —— 这才是判据。命中 0 个就是纯粹的
  死重量，删掉它只会让词表更诚实。
  （例外：这一层是模型无关的，`anime` / `photorealistic` 这类词对
  CLIP 型反推器有用，对 WD14 没用——留着成本为零。）

脚本还会标出**词头跨组**：某个关键词命中的标签里，头名词属于别的组。这一类是
两趟匹配算法（先词头、后任意词）的软肋，需要人看一遍——`animal` 命中
`animal_ears` 就是典型，靠"词头优先"解决了，但解决得对不对得看具体标签。

用法：`python check_vocab.py`
"""

from __future__ import annotations

import csv
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CSV = os.path.join(HERE, "models", "selected_tags.csv")
TAGS_RS = os.path.normpath(
    os.path.join(HERE, "..", "..", "crates", "styx-vision", "src", "tags.rs")
)


# ------------------------------------------------------- 从 tags.rs 读词表

def parse_tag_rules(path: str) -> list[tuple[str, str, list[str]]]:
    """解析 `tags.rs`，返回 `[(常量名, 组名, [关键词...])]`，**保持规则表顺序**。

    顺序很重要：组间优先级就是靠它实现的，报告里按顺序排才能看懂冲突。
    """
    with open(path, encoding="utf-8") as fh:
        src = fh.read()

    # 先把注释剥掉，否则注释里的中文词会被当成关键词
    src = re.sub(r"//[^\n]*", "", src)

    tables: dict[str, list[str]] = {}
    for m in re.finditer(r"const\s+([A-Z_]+)\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\];", src, re.S):
        name, body = m.group(1), m.group(2)
        tables[name] = re.findall(r'"([^"]*)"', body)

    rules_m = re.search(r"const\s+TAG_RULES\s*:\s*&\[\(&\[&str\],\s*TagGroup\)\]\s*=\s*&\[(.*?)\];", src, re.S)
    if not rules_m:
        raise SystemExit(f"在 {path} 里找不到 TAG_RULES，脚本需要跟着改")

    out = []
    for const, group in re.findall(r"\(\s*([A-Z_]+)\s*,\s*TagGroup::([A-Za-z]+)\s*\)", rules_m.group(1)):
        if const not in tables:
            raise SystemExit(f"TAG_RULES 引用了未定义的 {const}")
        out.append((const, group, tables[const]))
    return out


# ------------------------------------------------------------------ 主逻辑

def main() -> int:
    if not os.path.exists(CSV):
        print(f"找不到词表 {CSV}，先跑 `python fetch_models.py --only tagger`")
        return 1

    with open(CSV, encoding="utf-8") as fh:
        names = [r["name"] for r in csv.DictReader(fh)]
    rules = parse_tag_rules(TAGS_RS)
    # 归一化一次，别在 matches 里反复算
    norm_names = [n.replace("-", "_").replace(" ", "_") for n in names]
    norm_tokens = [[t for t in x.split("_") if t] for x in norm_names]

    print(f"词表 {len(names)} 条；tags.rs 里 {len(rules)} 组、"
          f"{sum(len(words) for _, _, words in rules)} 个关键词\n")

    def tokens_of(name):
        return [t for t in name.split("_") if t]

    def word_eq(token, kw):
        """和 tags.rs 的 `word_eq` 一致：整词相等，容忍一个复数 s。"""
        return token == kw or (token.endswith("s") and token[:-1] == kw)

    def matches(kw):
        """这个关键词能命中哪些真实标签。**必须和 tags.rs 的匹配语义一致**。

        两处踩过的坑，都记在这里：

        - 早先只做了整词相等，`upper_body` / `closed_eyes` / `no_humans`
          全被报成"死重量"——它们明明是词组。
        - 词组包含还得拿**归一化之后**的标签名去比。真实标签写作 `close-up`
          （连字符），关键词写作 `close_up`（下划线），直接 `kw in name`
          会漏掉它，于是 `close_up` 被冤枉成死重量。
        """
        if "_" in kw:
            return [n for n, x in zip(names, norm_names) if kw in x]
        return sorted({n for n, toks in zip(names, norm_tokens) for t in toks if word_eq(t, kw)})

    # 关键词 → 所属组，用于判断"词头跨组"
    kw_group: dict[str, str] = {}
    for _, group, words in rules:
        for kw in words:
            kw_group.setdefault(kw, group)

    def head_group(tag: str) -> str | None:
        """按 tags.rs 的第一趟（词组 + 词头）算出这个标签归哪组。

        必须跟着 tags.rs 的规则顺序走：`bikini_shorts` 的词头 `shorts` 应当
        命中 CLOTHING 里的 `shorts`，而不是被剥成 `short` 之后落到 APPEARANCE。
        整词先试、复数后试，顺序反了就会把 Clothing 和 Appearance 判反。
        """
        x = tag.replace("-", "_").replace(" ", "_")
        toks = [t for t in x.split("_") if t]
        head = toks[-1] if toks else ""
        for _, group, words in rules:
            for kw in words:
                if "_" in kw:
                    if kw in x:
                        return group
                elif head == kw or (head.endswith("s") and head[:-1] == kw):
                    return group
        return None

    # ---- 判据一：命中 0 个真实标签的关键词是死重量 ----
    print("=" * 74)
    print("死重量：命中 0 个真实标签的关键词")
    print("=" * 74)
    dead_total = 0
    for const, group, words in rules:
        dead = [kw for kw in words if not matches(kw)]
        dead_total += len(dead)
        print(f"\n{group}（{const}）：{len(words)} 个关键词，{len(dead)} 个命中 0 个标签")
        if dead:
            print("  " + ", ".join(dead))
    total = sum(len(w) for _, _, w in rules)
    print(f"\n合计 {dead_total}/{total} 个死重量。删掉它们不会改变任何行为，")
    print("但会让词表诚实——也可以留着，因为换个反推器可能就用上了。")

    # ---- 判据二：词头跨组 ----
    print()
    print("=" * 74)
    print("词头跨组：关键词命中的标签里，头名词属于**别的组**")
    print("=" * 74)
    print("这两趟匹配（先词头、后任意词）的软肋。看一遍，判断「词头优先」决定得对不对。")
    print("注意：**这一节有输出是正常的**，不是错误清单。绝大多数条目经「词头优先」")
    print("      之后分类是对的（`cat_ears` 归外貌、`bean_bag_chair` 归场景）；")
    print("      真正要看的是修饰语是不是比词头更该决定分组。")
    total_pairs = 0
    for _, group, words in rules:
        for kw in words:
            for n in matches(kw):
                toks = tokens_of(n)
                if not toks or toks[-1] == kw:
                    continue
                if head_group(n) != group:
                    total_pairs += 1
    print(f"\n共 {total_pairs} 处「修饰语关键词命中、但分组由词头决定」。")
    any_found = False
    for const, group, words in rules:
        lines = []
        for kw in words:
            for n in matches(kw):
                toks = tokens_of(n)
                if not toks or toks[-1] == kw:
                    continue  # 这个词就是头名词，不冲突
                decided = head_group(n)
                if decided and decided != group:
                    lines.append((kw, n, toks[-1], decided))
        if lines:
            any_found = True
            print(f"\n--- {group} ---")
            for kw, n, head, decided in sorted(set(lines)):
                print(f"  {kw:14s} 命中 {n:36s} 词头 {head:12s} → 判给 {decided}")
    if not any_found:
        print("  （没有）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
