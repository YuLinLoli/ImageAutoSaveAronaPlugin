//! 本插件的配置区，落到框架运行目录的 `config/image-auto-save/arona.yml`。
//!
//! 只有四项，而且四项都能在 GUI「插件管理 → 详情 → 配置」那一块直接改：
//! 框架给标量叶子画控件（布尔=勾选框，字符串=文本框），**表与数组一律跳过**（§12）。
//! 所以两个好友列表存成一个逗号分隔的字符串，而不是 `Vec<i64>`——登记成数组的话
//! GUI 就一格控件都不给，"在界面上勾名单"这件事结构上做不到。
//! 手改 YAML 时两种写法都认：`friends: 10001,10002` 与
//! ```yaml
//! friends:
//!   - 10001
//!   - 10002
//! ```
//!
//! Windows 路径相关的东西这里一项都没有：落盘位置固定用框架分配的
//! `data/image-auto-save/image/`（§7「插件不要自己拼路径」）。

use arona::config::arona::PluginConfig;
use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// 配置区键名，与插件 id 一致
pub const SECTION: &str = "image-auto-save";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageAutoSaveConfig {
    /// 总开关；关掉之后钩子收到好友图片也不取、不存、不预览。
    /// 默认关：自动把别人发的图落到本机硬盘，得由用户自己勾上
    pub enabled: bool,
    /// 接收图片的好友列表
    pub friends: FriendList,
    /// 是否启用发送给好友预览
    pub preview_enabled: bool,
    /// 发送给好友列表（预览发到这些人的私聊）
    pub preview_friends: FriendList,
}

impl ImageAutoSaveConfig {
    /// 两档门控的顺序：先看总开关，再看点名。都过了才去碰网络
    pub fn accepts(&self, user_id: i64) -> bool {
        self.enabled && self.friends.contains(user_id)
    }
}

impl PluginConfig for ImageAutoSaveConfig {
    const TITLE: &'static str = "好友图片自动保存";
    const DOC: &'static str = "只收好友私聊发来的图片，存进 data/image-auto-save/image/。\
四项都能在这一页直接改，勾上「是否开启」并把 QQ 号填进好友列表才会真的存图。改完保存即热生效。";

    /// 注释的唯一来源：框架生成 config/image-auto-save/arona.yml 时按路径逐字段取
    fn comment(path: &str) -> Option<&'static str> {
        Some(match path {
            "enabled" => "是否开启：关掉之后收到好友的图也不取、不存、不预览",
            "friends" => {
                "接收图片的好友列表，逗号分隔的QQ号（也可以写成YAML列表）；名单外的人发的图一概不存"
            }
            "preview_enabled" => "是否启用发送给好友预览：存下来的图再发给下面列表里的人",
            "preview_friends" => {
                "发送给好友列表，逗号分隔的QQ号；每人一条「图片 + 发送人：昵称(QQ号)」"
            }
            _ => return None,
        })
    }
}

/// 一组 QQ 号。存成一个逗号分隔的标量字符串，为的是让 GUI 给它文本框；
/// 读的时候列表写法也照收，手改 YAML 不必迁就界面上的写法
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FriendList(Vec<i64>);

/// 分隔符收得宽一点：中英文逗号、分号、顿号加任意空白，用户从别处粘一份名单过来不该失败
const SEPARATORS: [char; 5] = [',', '，', ';', '；', '、'];

impl FriendList {
    pub fn contains(&self, user_id: i64) -> bool {
        self.0.contains(&user_id)
    }

    pub fn ids(&self) -> &[i64] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn joined(&self) -> String {
        self.0
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 分隔符收得宽：从别处粘一份名单过来不该因为写法失败。
    /// 重复的那个号只留一次——名单里写两遍不代表要存两遍
    pub(crate) fn parse(text: &str) -> Result<FriendList, String> {
        let mut ids: Vec<i64> = Vec::new();
        for token in text.split(|ch: char| SEPARATORS.contains(&ch) || ch.is_whitespace()) {
            if token.is_empty() {
                continue;
            }
            let id = token.parse::<i64>().map_err(|_| {
                format!("好友列表里的「{token}」不是QQ号，列表要写成逗号分隔的数字")
            })?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(FriendList(ids))
    }
}

impl Serialize for FriendList {
    /// 序列化成标量字符串：框架生成模板与 GUI 摊平叶子都按标量走，文本框才画得出来
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.joined())
    }
}

impl<'de> Deserialize<'de> for FriendList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FriendListVisitor)
    }
}

struct FriendListVisitor;

impl<'de> Visitor<'de> for FriendListVisitor {
    type Value = FriendList;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("一组QQ号，逗号分隔的字符串或 YAML 列表都行")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<FriendList, E> {
        FriendList::parse(value).map_err(serde::de::Error::custom)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<FriendList, A::Error> {
        let mut ids: Vec<i64> = Vec::new();
        while let Some(id) = seq.next_element::<FriendId>()? {
            if !ids.contains(&id.0) {
                ids.push(id.0);
            }
        }
        Ok(FriendList(ids))
    }
}

/// 列表里的每一项：`- 10001` 与 `- "10001"` 都得吃下，YAML 里加引号是极常见的写法
struct FriendId(i64);

impl<'de> Deserialize<'de> for FriendId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FriendIdVisitor)
    }
}

struct FriendIdVisitor;

impl<'de> Visitor<'de> for FriendIdVisitor {
    type Value = FriendId;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("一个QQ号")
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<FriendId, E> {
        Ok(FriendId(value))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<FriendId, E> {
        i64::try_from(value)
            .map(FriendId)
            .map_err(|_| serde::de::Error::custom("QQ号超出可表示的范围"))
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<FriendId, E> {
        value
            .trim()
            .parse::<i64>()
            .map(FriendId)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认必须"装上不做事"：没勾开关、没点名，一张图都不该往硬盘上落
    #[test]
    fn defaults_stay_quiet() {
        let config = ImageAutoSaveConfig::default();
        assert!(
            !config.enabled,
            "总开关默认关：自动收图是把别人的文件落到本机"
        );
        assert!(!config.preview_enabled);
        assert!(config.friends.is_empty() && config.preview_friends.is_empty());
        assert!(!config.accepts(10001), "开着都没点名，就不该收");
    }

    /// 结构性收口：字段表与注释表谁漏了谁红用例，而不是等用户看着没注释的键猜
    #[test]
    fn every_field_has_a_comment() {
        let value = serde_yaml::to_value(ImageAutoSaveConfig::default()).expect("默认值总能序列化");
        let serde_yaml::Value::Mapping(map) = value else {
            panic!("配置区顶层应当是映射");
        };
        let missing: Vec<String> = map
            .keys()
            .filter_map(|key| {
                let name = key.as_str().expect("顶层键名应当是字符串").to_string();
                ImageAutoSaveConfig::comment(&name)
                    .is_none()
                    .then_some(name)
            })
            .collect();
        assert!(missing.is_empty(), "这些字段漏了注释: {missing:?}");
    }

    /// GUI 那一页只给标量叶子画控件，所以四项都必须是标量——数组会被整格跳过
    #[test]
    fn all_four_keys_render_as_scalars() {
        let value = serde_yaml::to_value(ImageAutoSaveConfig::default()).unwrap();
        let serde_yaml::Value::Mapping(map) = value else {
            panic!("顶层应当是映射")
        };
        assert_eq!(
            map.keys().count(),
            4,
            "配置面就四项，多一项都得先过 GUI 这一关"
        );
        for (key, item) in map {
            assert!(
                matches!(
                    item,
                    serde_yaml::Value::Bool(_) | serde_yaml::Value::String(_)
                ),
                "「{}」不是标量，GUI 给不出控件",
                key.as_str().unwrap_or_default(),
            );
        }
    }

    #[test]
    fn list_writes_back_as_one_string() {
        let list = FriendList::parse("10001,10002").unwrap();
        // 断言落在 Value 上而不是序列化出来的文本：YAML 的 plain 风格爱不爱加引号，
        // 不是本插件该钉住的东西
        let value = serde_yaml::to_value(&list).unwrap();
        assert_eq!(value, serde_yaml::Value::String("10001,10002".to_string()));
        let text = serde_yaml::to_string(&list).unwrap();
        let round: FriendList = serde_yaml::from_str(&text).unwrap();
        assert_eq!(round, list, "写出去的字符串要能原样读回来");
    }

    /// 分隔符收得宽：从别处粘一份名单过来不该因为写法失败
    #[test]
    fn parse_accepts_every_widened_separator() {
        let expected: &[i64] = &[10001, 10002];
        for spelling in [
            "10001,10002",
            "10001，10002",
            "10001, 10002",
            "10001 10002",
            "10001;\n10002\n",
            "10001、10002",
            " 10001 , 10002 ",
        ] {
            assert_eq!(
                FriendList::parse(spelling).unwrap().ids(),
                expected,
                "写法「{spelling:?}」应当等价"
            );
        }
    }

    #[test]
    fn list_reads_both_yaml_spellings() {
        let expected = FriendList::parse("10001,10002").unwrap();
        let stringed: FriendList = serde_yaml::from_str("'10001, 10002'").unwrap();
        assert_eq!(stringed, expected, "界面写回去的那一串要能读回来");
        let listed: FriendList = serde_yaml::from_str("- \"10001\"\n- 10002\n").unwrap();
        assert_eq!(
            listed, expected,
            "YAML 列表写法也要认，带引号的那一项同样得吃下"
        );
    }

    #[test]
    fn duplicates_in_the_list_collapse() {
        let parsed = FriendList::parse("10001,10001,10002").unwrap();
        assert_eq!(
            parsed.ids(),
            &[10001, 10002],
            "粘一份带重复的名单进来不该存两遍"
        );
    }

    #[test]
    fn a_bad_token_says_which_one() {
        let err = FriendList::parse("10001,abc").unwrap_err();
        assert!(err.contains("abc"), "要把坏的那个词报出来: {err}");
    }

    /// 用户只写一项也能读出来：其余字段由 Default 兜着
    #[test]
    fn partial_yaml_still_loads() {
        let config: ImageAutoSaveConfig =
            serde_yaml::from_str("enabled: true\nfriends: '10001'\n").unwrap();
        assert!(config.enabled);
        assert!(config.accepts(10001));
        assert!(!config.accepts(10002), "没点名的人照样不收");
        assert!(!config.preview_enabled && config.preview_friends.is_empty());
    }

    #[test]
    fn gate_order_is_switch_then_roster() {
        let on = ImageAutoSaveConfig {
            enabled: false,
            friends: FriendList::parse("10001").unwrap(),
            ..Default::default()
        };
        assert!(
            !on.accepts(10001),
            "勾掉总开关之后，名单里的人也不该再存图——关就是关，别留半开"
        );
    }
}
