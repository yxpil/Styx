# tools

主仓库自己的开发工具，不参与构建，也不进任何镜像。

| 文件 | 作用 |
|---|---|
| `sync_vision_repo.py` | 把视觉部分物化成独立的 `styx-vision` 仓库 |
| `vision-repo/root-README.md` | 镜像仓库的根 `README.md`（**不是**给本仓库看的文档） |
| `vision-repo/ci.yml` | 镜像仓库的 CI 配置，落到 `.github/workflows/ci.yml` |

## 为什么视觉要单独发一个仓库

因为「让语言模型看懂一张图」这件事本身可以独立使用——不需要角色扮演内核。
但主仓库又必须包含全部内容（裸克隆就该是完整的、能编译的、不依赖
`--recursive`）。所以事实来源留在主仓库，`sync_vision_repo.py` 负责把子集
复制出去。

## 为什么镜像的 README / CI 放在这个目录里

它们是**镜像专用**的文件：镜像的首页要讲「你从哪来、去哪改」，而主仓库里
`crates/styx-vision/` 下面显然不该出现一份讲镜像的 README。放在这里，
既进了版本控制，又不会被误当成主仓库的文档。

文件名（`root-README.md`）与落点（`README.md`）刻意不同名，就是为了让人在
主仓库里一眼看出「这不是一份普通文档」。
