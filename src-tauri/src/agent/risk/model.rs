use super::checker::{is_listing_only, is_mkfs, is_read_only_system_query, DISK_COMMANDS};
use super::disposition::Assessment;
use super::parser::ParsedSegment;

/// 系统级命令：动的是整台机器的全局状态（服务、用户、防火墙、挂载、权限）。
///
/// 这类命令不管在什么模式下都得有人点头 —— 它们是「强制审批」这一档的主要来源。
/// 判据是"影响范围超出当前工作目录"，不是"命令名听起来危险"。
///
/// 杀进程的三兄弟不在这里：`kill <PID>` 是精确操作（查到 PID 才会杀），交给
/// 名单层管；按名字批量杀的 `pkill` / `killall` 走直接拒绝（见
/// [`NAME_BASED_KILL_COMMANDS`]）—— 那种写法不值得弹窗打扰人。
pub const SYSTEM_LEVEL_COMMANDS: &[&str] = &[
    "reboot",
    "shutdown",
    "poweroff",
    "halt",
    "init",
    "mount",
    "umount",
    "useradd",
    "userdel",
    "usermod",
    "groupadd",
    "groupdel",
    "passwd",
    "su",
    "sudo",
    "chroot",
    "systemctl",
    "service",
    "iptables",
    "ip6tables",
    "nft",
    "ufw",
    "firewall-cmd",
    "crontab",
    "at",
    "chmod",
    "chown",
    "chgrp",
];

/// 按名字批量杀进程的命令：命中面由名字匹配决定（`pkill -f ssh` 连自己的
/// SSH 会话都能一起带走），agent 拿它动手等于草率，而且几乎总有更精确的替代。
/// 不弹窗要人看 —— **直接拒绝并把替代写法写进理由**，模型会照着换，用户零打扰。
pub const NAME_BASED_KILL_COMMANDS: &[&str] = &["pkill", "killall"];

/// 单段命令的**基础档位**：只看命令名与 `sudo` 包裹，不看策略、不看路径参数。
///
/// 返回 `Allow` 不代表"这条命令随便跑" —— 它只表示**这一层没有意见**，档位交给
/// 后面的命令名单去定（白名单命中就放行、黑名单命中就要审批）。所以这里刻意不再
/// 分"低风险 / 中风险"：那两级算出来也没人拿它做不同的决定，只是徒增维护点。
///
/// `Approval` 目前只有一个来源 —— sudo 包装的保底档（见下）。它与 `Allow` 的差别
/// 是普通模式下必问一次，Auto 模式跳过。
///
/// 反过来，返回 `ForceApproval` 是**这一层的最终意见**：不管名单怎么配、不管
/// 是不是 Auto 模式，都要有人确认。
pub fn base_assessment(parsed: &ParsedSegment) -> Assessment {
    let base = parsed.base_cmd.as_str();
    // sudo 包装只**保底**抬到「请求审批」：Auto 模式跳过这档，普通模式要人点头。
    // 里面的命令照常逐条评估取最严 —— `sudo systemctl restart nginx` 被系统级命令
    // 抬回强制审批，`sudo rm -rf /` 走灾难判定直接拒绝；sudo 自己只贡献这一档，
    // 判据是"里面跑了什么"，不是"有没有 sudo"。`-n` 只是非交互（要密码就失败），
    // 不改变提权这件事本身，也不额外加重。
    let mut worst = if parsed.sudo_wrapped {
        Assessment::approval("命令经 sudo 提权执行")
    } else {
        Assessment::allow()
    };
    // 命令位置是个变量（`$CMD` / `${CMD}`，含 `bash -c "$CMD"` 递归进来的段）：
    // 展开成什么运行时才知道，静态判定评的是假命令。直接拒绝并要求写明原文。
    // 参数位置的变量（`echo $HOME`）不在此列。
    if base.starts_with('$') {
        worst = worst.worst(Assessment::denied(
            "命令写在了变量里（$…），展开后的内容无法静态判定 —— 请把要执行的命令原文写出来",
        ));
    }
    if NAME_BASED_KILL_COMMANDS.contains(&base) {
        worst = worst.worst(Assessment::denied(format!(
            "`{}` 按名字批量杀进程，误伤面不可控 —— 请用 `ps`/`pgrep` 查到 PID 后 `kill <PID>`，或用 `systemctl` 管理服务",
            base
        )));
    }
    if SYSTEM_LEVEL_COMMANDS.contains(&base) {
        // `systemctl status` / `service x status` / 裸 `mount` / `crontab -l` 这类
        // 查询形态什么也不改，别把它们和 `restart` / `umount` 一起抬档。
        if !is_read_only_system_query(base, &parsed.args) {
            worst = worst.worst(Assessment::forced(format!(
                "`{}` 是系统级命令，影响整台机器",
                base
            )));
        }
    } else if is_mkfs(base) || DISK_COMMANDS.contains(&base) {
        // `fdisk -l` / `parted --list` 只是把分区表打出来看，没有写盘动作。
        if !is_listing_only(base, &parsed.args) {
            worst = worst.worst(Assessment::forced(format!(
                "`{}` 直接操作磁盘，数据无法恢复",
                base
            )));
        }
    }
    worst
}
