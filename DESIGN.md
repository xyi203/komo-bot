---
name: komo
description: 树荫下的账本：墨与纸记账，叶绿只标出此刻在场与正在运行的东西。
colors:
  ground: "#f2f3ef"
  surface: "#ffffff"
  raised: "#f1f2ee"
  active: "#e3e6de"
  hover: "#e9ebe5"
  hover-on-white: "#f3f4f0"
  border: "#e4e6df"
  rule: "#eef0ea"
  control: "#d3d7cc"
  text: "#22281c"
  muted: "#565d4e"
  faint: "#636a5a"
  primary: "#20251b"
  primary-hover: "#343b2c"
  accent: "#4f6b38"
  accent-pale: "#e7eedd"
  negative: "#b3261e"
  negative-pale: "#fdf1f0"
  negative-ring: "#f1c9c5"
  pill-bg: "#eef0ea"
  pill-ink: "#3f4637"
  pill-wait-bg: "#fbf3d6"
  pill-wait-ink: "#6b4f0a"
  pill-bad-bg: "#fdecea"
  pill-bad-ink: "#8a1c15"
  wait-dot: "#c98a12"
  memory-off: "#a9ae9f"
  focus: "hsl(212 100% 48%)"
typography:
  page-title:
    fontFamily: "Noto Sans, Noto Sans SC, PingFang SC, system-ui, sans-serif"
    fontSize: "24px"
    fontWeight: 600
    lineHeight: "32px"
    letterSpacing: "-0.01em"
  dialog-title:
    fontFamily: "Noto Sans, Noto Sans SC, PingFang SC, system-ui, sans-serif"
    fontSize: "18px"
    fontWeight: 600
    lineHeight: "26px"
  section-title:
    fontFamily: "Noto Sans, Noto Sans SC, PingFang SC, system-ui, sans-serif"
    fontSize: "16px"
    fontWeight: 600
    lineHeight: "24px"
  read:
    fontFamily: "Noto Sans, Noto Sans SC, PingFang SC, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 400
    lineHeight: "26px"
  body:
    fontFamily: "Noto Sans, Noto Sans SC, PingFang SC, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: "20px"
  label:
    fontFamily: "Noto Sans, Noto Sans SC, PingFang SC, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: "20px"
  small:
    fontFamily: "Noto Sans, Noto Sans SC, PingFang SC, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: "18px"
  mono:
    fontFamily: "Geist Mono, Noto Sans Mono, ui-monospace, monospace"
    fontSize: "0.93em"
    fontWeight: 400
rounded:
  xs: "4px"
  sm: "6px"
  control: "8px"
  inner: "10px"
  card: "12px"
  dialog: "16px"
  pill: "999px"
spacing:
  "1": "4px"
  "2": "8px"
  "3": "12px"
  "4": "16px"
  "5": "20px"
  "6": "24px"
  "7": "28px"
  "8": "32px"
components:
  button-primary:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.surface}"
    typography: "{typography.label}"
    rounded: "{rounded.control}"
    padding: "0 12px"
    height: "32px"
  button-primary-hover:
    backgroundColor: "{colors.primary-hover}"
  button-secondary:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
    padding: "0 12px"
    height: "32px"
  button-secondary-hover:
    backgroundColor: "{colors.hover-on-white}"
  button-send:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.surface}"
    rounded: "{rounded.pill}"
    size: "32px"
  input:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
    padding: "8px 10px"
    height: "36px"
  composer:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text}"
    typography: "{typography.read}"
    rounded: "{rounded.dialog}"
    padding: "6px"
  card:
    backgroundColor: "{colors.surface}"
    rounded: "{rounded.card}"
    padding: "16px"
  pill:
    backgroundColor: "{colors.pill-bg}"
    textColor: "{colors.pill-ink}"
    typography: "{typography.small}"
    rounded: "{rounded.pill}"
    padding: "0 7px"
    height: "20px"
  pill-wait:
    backgroundColor: "{colors.pill-wait-bg}"
    textColor: "{colors.pill-wait-ink}"
  nav-item:
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
    padding: "0 8px"
    height: "34px"
  nav-item-hover:
    backgroundColor: "{colors.hover}"
  nav-item-active:
    backgroundColor: "{colors.active}"
  tab-selected:
    backgroundColor: "{colors.active}"
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
    height: "30px"
  switch-on:
    backgroundColor: "{colors.primary}"
    rounded: "{rounded.card}"
    width: "40px"
    height: "24px"
  switch-off:
    backgroundColor: "{colors.control}"
  bubble-user:
    backgroundColor: "{colors.raised}"
    textColor: "{colors.text}"
    typography: "{typography.read}"
    rounded: "{rounded.dialog}"
    padding: "8px 14px"
---

# Design System: komo

产品事实见 [PRODUCT.md](PRODUCT.md)；本文只记录**已经建成**的东西，不写打算做的事。界面与交互的规格在 [docs/web.md](docs/web.md)，以后的客户端方案在 [docs/clients.md](docs/clients.md)。token 的实现在 [apps/web/styles.css](apps/web/styles.css) 的 `:root`，本文头部的 YAML 与它一一对应。

## Overview

**Creative North Star: "树荫下的账本"**

komo 的世界叫 **komorebi（木漏れ日）**：树叶间漏下的阳光。界面是一本放在树荫下的账本，墨与纸，记得清楚、经得起核对。光只从叶缝里漏下几处，**叶绿永远表示"此刻在场"或"正在运行"**，比如这一轮带上的记忆、上下文的占用、在跑的任务。光不是装饰，它的位置本身就是信息。小片刻攒成恒久之物，正是记忆随时间积累。

布局照 Claude 桌面版：左边带文字的侧栏，中间是对话，右栏可以折叠。密度是工作台级别的，字号 12–15，行高宽松，没有大标题区，也没有营销式留白。人格是"树荫下安静的朋友"：不惊扰，动效只做反馈，状态写实，数字不粉饰。

**动效人格。** 曲线统一用 `--ease: cubic-bezier(0.16, 1, 0.3, 1)`（指数缓出，没有回弹）。控件反馈 120ms（`--t`），抽屉 220ms，弹窗淡入 180ms。只有折叠、抽屉、弹窗出现和转圈这几种动效，没有入场动画；`prefers-reduced-motion` 下全部关掉。流式回复的光标是一个叶绿竖条，1s 呼吸一次。

**Key Characteristics:**
- 无彩的墨与纸，只用一种色相：叶绿。
- 边线画在阴影层，1px 零模糊的环，不用 `border`。
- 机器的名字（变量名、模型 id、路径）用等宽字，和中文正文一眼分开。
- 光点按记忆离 prompt 的远近编码，从实心到空心到灰。
- 一份浅色主题，没有暗色。

## Colors

一叠低彩度的鼠尾草灰纸，配苔墨正文，外加一种叶绿。

### Primary
- **苔墨** (`#20251b`, `--primary`)：唯一的实心按钮底色，用在发送、主操作、开关打开。悬停时变成 `#343b2c`。
- **叶绿** (`#4f6b38`, `--accent`)：唯一的色相，只落在"在场"与"运行中"上，见下面的叶隙光规则。淡叶绿 (`#e7eedd`, `--accent-pale`) 只用作转圈的轨道。

### Neutral
- **林下纸** (`#f2f3ef`, `--ground`)：侧栏、设置弹窗左栏、编辑中的 provider 面板。一级离白，低彩度。
- **白纸** (`#ffffff`, `--surface`)：对话、右栏、卡片、弹窗。
- **浅纸** (`#f1f2ee`, `--raised`)：用户气泡、代码块、键值井、行内 `code`。
- **按下** (`#e3e6de`, `--active`)：当前项（会话、导航、标签、选中的行）、上下文条的空轨。
- **悬停** (`#e9ebe5`, `--hover`)：用在纸色底上；白底上用 `#f3f4f0`（`--hover-on-white`）。
- **线** (`#e4e6df`, `--border`) 与 **细线** (`#eef0ea`, `--rule`)：前者是区块分界，后者是列表行之间。
- **控件灰** (`#d3d7cc`, `--control`)：关着的开关、虚线框、树状连线、"不再带"的光点。
- **苔墨正文** (`#22281c`, `--text`)；**次要** (`#565d4e`, `--muted`)；**更淡** (`#636a5a`, `--faint`)。三档都对白底过 4.5:1。

### 状态色
- **失败** (`#b3261e`, `--negative`)：错误文字、危险按钮的描边、待处理计数的底。危险悬停底 `#fdf1f0`（`--negative-pale`），危险框的环 `#f1c9c5`（`--negative-ring`），出错药丸是 `#fdecea` 底配 `#8a1c15` 字（`--pill-bad-*`）。
- **在等**：琥珀浅底 `#fbf3d6` 配深琥珀字 `#6b4f0a`（`--pill-wait-*`）；琥珀点 `#c98a12`（`--wait-dot`）表示缺凭证、要登录、在重试。
- **焦点蓝** (`hsl(212 100% 48%)`, `--focus`)：只给键盘焦点用。

### Named Rules
**叶隙光规则。** 叶绿只表示"在场"和"运行中"。品牌装饰、头像、链接、选中态、完成反馈、开关打开都不用叶绿。开关是"选择"，不是"在场"，所以打开时是墨色；回复里的链接是墨色字配 `--faint` 下划线，悬停时下划线变实。

**一种色相规则。** 状态底色都是浅底配深字成对出现。上下文构成的分段条只用同一色相的三档叶绿加几级灰，不引入第二种色相。

## Typography

**正文字体：** Noto Sans + Noto Sans SC（回退 PingFang SC、system-ui）
**等宽字体：** Geist Mono（回退 Noto Sans Mono、ui-monospace；只载了 400 / 500）

**性格：** 一套无衬线把中英文压在同一条基线上。中文不收紧字距，中英文之间由 `text-autospace` 自动留空。

### Hierarchy
- **页标题**（600，24/32，-0.01em）：记忆、事项、定时、待处理页的 h1。
- **弹窗标题**（600，18/26）：设置弹窗左栏标题与各节标题。
- **节标题**（600，16/24）：图表标题、列表头、详情标题、窄屏右栏标题。
- **阅读**（400，15/26）：komo 的回复正文；用户气泡是 15/24，输入框是 15/20。阅读列宽上限 720px。
- **正文**（400，14/20）：界面默认字号。
- **标签**（13/20）：次要控件、表格数值、提示；可点的用 500。
- **小字**（400，12/18）：时间、计数、第二行说明。

### Named Rules
**三档字重规则。** 只用三档字重：400 用来读，500 表示可点，600 用于标题。`strong` 也压到 600。

**机器名字规则。** 环境变量名、模型 id、端点、路径、句柄、配置键都用 Geist Mono（`.mono`，0.93em）。等宽字体用作标题时只用 500，不让浏览器合成假粗体。

**等宽数位规则。** 数字一律用等宽数位（`.num`，`tabular-nums`），列里的数右对齐。

## Layout

- **外框**（1440 宽）：侧栏 280 + 主区 1160。主区是对话（`1fr`，阅读列 720 居中）加右栏 336（卡片 304，两侧各 16）。右栏收起时对话独占主区，阅读列仍是 720 居中。右栏开关记住状态。
- **页面**（记忆、事项、定时、待处理）：内边距 28 / 32，主体是列表 `1fr` 加详情 360，间距 24。详情在宽屏贴顶常驻；没有选中时，定时与待处理页由列表独占。
- **设置弹窗**：固定 960 × 680，左栏 216 用纸色，右栏只在内部滚动。节标题吸顶，换节时外框不动。
- **断点**：
  - ≤1180：右栏变右侧抽屉，默认收起，带遮罩；页面详情推成一层，左上角有返回。
  - ≤900：侧栏变左侧抽屉。
  - ≤600：手机。左右边距 16；设置弹窗铺满，分节变成顶上横滑的药丸；表格行变两行；触屏上的图标按钮放大到 44。
- **节奏**：以 4 为步长，常用 4 / 8 / 12 / 16 / 20 / 24 / 28 / 32。卡片间距 12，列表行高 34（侧栏）或 12 的上下内边距（表格行）。

## Elevation & Depth

深度靠阴影层表达，分三级。纸色与白色两层底本身就构成第一层分层：侧栏是纸，工作区是白纸。

### Shadow Vocabulary
- **环** (`--ring: 0 0 0 1px rgb(34 40 28 / 0.12)`)：卡片、标签、预设卡、键值对的边线。
- **控件环** (`--ring-control: 0 0 0 1px rgb(34 40 28 / 0.2)`)：按钮、输入框、下拉框，比卡片深一级。
- **墨环** (`--ring-strong: 0 0 0 1px var(--text)`)：输入框聚焦、预设被选中。
- **轻抬** (`--lift: var(--ring), 0 2px 2px rgb(34 40 28 / 0.04)`)：所有卡片。
- **浮起**：输入框是 `0 2px 8px / 0.05`；菜单是 `0 8px 24px / 0.12`；设置与 ⌘K 弹窗是 `0 24px 48–64px rgb(20 24 16 / 0.18)`。
- **遮罩**：抽屉用 `rgb(20 24 16 / 0.28)`，弹窗 `::backdrop` 用 0.32。

### Named Rules
**阴影描边规则。** 卡片、输入框、按钮、标签都不用 `border`，而是画 1px 零模糊的环。环的透明度随底色走，放在纸色侧栏上也不会发灰。列表行之间的分隔线和虚线框（空状态、添加卡）仍然用 `border`。

**真浮起规则。** 只有真正浮在别的东西之上的元素才有远处的柔影，也就是输入框、菜单和弹窗。

## Shapes

- 圆角分档：
  - 控件 8：按钮、输入框、导航行、标签页；
  - 卡片 12：卡片、菜单、表格容器；
  - 弹窗与气泡 16：设置、⌘K、输入框外壳、用户气泡；
  - 内层 10：预设卡、目录行、⌘K 结果行；
  - 小件 4 / 6：行内 code、`…` 按钮、小标签；
  - 药丸 999：状态药丸、模型筹码。
- 圆形：发送按钮、头像、光点、状态点。
- 方形 2px 圆角：领域标记、上下文图例色块。
- 树状连线：子会话前是一根 1px 的 `--control` 折线，左下角 4px 圆角。
- 虚线框：只用于空状态和"添加"卡，用 1px dashed `--control`。

## Components

### Buttons
- **形状**：8 圆角（`--radius-control`），高 32（小号 28），左右内边距 12。
- **主按钮**：苔墨底白字，没有环；悬停变 `#343b2c`。一屏最多一个，最常见的就是发送。
- **普通按钮**：白底配控件环，字重 500；悬停用 `--hover-on-white`，按下时缩放到 0.97。
- **危险按钮**：用 `--negative` 描边配红字，悬停变 `#fdf1f0` 底。
- **图标按钮**：32 见方，没有底，`--muted` 色；悬停出灰底并变成正文色。开着的状态（如右栏开关）用 `--active` 底。
- **文字按钮**：正文色、500，带下划线（偏移 3px）。安静版是 400、`--muted`，悬停时才出下划线。

### Inputs / Fields
- **样式**：白底配控件环，8 圆角，高 36，内边距 8 / 10；占位字用 `--faint`。
- **聚焦**：环变成墨色（`--ring-strong`），不画外发光。键盘焦点另有 2px `--focus` 描边，偏移 2px。
- **开关**：40 × 24，关着是控件灰，开着是墨色，白色圆钮 160ms 滑动。
- **分段切换**：放在 `--hover` 灰槽里，选中段是白底加环。
- **键值对**：左列是浮起的浅灰底标签，右列是无环输入框，整组外面一圈环，圆角 10。

### Composer
一行：加号、文字、发送。白底，16 圆角，控件环外叠一层柔影，浮在对话之上；聚焦时环变成 `--muted`。框下面一行放模型、推理强度和上下文环（一个 16px 的叶绿进度圈）。不能发送时底变 `--raised`，发送按钮降到 35% 透明。

### Cards / Containers
- **圆角** 12，**白底**，**轻抬**（`--lift`），没有 `border`。
- **右栏卡片**：44 高的可折叠头，标题 500，右边是数值；内边距 0 / 14 / 12。
- **设置卡**：一项一张卡，左边是名字（15/600）加一句说明，右边是控件；手机上控件换到说明下一行（开关除外）。
- **键值井**：`--raised` 底，8 圆角，用来展示来历、元信息这类只读键值。

### Navigation
- **侧栏行**：高 34，8 圆角。悬停是 `--hover`；当前项是 `--active` 加 500 字重。图标用 16px 线描，颜色 `--muted`。
- **会话行**：只显示标题。行尾的 `…` 在悬停、键盘聚焦或菜单打开时才出现，触屏上一直显示；菜单里的删除需要二次确认。子任务会话缩进 28，前面有树状连线。
- **⌘K 搜索弹窗**：760 宽，距顶 12vh。顶上是 16px 的输入，下面一排类型标签（会话、事项、记忆、定时），中间是分组结果（行高 40，圆角 10，选中行用 `--active` 底并在行尾显示回车图标），底部是快捷键提示，手机上隐藏。
- **设置弹窗**：左栏是纸色上的一列分节（和侧栏同一套灰药丸），待处理数用红色计数，在等的用琥珀计数。

### Conversation
- **用户消息**：靠右的灰气泡（`--raised`，16 圆角，最宽 80%），下面一行是时间和复制按钮。
- **komo 的回复**：没有气泡，直接是 15/26 的正文。链接是墨色字配淡下划线；表格只用细横线。
- **一轮**：照 DeepSeek Harness 的 Chat。"已完成，用时 …"一行下面有一条细线，点它收起或展开这一轮的过程；回复始终可见，收起只收过程。相邻的思考与工具调用并成一组，组头是类别图标加活动摘要（"已搜索代码并读取文件"），悬停或展开时图标换成箭头；展开后每行"动作 · 详情"，默认 `--faint`、悬停变墨，行距 4，组头与内容 8，回复与过程行 12。调用再展开是 `--raised` 底的卡，命令在上可复制，输出在下是白底、横向滚动，卡下一个描边小药丸"查看"跳到轨迹。召回收成一行；插话带一个浅灰小标签。
- **轨迹**：标题旁"对话 / 轨迹 / 改动"三个标签。顶上一排工具（时长、轮次、调用）和搜索；时间概览是三条泳道（输入 / 模型 / 工具），墨色深浅区分，不另起色相，叶绿只给正在跑的那格，拖选区间用一层极淡的墨色加两条细边。记录表一行一条：22px 的小方块图标（用户墨底白字，助手 `--active` 底，思考、系统、上下文描边），内容一行截断，右边四列数字；轮头吸顶。选中行左侧一条 2px 墨线。详情是浮起的白卡（`--lift`），标签页下划线式，窄屏从底部升起成 16 圆角的抽屉。
- **改动**：一轮末尾一张卡（`--ring`，12 圆角）：抬头 24px 描边小方块里一个图标、"N 个文件已更改"和合计，下面一行一个文件，目录 `--faint`、文件名墨色，增删数等宽小字；"新建 / 删除"是灰药丸。改动标签里一个文件一张卡，展开时抬头铺 `--raised`，下面是带新旧行号的 unified diff，hunk 头一行 `--raised` 底淡字。增删不另起色相：加的行 `--hover` 浅底、删的行淡字，左边一列 + / − 符号；新建的文件整段都是加的，不再逐行铺底。
- **加载**：放平的灰条骨架，没有微光；出错和空状态都留在出问题的那一块里，一句话加一个动作。

### 光点与在场光阶（Signature）
光点表示记忆离模型的 prompt 有多近。直径 8，在列表里 7。

| 档 | 光点 |
|---|---|
| 每次都带（生效的约定） | 实心叶绿 |
| 相关才带（生效的线索、未分类） | 1.5px 叶绿空心圈 |
| 暂停 | 候选是 `--faint` 虚线圈，有争议是 `--negative` 细圈 |
| 不再带（遗忘、取代、归档） | 灰点（`--control`，图上用 `#a9ae9f`） |

光点出现在每一轮"完成，用时 …"那一行的右侧、展开后过程的第一行、右栏的"带上的记忆"，以及记忆页的"攒下的记忆"图。那张图的横轴是记下的日子，点的面积是带进对话的次数；上方的图例同时也是筛选。事项页每行右边用同一套点，表示为这件事记下的记忆。

### 状态点
- **事项、定时、待处理**：只用墨与灰。
  - 在做的目标：墨点
  - 领域：墨方块
  - 在等：琥珀圈
  - 搁置：虚线圈
  - 结案：灰点
  - 出错：红点
- **设置里的连接状态**（`kstate`）：
  - 能运行（已设置、已连上、在跑）：叶绿小点
  - 缺凭证、要登录、在重试：琥珀点加琥珀字
  - 起不来：红点加红字
  - 已断开、没起：灰点

## Do's and Don'ts

### Do:
- **Do** 颜色只引用 `:root` 的语义 token。新增颜色先加 token，再写进本文头部。
- **Do** 用 1.5px 描边的内联 SVG 画图标（16px，`svg.i`）。
- **Do** 用产品自己的话写文案：界面用中文，术语与 `docs/komo_bot.md`、`docs/memory.md` 一致（约定、线索、目标、领域、任务会话、插话）。
- **Do** 如实写数：没有的写"—"，零就写 0；估算的数在旁边说明口径。
- **Do** 键盘焦点一律画 `--focus` 蓝色 2px 描边。

### Don't:
- **Don't** 把叶绿用在品牌装饰、强调、选中态、完成反馈或开关上。
- **Don't** 引入第二种色相，包括第二种绿。
- **Don't** 给卡片、输入框、按钮写 `border`。
- **Don't** 用 emoji 或 Unicode 字符充当图标。
- **Don't** 加入场动画、回弹曲线或骨架微光。
- **Don't** 给只是放在原地的东西加柔影。

v0.8 之前的前端（`apps/app` 共享渲染层、dusk 暗色主题、叶隙光斑材质 `.komorebi-dapple`、变动余光、五级记忆光阶）随 v0.8 脚手架删除（`ddaaabeb`），要参考请看 git 历史。
