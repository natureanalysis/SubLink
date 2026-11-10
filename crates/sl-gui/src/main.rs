// 发行构建不弹控制台窗口；调试构建保留控制台，方便看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    sl_gui::run();
}
