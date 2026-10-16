//! 取字节 → 定名 → 落盘这一段的全部纯逻辑，生命周期与发消息留在 lib.rs。
//!
//! 两条跨镜像出口都交给框架（PLUGIN_DEVELOPMENT.md §14.1、§9.1）：下载走
//! `arona::runtime::http`（宿主那份 reqwest 代发，插件不链接 reqwest），发消息走
//! `runtime::services::send_message`（过出站钩子那条正路）。
//!
//! 目录不在本模块手里：落点固定是框架分配的 `data/image-auto-save/image/`（§7），
//! 这里只管文件名与写入。

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, Local};
use percent_encoding::percent_decode_str;
use serde_json::Value;

use arona::onebot::api::OneBotApi;
use arona::runtime::message::MessageSegment;

/// QQ 图床直链带签名、会过期，一次下载给足 15 秒就该判死；
/// 别学宿主默认 20 秒——一条消息几张图、一轮几十条，超时是按张数乘出来的
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// 同一毫秒撞名时的重试上限（写入用 create_new，绝不覆盖已有文件）
const NAME_RETRY: u32 = 1000;

/// 昵称留在文件名里的长度上限，防止一条超长名片把整条路径撑过 Windows 的 260 字符
pub const NAME_MAX_CHARS: usize = 24;

/// mirai 参考里那份清洗表：Windows 目录项不许用的字符一律换成下划线
const FORBIDDEN: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Windows 的保留设备名：`CON.png` 这种目录建得出来、文件写不进去，得连后缀一起判
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

// ==================== 取字节 ====================

/// 按「内联字节 → 直链 → 本机路径 → file_id」逐级回退。
/// 前一级的失败原因留着一起报，省得只看到"最后一招也没成"。
pub async fn fetch_bytes(api: &OneBotApi, segment: &MessageSegment) -> Result<Vec<u8>, String> {
    let MessageSegment::Image {
        url,
        file,
        data,
        file_id,
    } = segment
    else {
        return Err("这条消息里没有图片段".to_string());
    };
    let mut failures: Vec<String> = Vec::new();
    if let Some(bytes) = data.as_ref().filter(|bytes| !bytes.is_empty()) {
        return Ok(bytes.clone());
    }
    if let Some(link) = url.as_deref().filter(|link| is_http(link)) {
        match download(link).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => failures.push(format!("直链 {link}: {err}")),
        }
    }
    if let Some(path) = file.as_deref().filter(|path| !path.trim().is_empty()) {
        match read_local(path) {
            Ok(bytes) => return Ok(bytes),
            Err(err) => failures.push(format!("本机路径 {path}: {err}")),
        }
    }
    if let Some(id) = file_id.as_deref().filter(|id| !id.trim().is_empty()) {
        match resolve_by_file_id(api, id).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => failures.push(format!("file_id {id}: {err}")),
        }
    }
    if failures.is_empty() {
        return Err("实现端没给可取的线索（直链、本机路径、内联字节、file_id 全为空）".to_string());
    }
    Err(failures.join("；"))
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    arona::runtime::http::send(
        arona::runtime::http::Request::get(url)
            .with_header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0 Safari/537.36",
            )
            .with_timeout(DOWNLOAD_TIMEOUT),
    )
    .await
}

/// 只剩 `file_id` 时向实现端要一次图（NapCat/LLOneBot 的 `get_image`），
/// 它给直链、给 base64、给本机路径都有可能，三样都接住。
async fn resolve_by_file_id(api: &OneBotApi, file_id: &str) -> Result<Vec<u8>, String> {
    let value = api
        .call("get_image", serde_json::json!({ "file": file_id }))
        .await
        .map_err(|err| format!("get_image 失败: {err}"))?;
    let text = |keys: [&str; 3]| -> Option<String> {
        keys.into_iter()
            .filter_map(|key| value.get(key).and_then(Value::as_str))
            .find(|value| !value.trim().is_empty())
            .map(str::to_string)
    };
    if let Some(link) = text(["url", "download_url", "file_url"]).filter(|link| is_http(link)) {
        return download(&link).await;
    }
    if let Some(payload) = text(["base64", "b64", "data"]) {
        let body = payload
            .split_once(',')
            .map(|(_, rest)| rest)
            .unwrap_or(payload.as_str());
        return base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .map_err(|err| format!("get_image 回的 base64 解不开: {err}"));
    }
    if let Some(path) = text(["file_path", "path", "file"]) {
        return read_local(&path);
    }
    Err("实现端的 get_image 没回直链、字节也没回路径".to_string())
}

fn read_local(text: &str) -> Result<Vec<u8>, String> {
    let path = local_path(text);
    fs::read(&path).map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            format!(
                "文件不在实现端本机上（{}），多半是 OneBot 与框架不同机",
                path.display()
            )
        } else {
            format!("读取 {} 失败: {err}", path.display())
        }
    })
}

/// `file:///D%3A/%E5%9B%BE/1.jpg`、`D:\图\1.jpg`、`"D:\图\1.jpg"` 三种写法都折成同一个路径
fn local_path(text: &str) -> PathBuf {
    let trimmed = text.trim().trim_matches('"');
    if has_prefix_ci(trimmed, "file://") {
        let mut rest = &trimmed[7..];
        if let Some(tail) = strip_prefix_ci(rest, "localhost") {
            rest = tail;
        }
        let decoded = percent_decode_str(rest).decode_utf8_lossy().into_owned();
        // file:///C:/x 解出来是 /C:/x，Windows 那头要去掉打头那个斜杠
        let body = decoded
            .strip_prefix('/')
            .filter(|tail| tail.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(&decoded);
        return PathBuf::from(body);
    }
    PathBuf::from(trimmed)
}

// ==================== 定名 ====================

/// 实现端给的直链或本机路径，只用来猜后缀；两个都空就没得提示
pub fn hint_of(segment: &MessageSegment) -> Option<&str> {
    let MessageSegment::Image { url, file, .. } = segment else {
        return None;
    };
    url.as_deref()
        .or(file.as_deref())
        .filter(|text| !text.trim().is_empty())
}

/// 后缀先按字节头嗅，嗅不出来再照实现端给的直链/路径后缀，两样都没有才兜底 jpg。
/// mirai 参考比的是 HTTP 的 `Content-Type`；这里字节已经到手，直接看头几个字节更准，
/// 也不必为拿响应头去举 `wanting_headers` 旗（§14.1：逐条拷头是白花的钱）。
pub fn extension_of(bytes: &[u8], hint: Option<&str>) -> &'static str {
    sniff(bytes)
        .or_else(|| hint.and_then(hint_extension))
        .unwrap_or("jpg")
}

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let head = |skip: usize, take: usize| bytes.get(skip..skip + take);
    if head(0, 8).is_some_and(|magic| magic == b"\x89PNG\r\n\x1a\n") {
        return Some("png");
    }
    if head(0, 3).is_some_and(|magic| magic == b"\xff\xd8\xff") {
        return Some("jpg");
    }
    if head(0, 6).is_some_and(|magic| magic == b"GIF87a" || magic == b"GIF89a") {
        return Some("gif");
    }
    if head(0, 4).is_some_and(|magic| magic == b"RIFF")
        && head(8, 4).is_some_and(|magic| magic == b"WEBP")
    {
        return Some("webp");
    }
    if head(0, 2).is_some_and(|magic| magic == b"BM") {
        return Some("bmp");
    }
    None
}

fn hint_extension(text: &str) -> Option<&'static str> {
    let name = text
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tail = name.rsplit('.').next().filter(|tail| !tail.is_empty())?;
    // 只认这张表里的后缀：把一长串查询参数当后缀写进文件名会更糟
    match tail {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "bmp" => Some("bmp"),
        _ => None,
    }
}

/// 清洗一个文件名片段：禁用字符换下划线、连续空白折一格、结尾不许留点或空格、
/// 命中 Windows 保留名或整串为空时换兜底名，最后按字符数截断
pub fn sanitize(raw: &str, fallback: &str, max_chars: usize) -> String {
    let replaced: String = raw
        .trim()
        .chars()
        .map(|ch| {
            if FORBIDDEN.contains(&ch) || (ch as u32) < 0x20 {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapse_underscores(collapsed.trim_end_matches(['.', ' ']));
    let kept = take_chars(&trimmed, max_chars);
    let kept = kept.trim_end_matches(['.', ' ']).to_string();
    if kept.is_empty() || is_reserved(&kept) {
        fallback.to_string()
    } else {
        kept
    }
}

/// 清洗把 `<>`、`:` 这些逐个换成了下划线，连排三四个只会在文件名里堆噪音
fn collapse_underscores(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous = false;
    for ch in text.chars() {
        let underscore = ch == '_';
        if !(underscore && previous) {
            out.push(ch);
        }
        previous = underscore;
    }
    out
}

fn is_reserved(name: &str) -> bool {
    let head = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    RESERVED.contains(&head.as_str())
}

fn take_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// `%Y-%m-%d_%H-%M-%S.毫秒`：mirai 参考用的就是这个样式，冒号本来就不许出现在文件名里
pub fn stamp(when: &DateTime<Local>) -> String {
    when.format("%Y-%m-%d_%H-%M-%S%.3f").to_string()
}

/// 预览那条消息上的说明，按点名的写法：`发送人：好友名(QQ号)`
pub fn caption(name: &str, user_id: i64) -> String {
    format!("发送人：{name}({user_id})")
}

// ==================== 落盘 ====================

/// `create_new` 写第一份不覆盖：同毫秒撞名就加 `-1`、`-2` 往后试，
/// 绝不出现"后一张把前一张悄悄换掉、对方还以为存过了"
pub fn write_new(dir: &Path, stem: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|err| format!("建目录 {} 失败: {err}", dir.display()))?;
    for index in 0..=NAME_RETRY {
        let name = if index == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{index}.{ext}")
        };
        let path = dir.join(name);
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                if let Err(err) = file.write_all(bytes) {
                    return Err(format!("写 {} 失败: {err}", path.display()));
                }
                return Ok(path);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("创建 {} 失败: {err}", path.display())),
        }
    }
    Err(format!(
        "{} 里同名文件已经排到 -{NAME_RETRY}",
        dir.display()
    ))
}

fn is_http(url: &str) -> bool {
    let text = url.trim();
    has_prefix_ci(text, "http://") || has_prefix_ci(text, "https://")
}

fn has_prefix_ci(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text
            .as_bytes()
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    has_prefix_ci(text, prefix).then(|| &text[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn png() -> Vec<u8> {
        [b"\x89PNG\r\n\x1a\n".to_vec(), vec![0u8; 24]].concat()
    }

    fn sample_time() -> DateTime<Local> {
        Local
            .timestamp_opt(1_760_000_000, 123_000_000)
            .single()
            .unwrap()
    }

    #[test]
    fn extension_sniffs_magic_bytes() {
        assert_eq!(extension_of(&png(), None), "png");
        assert_eq!(extension_of(b"\xff\xd8\xff\xe0\x10JFIF", None), "jpg");
        assert_eq!(extension_of(b"GIF89a....", None), "gif");
        let mut webp = b"RIFF\x04\x00\x00\x00".to_vec();
        webp.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(extension_of(&webp, None), "webp");
        assert_eq!(extension_of(b"BM........", None), "bmp");
    }

    #[test]
    fn extension_falls_back_to_hint_then_jpg() {
        let junk = b"not a header at all".to_vec();
        assert_eq!(
            extension_of(&junk, Some("https://multimedia.example/a/b.JPG?sign=xyz#1")),
            "jpg",
            "查询与片段都得剥掉，大小写归一，jpeg 折成 jpg"
        );
        assert_eq!(extension_of(&junk, Some("https://x/y.bin")), "jpg");
        assert_eq!(extension_of(&junk, None), "jpg");
    }

    #[test]
    fn names_are_windows_safe() {
        assert_eq!(
            sanitize("a<b>:c|d/e\\f*g?\"h", "好友", 24),
            "a_b_c_d_e_f_g_h"
        );
        assert_eq!(sanitize("  多  少  空格  ", "好友", 24), "多 少 空格");
        assert_eq!(sanitize("老师.", "好友", 24), "老师", "结尾不许留点");
        assert_eq!(sanitize("CON", "好友", 24), "好友", "Windows 保留名要躲开");
        assert_eq!(sanitize("nul.png", "好友", 24), "好友");
        assert_eq!(sanitize("", "好友", 24), "好友");
        assert_eq!(sanitize("\u{1}控制符", "好友", 24), "_控制符");
        assert_eq!(
            sanitize("一二三四五六七八九十", "好友", 4).chars().count(),
            4
        );
    }

    #[test]
    fn long_nicknames_are_cut_not_dropped() {
        let long = "あ".repeat(200);
        let kept = sanitize(&long, "好友", NAME_MAX_CHARS);
        assert_eq!(kept.chars().count(), NAME_MAX_CHARS);
        assert!(!kept.is_empty());
    }

    #[test]
    fn stamp_is_a_legal_file_name_fragment() {
        let when = sample_time();
        let text = stamp(&when);
        assert_eq!(text.matches(':').count(), 0, "时间戳里不许留冒号");
        assert_eq!(text.matches('.').count(), 1, "只该有毫秒那一个点: {text}");
        assert_eq!(text, when.format("%Y-%m-%d_%H-%M-%S%.3f").to_string());
    }

    #[test]
    fn file_uri_and_plain_paths_land_on_same_file() {
        assert_eq!(
            local_path("file:///D:/%E5%9B%BE/1.jpg"),
            PathBuf::from("D:/图/1.jpg")
        );
        assert_eq!(local_path("D:\\图\\1.jpg"), PathBuf::from("D:\\图\\1.jpg"));
        assert_eq!(
            local_path("\"D:\\图\\1.jpg\""),
            PathBuf::from("D:\\图\\1.jpg")
        );
    }

    #[test]
    fn write_never_overwrites_a_previous_file() {
        let dir = temp_dir("write_new");
        let first = write_new(&dir, "同一毫秒", "png", b"first").expect("第一次该建出来");
        let second = write_new(&dir, "同一毫秒", "png", b"second").expect("撞名要往后排一个号");
        assert_ne!(first, second);
        assert_eq!(second.file_name().unwrap(), "同一毫秒-1.png");
        assert_eq!(
            fs::read(&first).unwrap(),
            b"first",
            "前一张不许被后一张换掉"
        );
        assert_eq!(fs::read(&second).unwrap(), b"second");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 目录不存在时由 `write_new` 自己建出来——落点是框架按 §7 给的，插件不预设它已经存在
    #[test]
    fn write_creates_the_missing_tree() {
        let dir = temp_dir("mkdir");
        let deep = dir.join("下一层").join("再下一层");
        let path = write_new(&deep, "a", "png", b"body").expect("缺的目录要补上");
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_local_file_says_why() {
        let err = read_local("Z:\\一定不存在\\没有.jpg").unwrap_err();
        assert!(
            err.contains("不同机"),
            "本机路径读不到最常见的理由就是不同机: {err}"
        );
    }

    #[test]
    fn caption_matches_the_asked_format() {
        assert_eq!(caption("阿罗娜", 10001), "发送人：阿罗娜(10001)");
    }

    /// 空图片段（实现端什么线索都没给）要当场报原因，一次网络都不该发
    #[tokio::test(flavor = "current_thread")]
    async fn segment_without_any_lead_fails_without_network() {
        let framework = arona::framework::Framework::new();
        let api = OneBotApi::of(&framework);
        let segment = MessageSegment::Image {
            url: None,
            file: None,
            data: None,
            file_id: None,
        };
        let err = fetch_bytes(&api, &segment).await.unwrap_err();
        assert!(
            err.contains("没给可取的线索"),
            "空图片段要说清楚为什么: {err}"
        );
    }

    /// 非图片段进不了这条链，报错也要报得对得上
    #[tokio::test(flavor = "current_thread")]
    async fn non_image_segment_is_refused() {
        let framework = arona::framework::Framework::new();
        let api = OneBotApi::of(&framework);
        let err = fetch_bytes(&api, &MessageSegment::Text("没有图".to_string()))
            .await
            .unwrap_err();
        assert!(err.contains("没有图片段"), "别把文本段当图处理: {err}");
    }

    /// 内联字节这一档不碰网络，所以「取字节 → 定后缀 → 落盘」整条能在单测里跑通
    #[tokio::test(flavor = "current_thread")]
    async fn inline_bytes_take_priority_over_a_stale_url() {
        let framework = arona::framework::Framework::new();
        let api = OneBotApi::of(&framework);
        let segment = MessageSegment::Image {
            url: Some("https://不给图的域名.invalid/a.gif".to_string()),
            file: None,
            data: Some(png()),
            file_id: None,
        };
        let bytes = fetch_bytes(&api, &segment)
            .await
            .expect("有内联字节就不必发请求");
        assert_eq!(bytes, png(), "直链写着 gif，字节是 png 就按 png 走");
        assert_eq!(extension_of(&bytes, hint_of(&segment)), "png");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("image-auto-save-test-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("临时目录建得出来");
        dir
    }
}
