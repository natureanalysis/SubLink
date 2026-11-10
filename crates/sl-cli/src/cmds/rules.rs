//! `rules` 子命令：规则的增删查与期望开关。

use sl_core::i18n::{t, tf};
use sl_core::store::{NewRule, Store, CATEGORIES};
use sl_core::{SlError, SlResult};

use crate::output::{print_rule_row, yn};
use crate::parse::Parsed;

pub(crate) fn cmd_rules(store: &Store, p: &Parsed) -> SlResult<()> {
    match p.pos(0).unwrap_or("list") {
        "list" => {
            let category = p.value("category");
            let search = p.value("search");
            let enabled_only = p.has("enabled");
            let rules = store.query_rules(category, enabled_only, search)?;
            if p.json() {
                println!("{}", serde_json::to_string_pretty(&rules)?);
                return Ok(());
            }
            if rules.is_empty() {
                println!("{}", t("cli.rules.list_empty"));
                return Ok(());
            }
            let cat = category.unwrap_or_else(|| t("cli.rules.category_all"));
            println!(
                "{}",
                tf("cli.rules.list_count", &[("count", &rules.len().to_string()), ("category", cat)])
            );
            println!();
            for (i, r) in rules.iter().enumerate() {
                print_rule_row(r);
                if i + 1 < rules.len() {
                    println!();
                }
            }
            Ok(())
        }
        "add" => {
            let name = p.required(1, "<名称>")?;
            let source = p.required(2, "<源路径模板，如 %LOCALAPPDATA%\\X\\Cache>")?;
            let subpath = p.required(3, "<目标子目录，如 browsers\\chrome>")?;

            let root_id = match p.value("root") {
                Some(v) => v
                    .parse::<i64>()
                    .map_err(|_| SlError::Other("--root 必须是整数 id".into()))?,
                None => {
                    let roots = store.list_roots()?;
                    match roots.len() {
                        0 => {
                            return Err(SlError::Other(
                                "还没有配置目标根，先执行：roots add RAMDisk R:".into(),
                            ))
                        }
                        1 => roots[0].id,
                        _ => {
                            let list = roots
                                .iter()
                                .map(|r| format!("#{} {}", r.id, r.label))
                                .collect::<Vec<_>>()
                                .join("、");
                            return Err(SlError::Other(format!(
                                "有多个目标根，请用 --root 指定：{list}"
                            )));
                        }
                    }
                }
            };

            let category = p.value("category").unwrap_or("other").to_string();
            if !CATEGORIES.contains(&category.as_str()) {
                return Err(SlError::Other(format!(
                    "未知分类 {category}，可用：{}",
                    CATEGORIES.join(" / ")
                )));
            }
            let link_type = p.value("link-type").unwrap_or("junction").to_string();
            if sl_core::link::LinkType::parse(&link_type).is_none() {
                return Err(SlError::Other(
                    "--link-type 只能是 junction 或 symlink_dir".into(),
                ));
            }

            // 重定向方式：link（建 Junction）/ setting（直接改系统设置）。
            let redirect_mode = p
                .value("redirect-mode")
                .unwrap_or("link")
                .trim()
                .to_string();
            if redirect_mode != sl_core::store::REDIRECT_MODE_LINK
                && redirect_mode != sl_core::store::REDIRECT_MODE_SETTING
            {
                return Err(SlError::Other(
                    "--redirect-mode 只能是 link 或 setting".into(),
                ));
            }

            // 系统设置模式：要改哪些环境变量（TEMP/TMP）、
            // 写到哪一层（HKCU 用户级 / HKLM 机器级）。
            let env_vars: Vec<String> = if redirect_mode == sl_core::store::REDIRECT_MODE_SETTING {
                let raw = p.value("env-vars").unwrap_or("TEMP,TMP");
                let names: Vec<String> = raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                // 白名单校验：变量名必须是明确支持的。
                sl_core::sysenv::normalize_vars(&names)?;
                names
            } else {
                Vec::new()
            };
            let env_scope = p.value("env-scope").unwrap_or("user").trim().to_string();
            if redirect_mode == sl_core::store::REDIRECT_MODE_SETTING
                && sl_core::sysenv::EnvScope::parse(&env_scope).is_none()
            {
                return Err(SlError::Other(
                    "--env-scope 只能是 user（HKCU，免提权）或 machine（HKLM，需管理员）".into(),
                ));
            }

            // CLI 直连 store，不经过 usecase 的表单校验，这里复用同一套
            // 排除模式校验，防止垃圾模式静默入库（清理时被容错为空而失去保护）。
            let excludes: Vec<String> = p.values("exclude").to_vec();
            sl_core::fsops::exclude::Excludes::new(&excludes)?;

            // 水位预授权策略同理：格式在入口就拦（空 = 未启用）。
            let auto_clean_policy = p.value("auto-clean").unwrap_or("").trim().to_string();
            sl_core::usecase::water::validate_auto_policy(&auto_clean_policy)?;

            // 文件夹级容量上限（MB，0/缺省 = 不启用）。配了上限就必须配策略：
            // 策略决定删什么，上限决定何时删。
            let auto_clean_limit_bytes: i64 = match p.value("auto-clean-limit") {
                Some(v) => {
                    let mb: i64 = v.trim().parse().map_err(|_| {
                        SlError::Other("--auto-clean-limit 必须是非负整数（单位 MB）".into())
                    })?;
                    if mb < 0 {
                        return Err(SlError::Other(
                            "--auto-clean-limit 不能为负数（单位 MB）".into(),
                        ));
                    }
                    mb.saturating_mul(1024 * 1024)
                }
                None => 0,
            };
            if auto_clean_limit_bytes > 0 && auto_clean_policy.is_empty() {
                return Err(SlError::Other(
                    "--auto-clean-limit 需要同时给 --auto-clean（策略决定删什么，上限决定何时删）".into(),
                ));
            }

            let nr = NewRule {
                name,
                category,
                source_template: source,
                target_root_id: root_id,
                target_subpath: subpath,
                link_type,
                redirect_mode,
                env_vars,
                env_scope,
                want_enabled: p.has("enable"),
                rebuild_on_boot: !p.has("no-boot"),
                notes: p.value("notes").map(|s| s.to_string()),
                excludes,
                auto_clean_policy,
                auto_clean_limit_bytes,
            };
            let id = store.insert_rule(&nr)?;
            let rule = store.get_rule(id)?;
            println!("{}", tf("cli.rules.added", &[("id", &id.to_string())]));
            print_rule_row(&rule);
            if rule.needs_elevation {
                println!("{}", t("cli.rules.elev_hint"));
            }
            Ok(())
        }
        "rm" | "remove" | "delete" => {
            let ids = p.ids(1)?;
            // 删掉的规则进回收站，可用 `rules undo` 搬回来。
            let n = sl_core::usecase::rules::delete_rules(store, &ids)?;
            for id in &ids {
                println!("{}", tf("cli.rules.deleted", &[("id", &id.to_string())]));
            }
            if n > 0 {
                println!("{}", tf("cli.rules.deleted_hint", &[("n", &n.to_string())]));
            }
            Ok(())
        }
        "undo" => {
            // 撤销最近一次操作：删除（把规则搬回规则库）或修改（把字段改回去）。
            let out = sl_core::usecase::rules::undo_last(store)?;
            if out.restored.is_empty() && out.skipped.is_empty() {
                println!("{}", t("cli.rules.undo_empty"));
                return Ok(());
            }
            let is_update = out.op.as_deref() == Some("update");
            for name in &out.restored {
                if is_update {
                    println!("{}", tf("cli.rules.undone_edit", &[("name", name)]));
                } else {
                    println!("{}", tf("cli.rules.undone", &[("name", name)]));
                }
            }
            for s in &out.skipped {
                println!("{}", s);
            }
            Ok(())
        }
        "enable" | "disable" => {
            let on = p.pos(0) == Some("enable");
            let ids = if p.has("all") {
                store.list_rules()?.into_iter().map(|r| r.id).collect()
            } else {
                p.ids(1)?
            };
            if ids.is_empty() {
                return Err(SlError::Other("请指定规则 id 或 --all".into()));
            }
            // 走用例层：整批开关是一次操作，撤销能整批改回去。
            sl_core::usecase::rules::set_enabled(store, &ids, on)?;
            for id in ids {
                println!(
                    "{}",
                    tf("cli.rules.toggle", &[("id", &id.to_string()), ("on", yn(on))])
                );
            }
            Ok(())
        }
        other => Err(SlError::Other(format!(
            "未知的 rules 子命令：{other}（可用：list / add / rm / undo / enable / disable）"
        ))),
    }
}