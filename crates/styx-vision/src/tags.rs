//! # 反向推断出来的属性标签（"提示词反推"）
//!
//! Stable Diffusion 生态里的 interrogator 会把一张图反推成一串提示词：
//! `1girl, solo, long_hair, blue_eyes, school_uniform, smile, indoors,
//! upper_body, anime`。这一层对角色扮演的价值比目标检测高得多——
//! YOLO 只能告诉你"画面里有一个人"，而标签串直接给出**发色、神情、
//! 穿着、构图、画风**，也就是"这个人看到了什么"。
//!
//! ## 但它和 YOLO 是同一个档次的东西：可选、可替换
//!
//! 标签来自一个多标签分类器（WD14 tagger、CLIP 相似度打分、BLIP caption
//! 再切词……），不同实现给出的词表完全不同。所以这里只定义**形状**，
//! 不绑定任何模型：
//!
//! ```text
//!   WD14 tagger（ONNX，约 300MB）    → 二次元向，标签密度高
//!   CLIP 相似度打分（ONNX，约 350MB） → 通用，词表自己写，实拍/插画都行
//!   任意 HTTP 端点                   → 后端给什么用什么
//! ```
//!
//! ## 为什么要分组
//!
//! 反推的结果原本是给 **Stable Diffusion** 吃的一长串，逗号分隔、没有结构。
//! 直接丢给语言模型有两个问题：一是 30 个下划线词糊在一起，模型抓不住重点；
//! 二是不同维度混在一起（人数、发色、构图、画风），模型容易把"构图词"
//! 当成"画面内容"。
//!
//! 所以这里做的核心加工就是**归组 + 限流**：把标签按维度归类，每组只留
//! 分数最高的几个。出来的是一句有结构的话，而不是一串词。
//!
//! ## 诚实的边界
//!
//! 分类器输出的是**概率最高的标签**，不是事实。WD14 这类动漫向模型遇到
//! 实拍照片会把成年女性标成 `1girl`。所以：
//! - 渲染时明说是"自动标注"；
//! - 来源可疑（动漫向）时追加一句提醒；
//! - 措辞一律是"读出的特征"，不是"图里就是"。

use serde::{Deserialize, Serialize};

/// 标签所属的维度。
///
/// 顺序就是渲染顺序，从"最具体的主体"到"最外围的风格"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TagGroup {
    /// 主体与人数：`1girl` / `solo` / `no_humans` / `cat`。
    Subject,
    /// 外貌特征：发色发型、瞳色、体型、年龄感。
    Appearance,
    /// 神情与情绪：`smile` / `crying` / `blush`。
    Expression,
    /// 穿着。
    Clothing,
    /// 场景与背景：室内外、天气、时间、场所、摆设。
    Scene,
    /// 构图与镜头：半身/全身、俯仰、景深、逆光、剪影。
    Composition,
    /// 风格与媒介：动画/写实/水彩/黑白/复古胶片。
    Style,
    /// 关键词没命中任何一组。
    Other,
}

impl TagGroup {
    /// 渲染用的组名。
    pub fn label(self) -> &'static str {
        match self {
            TagGroup::Subject => "主体",
            TagGroup::Appearance => "外貌",
            TagGroup::Expression => "神情",
            TagGroup::Clothing => "穿着",
            TagGroup::Scene => "场景",
            TagGroup::Composition => "构图",
            TagGroup::Style => "画风",
            TagGroup::Other => "其他",
        }
    }

    /// 全部组，按渲染顺序。
    pub const ALL: [TagGroup; 8] = [
        TagGroup::Subject,
        TagGroup::Appearance,
        TagGroup::Expression,
        TagGroup::Clothing,
        TagGroup::Scene,
        TagGroup::Composition,
        TagGroup::Style,
        TagGroup::Other,
    ];
}

/// 一个反推出来的标签。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagHit {
    /// 原始标签（多为 `long_hair` 这种下划线词）。
    pub tag: String,
    /// 中文（词表能给就给；给不了就空，渲染时退回原始标签）。
    #[serde(default)]
    pub tag_zh: String,
    /// 置信度 / 相似度，0..1。
    pub score: f32,
    /// 哪一类模型给的（用于在描述里标注来源，也用于判断要不要提醒）。
    #[serde(default)]
    pub source: String,
}

impl TagHit {
    pub fn new(tag: impl Into<String>, score: f32) -> Self {
        TagHit {
            tag: tag.into(),
            tag_zh: String::new(),
            score: score.clamp(0.0, 1.0),
            source: String::new(),
        }
    }

    pub fn with_zh(mut self, zh: impl Into<String>) -> Self {
        self.tag_zh = zh.into();
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// 给人（和模型）看的写法：中文优先，否则把下划线换空格。
    pub fn display(&self) -> String {
        if !self.tag_zh.trim().is_empty() {
            return self.tag_zh.trim().to_string();
        }
        self.tag.replace('_', " ")
    }

    /// 所属维度。按 [`crate::tags::TAG_RULES`] 的顺序做子串匹配，先命中先算。
    pub fn group(&self) -> TagGroup {
        group_of(&self.tag)
    }

    /// 这个标签是不是来自一个明显偏二次元的模型。
    ///
    /// 先把来源名里的非字母数字全去掉再比。这一步不是洁癖：WD 系列的命名
    /// 五花八门（`wd-v1-4-moat`、`wd14`、`wd_v1_4_convnext`、`WD-1.4`），
    /// 直接子串匹配会**漏掉真实的那一个**——`wd-v1-4-moat` 并不包含
    /// `wd-1`（中间隔着一个 `v`），于是这条提醒在最需要它的时候一声不吭。
    /// 归一化成 `wdv14moat` 之后就稳了。
    pub fn from_anime_tuned_source(&self) -> bool {
        let s: String = self
            .source
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        ["wdv1", "wd14", "danbooru", "waifu", "anime"]
            .iter()
            .any(|k| s.contains(k))
    }
}

// ----------------------------------------------------------------- 归组规则

/// 穿着 —— 放在最前面，因为 `hairband` / `nightgown` 这类词会被
/// 后面的"外貌(hair)"和"场景(night)"抢走。
///
/// 大多写单数：词头匹配容忍一个复数 `s`（见 [`word_eq`]），所以 `boot` 同时
/// 命中 `boot` 和 `boots`，写 `boots` 反而漏掉 `boot`。
///
/// **唯一的例外是 `shorts`**：它的单数 `short` 是"个子矮"，在 `Appearance` 组里。
/// 这里必须留复数——`word_eq` 只从**标签**上剥 `s`、不从关键词上剥，所以
/// 关键词写 `shorts` 时标签 `short` 不会命中，两个意思才分得开。
const CLOTHING: &[&str] = &[
    "hairband",
    "hairbow",
    "hair_ornament",
    "hair_ribbon",
    "hairclip",
    "hairpin",
    "scrunchie",
    "headband",
    "hat",
    "cap",
    "helmet",
    "crown",
    "veil",
    "headphones",
    "nightgown",
    "nightdress",
    "nightwear",
    "shirt",
    "t-shirt",
    "blouse",
    "sweater",
    "cardigan",
    "hoodie",
    "hood",
    "jacket",
    "coat",
    "cloak",
    "cape",
    "vest",
    "suit",
    "dress",
    "gown",
    "skirt",
    "pant",
    "trouser",
    "jean",
    "shorts",
    "uniform",
    "necktie",
    "tie",
    "bowtie",
    "collar",
    "sleeve",
    "scarf",
    "belt",
    "button",
    "badge",
    "brooch",
    "glove",
    "mitten",
    "wristband",
    "earring",
    "necklace",
    "bracelet",
    "boot",
    "shoe",
    "sneaker",
    "sandal",
    "slipper",
    "sock",
    "stocking",
    "pantyhose",
    "thighhigh",
    "apron",
    "kimono",
    "yukata",
    "robe",
    "armor",
    "armour",
    "swimsuit",
    "bikini",
    "ribbon",
    "glasses",
    "sunglasses",
    "mask",
    "backpack",
    "bag",
    "umbrella",
];

/// 主体与人数：画面里的"东西"本身（人 / 动物 / 器物 / 交通工具 / 食物）。
const SUBJECT: &[&str] = &[
    "1girl",
    "2girls",
    "3girls",
    "1boy",
    "2boys",
    "3boys",
    "multiple_girls",
    "multiple_boys",
    "solo",
    "couple",
    "group",
    "people",
    "crowd",
    "person",
    "girl",
    "boy",
    "woman",
    "man",
    "child",
    "baby",
    "male",
    "female",
    "no_humans",
    "cat",
    "dog",
    "puppy",
    "kitten",
    "bird",
    "horse",
    "fish",
    "insect",
    "butterfly",
    "flower",
    "tree",
    "animal",
    "pet",
    "robot",
    "doll",
    "statue",
    "food",
    "fruit",
    "cake",
    "bread",
    "pizza",
    "sushi",
    "ramen",
    "sandwich",
    "candy",
    "ice_cream",
    "coffee",
    "tea",
    "wine",
    "beer",
    "drink",
    "bottle",
    "cup",
    "glass",
    "plate",
    "bowl",
    "spoon",
    "fork",
    "knife",
    "book",
    "letter",
    "paper",
    "pen",
    "pencil",
    "phone",
    "computer",
    "laptop",
    "toy",
    // 实拍照片里最常见的"东西"就是一整排车。少了这些词，一张街景照片在
    // "主体"组里会一个标签都没有，只剩下构图和画风。
    "car",
    "vehicle",
    "ground_vehicle",
    "bicycle",
    "bike",
    "motorcycle",
    "bus",
    "train",
    "truck",
    "boat",
    "ship",
    "airplane",
];

/// 构图与镜头。放在主体之后、风格之前。
const COMPOSITION: &[&str] = &[
    "close-up",
    "close_up",
    "portrait",
    "upper_body",
    "lower_body",
    "full_body",
    "cowboy_shot",
    "wide_shot",
    "from_above",
    "from_below",
    "from_side",
    "from_behind",
    "dutch_angle",
    "facing_viewer",
    "angle",
    "perspective",
    "pov",
    "depth_of_field",
    "blurry",
    "bokeh",
    "motion_blur",
    "silhouette",
    "backlighting",
    "backlight",
    "centered",
    "symmetry",
    "profile",
    "looking_at_viewer",
    "looking_away",
    "looking_back",
    "off_shoulder",
    "cropped",
    "framing",
];

/// 风格与媒介。
///
/// ## `anime` 不在 WD14 的词表里
///
/// 这一条是核过真实词表才发现的：`selected_tags.csv` 的 9083 条里**没有**
/// `anime`、`manga`、`cartoon`、`photorealistic` 这些词。WD14 的题材标签走的是
/// 另一套约定——`official_style`、`retro_artstyle`、`1990s_(style)`、
/// `alphes_(style)`（画师名）。所以拿 WD14 反推二次元图时，"画风"这一组
/// **基本不会出现**，这是模型的性质，不是我们的 bug。
///
/// 那为什么还留着这些词？因为这一层是**模型无关**的（见模块文档）：换成基于
/// CLIP 相似度的反推器，输出的就是 `anime` / `photorealistic` 这类词。留着它们
/// 的成本是零——匹配不到任何标签的关键词不可能抢错组。
const STYLE: &[&str] = &[
    "anime",
    "manga",
    "cartoon",
    "comic",
    "chibi",
    "illustration",
    "drawing",
    "painting",
    "watercolor",
    "oil_painting",
    "sketch",
    "lineart",
    "cel_shading",
    "flat_color",
    "photo",
    "photograph",
    "photographic",
    "realistic",
    "photorealistic",
    "3d",
    "render",
    "cg",
    "digital_art",
    "pixel_art",
    "monochrome",
    "greyscale",
    "grayscale",
    "sepia",
    "vintage",
    "retro",
    "film",
    "polaroid",
    "screenshot",
    "screencap",
    "concept_art",
    "traditional_media",
    "abstract",
    "surreal",
    // 下面这几个是核过词表的：`style` 命中 `indian_style` / `official_style` /
    // `style_parody`，`contrast` / `muted` / `pastel` 也各自命中真实的色调标签。
    "style",
    "muted",
    "pastel",
    "contrast",
];

/// 神情与情绪。
const EXPRESSION: &[&str] = &[
    "smile",
    "grin",
    "smirk",
    "smug",
    "laughing",
    "tears",
    "crying",
    "cry",
    "weeping",
    "frown",
    "angry",
    "anger",
    "annoyed",
    "sad",
    "sorrow",
    "depressed",
    "surprised",
    "surprise",
    "shocked",
    "embarrassed",
    "blush",
    "blushing",
    "shy",
    "scared",
    "afraid",
    "worried",
    "nervous",
    "calm",
    "serious",
    "sleepy",
    "tired",
    "bored",
    "sigh",
    "screaming",
    "open_mouth",
    "closed_mouth",
    "closed_eyes",
    "half-closed_eyes",
    "one_eye_closed",
    "wink",
    "yawning",
    "pout",
    "expression",
    "emotion",
];

/// 外貌特征。
///
/// 一律写**单数**：词头匹配只从**标签**上剥 `s`，所以关键词 `eye` 同时命中
/// `eye` 和 `eyes`，而关键词 `eyes` 漏掉 `mole_under_eye` 这种真实标签。
/// 成对的器官在词表里大多写作复数（`eyes`、`ears`、`shoulders`），
/// 写单数是唯一两头都覆盖的写法。
///
/// `ear` / `fluff` / `fur` 看着奇怪，但不加就有具体后果：`animal_ears` 的
/// 词头是 `ears`，`animal_ear_fluff` 的词头是 `fluff`。这两个词头不在表里，
/// 标签就会掉进"其他"组或者被修饰语 `animal` 拉到"主体"组去。
///
/// 整词相等在这里救了命：`scar`（疤）不会命中 `scarf`（围巾），
/// `ear` 不会命中 `early`。这是不用子串匹配换来的。
const APPEARANCE: &[&str] = &[
    "hair",
    "bangs",
    "ponytail",
    "twintails",
    "braid",
    "bun",
    "mohawk",
    "afro",
    "ahoge",
    "sidelocks",
    "eye",
    "eyebrow",
    "eyelash",
    "pupil",
    "freckles",
    "mole",
    "beard",
    "mustache",
    "skin",
    "complexion",
    "tall",
    "short",
    "chubby",
    "slim",
    "slender",
    "muscular",
    "mature",
    "young",
    "old",
    "elderly",
    "teenage",
    "adult",
    "wrinkle",
    "scar",
    "tattoo",
    "fang",
    "pointy_ears",
    "wing",
    "tail",
    "horn",
    "ear",
    "fluff",
    "fur",
    "navel",
    "collarbone",
    "abs",
    "shoulder",
];

/// 场景与背景。
const SCENE: &[&str] = &[
    "indoors",
    "outdoors",
    "background",
    "room",
    "bedroom",
    "kitchen",
    "bathroom",
    "classroom",
    "office",
    "library",
    "shop",
    "store",
    "cafe",
    "restaurant",
    "bar",
    "street",
    "alley",
    "city",
    "town",
    "village",
    "countryside",
    "building",
    "house",
    "houseplant",
    "sky",
    "cloud",
    "clouds",
    "sunset",
    "sunrise",
    "dusk",
    "dawn",
    "night",
    "day",
    "evening",
    "morning",
    "noon",
    "rain",
    "raining",
    "snow",
    "snowing",
    "fog",
    "mist",
    "wind",
    "storm",
    "forest",
    "woods",
    "mountain",
    "hill",
    "field",
    "garden",
    "park",
    "beach",
    "sea",
    "ocean",
    "lake",
    "river",
    "water",
    "underwater",
    "desert",
    "snowfield",
    "space",
    "starry_sky",
    "moon",
    "sun",
    "stars",
    "window",
    "door",
    "stairs",
    "roof",
    "balcony",
    "bed",
    "desk",
    "chair",
    "table",
    "sofa",
    "shelf",
    "bookshelf",
    "fireplace",
    "candle",
    "lamp",
    "light",
    "shadow",
    "blossom",
    "cherry_blossoms",
    "leaves",
    "grass",
    "moss",
    "ruins",
    "bridge",
    "road",
    "sidewalk",
    "crosswalk",
    "fence",
    "wall",
    "ceiling",
    "floor",
    "traffic_light",
    "sign",
    "curtain",
    "mirror",
    "clock",
];

/// 规则表：`(关键词, 所属组)`，**按顺序匹配，第一个命中的生效**。
///
/// 顺序大部分时候不重要（同一个词不会出现在两组里），但有几处是真冲突，
/// 靠顺序解决：
/// - `shorts`（裤子）与 `short`（个子矮）：`Clothing` 在前，裤子赢；
/// - `glasses`（眼镜）与 `eyes`：前者在 `Clothing`，后者在 `Appearance`；
/// - `Clothing` 排在 `Scene` 前，让 `hairband` 不被外貌词抢走。
///
/// 关键词里带 `_` 的（`no_humans`、`close-up`）按整串包含匹配，其余按整词相等，
/// 细节见 [`group_of`]。
const TAG_RULES: &[(&[&str], TagGroup)] = &[
    (CLOTHING, TagGroup::Clothing),
    (SUBJECT, TagGroup::Subject),
    (COMPOSITION, TagGroup::Composition),
    (STYLE, TagGroup::Style),
    (EXPRESSION, TagGroup::Expression),
    (APPEARANCE, TagGroup::Appearance),
    (SCENE, TagGroup::Scene),
];

/// 判断一个标签属于哪一组。命中不到就是 [`TagGroup::Other`]。
///
/// ## 匹配分两趟：先看词头，再看任意词
///
/// 复合标签里**头名词**（最后一个词）才是决定维度的那个，前面的都是修饰语。
/// `animal_ears` 的 `animal` 是修饰语、`ears` 才是头；只按"任意词命中"匹配的话，
/// `SUBJECT` 里的 `animal` 会先命中，于是猫耳被归进"主体"组——渲染出来就是
/// 「主体：1girl、solo、animal ears」，读起来像是画面里有三样东西。
/// `cat_ears`、`dog_tail`、`sailor_collar`、`long_sleeves`、`ground_vehicle`
/// 全都是同一个毛病。
///
/// 所以：
///
/// 1. **第一趟（词头优先）**：只拿**最后一个词**去比单词关键词。词组关键词
///    （`no_humans`、`closed_eyes`）覆盖整串，天然不受词序影响，也在这一趟比。
/// 2. **第二趟（任意词兜底）**：仍然按任意词匹配，兜住 `shirt_tucked_in` 这种
///    "头名词是介词"的标签。
///
/// 两趟都按 [`TAG_RULES`] 的顺序扫描，所以组间优先级（`Clothing` 排在
/// `Appearance` 之前，让 `hairband` 不被 `hair` 抢走）在每一趟内部都保持。
///
/// ## 为什么不用朴素子串匹配
///
/// `totally` 里含 `tall`、`installing` 里含 `tall`、`metallic` 里含 `tall`……
/// 一个子串规则就能让一堆无关标签挤进"外貌"组，而"外貌"组只有 4 个名额，
/// 挤满之后真正有用的 `long_hair` 反而被挤掉了。
///
/// 词组仍然用包含匹配是必要的：`close-up` 切成词之后是 `close` 和 `up`，
/// 而 `up` 是一个会在几十个标签里出现的噪声词。
///
/// ## 已知的不足（不打算修）
///
/// `X_on_Y` / `X_with_Y` 这一族的词头见词表并不在最后：`coat_on_shoulders`
/// 是"搭在肩上的外套"，词头其实是 `coat`；`hat_with_ears` 是"带耳朵的帽子"，
/// 词头是 `hat`。上面两条规则会把它们判成外貌（因为词头 `shoulders` / `ears`
/// 是外貌词），而不是穿着。
///
/// 想过加一条"遇到介词就把前一个词当词头"的规则，**试算之后放弃了**：
/// 它修好上面这类，同时把 `bags_under_eyes`（眼袋，明显是外貌）判成穿着，
/// 把 `bike_shorts_under_skirt` 判成主体。四个修好、两个弄错，不划算。
///
/// 而且这两类的**后果轻重不同**：`coat on shoulders` 放在哪一组里，
/// 读起来都是"搭在肩上的外套"——信息没丢，只是分组标签不太贴切。相比之下
/// `animal ears` 被算成"主体"是会让模型以为画面里有三样东西的。所以宁可
/// 保留一条能一句话讲清楚的规则（词头在最后一个词），也不加特例。
///
/// `services/vision/check_vocab.py` 会把全部这类标签列出来供人工复核。
pub fn group_of(tag: &str) -> TagGroup {
    let whole = normalize(tag);
    if whole.is_empty() {
        return TagGroup::Other;
    }
    let tokens: Vec<&str> = whole.split('_').filter(|t| !t.is_empty()).collect();
    let head = tokens.last().copied().unwrap_or("");

    // 第一趟：词组按整串包含，单词只认词头。
    for (words, group) in TAG_RULES {
        for w in *words {
            let kw = normalize(w);
            if kw.is_empty() {
                continue;
            }
            let hit = if kw.contains('_') {
                whole.contains(&kw)
            } else {
                word_eq(head, &kw)
            };
            if hit {
                return *group;
            }
        }
    }

    // 第二趟：任意词。只比单词关键词——词组在第一趟已经全查过了。
    for (words, group) in TAG_RULES {
        for w in *words {
            let kw = normalize(w);
            if !kw.is_empty() && !kw.contains('_') && tokens.iter().any(|t| word_eq(t, &kw)) {
                return *group;
            }
        }
    }

    TagGroup::Other
}

/// 整词相等，容忍一个复数 `s`：`eyes` 命中 `eye`、`girls` 命中 `girl`。
///
/// 刻意不做 `starts_with` / `ends_with`：那会把 `tall` 送给 `totally`。
fn word_eq(token: &str, kw: &str) -> bool {
    token == kw || token.strip_suffix('s') == Some(kw)
}

/// 统一大小写与分隔符：`Long Hair` / `long-hair` / `long_hair` 视为同一个词。
fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

// ------------------------------------------------------------------- 加工

/// 低于这个分数的标签直接丢掉。
///
/// 0.35 是个偏严的取值：多标签分类器的输出里，真正在场的属性通常在 0.5
/// 以上，而 0.2~0.35 这一段基本是"词表里相邻的颜色/材质"在互相干扰。
pub const DEFAULT_MIN_SCORE: f32 = 0.35;

/// 每一组最多保留几个。
pub const MAX_PER_GROUP: usize = 4;

/// 合并后最多保留几个（渲染成一句话的长度上限）。
pub const MAX_TAGS: usize = 18;

/// 按分数排序、按标签去重、按组限流。
///
/// 之所以**先排序再按组限流**（而不是先分组再排序）是因为分组是"标签文字
/// 决定的"，而分数才是可信度——应该让高分标签优先占名额。
pub fn prune(tags: &mut Vec<TagHit>, min_score: f32) {
    tags.retain(|t| t.score >= min_score && !t.tag.trim().is_empty());
    tags.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut seen: Vec<String> = Vec::new();
    let mut per_group = std::collections::BTreeMap::new();
    let mut out: Vec<TagHit> = Vec::new();
    for t in tags.iter() {
        let key = t.tag.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        let g = t.group();
        let n = per_group.entry(g).or_insert(0usize);
        if *n >= MAX_PER_GROUP {
            continue;
        }
        *n += 1;
        seen.push(key);
        out.push(t.clone());
        if out.len() >= MAX_TAGS {
            break;
        }
    }
    *tags = out;
}

/// 把标签渲染成一句有结构的话。
///
/// 输出形如：`主体：1girl、solo；外貌：long hair、blue eyes；构图：上半身`。
/// 没有任何标签时返回空串（调用方应当据此整段省略，而不是输出"无标签"）。
pub fn summary(tags: &[TagHit]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for group in TagGroup::ALL {
        let in_group: Vec<String> = tags
            .iter()
            .filter(|t| t.group() == group)
            .map(|t| t.display())
            .collect();
        if !in_group.is_empty() {
            parts.push(format!("{}：{}", group.label(), in_group.join("、")));
        }
    }
    parts.join("；")
}

/// 需要跟着标签一起说出口的注意事项。
///
/// 这些不是"日志"，是要交给语言模型的——它得知道手上这份标签有多可信，
/// 否则会把 `1girl` 当成板上钉钉的事实讲给用户。
pub fn caveats(tags: &[TagHit]) -> Vec<String> {
    let mut out = Vec::new();
    if tags.is_empty() {
        return out;
    }
    if tags.iter().any(|t| t.from_anime_tuned_source()) {
        let src = tags
            .iter()
            .find(|t| t.from_anime_tuned_source())
            .map(|t| t.source.clone())
            .unwrap_or_default();
        out.push(format!(
            "这些标签来自偏向二次元图像的模型（{src}），用在真实照片上可能把人或物认错"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(tag: &str, score: f32) -> TagHit {
        TagHit::new(tag, score)
    }

    #[test]
    fn groups_cover_the_usual_tagger_vocabulary() {
        let cases = [
            ("1girl", TagGroup::Subject),
            ("solo", TagGroup::Subject),
            ("no_humans", TagGroup::Subject),
            ("cat", TagGroup::Subject),
            ("long_hair", TagGroup::Appearance),
            ("blue_eyes", TagGroup::Appearance),
            ("smile", TagGroup::Expression),
            ("crying", TagGroup::Expression),
            ("school_uniform", TagGroup::Clothing),
            ("white_shirt", TagGroup::Clothing),
            ("indoors", TagGroup::Scene),
            ("night", TagGroup::Scene),
            ("cherry_blossoms", TagGroup::Scene),
            ("upper_body", TagGroup::Composition),
            ("depth_of_field", TagGroup::Composition),
            ("anime", TagGroup::Style),
            ("monochrome", TagGroup::Style),
            ("totally_unknown_thing", TagGroup::Other),
        ];
        for (tag, want) in cases {
            assert_eq!(group_of(tag), want, "{tag} 应当属于 {}", want.label());
        }
    }

    #[test]
    fn clothing_wins_over_hair_and_night() {
        // 这几个是分词匹配最容易翻车的地方，单独锁住
        assert_eq!(group_of("hairband"), TagGroup::Clothing);
        assert_eq!(group_of("nightgown"), TagGroup::Clothing);
        assert_eq!(group_of("shorts"), TagGroup::Clothing, "裤子，不是个子矮");
        // 而正常的头发/夜晚/个子矮仍然归到各自那组
        assert_eq!(group_of("long_hair"), TagGroup::Appearance);
        assert_eq!(group_of("night"), TagGroup::Scene);
        assert_eq!(group_of("short"), TagGroup::Appearance);
    }

    #[test]
    fn a_word_inside_another_word_does_not_count() {
        // 回归：早先用朴素子串匹配时，`totally` 里的 `tall` 会把它扫进"外貌"，
        // 于是外貌组 4 个名额被无关标签占满，真正的 long_hair 反而被挤掉。
        assert_eq!(group_of("totally_unknown_thing"), TagGroup::Other);
        assert_eq!(group_of("installing"), TagGroup::Other);
        assert_eq!(group_of("metallic"), TagGroup::Other);
        // 但真正的词仍然要命中
        assert_eq!(group_of("tall"), TagGroup::Appearance);
    }

    #[test]
    fn the_head_noun_decides_which_group_a_compound_belongs_to() {
        // 回归：只做"任意词命中"时，修饰语会抢走整组。
        // `animal_ears` 的 `animal` 先命中 SUBJECT，于是猫耳被算成"主体"——
        // 渲染出来是「主体：1girl、solo、animal ears」，读起来像画面里有三样东西。
        assert_eq!(
            group_of("animal_ears"),
            TagGroup::Appearance,
            "耳朵是外貌，animal 只是修饰语"
        );
        assert_eq!(group_of("cat_ears"), TagGroup::Appearance);
        assert_eq!(group_of("animal_ear_fluff"), TagGroup::Appearance);
        assert_eq!(group_of("dog_tail"), TagGroup::Appearance);
        // 同一条规则也救了 `ground` 与 `collar` / `sleeve`
        assert_eq!(
            group_of("ground_vehicle"),
            TagGroup::Subject,
            "词头是 vehicle，不是 ground"
        );
        assert_eq!(group_of("sailor_collar"), TagGroup::Clothing);
        assert_eq!(group_of("long_sleeves"), TagGroup::Clothing);
    }

    #[test]
    fn a_whole_tag_phrase_still_beats_the_head_noun() {
        // `closed_eyes` 的词头是 `eyes`（外貌词），但它整体说的是神情。
        // 词组关键词覆盖整串，所以在"词头优先"这一趟里就先命中了 EXPRESSION。
        assert_eq!(group_of("closed_eyes"), TagGroup::Expression);
        assert_eq!(group_of("half-closed_eyes"), TagGroup::Expression);
        assert_eq!(group_of("one_eye_closed"), TagGroup::Expression);
        // 而蓝眼睛确实是外貌，不能被一起卷走
        assert_eq!(group_of("blue_eyes"), TagGroup::Appearance);
        assert_eq!(group_of("upper_body"), TagGroup::Composition);
        assert_eq!(group_of("close-up"), TagGroup::Composition);
    }

    #[test]
    fn the_any_token_pass_catches_compounds_whose_head_is_a_function_word() {
        // 词头是介词/方位词的真实标签，第一趟（词组 + 词头）必然落空，
        // 只能靠第二趟的任意词兜住。这几个都核过 `selected_tags.csv`。
        assert_eq!(group_of("sleeves_rolled_up"), TagGroup::Clothing);
        assert_eq!(group_of("sleeves_past_wrists"), TagGroup::Clothing);
        assert_eq!(group_of("shirt_tucked_in"), TagGroup::Clothing);
        assert_eq!(group_of("swimsuit_under_clothes"), TagGroup::Clothing);
        assert_eq!(group_of("food_on_face"), TagGroup::Subject);
        assert_eq!(
            group_of("image_sample"),
            TagGroup::Other,
            "谁都不沾边就是其他"
        );
    }

    /// 锁住**已知的不足**，而不是锁住"正确行为"。
    ///
    /// `coat_on_shoulders` / `hat_with_ears` 的实际词头是 `coat` / `hat`，
    /// 但我们的规则取最后一个词，所以判成了外貌。这是有意接受的取舍
    /// （试算过"遇介词改词头"的规则，修四个错两个，详见 [`group_of`] 的文档）。
    ///
    /// 断言"错的那一侧"是有意的：哪天有人改了匹配规则，这个测试会亮，
    /// 提醒他先回来读一遍那段取舍说明，而不是悄悄地把它改成另一个样子。
    #[test]
    fn the_known_preposition_compounds_are_grouped_head_finally() {
        assert_eq!(group_of("coat_on_shoulders"), TagGroup::Appearance);
        assert_eq!(group_of("hat_with_ears"), TagGroup::Appearance);
        // 而真正要紧的那些复合词是判对的——这才是引入词头优先的原因
        assert_eq!(group_of("animal_ears"), TagGroup::Appearance);
        assert_eq!(group_of("cat_ears"), TagGroup::Appearance);
        assert_eq!(group_of("bikini_shorts"), TagGroup::Clothing);
        assert_eq!(group_of("bean_bag_chair"), TagGroup::Scene);
    }

    #[test]
    fn singular_keywords_also_catch_the_plural_tags() {
        // 词表里成对的器官几乎都写作复数，但 `mole_under_eye` 这种又写单数。
        // 关键词一律写单数就两头都覆盖——因为复数容差只从**标签**上剥 `s`。
        assert_eq!(group_of("eyes"), TagGroup::Appearance, "复数标签");
        assert_eq!(group_of("mole_under_eye"), TagGroup::Appearance, "单数词头");
        assert_eq!(group_of("body_fur"), TagGroup::Appearance);
        // 而整词相等保证 `scar`（疤）不会顺手命中 `scarf`（围巾）
        assert_eq!(group_of("scar"), TagGroup::Appearance);
        assert_eq!(group_of("scarf"), TagGroup::Clothing);
    }

    #[test]
    fn the_things_that_show_up_in_real_photographs_are_covered() {
        // 真实照片里最常出现的就是一整排车和一条街。少了这些词，一张街景
        // 在"主体"组里会一个标签都没有——只剩构图和画风，等于没说看到了什么。
        // 下面这些词全部核过 `selected_tags.csv`，都是真实标签。
        for tag in [
            "car",
            "ground_vehicle",
            "bicycle",
            "bus",
            "train",
            "airplane",
        ] {
            assert_eq!(group_of(tag), TagGroup::Subject, "{tag} 应当是主体");
        }
        for tag in ["road", "sidewalk", "traffic_light", "sign", "fence"] {
            assert_eq!(group_of(tag), TagGroup::Scene, "{tag} 应当是场景");
        }
    }

    #[test]
    fn the_real_wd14_style_tags_are_matched() {
        // 回归：早先因为词表里多了一个 `style`，测试里那个编出来的
        // `hair_style_7` 被分进了"画风"。核过真实词表之后才确认：`style`
        // 命中的是 `indian_style` / `official_style` / `style_parody`，
        // 这三个**确实**是画风，所以关键词该留。真正编出来的是那个测试标签。
        //
        // 顺带锁住一个反例：`doggystyle` 和 `alternate_hairstyle` 里含 `style`
        // 这几个字母，但整词匹配要求 `style` 自己是一个词，所以不会误命中。
        assert_eq!(group_of("indian_style"), TagGroup::Style);
        assert_eq!(group_of("official_style"), TagGroup::Style);
        assert_eq!(group_of("doggystyle"), TagGroup::Other);
        assert_eq!(group_of("alternate_hairstyle"), TagGroup::Other);
    }

    #[test]
    fn separators_and_case_are_normalised() {
        assert_eq!(group_of("Long Hair"), TagGroup::Appearance);
        assert_eq!(group_of("long-hair"), TagGroup::Appearance);
        assert_eq!(group_of("LONG_HAIR"), TagGroup::Appearance);
        // 词组关键词要按整串匹配，切成词就散了
        assert_eq!(group_of("close-up"), TagGroup::Composition);
        assert_eq!(group_of("no_humans"), TagGroup::Subject);
    }

    #[test]
    fn plurals_still_hit_their_singular_keyword() {
        assert_eq!(group_of("cherry_blossoms"), TagGroup::Scene);
        assert_eq!(group_of("clouds"), TagGroup::Scene);
    }

    #[test]
    fn an_empty_tag_is_other_not_a_panic() {
        assert_eq!(group_of(""), TagGroup::Other);
        assert_eq!(group_of("___"), TagGroup::Other);
    }

    #[test]
    fn low_confidence_tags_are_dropped() {
        let mut tags = vec![t("smile", 0.9), t("cat", 0.2)];
        prune(&mut tags, DEFAULT_MIN_SCORE);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "smile");
    }

    #[test]
    fn duplicates_and_group_flooding_are_both_stopped() {
        let mut tags = vec![
            t("smile", 0.9),
            t("SMILING", 0.8), // 不同写法不当成重复（它确实是另一个词）
            t("smile", 0.7),   // 真重复
        ];
        // 用真实的发色标签把"外貌"组塞满 10 个，最后只该留 MAX_PER_GROUP 个。
        // 特意用真实标签而不是编一点的 `hair_x_{i}`：编出来的标签分类对不对
        // 无从验证，而"发色属于外貌"是可以拿去和词表对的。
        for color in [
            "blonde", "brown", "black", "blue", "pink", "purple", "red", "green", "grey", "white",
        ] {
            tags.push(t(&format!("{color}_hair"), 0.5));
        }
        prune(&mut tags, DEFAULT_MIN_SCORE);
        assert_eq!(tags.iter().filter(|x| x.tag == "smile").count(), 1);
        let appearance = tags
            .iter()
            .filter(|x| x.group() == TagGroup::Appearance)
            .count();
        assert_eq!(appearance, MAX_PER_GROUP);
        assert!(tags.len() <= MAX_TAGS);
    }

    #[test]
    fn highest_scores_get_the_slots() {
        let mut tags = vec![
            t("hair_a", 0.4),
            t("hair_b", 0.95),
            t("hair_c", 0.8),
            t("hair_d", 0.6),
            t("hair_e", 0.3),
        ];
        prune(&mut tags, DEFAULT_MIN_SCORE);
        let kept: Vec<&str> = tags.iter().map(|x| x.tag.as_str()).collect();
        assert_eq!(kept, vec!["hair_b", "hair_c", "hair_d", "hair_a"]);
    }

    #[test]
    fn summary_is_grouped_and_readable() {
        let tags = vec![
            t("1girl", 0.95),
            t("solo", 0.92),
            t("long_hair", 0.9),
            t("smile", 0.85),
            t("school_uniform", 0.8),
            t("indoors", 0.7),
        ];
        let s = summary(&tags);
        assert!(s.contains("主体：1girl、solo"), "高分在前、组内用顿号：{s}");
        assert!(s.contains("外貌：long hair"), "下划线要变成空格：{s}");
        assert!(s.contains("神情：smile"), "{s}");
        assert!(s.contains("穿着：school uniform"), "{s}");
        assert!(s.contains("场景：indoors"), "{s}");
        // 组之间用分号
        assert!(s.contains("；"), "{s}");
        // 渲染顺序按 TagGroup::ALL：主体在场景之前
        assert!(s.find("主体").unwrap() < s.find("场景").unwrap(), "{s}");
    }

    #[test]
    fn chinese_label_wins_when_the_vocabulary_has_one() {
        let tags = vec![TagHit::new("blue_eyes", 0.9).with_zh("蓝眼睛")];
        assert!(summary(&tags).contains("蓝眼睛"));
    }

    #[test]
    fn an_empty_tag_list_renders_nothing() {
        assert_eq!(summary(&[]), "");
        assert!(caveats(&[]).is_empty());
    }

    #[test]
    fn anime_tuned_sources_come_with_a_warning() {
        let tags = vec![TagHit::new("1girl", 0.9).with_source("wd14-tagger")];
        let c = caveats(&tags);
        assert_eq!(c.len(), 1);
        assert!(c[0].contains("二次元"), "{:?}", c);
        // 通用来源不该触发这条提醒
        let generic = vec![TagHit::new("1girl", 0.9).with_source("clip")];
        assert!(caveats(&generic).is_empty());
    }

    #[test]
    fn the_warning_fires_for_the_real_world_model_names() {
        // 回归：这条提醒曾经对**真实用着的那几个模型**全都哑火——
        // `wd-v1-4-moat` 里并不含子串 `wd-1`（中间隔着一个 v），
        // 于是"这模型偏二次元"这个唯一的诚实提示从未出现过。
        for name in [
            "wd-v1-4-moat",
            "wd-v1-4-convnext-tagger-v2",
            "wd_v1_4_swinv2",
            "WD14",
            "wd-1.4",
            "deepdanbooru",
        ] {
            assert!(
                TagHit::new("1girl", 0.9)
                    .with_source(name)
                    .from_anime_tuned_source(),
                "{name} 应当被判为二次元向模型"
            );
        }
        for name in ["clip", "blip", "tagger", "my-own-model", "siglip"] {
            assert!(
                !TagHit::new("cat", 0.9)
                    .with_source(name)
                    .from_anime_tuned_source(),
                "{name} 不该被误判"
            );
        }
    }
}
