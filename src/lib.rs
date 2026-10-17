//! 好友图片自动保存（Arona 功能插件，参照 mirai 的 ImageAutoSave-YuLinMiraiPlugin）。
//!
//! 编译成 `image_auto_save_plugin.dll`，放进 `<运行目录>/plugins/image-auto-save/`，
//! 框架启动时扫描、握手、装载。配置只有四项，全在 GUI「插件管理 → 详情 → 配置」那一页：
//! 是否开启、接收图片的好友列表、是否启用发送给好友预览、发送给好友列表。
//!
//! 每条处理完都**引用回复发图的人**：存成了回「图片已保存」，有没存成的再补一句
//! 「图片保存失败」（一张都没存成时只回失败，绝不回成功的假话）。文案固定、不占配置项。
//!
//! 这是一个纯私聊插件：钩子只订 `BodyFilter::private("friend")`，群消息与群里的临时会话
//! 都不进这一档，也一条命令都不接。所以 `meta()` 里声明了 `with_group_scoped(false)`：
//! 框架据此收起「启用的群」那一格，`group_settings.<群>.disabled_plugins` 里就算残留着
//! 本插件的 id 也一律放行（框架文档 §6、§13）。本插件同样不登记功能开关——那会在群功能
//! 清单里占一行永远勾不动的格子。全局停用走 GUI 的插件总开关
//! （`disabled_plugins: [image-auto-save]`），框架会连钩子一起收掉（§13）。
//!
//! 与 mirai 参考的两处差别是这边框架的规矩定的（框架仓库 `PLUGIN_DEVELOPMENT.md`）：
//! - 落盘目录不给配置项，固定用框架分配的 `data/image-auto-save/image/`（§7）；
//! - 下载与写盘放在 `ctx.spawn_as` 里，钩子立刻 `HookFlow::Pass`：不吞消息、不占事件循环，
//!   插件被停用时这个任务也照样收得回去（§10）。

mod config;
mod save;

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Local};

use arona::onebot::EventContext;
use arona::onebot::api::OneBotApi;
use arona::onebot::hooks::{BodyFilter, HookFlow, event_handler};
use arona::plugin::{AronaPlugin, PluginContext, PluginMeta, PluginRegistrar};
use arona::runtime::log;
use arona::runtime::message::{MessageSegment, MessageTarget, OutgoingMessage};
use arona::runtime::priority::ListenerPriority;
use arona::runtime::services::send_message;

use crate::config::{ImageAutoSaveConfig, SECTION};

/// 插件 id：磁盘上的身份，`plugins/<id>/`、`config/<id>/arona.yml`、`data/<id>/` 全用它，
/// 与 `meta().id`、配置区键名共用这一个来源。定了就别改——改名等于换一份用户数据。
const PLUGIN_ID: &str = "image-auto-save";

/// 存成之后引用回复发送者那一句
const SAVED_NOTE: &str = "图片已保存";

/// 有图没存成时引用回复发送者那一句
const FAILED_NOTE: &str = "图片保存失败";

/// 本插件不带任何跨调用状态：名单与开关每轮从配置区现读，所以是单位结构体
#[derive(Default)]
pub struct ImageAutoSavePlugin;

impl AronaPlugin for ImageAutoSavePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(
            PLUGIN_ID,
            "ImageAutoSave",
            env!("CARGO_PKG_VERSION"),
            "自动保存白名单好友私聊发来的图片，并把图片连发送人一起预览给指定好友",
        )
        // 只收好友私聊、没有一条命令，分群停用名单对它没有可关的东西：
        // 声明之后框架不再给它画「启用的群」那一格（框架文档 §6）
        .with_group_scoped(false)
        .with_author("YuLin")
    }

    /// 登记阶段只交出一块配置区：框架据此生成带注释模板，四个键都是标量，GUI 才画得出控件
    fn install(&self, reg: &PluginRegistrar) -> Result<(), String> {
        reg.config::<ImageAutoSaveConfig>(SECTION);
        Ok(())
    }

    fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
        let setup = ctx.clone();
        ctx.listen_where(
            &[BodyFilter::private("friend")],
            ListenerPriority::Normal,
            event_handler(move |event: Arc<EventContext>| {
                let setup = setup.clone();
                async move {
                    let config = setup.config::<ImageAutoSaveConfig>(SECTION).get();
                    let user_id = event.user_id();
                    // 两档门控都在发任何请求之前：关着或没点名，就当本插件不存在
                    if !config.accepts(user_id) {
                        return HookFlow::Pass;
                    }
                    let images: Vec<MessageSegment> = event
                        .segments
                        .iter()
                        .filter(|segment| matches!(segment, MessageSegment::Image { .. }))
                        .cloned()
                        .collect();
                    if images.is_empty() {
                        return HookFlow::Pass;
                    }
                    // sender_name() 群名片优先、退回昵称，实现端两个都没给时是 None
                    let name = event.sender_name().unwrap_or("好友").to_string();
                    let api = event.api();
                    let dir = setup.image_dir();
                    // 引用回复要挂的那条消息号；实现端没给就是 None，回执照样发、只是不引用
                    let quoted = event.message_id();
                    let total = images.len();
                    // 时间戳在投递任务之前定一次：一条消息里的几张图共用同一个前缀
                    let when = Local::now();
                    setup.spawn_as("保存图片", async move {
                        let saved = save_all(&api, &dir, &when, user_id, &name, &images).await;
                        announce(&config, quoted, user_id, &name, &saved, total).await;
                    });
                    HookFlow::Pass
                }
            }),
        );
        Ok(())
    }

    /// 启动阶段：把生效后的落点与名单打印一遍，用户改完配置立刻找得到目录。
    /// 回收本来就是框架的事，所以这里没有对应的 `stop()` 要写（§13）。
    fn start(&self, ctx: &PluginContext) -> Result<(), String> {
        report(ctx, "已启用");
        Ok(())
    }

    /// 配置热重载后回调：同样报一遍，让「改没改上」这件事当场可见
    fn on_config_reload(&self, ctx: &PluginContext) {
        report(ctx, "配置已重载");
    }
}

/// 一条好友消息里所有图片的取字节 + 落盘，返回真正存下来的那些段。
/// 单张失败只写日志，不拖着其余几张一起失败。
async fn save_all(
    api: &OneBotApi,
    dir: &Path,
    when: &DateTime<Local>,
    user_id: i64,
    name: &str,
    images: &[MessageSegment],
) -> Vec<MessageSegment> {
    // 文件名 = 清洗过的昵称-号-时间戳，mirai 参考用的就是这一族；
    // 同一条消息里几张图撞名时靠 write_new 往后排号，不覆盖已经落盘的那一份
    let stem = format!(
        "{}-{}-{}",
        save::sanitize(name, "好友", save::NAME_MAX_CHARS),
        user_id,
        save::stamp(when)
    );
    let mut saved: Vec<MessageSegment> = Vec::new();
    for segment in images {
        let bytes = match save::fetch_bytes(api, segment).await {
            Ok(bytes) => bytes,
            Err(err) => {
                log::warning(format!("{name}({user_id}) 的一张图取字节失败: {err}"));
                continue;
            }
        };
        let extension = save::extension_of(&bytes, save::hint_of(segment));
        match save::write_new(dir, &stem, extension, &bytes) {
            Ok(path) => {
                log::info(format!(
                    "图片已保存：{}（{} 字节，来自 {name}({user_id})）",
                    path.display(),
                    bytes.len()
                ));
                saved.push(segment.clone());
            }
            Err(err) => log::warning(format!("{name}({user_id}) 的一张图落盘失败: {err}")),
        }
    }
    saved
}

/// 该引用回复发送者哪几句：存成才报成功，有失败就报失败。
/// 一张都没存成时绝不能回「图片已保存」——那种假回执比不回更糟
fn notes(saved: usize, total: usize) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    if saved > 0 {
        out.push(SAVED_NOTE);
    }
    if saved < total {
        out.push(FAILED_NOTE);
    }
    out
}

/// 回执与预览都走框架统一发送口（`send_message`），所以出站钩子与黑名单都在这条路上；
/// 没用 `api.send_private_msg` 那条协议层旁路（§9.1）。
async fn announce(
    config: &ImageAutoSaveConfig,
    quoted: Option<i64>,
    user_id: i64,
    name: &str,
    saved: &[MessageSegment],
    total: usize,
) {
    // 引用回复原那条图片消息：mirai 的 QuoteReply 同一档（§19 的 Reply 段）。
    // 实现端没给消息号时退成不引用的回话，别为了挂引用就把回执整条丢掉。
    for text in notes(saved.len(), total) {
        let message = match quoted {
            Some(id) => OutgoingMessage::quoted(id, text),
            None => OutgoingMessage::text(text),
        };
        let _ = send_message(MessageTarget::Private(user_id), message).await;
    }
    if !config.preview_enabled || saved.is_empty() {
        return;
    }
    for segment in saved {
        // 一条预览：原样的图片段 + 一行「发送人：好友名(QQ号)」。
        // 图片段带着实现端给的线索回发，有直链时不必把字节再上传一遍（§19）。
        let message = OutgoingMessage::new(vec![
            segment.clone(),
            MessageSegment::Text(save::caption(name, user_id)),
        ]);
        for friend in config.preview_friends.ids() {
            let receipt = send_message(MessageTarget::Private(*friend), message.clone()).await;
            if receipt.message_id.is_none() && receipt.real_id.is_none() {
                log::warning(format!(
                    "预览发给 {friend} 没拿到回执，可能被出站钩子拦下了"
                ));
            }
        }
    }
}

/// 启用与热重载两处共用的一条汇报：内容一样，只是前缀不同
fn report(ctx: &PluginContext, stage: &str) {
    let config = ctx.config::<ImageAutoSaveConfig>(SECTION).get();
    let switch = |on: bool| if on { "开着" } else { "关着" };
    log::info(format!(
        "图片自动保存{stage}：总开关{}，保存目录 {}，接收名单 {} 人，预览{}（{} 人）",
        switch(config.enabled),
        ctx.image_dir().display(),
        config.friends.len(),
        switch(config.preview_enabled),
        config.preview_friends.len(),
    ));
    if !config.enabled {
        log::warning(
            "总开关没开：现在一张图都不会保存，勾上「是否开启」并把 QQ 号填进接收名单才会动。",
        );
    } else if config.friends.is_empty() {
        log::warning("开关开着但接收名单是空的：谁发图来都不存，先把 QQ 号填进接收好友列表。");
    }
}

// 框架装载本 dll 的入口：导出 ABI 号、工具链指纹、宿主上行通道、造实例四个 C 符号。
// 本插件没有跨调用状态，所以直接写类型名而不是 `::default()`（§15，否则 clippy 会念一句）。
arona::export_arona_plugin!(ImageAutoSavePlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use arona::framework::Framework;
    use std::path::PathBuf;

    fn image(bytes: Option<Vec<u8>>, url: Option<&str>) -> MessageSegment {
        MessageSegment::Image {
            url: url.map(str::to_string),
            file: None,
            data: bytes,
            file_id: None,
        }
    }

    fn png() -> Vec<u8> {
        [b"\x89PNG\r\n\x1a\n".to_vec(), vec![0u8; 16]].concat()
    }

    fn jpeg() -> Vec<u8> {
        [b"\xff\xd8\xff\xe0".to_vec(), vec![7u8; 16]].concat()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("image-auto-save-lib-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录建得出来");
        dir
    }

    /// 内联字节这一档不碰网络，整条「取名 → 定后缀 → 落盘」在单测里就能跑到硬盘上
    #[tokio::test(flavor = "current_thread")]
    async fn inline_images_land_on_disk() {
        let framework = Framework::new();
        let api = OneBotApi::of(&framework);
        let dir = temp_dir("inline");
        let when = Local::now();
        // 两张的字节头与 url 后缀互相矛盾，正好验「字节优先于提示」这条口径
        let images = vec![
            image(Some(png()), None),
            image(Some(jpeg()), Some("https://x/y.gif")),
        ];
        let saved = save_all(&api, &dir, &when, 10001, "阿罗娜", &images).await;
        assert_eq!(saved.len(), 2, "两张都该落盘");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2);
        assert!(
            names.iter().all(|name| name.starts_with("阿罗娜-10001-")),
            "文件名要带发送人这一档: {names:?}"
        );
        assert!(
            names.iter().any(|name| name.ends_with(".png"))
                && names.iter().any(|name| name.ends_with(".jpg")),
            "后缀按字节头嗅，url 写着 gif 不算: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一条消息里混着坏段时，失败的那几张只写日志，剩下的照存，整条链不该 panic
    #[tokio::test(flavor = "current_thread")]
    async fn bad_segments_fail_alone() {
        let framework = Framework::new();
        let api = OneBotApi::of(&framework);
        let dir = temp_dir("bad");
        let when = Local::now();
        let images = vec![image(None, None), image(Some(png()), None)];
        let saved = save_all(&api, &dir, &when, 10001, "阿罗娜", &images).await;
        assert_eq!(saved.len(), 1, "只有有字节的那张成事");
        assert_eq!(std::fs::read_dir(&dir).unwrap().flatten().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// README「发布三件事」里手抄的符号布局号必须等于编译进这份 dll 的那个常量。
    /// 数字漂了的后果是用户拿旧宿主装新 dll（或反过来），而拒载日志只会说"布局不匹配"。
    /// 光钉常量还不够——那一行是手抄的，所以把 README 里每处「布局 <数字>」都扫一遍。
    #[test]
    fn release_note_layout_matches_the_built_abi() {
        assert_eq!(
            arona::plugin::abi::ABI_LAYOUT,
            10,
            "框架抬了布局号，README 与本用例都要跟着改"
        );
        let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
            .expect("发布说明就在仓库根，读不到是路径错了");
        let mut claimed: Vec<u32> = Vec::new();
        for (at, _) in readme.match_indices("布局") {
            let digits: String = readme[at + "布局".len()..]
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                claimed.push(digits.parse().expect("全数字串总能解析"));
            }
        }
        assert!(
            !claimed.is_empty(),
            "README 里一处「布局 <数字>」都没有，这条判据就空转了"
        );
        assert!(
            claimed
                .iter()
                .all(|value| *value == arona::plugin::abi::ABI_LAYOUT),
            "README 写过的布局号 {:?} 与当前 {} 不一致",
            claimed,
            arona::plugin::abi::ABI_LAYOUT,
        );
    }

    /// 回执的判据：成与败各回一句都要发，但一张都没存成时绝不能回「图片已保存」
    #[test]
    fn receipt_never_fakes_a_success() {
        assert_eq!(notes(2, 2), vec![SAVED_NOTE]);
        assert_eq!(notes(0, 1), vec![FAILED_NOTE], "全失败只回失败");
        assert_eq!(notes(1, 3), vec![SAVED_NOTE, FAILED_NOTE], "混着时就回两句");
        assert!(notes(0, 0).is_empty(), "没图可存时一句都不回");
    }

    /// 名单与开关的判定顺序：钩子在任何网络动作之前就要把这两档挡干净
    #[test]
    fn gate_blocks_before_any_network() {
        let off = ImageAutoSaveConfig {
            enabled: true,
            friends: config::FriendList::parse("10001").unwrap(),
            ..Default::default()
        };
        assert!(off.accepts(10001));
        assert!(!off.accepts(10002), "没点名的人不收");
        let disabled = ImageAutoSaveConfig {
            enabled: false,
            ..off.clone()
        };
        assert!(
            !disabled.accepts(10001),
            "总开关一关，名单里的人也不该再动——关就是关，不留半开"
        );
    }
}
