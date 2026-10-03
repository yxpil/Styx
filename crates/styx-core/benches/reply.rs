//! `Reply::parse` 的基准。
//!
//! 选它的理由：**每个回合都要跑一次，而且跑在关键路径上**。模型吐出的
//! 原文先经过它，才能变成台词 / 动作 / 内心 / 记忆。它慢一点点，
//! 用户等待的每一秒里都有这一份。
//!
//! 基准里刻意混进了四类"退化输入"（见下），因为真实模型不会只吐规整
//! 格式——退化路径上的分支（剥引号、并段落、过滤回显）才是这条路上
//! 真正会变慢的地方，只测理想输入等于没测。

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use styx_core::Reply;

/// 规整输出：标签都在行首，一行一个通道。
const CLEAN: &str = "\
[做] 把相册合上，放回原位
[说] 不卖。
[说] 这本来就不是拿来卖的。
[想] 他明天还会来
[忆] 母亲留下了一张旧照片 | 标签=照片,母亲 | 重要度=0.85
";

/// 退化一：模型自己套了一层"说话人 + 引号"。
const QUOTED: &str = "\
[做] 林夏：「不卖。」
[说] 陈默：「我只是看看。」

[忆] 某件事 | 标签=a | 重要度=0.8
";

/// 退化二：标签和正文分了两行。
const SPLIT: &str = "\
[想]
他明天还会来
[忆]
旧照片 | 标签=照片 | 重要度=0.7
";

/// 退化三：完全放弃标签，退回剧本排版。
const SCREENPLAY: &str = "\
（她把相册合上，放回原位）
林夏：「不卖。」
（林夏心想：他明天还会来）
";

fn bench_reply_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("reply_parse");

    for (name, text) in [
        ("clean", CLEAN),
        ("quoted_speaker", QUOTED),
        ("tag_split_from_body", SPLIT),
        ("screenplay_fallback", SCREENPLAY),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let reply = Reply::parse(black_box(text)).expect("应当能解析");
                // 把解析结果也读一遍：否则优化器可能把整个解析当成死代码。
                black_box(reply.speech.len() + reply.actions.len() + reply.thoughts.len())
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_reply_parse);
criterion_main!(benches);
