//! 帮助文本。

use sl_core::i18n::{locale, Locale};

pub(crate) fn print_help() {
    match locale() {
        Locale::Zh => print_help_zh(),
        Locale::En => print_help_en(),
    }
}

fn print_help_zh() {
    println!(
        r#"SubLink —— 把软件缓存目录重定向到 RAM 盘的软链接管理器

用法：sublink <命令> [参数] [--json]

全局参数：
  --data-dir <路径>          覆盖数据目录（等效环境变量 SUBLINK_DATA_DIR）
  --lang zh|en               界面语言（等效环境变量 SUBLINK_LANG）

=== 概览 ===
  doctor                     环境自检：数据目录、链接能力、目标盘、水位、规则状态、待修复项
  version                    版本信息

=== 目标根（RAM 盘）===
  roots list
  roots add <名称> <路径> [--timeout 毫秒] [--hdd]
  roots rm <id>
  volume <R: 或 R:\Dir> [--timeout 毫秒]     探测 / 等待卷就绪
   water [--json]                             RAM 盘水位巡检（级别变化才落事件，
                                               阈值默认 85%/95%，settings 的
                                               water_warn_pct / water_critical_pct 可覆盖）
   water sweep <策略> [--yes]                 水位一键清理：全部规则按统一策略批量
                                               预演/执行（策略与 clean 相同：
                                               --empty / --yesterday / --older-than 1h …）

=== 规则 ===
  rules list [--enabled] [--category 分类] [--search 关键词] [--json]
  rules add <名称> <源路径模板> <目标子目录> [--root id] [--category 分类]
            [--link-type junction|symlink_dir] [--enable] [--no-boot] [--notes 备注]
            [--redirect-mode link|setting]           重定向方式：link=在源路径建 Junction（默认）
                                                 setting=直接改系统环境变量（不建链接）
            [--env-vars TEMP,TMP]                   setting 模式要改的环境变量（逗号分隔）
                                                 可选：TEMP / TMP
            [--env-scope user|machine]              setting 模式的写入作用域：
                                                 user=HKCU（当前用户，免提权）
                                                 machine=HKLM（全机，需管理员）
            [--exclude 模式]                     清理时跳过的子目录/文件，可重复多条
                                                 如 --exclude sessions --exclude "logs\\old"
            [--auto-clean 策略]                  水位到严重线（默认95%）时的预授权清理，两种口径：
                                                 empty（清空）/ older:<秒>（只清早于该秒修改的），
                                                 或 keep:<字节>（按空间：删到不高于该下限、保留时间最近的），
                                                 如 --auto-clean older:86400 或 keep:104857600（100MB）；缺省 = 不自动清理
            [--auto-clean-limit MB]               文件夹级容量上限（MB）：该规则目录自身占用达到
                                                 上限即触发一次水位自动清理（按上面的策略清），
                                                 不看整块盘水位；策略用 keep 时它只管何时触发、
                                                 保留量由 keep 决定；缺省 = 不启用（须与 --auto-clean 同给）
  rules rm <id...>                            移入回收站（不真删），可撤销
  rules undo                                  撤销最近一次操作（删除或修改）
  rules enable|disable <id...|--all>

=== 体检与操作 ===
  status [--enabled|--all] [--category 分类] [--search 关键词] [--no-persist] [--json]
  locks <规则id|目录路径> [--json]          检测占用并指出是哪个进程占着
  clean <规则id|目录路径> <策略> [--yes] [--json]
        清理文件夹（永久删除，默认只预演，加 --yes 才执行）
        策略五选一：
          --empty                        清空全部文件
          --yesterday                    昨天及更早修改的（本地零点前）
          --older-than <时长>            早于某时刻，如 1h / 30m / 7d / today / Unix秒
          --from <时> --to <时>          指定时间段 [from,to)，如 --from 7d --to 1d
          --keep <MB>                    按空间：从最旧删起，直到目录占用不高于该水位下限（MB），
                                         保留时间最近的 <MB>；始终至少保留最新 1 个文件
  apply <id...|--all> [--dry-run] [--merge] [--sha256] [--skip-lock-check] [--best-effort]
  restore <id...|--all> [--dry-run] [--sha256] [--best-effort]
  relink <id>                                只重建链接（不动数据）
  --best-effort                              尽力模式：单条目读/拷失败就跳过；删不掉的源条目
                                             会报错并保留已复制的数据（受保护目录配合提权使用）

=== 权限 ===
  acl check [--json] [--sid <SID>]              体检：逐个查看目标根 / 目标目录 / 链接条目
                                             对当前用户（令牌里的账号）有没有完全控制
  acl fix [--json] [--sid <SID>]               修复：把缺权限的目录补上（不动数据、不改卷根）
                                             --sid 显式指定要授权的账号（提权时用，
                                             防止 UAC 换了管理员账号就授错人）

=== 开机重建 ===
  boot-rebuild [--json] [--elevated-only|--skip-elevated|--setting-only]
                                             重建目标目录并修复链接；--elevated-only 只处理
                                             受保护源（C:\Windows 等），--skip-elevated 相反，
                                             --setting-only 只处理系统设置模式规则（登录前那一趟）
  autostart status                           查看"登录时自动重建"任务是否已注册（含高权限任务）
  autostart enable [--exe <路径>]            注册（用户级，不需要管理员权限）
  autostart enable --elevated                注册高权限登录任务（只重建受保护目录，会弹 UAC）
  autostart enable --startup                 注册系统启动时任务（SYSTEM 身份，登录前重建
                                             系统设置模式规则的目录，会弹 UAC）
  autostart disable [--elevated|--startup]   取消注册（--elevated 取消高权限任务，
                                             --startup 取消系统启动时任务，都会弹 UAC）
  autostart run                              立刻手动触发一次，验证配置

=== 提权 worker（一般不需要手动调）===
  elevated <job.json>                        以管理员身份执行一份 job（只认规则 id，不认路径）

=== 配置迁移 ===
  export <文件.json>
  import <文件.json> [--dry-run] [--ram-root R:]

=== 审计 ===
  events [--limit N]
  ops [--limit N]                            操作流水（含待修复项）
  stats [--json]                             效益统计：重定向规模 / 累计搬运 / 累计清理

分类取值：browser / design / dev / system / media / office / game / other

示例：
  sublink roots add RAMDisk R:
  sublink rules add "Chrome 缓存" "%LOCALAPPDATA%\Google\Chrome\User Data\Default\Cache" browsers\chrome-cache --category browser --enable
  sublink status --all
  sublink apply --all --dry-run
  sublink apply 1
  sublink restore 1
  sublink clean 3 --older-than 1d          预演：规则#3 真实目录里 1 天前的文件
  sublink clean 3 --older-than 1d --yes    确认后执行删除
  sublink autostart enable
"#
    );
}

fn print_help_en() {
    println!(
        r#"SubLink — redirect software cache directories to a RAM disk via directory links

Usage: sublink <command> [options] [--json]

Global options:
  --data-dir <path>          Override data directory (equivalent to env var SUBLINK_DATA_DIR)
  --lang zh|en               UI language (equivalent to env var SUBLINK_LANG)

=== Overview ===
  doctor                     Environment self-check: data dir, link capability, target disk, water level, rule status, pending repairs
  version                    Version information

=== Target roots (RAM disk) ===
  roots list
  roots add <name> <path> [--timeout ms] [--hdd]
  roots rm <id>
  volume <R: or R:\Dir> [--timeout ms]        Probe / wait for volume to be ready
   water [--json]                             RAM disk water level patrol (events only on level change;
                                               thresholds default 85%/95%, overridable via settings
                                               water_warn_pct / water_critical_pct)
   water sweep <policy> [--yes]               One-click water cleanup: batch dry-run/execute all rules
                                               with a unified policy (same as clean:
                                               --empty / --yesterday / --older-than 1h …)

=== Rules ===
  rules list [--enabled] [--category cat] [--search keyword] [--json]
  rules add <name> <source-template> <target-subpath> [--root id] [--category cat]
            [--link-type junction|symlink_dir] [--enable] [--no-boot] [--notes note]
            [--redirect-mode link|setting]           Redirect method: link=build a junction at the source (default)
                                                 setting=rewrite system environment variables (no link)
            [--env-vars TEMP,TMP]                   Env vars to rewrite in setting mode (comma separated)
                                                 Choices: TEMP / TMP
            [--env-scope user|machine]              Write scope in setting mode:
                                                 user=HKCU (current user, no elevation)
                                                 machine=HKLM (whole machine, needs admin)
            [--exclude pattern]                     Subdirs/files to skip during clean; can repeat
                                                 e.g. --exclude sessions --exclude "logs\\old"
            [--auto-clean policy]                  Pre-authorized cleanup when water hits critical (default 95%), two flavors:
                                                 empty (clear all) / older:<seconds> (only files modified before that),
                                                 or keep:<bytes> (by space: delete until usage is at or below the floor, keeping the newest),
                                                 e.g. --auto-clean older:86400 or keep:104857600 (100MB); default = no auto-clean
            [--auto-clean-limit MB]                Per-folder capacity limit (MB): when the rule's own
                                                 directory reaches it, that triggers a water-level
                                                 auto-clean (by the policy above) without waiting for
                                                 the volume water level; with the keep policy it only decides when to trigger,
                                                 the floor decides how much to keep; default = off (requires
                                                 --auto-clean)
  rules rm <id...>                            Move to the recycle bin (not a real delete); can be undone
  rules undo                                  Undo the most recent operation (delete or edit)
  rules enable|disable <id...|--all>

=== Inspection & operations ===
  status [--enabled|--all] [--category cat] [--search keyword] [--no-persist] [--json]
  locks <rule-id|dir-path> [--json]          Detect occupancy and identify which process holds it
  clean <rule-id|dir-path> <policy> [--yes] [--json]
        Clean a folder (permanent deletion; dry-run by default, add --yes to execute)
        Policy (pick one):
          --empty                        Clear all files
          --yesterday                    Modified yesterday or earlier (before local midnight)
          --older-than <duration>        Older than a moment, e.g. 1h / 30m / 7d / today / Unix-seconds
          --from <time> --to <time>      Time range [from,to), e.g. --from 7d --to 1d
          --keep <MB>                    By space: delete oldest first until the folder usage is at or
                                         below this floor (MB); always keeps at least the newest file
  apply <id...|--all> [--dry-run] [--merge] [--sha256] [--skip-lock-check] [--best-effort]
  restore <id...|--all> [--dry-run] [--sha256] [--best-effort]
  relink <id>                                Rebuild link only (no data movement)
  --best-effort                              Best effort: skip entries that cannot be read/copied;
                                             if source entries cannot be deleted, report and keep the
                                             copied data (use with elevation for protected folders)

=== Permissions ===
  acl check [--json] [--sid <SID>]             Check whether the current user (from the process token)
                                             has full control over every target root / target dir / link
  acl fix [--json] [--sid <SID>]               Grant the missing permissions (no data moves, volume roots
                                             are left alone); --sid pins the account to grant

=== Boot rebuild ===
  boot-rebuild [--json] [--elevated-only|--skip-elevated|--setting-only]
                                             Rebuild target dirs and repair links; --elevated-only
                                             handles protected sources only (C:\Windows, ...),
                                             --skip-elevated does the opposite,
                                             --setting-only handles system-setting rules only
                                             (the run before user logon)
  autostart status                           Check the "rebuild at logon" tasks (normal + elevated)
  autostart enable [--exe <path>]            Register (user-level, no administrator required)
  autostart enable --elevated                Register the elevated task (protected folders only, UAC)
  autostart enable --startup                 Register the system startup task (runs as SYSTEM
                                             before logon, rebuilds system-setting rule dirs, UAC)
  autostart disable [--elevated|--startup]   Unregister (--elevated removes the elevated task,
                                             --startup removes the startup task; both need UAC)
  autostart run                              Trigger manually once to verify configuration

=== Elevated worker (usually not called manually) ===
  elevated <job.json>                        Execute a job as administrator (rule IDs only, no paths)

=== Config migration ===
  export <file.json>
  import <file.json> [--dry-run] [--ram-root R:]

=== Audit ===
  events [--limit N]
  ops [--limit N]                            Operation log (including pending repairs)
  stats [--json]                             Benefit stats: redirect scale / cumulative moves / cumulative cleans

Category values: browser / design / dev / system / media / office / game / other

Examples:
  sublink roots add RAMDisk R:
  sublink rules add "Chrome cache" "%LOCALAPPDATA%\Google\Chrome\User Data\Default\Cache" browsers\chrome-cache --category browser --enable
  sublink status --all
  sublink apply --all --dry-run
  sublink apply 1
  sublink restore 1
  sublink clean 3 --older-than 1d          Dry-run: files older than 1 day in rule #3's real directory
  sublink clean 3 --older-than 1d --yes    Confirm and execute deletion
  sublink autostart enable
"#
    );
}