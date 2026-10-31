//! 更新下载进度的系统通知镜像（仅 Android 真正发通知）。
//!
//! **纯功能**：只订阅「更新状态写出」这一个出口（`mod.rs` 的 `publish`），把
//! `UpdateState` 翻译成「通知栏现在该显示什么」，不持有更新器状态、不参与下载与
//! 安装决策。状态机怎么变，这里跟着变，不需要有人记得来改它。
//!
//! 为什么走通知栏而不是应用内进度条：手机屏幕上没有空位 —— 右上是重连/断开，
//! 底部是发送键与 Tab 栏，叠一条常驻胶囊必然挡住某处内容；而下载是后台行为，
//! 本就不该占用用户正在使用的界面。通知栏是 Android 上后台下载的惯例落点。
//!
//! 为什么由 Rust 直接推、而不是让前端订阅 `update://state` 再调桥接：下载在 Rust
//! 里跑，应用切后台后 WebView 可能被冻结，前端订阅会停摆，通知就卡在旧进度上。
//!
//! 与 Kotlin 的契约：[`NotifyCmd`] 序列化成 JSON，经 `MainActivity.updateDownloadNotification`
//! （`()Ljava/lang/String;` 同族的 `(String)→String` 签名，R8 keep 规则已覆盖）交给
//! `UpdateDownloadNotifier.kt`。改字段名/变体名必须同步那边，序列化形态有测试钉住。

// 非 Android 平台只有测试会用到这里的类型。
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use serde::Serialize;

use super::UpdateState;

/// 通知栏的目标形态（发给 Kotlin 的指令）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(super) enum NotifyCmd {
    /// 下载中：常驻进度条（不可划掉）。`indeterminate` = 总大小未知，显示不定进度。
    Downloading {
        version: String,
        percent: u8,
        indeterminate: bool,
    },
    /// 下载完成且应用在后台：留一条可点击的「已就绪」通知，点进应用走安装确认。
    Ready { version: String },
    /// 撤掉本功能发过的通知。
    Clear,
}

/// 与前端 `updatePercent` 同口径（四舍五入、封顶 100、总大小未知按 0）——
/// 通知栏与设置页的数字不能对不上。
fn percent_of(downloaded: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    (((downloaded as f64 / total as f64) * 100.0).round() as u64).min(100) as u8
}

/// 当前状态下，通知栏**应该**显示什么；`None` = 什么都不该有。
///
/// - `Ready` 且应用在前台：不留通知 —— 用户眼前就是「新版本已就绪」的浮层，
///   再在通知栏放一条只会重复；
/// - `Idle` / `Available` / `Failed`：下载没在进行，撤掉进度条。失败原因由应用内
///   浮层负责说明，后台时的失败不值得打扰（下个检查周期会自动重试）。
fn desired(state: &UpdateState, in_foreground: bool) -> Option<NotifyCmd> {
    match state {
        UpdateState::Downloading {
            version,
            downloaded,
            total,
        } => Some(NotifyCmd::Downloading {
            version: version.clone(),
            percent: percent_of(*downloaded, *total),
            indeterminate: *total == 0,
        }),
        UpdateState::Ready { version } if !in_foreground => Some(NotifyCmd::Ready {
            version: version.clone(),
        }),
        UpdateState::Ready { .. }
        | UpdateState::Idle
        | UpdateState::Available { .. }
        | UpdateState::Failed { .. } => None,
    }
}

/// 决定要不要给 Kotlin 发指令。`last` 记「通知栏上现在是什么」（`None` = 空）。
///
/// 只在**显示内容真的变了**时才发：下载循环每个 chunk 都会推进度，而同一个整数
/// 百分比最多只需要发一次（系统对通知更新有频率限制，发多了还会被丢）。
/// 反过来，本来就是空的时候不重复发 `Clear`。
pub(super) fn plan(
    state: &UpdateState,
    in_foreground: bool,
    last: &mut Option<NotifyCmd>,
) -> Option<NotifyCmd> {
    let want = desired(state, in_foreground);
    if *last == want {
        return None;
    }
    let cmd = want.clone().unwrap_or(NotifyCmd::Clear);
    *last = want;
    Some(cmd)
}

/// 把一次状态写出镜像到通知栏。非 Android 平台是空操作。
pub(super) fn mirror(state: &UpdateState) {
    #[cfg(target_os = "android")]
    android::mirror(state);
    #[cfg(not(target_os = "android"))]
    let _ = state;
}

#[cfg(target_os = "android")]
mod android {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};

    use super::{plan, NotifyCmd};
    use crate::updater::{install, UpdateState};

    /// 通知栏上现在是什么（`None` = 空）。进程内唯一一份。
    static LAST: Mutex<Option<NotifyCmd>> = Mutex::new(None);
    static TX: OnceLock<Sender<NotifyCmd>> = OnceLock::new();

    pub(super) fn mirror(state: &UpdateState) {
        let in_foreground = crate::notification::is_app_in_foreground();
        let Ok(mut last) = LAST.lock() else {
            return;
        };
        // 入队放在 LAST 锁内：「定档 + 入队」是一次原子动作，指令的投递顺序
        // 与状态的先后顺序一致（调用方 publish 本身也在更新器状态锁内）。
        if let Some(cmd) = plan(state, in_foreground, &mut last) {
            let _ = sender().send(cmd);
        }
    }

    /// 专用投递线程：JNI 调用要走 Binder 到系统进程，系统繁忙时可能卡住；
    /// 不能让它占着更新器状态锁或下载任务的 async 线程。无界队列不会阻塞发送方，
    /// 且上游已按「显示内容变化」限流，积压量有上界（至多百来条）。
    fn sender() -> &'static Sender<NotifyCmd> {
        TX.get_or_init(|| {
            let (tx, rx) = channel::<NotifyCmd>();
            let spawned = std::thread::Builder::new()
                .name("update-notify".into())
                .spawn(move || {
                    for cmd in rx {
                        deliver(&cmd);
                    }
                });
            if let Err(e) = spawned {
                // 线程起不来：rx 随之释放，后续 send 全部静默失败 ——
                // 只是少了通知栏进度，应用内（设置页）进度与更新流程不受影响。
                log::warn!("更新通知投递线程启动失败，下载进度将不出现在通知栏: {}", e);
            }
            tx
        })
    }

    fn deliver(cmd: &NotifyCmd) {
        let Ok(payload) = serde_json::to_string(cmd) else {
            return;
        };
        match install::call_native("updateDownloadNotification", Some(&payload)) {
            Ok(status) if status == "ok" => {}
            Ok(other) => log::warn!("更新通知未发出: {}", other),
            Err(e) => log::warn!("更新通知调用失败: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downloading(downloaded: u64, total: u64) -> UpdateState {
        UpdateState::Downloading {
            version: "1.7.3".into(),
            downloaded,
            total,
        }
    }

    fn ready() -> UpdateState {
        UpdateState::Ready {
            version: "1.7.3".into(),
        }
    }

    fn progress(percent: u8) -> NotifyCmd {
        NotifyCmd::Downloading {
            version: "1.7.3".into(),
            percent,
            indeterminate: false,
        }
    }

    #[test]
    fn percent_matches_frontend_rounding_and_bounds() {
        assert_eq!(percent_of(0, 100), 0);
        assert_eq!(percent_of(1, 3), 33);
        assert_eq!(percent_of(2, 3), 67);
        assert_eq!(percent_of(50, 100), 50);
        // 服务端 Content-Length 偏小：封顶 100，不溢出
        assert_eq!(percent_of(140, 100), 100);
        // 总大小未知：按 0%，不出现除零
        assert_eq!(percent_of(4096, 0), 0);
    }

    #[test]
    fn downloading_maps_to_progress_with_version_and_percent() {
        let mut last = None;
        let cmd = plan(&downloading(15, 30), true, &mut last);
        assert_eq!(cmd, Some(progress(50)));
        assert_eq!(last, Some(progress(50)));
    }

    #[test]
    fn unknown_total_is_indeterminate_not_zero_percent_bar() {
        let mut last = None;
        let cmd = plan(&downloading(4096, 0), true, &mut last);
        assert_eq!(
            cmd,
            Some(NotifyCmd::Downloading {
                version: "1.7.3".into(),
                percent: 0,
                indeterminate: true,
            })
        );
    }

    #[test]
    fn same_percent_is_sent_once_even_if_bytes_keep_arriving() {
        let mut last = None;
        assert!(plan(&downloading(500, 1000), true, &mut last).is_some());
        // 500 → 501 字节：仍是 50%，不重复发（系统对通知更新有频率限制）
        assert_eq!(plan(&downloading(501, 1000), true, &mut last), None);
        assert_eq!(plan(&downloading(504, 1000), true, &mut last), None);
        // 跨过整数百分比才再发一次
        assert_eq!(
            plan(&downloading(510, 1000), true, &mut last),
            Some(progress(51))
        );
    }

    #[test]
    fn ready_in_background_leaves_a_tappable_notification() {
        let mut last = Some(progress(100));
        let cmd = plan(&ready(), false, &mut last);
        assert_eq!(
            cmd,
            Some(NotifyCmd::Ready {
                version: "1.7.3".into()
            })
        );
    }

    #[test]
    fn ready_in_foreground_just_clears_the_progress_bar() {
        // 用户眼前就是「已就绪」浮层，通知栏不重复放一条，但进度条必须撤掉
        let mut last = Some(progress(100));
        assert_eq!(plan(&ready(), true, &mut last), Some(NotifyCmd::Clear));
        assert_eq!(last, None);
    }

    #[test]
    fn ready_in_foreground_with_empty_shade_sends_nothing() {
        let mut last = None;
        assert_eq!(plan(&ready(), true, &mut last), None);
    }

    #[test]
    fn every_non_download_state_clears_a_stale_progress_bar() {
        let others = [
            UpdateState::Idle,
            UpdateState::Available {
                version: "1.7.3".into(),
                release_url: "https://example.com".into(),
            },
            UpdateState::Failed {
                message: "磁盘空间不足".into(),
            },
        ];
        for state in others {
            let mut last = Some(progress(42));
            assert_eq!(
                plan(&state, false, &mut last),
                Some(NotifyCmd::Clear),
                "{state:?}"
            );
            assert_eq!(last, None);
        }
    }

    #[test]
    fn clear_is_not_repeated_when_shade_is_already_empty() {
        let mut last = None;
        assert_eq!(plan(&UpdateState::Idle, true, &mut last), None);
        assert_eq!(plan(&UpdateState::Idle, false, &mut last), None);
    }

    #[test]
    fn retry_after_failure_shows_progress_again() {
        let mut last = None;
        plan(&downloading(10, 100), true, &mut last);
        plan(
            &UpdateState::Failed {
                message: "网络中断".into(),
            },
            false,
            &mut last,
        );
        assert_eq!(last, None);
        // 失败后重试：同样的 10% 也要重新显示出来（通知栏是空的）
        assert_eq!(
            plan(&downloading(10, 100), false, &mut last),
            Some(progress(10))
        );
    }

    /// Kotlin 侧按这个形态解析（`UpdateDownloadNotifier.apply`）；字段名改了两边一起改。
    #[test]
    fn wire_format_is_what_kotlin_parses() {
        let json = |cmd: &NotifyCmd| serde_json::to_string(cmd).unwrap();
        assert_eq!(
            json(&progress(42)),
            r#"{"kind":"downloading","version":"1.7.3","percent":42,"indeterminate":false}"#
        );
        assert_eq!(
            json(&NotifyCmd::Ready {
                version: "1.7.3".into()
            }),
            r#"{"kind":"ready","version":"1.7.3"}"#
        );
        assert_eq!(json(&NotifyCmd::Clear), r#"{"kind":"clear"}"#);
    }
}
