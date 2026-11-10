-- SubLink 规则库结构 v1
--
-- 设计要点：
-- * `want_enabled`（用户期望态）与 `last_state`（上次检测到的真实状态）分开存。
--   UI 勾选框改的是前者，状态图标显示的是后者 —— "勾了但没生效"必须能被表达出来。
-- * `source_template` 存**未展开**的路径模板（含 %APPDATA% 等），运行时按当前用户展开，
--   这样才能一份规则库服务多用户 / 多机器。
-- * `acl_sddl` 只在需要时才非空：给重建出来的空目录回灌原本的 DACL，
--   避免"以 SYSTEM 身份重建的目录权限不对，应用写不进去"。
-- * 崩溃可恢复：非终态的 `operations` 行就是待修复清单，不给 rules 加布尔标记。

CREATE TABLE IF NOT EXISTS roots (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    label           TEXT    NOT NULL,
    path            TEXT    NOT NULL UNIQUE,
    is_ramdisk      INTEGER NOT NULL DEFAULT 1,
    wait_timeout_ms INTEGER NOT NULL DEFAULT 90000,
    created_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS rules (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT    NOT NULL,
    category          TEXT    NOT NULL DEFAULT 'other',
    source_template   TEXT    NOT NULL,
    source_norm       TEXT    NOT NULL UNIQUE,
    target_root_id    INTEGER NOT NULL REFERENCES roots(id) ON DELETE RESTRICT,
    target_subpath    TEXT    NOT NULL,
    link_type         TEXT    NOT NULL DEFAULT 'junction'
                              CHECK (link_type IN ('junction', 'symlink_dir')),
    -- 重定向方式：'link' = 在源路径上建 Junction/符号链接（默认）；
    --              'setting' = 直接改注册表里的环境变量（如 TEMP/TMP）。
    -- 两者互斥：setting 模式下 link_type 不生效（没有链接可言）。
    redirect_mode     TEXT    NOT NULL DEFAULT 'link'
                              CHECK (redirect_mode IN ('link', 'setting')),
    -- setting 模式要改的环境变量名（JSON 数组，如 ["TEMP","TMP"]）。
    -- link 模式恒为空串。
    env_vars          TEXT    NOT NULL DEFAULT '[]',
    -- setting 模式的写入作用域：'user' = HKCU（免提权）/ 'machine' = HKLM（需提权）。
    env_scope         TEXT    NOT NULL DEFAULT 'user'
                              CHECK (env_scope IN ('user', 'machine')),
    want_enabled      INTEGER NOT NULL DEFAULT 0,
    rebuild_on_boot   INTEGER NOT NULL DEFAULT 1,
    acl_sddl          TEXT,
    needs_elevation   INTEGER NOT NULL DEFAULT 0,
    notes             TEXT,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    -- 清理排除模式（JSON 数组，如 ["sessions","logs/old"]）。
    -- 只影响 clean：命中的子树不删不剪；apply/restore 搬运永远是全量。
    excludes          TEXT NOT NULL DEFAULT '[]',
-- 水位预授权：空 = 未启用；empty = 水位 critical 时清空（仍受 excludes 保护）；
    -- older:<秒> = 只清该秒之前修改的。仅 water 巡检触发，用户手动 clean 不受影响。
    auto_clean_policy TEXT NOT NULL DEFAULT '',
    -- 文件夹级容量上限（字节，0 = 不启用）：本规则目录自身占用达到这个量就按
    -- auto_clean_policy 清理，不看卷水位。与水位 critical 是两条独立的触发路径。
    auto_clean_limit_bytes INTEGER NOT NULL DEFAULT 0,

    -- 以下为状态缓存，真相由实时检测给出（last_* 只用于列表排序与离线展示）
    last_state        TEXT,
    last_checked_at   TEXT,
    last_error        TEXT,
    last_files        INTEGER,
    last_bytes        INTEGER,
    applied_at        TEXT,

    created_at        TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_rules_cat     ON rules(category, sort_order);
CREATE INDEX IF NOT EXISTS idx_rules_enabled ON rules(want_enabled, rebuild_on_boot);
CREATE INDEX IF NOT EXISTS idx_rules_root    ON rules(target_root_id);

CREATE TABLE IF NOT EXISTS operations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id     INTEGER REFERENCES rules(id) ON DELETE SET NULL,
    kind        TEXT NOT NULL,
    phase       TEXT NOT NULL,
    source_path TEXT,
    target_path TEXT,
    file_count  INTEGER,
    bytes       INTEGER,
    error       TEXT,
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_ops_open ON operations(phase);

CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id INTEGER REFERENCES rules(id) ON DELETE CASCADE,
    level   TEXT NOT NULL,
    kind    TEXT NOT NULL,
    message TEXT NOT NULL,
    at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_events_at ON events(at DESC);

CREATE TABLE IF NOT EXISTS settings (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);

-- 撤销栈：界面上的「撤销」按钮只有一条栈，删除与修改共用。
-- * seq：一次用户操作 = 一个序号。撤销弹掉最近那一个 seq（批量删 N 条就整批回来）。
-- * op：'delete' = 规则整行被搬走（回收站语义，载荷是整行快照 + 事件 + 隐藏态）；
--       'update' = 规则还在、只是字段被改了（载荷是字段级前后值）。
-- * 删除用整行快照、修改用字段级快照：状态缓存（last_state / applied_at …）是
--   磁盘现状的缓存，回滚它等于把上次检测的结果谎报成"改之前的样子"。
CREATE TABLE IF NOT EXISTS rule_undo (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    seq           INTEGER NOT NULL,
    op            TEXT    NOT NULL,
    rule_id       INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    payload       TEXT    NOT NULL,
    created_at    TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_undo_seq ON rule_undo(seq);
