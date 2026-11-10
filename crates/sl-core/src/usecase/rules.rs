//! 规则与目标根的增删改用例（含表单校验、ACL 模板存取）。

use crate::acl::export_sddl;
use crate::engine;
use crate::error::{SlError, SlResult};
use crate::i18n::{t, tf};
use crate::link::LinkType;
use crate::store::{Store, UndoOutcome, UndoState};

use super::view::RuleForm;

pub fn add_root(
    store: &Store,
    label: &str,
    path: &str,
    is_ramdisk: bool,
    wait_timeout_ms: i64,
) -> SlResult<i64> {
    if label.trim().is_empty() {
        return Err(SlError::InvalidPath(t("form.name_empty").into()));
    }
    if path.trim().is_empty() {
        return Err(SlError::InvalidPath(t("form.path_empty").into()));
    }
    store.ensure_root(label, path, is_ramdisk, wait_timeout_ms)
}

pub fn delete_root(store: &Store, id: i64) -> SlResult<()> {
    store.delete_root(id)
}

pub fn add_rule(store: &Store, form: &RuleForm) -> SlResult<i64> {
    validate_form(form)?;
    validate_setting_mode(store, form)?;
    store.insert_rule(&form.to_new_rule())
}

pub fn update_rule(store: &Store, id: i64, form: &RuleForm) -> SlResult<()> {
    validate_form(form)?;
    validate_setting_mode(store, form)?;
    let old = store.get_rule(id)?;
    let next = form.to_new_rule();

    // 目标（或链接类型）改了、源模板没动：源路径上已有的托管链接必须同步重建，
    // 否则它会继续指向旧目标 —— 之后「应用」会因为 target_mismatch 直接报错，
    // 「重建链接」与开机重建也会因为"链接有效"而放过它。
    //
    // 源模板也变了的话，旧链接挂在另一条路径上，这里不能擅自猜，仍交给用户重新应用。
    let retarget = old.source_template == next.source_template
        && (old.target_root_id != next.target_root_id
            || old.target_subpath != next.target_subpath
            || old.link_type != next.link_type);
    if retarget {
        let mut candidate = old.clone();
        candidate.target_root_id = next.target_root_id;
        candidate.target_subpath = next.target_subpath.clone();
        candidate.link_type = next.link_type.clone();
        // 重建失败会让整个保存失败 —— 数据库保持旧值，链接也回滚到旧目标，
        // 不会出现"配置已改、链接还指着别处"的中间态。
        engine::retarget_rule_link(store, &candidate)?;
    }

    store.update_rule(id, &next)
}

/// 分类名校验：内置分类之外允许自定义（勾选后批量归类可直接新建分类）。
/// 只阻挡空名 / 超长 / 控制字符 —— 侧栏、下拉和落库都按纯文本处理。
fn validate_category_name(category: &str) -> SlResult<()> {
    let name = category.trim();
    if name.is_empty() || name.chars().count() > 24 || name.chars().any(char::is_control) {
        return Err(SlError::InvalidPath(t("form.bad_category").into()));
    }
    Ok(())
}

/// 批量归类：把一批规则统一移到 `category`（GUI 勾选后「归类到…」共用）。
pub fn set_category(store: &Store, ids: &[i64], category: &str) -> SlResult<usize> {
    validate_category_name(category)?;
    store.set_rules_category(ids, category.trim())
}

pub(crate) fn validate_form(form: &RuleForm) -> SlResult<()> {
    if form.name.trim().is_empty() {
        return Err(SlError::InvalidPath(t("form.rule_name_empty").into()));
    }
    if form.source_template.trim().is_empty() {
        return Err(SlError::InvalidPath(t("form.source_empty").into()));
    }
    validate_category_name(&form.category)?;
    if form.target_subpath.trim().is_empty() {
        return Err(SlError::InvalidPath(t("form.subpath_empty").into()));
    }
    if LinkType::parse(&form.link_type).is_none() {
        return Err(SlError::InvalidPath(t("form.bad_link_type").into()));
    }
    // 重定向方式：只能是 link / setting。
    if form.redirect_mode != crate::store::REDIRECT_MODE_LINK
        && form.redirect_mode != crate::store::REDIRECT_MODE_SETTING
    {
        return Err(SlError::InvalidPath(t("form.bad_redirect_mode").into()));
    }
    // 系统设置模式：变量名必须在白名单内，作用域必须合法。
    if form.redirect_mode == crate::store::REDIRECT_MODE_SETTING {
        crate::sysenv::normalize_vars(&form.env_vars)?;
        if crate::sysenv::EnvScope::parse(&form.env_scope).is_none() {
            return Err(SlError::InvalidPath(t("form.bad_env_scope").into()));
        }
    }
    // 排除模式的结构校验（空/超长/超量/..）由 Excludes 统一给出。
    crate::fsops::exclude::Excludes::new(&form.excludes)?;
    // 水位预授权策略的格式校验（empty / older:<秒>）。
    super::water::validate_auto_policy(&form.auto_clean_policy)?;
    // 文件夹级容量上限：负数没有意义；配了上限却没策略 = 到了量也无从下手
    // （策略决定"删什么"，上限只决定"什么时候删"）。
    if form.auto_clean_limit_bytes < 0 {
        return Err(SlError::InvalidPath(tf(
            "form.bad_auto_limit",
            &[("n", &form.auto_clean_limit_bytes.to_string())],
        )));
    }
    if form.auto_clean_limit_bytes > 0 && form.auto_clean_policy.trim().is_empty() {
        return Err(SlError::InvalidPath(
            t("form.auto_limit_needs_policy").into(),
        ));
    }
    Ok(())
}

/// 「系统设置模式」的前提：源路径（或已应用过的目标路径）正好是那些变量当前指向的目录。
///
/// 这个模式改的是注册表里的环境变量值 —— 变量指向别处就落不了地
/// （apply 时会以 `source_mismatch` 被安全检查拒掉）。所以在**保存**这一关
/// 就拦下来，比等用户点「应用」失败、面对一句看不懂的报错强。
///
/// 放行两种情况：
/// * 变量当前指向**源路径** —— 这条规则还没应用过（正常形态）；
/// * 变量当前指向**目标路径** —— 这条规则已经应用过（本工具把变量改到了目标，
///   源位置只是它当初的来处）。
///
/// 变量在注册表里读不到（压根没这个值）时**放行**：那时无从判断，
/// 交给 apply 时的检查说话 —— 它对读不到的变量同样是跳过的。
fn validate_setting_mode(store: &Store, form: &RuleForm) -> SlResult<()> {
    if form.redirect_mode != crate::store::REDIRECT_MODE_SETTING {
        return Ok(());
    }
    let vars = crate::sysenv::normalize_vars(&form.env_vars)?;
    let Some(scope) = crate::sysenv::EnvScope::parse(&form.env_scope) else {
        return Err(SlError::InvalidPath(t("form.bad_env_scope").into()));
    };
    let (source, unknown) = crate::util::expand_path(&form.source_template);
    if !unknown.is_empty() {
        // 源路径里有认不出的变量：展开不了就判不了，不在这里误伤。
        return Ok(());
    }
    let mut allowed = vec![source];
    if let Ok(root) = store.get_root(form.target_root_id) {
        let (base, unk) = root.resolved_path();
        if unk.is_empty() {
            let sub = form.target_subpath.trim().trim_start_matches(['\\', '/']);
            allowed.push(if sub.is_empty() { base } else { base.join(sub) });
        }
    }
    for name in &vars {
        let Ok(Some(cur)) = crate::sysenv::read_env_expanded(scope, name) else {
            continue;
        };
        let cur = cur.trim();
        if cur.is_empty() {
            continue;
        }
        let cur_path = std::path::PathBuf::from(cur);
        if !allowed.iter().any(|p| crate::util::path_eq(p, &cur_path)) {
            return Err(SlError::InvalidPath(tf(
                "form.setting_source_mismatch",
                &[("var", name)],
            )));
        }
    }
    Ok(())
}

/// 删除规则：移入回收站（可撤销）。
///
/// **不是真删** —— 规则整行搬进撤销栈，界面上点「撤销」能原样搬回来。
/// 磁盘上的目录与链接不受影响（这与删除前的行为一致：删的只是规则记录）。
pub fn delete_rules(store: &Store, ids: &[i64]) -> SlResult<usize> {
    let (moved, _) = store.trash_rules(ids)?;
    for id in ids {
        store.log_event(
            None,
            "info",
            "rule_deleted",
            &tf("rules.deleted_log", &[("id", &id.to_string())]),
        )?;
    }
    Ok(moved)
}

/// 撤销最近一次操作：删除（把规则搬回规则库）或修改（把字段改回去）。
///
/// 一次操作 = 一个撤销序号，撤销弹掉最近那一个；连续撤销可以一路退回去。
///
/// **撤销只动记录，不搬数据、不删目录** —— 删除本来就没碰过磁盘上的东西。
/// 唯一的例外：修改把「目标」改掉了，正向操作会把托管链接重建到新目标，
/// 撤销后要把链接**指回**旧目标（`engine::retarget_rule_link`），
/// 否则会留下"记录说旧目标、链接却指着新目标"的中间态。
/// 这一步失败只提示、不回滚已恢复的记录 —— 用户可以在界面上「重建链接」补上。
///
/// 撤销栈是空的时候返回空结果（不是错误 —— "没什么可撤销"是正常状态，不是失败）。
pub fn undo_last(store: &Store) -> SlResult<UndoOutcome> {
    let mut outcome = store.undo_last()?;

    // 目标被改回去了 → 把链接指回旧目标。
    for id in outcome.need_relink.clone() {
        let rule = match store.get_rule(id) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match engine::retarget_rule_link(store, &rule) {
            Ok(_) => {}
            Err(e) => outcome.skipped.push(tf(
                "undo.relink_failed",
                &[("name", &rule.name), ("err", &e.to_string())],
            )),
        }
    }

    store.log_event(
        None,
        "info",
        "rule_undone",
        &tf(
            "rules.undone_log",
            &[("n", &outcome.restored.len().to_string())],
        ),
    )?;
    Ok(outcome)
}

/// 撤销栈现状：还有多少条等着撤销、栈顶是什么操作（界面据此决定「撤销」按钮的可用态与提示）。
pub fn undo_state(store: &Store) -> SlResult<UndoState> {
    store.undo_state()
}

pub fn set_enabled(store: &Store, ids: &[i64], enabled: bool) -> SlResult<usize> {
    // 整批走一次存储层调用：一次开关 = 一个撤销序号，撤销能整批改回去。
    store.set_rules_enabled(ids, enabled)?;
    Ok(ids.len())
}

/// settings 键：界面上被隐藏的规则 id（JSON 数组）。
///
/// 隐藏是**展示偏好**，不是规则属性：它不进 `rules` 表，所以不参与开机重建 /
/// 水位巡检 / 批量操作。但它**随配置导出导入**：导出时逐条挂在规则上
/// （`portable::BundleRule.hidden`），导入时按来源路径重新绑定到本机规则 ——
/// 规则 id 跨机器没有意义。
///
/// 读写原语在 `store` 层（`portable` 位于 `usecase` 之下，不能反向依赖本模块），
/// 这里只保留同名门面，避免调用方四处改。
pub use crate::store::KEY_HIDDEN_RULES;

/// 当前被隐藏的规则 id（转发 [`Store::hidden_rule_ids`]）。
///
/// 手工改库写坏 JSON 时按"没有隐藏"处理；返回值按 id 升序，便于落库稳定、也便于测试断言。
pub fn hidden_rule_ids(store: &Store) -> SlResult<Vec<i64>> {
    store.hidden_rule_ids()
}

/// 设置一批规则的隐藏态（转发 [`Store::set_rules_hidden`]）。
///
/// `hidden=true` 隐藏 / `false` 恢复显示；不存在的 id 静默忽略（可能刚被别处删掉）。
/// 返回写入后的隐藏总数。
pub fn set_hidden(store: &Store, ids: &[i64], hidden: bool) -> SlResult<usize> {
    store.set_rules_hidden(ids, hidden)
}

/// 同一分类内调整规则顺序（GUI 拖拽排序）。
///
/// 约束：`ordered_ids` 必须是**该分类的完整顺序**（即便界面正被筛选/搜索，
/// 前端也要从全量快照组装）。原因：`set_rules_order` 只重写传入的 id，
/// 若只传筛选后的子集，子集拿 1..n、隐藏规则保留旧值，新旧交错后顺序不可预期。
/// 这条约束在这里强制校验（数量必须等于该分类规则总数），不靠调用方自觉。
pub fn reorder_rules(store: &Store, ordered_ids: &[i64]) -> SlResult<usize> {
    if ordered_ids.is_empty() {
        return Ok(0);
    }
    let mut seen = std::collections::HashSet::new();
    for id in ordered_ids {
        if !seen.insert(*id) {
            return Err(SlError::Other(tf(
                "form.duplicate_id",
                &[("id", &id.to_string())],
            )));
        }
    }
    // 先全部取出（不存在的 id 在这一步就报错），同时校验同类。
    let mut rules = Vec::with_capacity(ordered_ids.len());
    for id in ordered_ids {
        rules.push(store.get_rule(*id)?);
    }
    let category = rules[0].category.clone();
    if let Some(r) = rules.iter().find(|r| r.category != category) {
        return Err(SlError::Other(tf(
            "form.cross_category",
            &[
                ("name", &r.name),
                ("id", &r.id.to_string()),
                ("cat", &r.category),
                ("other", &category),
            ],
        )));
    }
    // 完整性：列表查询按 category 分组展示，缺一条就会让被漏掉的规则顺序悬空。
    let total = store
        .list_rules()?
        .iter()
        .filter(|r| r.category == category)
        .count();
    let given = ordered_ids.len();
    if total != given {
        return Err(SlError::Other(tf(
            "form.incomplete_order",
            &[
                ("category", &category),
                ("given", &given.to_string()),
                ("total", &total.to_string()),
            ],
        )));
    }
    let n = store.set_rules_order(ordered_ids)?;
    store.log_event(
        None,
        "info",
        "rules_reordered",
        &tf(
            "form.reordered_log",
            &[("category", &category), ("n", &n.to_string())],
        ),
    )?;
    Ok(n)
}

/// 保存一条目录的 DACL 作为重建模板（用于需要提权的规则）。
pub fn capture_acl(store: &Store, id: i64) -> SlResult<String> {
    let rule = store.get_rule(id)?;
    let resolved = engine::resolve(store, &rule)?;
    if !resolved.source.is_dir() {
        return Err(SlError::InvalidPath(tf(
            "form.acl_no_source",
            &[("path", &resolved.source.display().to_string())],
        )));
    }
    let sddl = export_sddl(&resolved.source)?;
    store.set_rule_flags(id, rule.rebuild_on_boot, &rule.link_type, Some(&sddl))?;
    store.log_event(
        Some(id),
        "info",
        "acl_captured",
        &tf("form.acl_captured_log", &[("name", &rule.name)]),
    )?;
    Ok(sddl)
}

pub fn clear_acl(store: &Store, id: i64) -> SlResult<()> {
    let rule = store.get_rule(id)?;
    store.set_rule_flags(id, rule.rebuild_on_boot, &rule.link_type, None)
}
