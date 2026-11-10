//! `roots` 子命令：目标根（RAM 盘）的增删查。

use sl_core::boot::probe_volume;
use sl_core::i18n::{t, tf};
use sl_core::store::Store;
use sl_core::{SlError, SlResult};

use crate::parse::Parsed;

pub(crate) fn cmd_roots(store: &Store, p: &Parsed) -> SlResult<()> {
    match p.pos(0).unwrap_or("list") {
        "list" => {
            let roots = store.list_roots()?;
            if roots.is_empty() {
                println!("{}", t("cli.roots.list_empty"));
            }
            for r in roots {
                let (path, unknown) = r.resolved_path();
                let (state, msg) = probe_volume(&path);
                let id_col = format!("{:<3}", r.id);
                let label_col = format!("{:<20}", r.label);
                let path_col = format!("{:<20}", r.path);
                let timeout_s = r.wait_timeout_ms.to_string();
                let disk = if r.is_ramdisk { "RAM" } else { t("cli.roots.disk_normal") };
                println!(
                    "{}",
                    tf(
                        "cli.roots.list_row",
                        &[
                            ("id", id_col.as_str()),
                            ("label", label_col.as_str()),
                            ("path", path_col.as_str()),
                            ("timeout", timeout_s.as_str()),
                            ("disk", disk),
                            ("state", state),
                            ("msg", msg.as_str()),
                        ],
                    ),
                );
                if !unknown.is_empty() {
                    println!(
                        "{}",
                        tf("cli.roots.unresolved_vars", &[("vars", &unknown.join(t("cli.roots.var_sep")))])
                    );
                }
            }
            Ok(())
        }
        "add" => {
            let label = p.required(1, "<label>")?;
            let path = p.required(2, "<path>")?;
            let is_ram = !p.has("hdd");
            let timeout = p
                .value("timeout")
                .map(|v| v.parse::<i64>())
                .transpose()
                .map_err(|_| SlError::Other("--timeout 必须是整数毫秒".into()))?
                .unwrap_or(90000);
            let id = store.insert_root(&label, &path, is_ram, timeout)?;
            println!(
                "{}",
                tf("cli.roots.added", &[("id", &id.to_string()), ("label", label.as_str()), ("path", path.as_str())])
            );
            Ok(())
        }
        "rm" | "remove" | "delete" => {
            let id = p.required(1, "<id>")?.parse::<i64>().map_err(|_| {
                SlError::Other("目标根 id 必须是整数".into())
            })?;
            store.delete_root(id)?;
            println!("{}", tf("cli.roots.deleted", &[("id", &id.to_string())]));
            Ok(())
        }
        other => Err(SlError::Other(format!(
            "未知的 roots 子命令：{other}（可用：list / add / rm）"
        ))),
    }
}