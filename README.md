<p align="center">
  <img src="assets/logo.svg" width="88" alt="拾光 logo">
</p>

<h1 align="center">拾光 · Shiguang</h1>

<p align="center"><strong>在自己的 Mac 上，找回值得留下的照片。</strong><br>
用自然语言搜索照片，发现相似画面，并把整理决定留给你。</p>

<p align="center">macOS 桌面照片整理工具 · Rust · Tauri · EmbeddingGemma 2</p>

---

拾光是一款面向 macOS 的本地照片整理应用。它连接 Mac「照片」图库，也可以读取通过 USB 连接的 iPhone 相机媒体；在本机建立图片向量索引，让你用文字描述查找照片，并通过可编辑的扫描规则整理待复核候选。

应用不会自动替你删除照片。扫描结果先进入复核流程，只有你选择照片并确认后，拾光才会请求系统图库或设备处理删除。

## 功能

- **自然语言搜图**：用描述查找已建立索引的照片，例如“海边的日落”或“雪地里的狗”。
- **相似照片检索**：从一张照片出发，查找向量相近的已索引照片。
- **自定义整理规则**：用正向描述、排除描述和匹配门槛扫描模糊、曝光异常、遮挡等候选。
- **人工复核**：标记保留、稍后处理，或把选中照片加入待删除列表；最终操作前再次确认。
- **连接照片来源**：浏览 Mac 照片图库，或查看 ImageCaptureCore 能读取的 USB iPhone 相机媒体。
- **本地索引**：图片向量、元数据、规则和复核状态保存在本机 SQLite 数据库。

## 隐私与数据

照片索引和向量搜索在本机完成。拾光通过 Apple 的 Photos、PhotoKit 和 ImageCaptureCore 框架访问照片，不会把原图上传到拾光服务器。首次需要向量计算时，应用会从模型仓库下载 EmbeddingGemma 2 模型文件；这需要网络连接，模型文件保存在本机。

- 照片访问由 macOS 权限控制，用户可以在系统设置中调整授权范围。
- 原始照片由「照片」图库或 iPhone 管理；应用本地保存用于展示的缩略图缓存。
- 删除操作只有在用户确认后才会发起，并受照片图库或连接设备的系统行为约束。
- 应用不提供云同步；卸载前如需保留索引或模型，可备份下方应用数据目录。

## 安装与运行

当前项目提供 macOS 源码构建方式。需要 macOS 13 或更新版本、Xcode Command Line Tools、Rust 工具链和 Node.js/npm。

```sh
git clone ssh://git@ssh.github.com:443/nowszhao/iMed.git
cd iMed
npm install
npm run dev
```

构建 macOS 应用：

```sh
npm run build
```

构建产物位于 `src-tauri/target/release/bundle/macos/拾光.app`。首次运行扫描或搜索时，应用会按需下载并加载 EmbeddingGemma 2 440M 图像模型；模型文件约 1.49 GB，具体下载量取决于模型仓库提供的文件和缓存状态。

## 首次连接图库

1. 打开拾光，点击左侧照片来源旁的「···」。
2. 选择「允许访问图库」，再按 macOS 提示授权。有限访问模式只会展示系统允许拾光访问的照片。
3. 连接 iPhone 时，使用 USB 连接并解锁手机；若系统询问，选择「信任此电脑」。
4. 选择照片来源并启动扫描。扫描完成后即可进行语义搜索和相似照片检索。

USB 来源基于 ImageCaptureCore 可枚举的相机媒体。它不等同于完整的 iPhone 文件系统访问；云端 iCloud 项目、应用私有内容或设备当前未提供的媒体可能无法读取。设备是否允许删除也取决于 iOS 与 ImageCaptureCore 返回的结果。

## 使用说明

### 语义搜索

在顶部搜索框输入自然语言，拾光会把查询编码成文本向量，并与图库中已索引的图片向量进行相似度排序。搜索结果只覆盖已扫描、成功建立向量索引的照片。

语义相似度是排序分数，不是照片中某个物体存在的概率。整图向量对场景和主体语义更合适；对“头发”“闭眼”等细小局部特征，召回效果可能不稳定。拾光目前不会自动为每张照片生成详细文字说明，也不承诺仅凭向量判断照片质量。

### 整理规则

规则用自然语言描述要查找的画面，也可以填写排除描述，减少艺术性模糊、夜景或特殊构图带来的误判。匹配门槛用于调整候选范围。模型分数仅供筛选和排序，请在加入待删除列表前逐张复核。

### 删除

删除流程分为选择、加入待删除列表、最终确认。Mac 照片图库通过 PhotoKit 发出删除请求。USB iPhone 上的删除能力因设备和系统返回结果而异；请以应用的操作回执和系统图库状态为准。

## 工作原理

```text
Mac「照片」图库 / USB iPhone
              │  Photos、PhotoKit、ImageCaptureCore
              ▼
         获取照片缩略图
              │
              ▼
EmbeddingGemma 2 440M（本机图片向量）
              │
       ┌──────┴──────┐
       ▼             ▼
SQLite 向量索引   自然语言查询向量
       └──────┬──────┘
              ▼
       本机余弦相似度排序
              │
              ▼
      人工复核与确认操作
```

桌面界面由 Tauri 2 和原生 Web 前端构成；Rust 负责应用命令、模型调用、索引和 SQLite 存储；Swift 桥接 macOS Photos、PhotoKit 与 ImageCaptureCore。当前向量保存在 SQLite BLOB 中，搜索时在本机计算余弦相似度；项目尚未依赖 LanceDB。

## 本地数据位置

默认应用数据目录：

```text
~/Library/Application Support/com.shiguang.photo-organizer/
├── shiguang.sqlite3   # 元数据、向量、规则、复核状态与缩略图缓存
└── models/            # EmbeddingGemma 2 模型文件
```

删除此目录会移除拾光保存的索引、设置数据和模型缓存，不会代替系统图库删除原始照片。

## 当前范围与限制

- 仅支持 macOS 13+ 桌面端；目前没有 Windows、Linux 或 iOS 配套应用。
- 当前模型用于跨模态语义向量搜索和候选排序，不是专门的照片质量分类器。
- 语义搜索只包含成功索引的照片；尚无关键词倒排索引或混合检索，因此精确文件名、罕见词和局部属性可能不如专用检索系统稳定。
- 相似照片功能按图片向量查找相近结果，不是像素级重复检测，也不会自动形成完整重复照片组。
- 模型与图片向量的效果会因语言、构图、主体大小、缩略图细节和照片库内容而变化。请把结果作为整理建议并人工检查。
- USB 读取和删除依赖 Apple 框架及连接设备的实际支持；完整 iPhone 媒体管理能力尚未在所有设备和 iOS 版本上验证。
- 当前没有内置更新服务、遥测、云备份或跨设备同步。

## 技术栈

| 部分 | 技术 |
|---|---|
| macOS 桌面容器 | Tauri 2 |
| 应用逻辑与数据层 | Rust |
| 原生照片接入 | Swift、Photos、PhotoKit、ImageCaptureCore |
| 图片与文本向量 | Google EmbeddingGemma 2（440M 图像配置） |
| 元数据与向量存储 | SQLite（向量以 float32 BLOB 保存） |
| 前端 | HTML、CSS、JavaScript、Vite |

## 许可证

本仓库当前尚未添加项目许可证文件。添加明确的开源许可证之前，请不要假设本项目可按某种开源许可证自由使用、修改或再发布。

## 致谢

- [EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2)，由 Google 提供开放权重及模型许可说明。
- [Tauri](https://tauri.app/)，跨平台桌面应用框架。
- Apple Photos、PhotoKit 与 ImageCaptureCore，为 macOS 照片图库及相机设备接入提供系统框架。

拾光与 Google、Apple 无隶属关系。产品名称、商标和模型许可分别归其权利人所有。
