use crate::command::chat::app::ChatApp;
use crate::command::chat::input::autocomplete::{
    AtPopupItem, complete_at_direct, complete_command_mention, complete_file_mention,
    complete_skill_mention, get_filtered_all_items, get_filtered_command_names, get_filtered_files,
    get_filtered_skill_names, get_filtered_slash_commands, update_at_filter, update_command_filter,
    update_file_filter, update_skill_filter,
};
use crossterm::event::{KeyCode, KeyEvent};

/// 弹窗拦截结果
pub(super) enum PopupResult {
    /// 弹窗已消费按键，主循环应直接返回 false
    Handled,
    /// 弹窗未消费此按键（通常已被关闭），主循环继续按正常逻辑处理
    PassThrough,
}

/// 上下键循环选择
fn move_up(selected: &mut usize, len: usize) {
    if len == 0 {
        return;
    }
    if *selected > 0 {
        *selected -= 1;
    } else {
        *selected = len - 1;
    }
}

fn move_down(selected: &mut usize, len: usize) {
    if len == 0 {
        return;
    }
    if *selected < len - 1 {
        *selected += 1;
    } else {
        *selected = 0;
    }
}

/// 处理 / 斜杠命令弹窗
pub(super) fn handle_slash_popup(app: &mut ChatApp, key: KeyEvent) -> PopupResult {
    let filtered = get_filtered_slash_commands(&app.ui.slash_popup_filter);
    match key.code {
        KeyCode::Up => {
            move_up(&mut app.ui.slash_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Down => {
            move_down(&mut app.ui.slash_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Tab | KeyCode::Enter => {
            if !filtered.is_empty() {
                let sel = app.ui.slash_popup_selected.min(filtered.len() - 1);
                let cmd = filtered[sel].clone();
                super::execute_slash_command(app, &cmd);
            }
            app.ui.slash_popup_active = false;
            PopupResult::Handled
        }
        KeyCode::Esc => {
            app.ui.slash_popup_active = false;
            PopupResult::Handled
        }
        KeyCode::Backspace => {
            if !app.ui.slash_popup_filter.is_empty() {
                app.ui.slash_popup_filter.pop();
                app.ui.set_input_text(
                    &format!("/{}", app.ui.slash_popup_filter),
                    app.ui.slash_popup_filter.len() + 1,
                );
                app.ui.slash_popup_selected = 0;
            } else {
                // filter 为空时关闭弹窗并删除 /
                app.ui.slash_popup_active = false;
                app.ui.clear_input();
            }
            PopupResult::Handled
        }
        KeyCode::Char(c) => {
            // 空格关闭斜杠弹窗，让后续输入正常处理
            if c == ' ' {
                app.ui.slash_popup_active = false;
                PopupResult::PassThrough
            } else {
                app.ui.slash_popup_filter.push(c);
                app.ui.set_input_text(
                    &format!("/{}", app.ui.slash_popup_filter),
                    app.ui.slash_popup_filter.len() + 1,
                );
                app.ui.slash_popup_selected = 0;
                PopupResult::Handled
            }
        }
        _ => PopupResult::PassThrough,
    }
}

/// 处理 @ 补全弹窗
///
/// **性能关键**：`get_filtered_all_items()` 基于内存文件索引做模糊匹配，
/// 大项目上单次仍可达数毫秒到数十毫秒。必须延迟求值——只在 Up/Down/Tab/Enter
/// 等真正需要过滤列表的分支内调用，绝不能提前到 match 之前。
/// 否则每次按键（包括最终走 PassThrough 的普通字符）都会阻塞主循环，
/// 导致输入卡顿/吞字符。
pub(super) fn handle_at_popup(app: &mut ChatApp, key: KeyEvent) -> PopupResult {
    let filtered;
    match key.code {
        KeyCode::Up => {
            filtered = get_filtered_all_items(app);
            move_up(&mut app.ui.at_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Down => {
            filtered = get_filtered_all_items(app);
            move_down(&mut app.ui.at_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Tab | KeyCode::Enter => {
            filtered = get_filtered_all_items(app);
            if !filtered.is_empty() {
                let sel = app.ui.at_popup_selected.min(filtered.len() - 1);
                let item = filtered[sel].clone();
                handle_at_select(app, &item);
            } else {
                app.ui.at_popup_active = false;
            }
            PopupResult::Handled
        }
        KeyCode::Esc => {
            app.ui.at_popup_active = false;
            PopupResult::Handled
        }
        KeyCode::Char(' ') => {
            // 空格关闭弹窗，正常处理字符
            app.ui.at_popup_active = false;
            PopupResult::PassThrough
        }
        KeyCode::Backspace => {
            app.ui.input_buffer.backspace();
            let cursor_pos = app.ui.cursor_char_idx();
            if cursor_pos <= app.ui.at_popup_start_pos {
                app.ui.at_popup_active = false;
            } else {
                update_at_filter(app);
            }
            PopupResult::Handled
        }
        _ => PopupResult::PassThrough,
    }
}

/// 处理在 @ 弹窗中选中项的逻辑
fn handle_at_select(app: &mut ChatApp, item: &AtPopupItem) {
    match item {
        AtPopupItem::Category(name) if name == "skill:" => {
            replace_at_with_prefix(app, "@skill:");
            app.ui.at_popup_active = false;
            app.ui.skill_popup_active = true;
            app.ui.skill_popup_start_pos = app.ui.at_popup_start_pos;
            app.ui.skill_popup_filter.clear();
            app.ui.skill_popup_selected = 0;
        }
        AtPopupItem::Category(name) if name == "command:" => {
            replace_at_with_prefix(app, "@command:");
            app.ui.at_popup_active = false;
            app.ui.command_popup_active = true;
            app.ui.command_popup_start_pos = app.ui.at_popup_start_pos;
            app.ui.command_popup_filter.clear();
            app.ui.command_popup_selected = 0;
        }
        AtPopupItem::Category(name) if name == "file:" => {
            replace_at_with_prefix(app, "@file:");
            app.ui.at_popup_active = false;
            app.ui.file_popup_active = true;
            app.ui.file_popup_start_pos = app.ui.at_popup_start_pos;
            app.ui.file_popup_filter.clear();
            app.ui.file_popup_selected = 0;
            // file: 弹窗激活时刷新文件索引
            app.file_index.refresh();
        }
        AtPopupItem::Skill(_) | AtPopupItem::Command(_) | AtPopupItem::File(_) => {
            complete_at_direct(app, item);
            app.ui.at_popup_active = false;
        }
        _ => {
            app.ui.at_popup_active = false;
        }
    }
}

/// 将 @ 起始位置后到光标位置的内容替换为指定前缀（如 "@skill:"）
fn replace_at_with_prefix(app: &mut ChatApp, replacement: &str) {
    let text = app.ui.input_text();
    let chars: Vec<char> = text.chars().collect();
    let cursor_pos = app.ui.cursor_char_idx();
    let before: String = chars[..app.ui.at_popup_start_pos].iter().collect();
    let after: String = if cursor_pos < chars.len() {
        chars[cursor_pos..].iter().collect()
    } else {
        String::new()
    };
    let new_cursor = before.chars().count() + replacement.chars().count();
    app.ui
        .set_input_text(&format!("{}{}{}", before, replacement, after), new_cursor);
}

/// 处理文件补全弹窗
///
/// **性能关键**：同 handle_at_popup，`get_filtered_files()` 基于内存文件索引匹配，
/// 必须延迟求值，不能提前到 match 之前调用。
pub(super) fn handle_file_popup(app: &mut ChatApp, key: KeyEvent) -> PopupResult {
    let filtered;
    match key.code {
        KeyCode::Up => {
            filtered = get_filtered_files(app);
            move_up(&mut app.ui.file_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Down => {
            filtered = get_filtered_files(app);
            move_down(&mut app.ui.file_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Tab | KeyCode::Enter => {
            filtered = get_filtered_files(app);
            if !filtered.is_empty() {
                let sel = app.ui.file_popup_selected.min(filtered.len() - 1);
                let entry = filtered[sel].clone();
                if entry.ends_with('/') {
                    // 目录：直接用 entry 作为新 filter（已包含完整路径）
                    app.ui.file_popup_filter = entry;
                    let text = app.ui.input_text();
                    let chars: Vec<char> = text.chars().collect();
                    let cursor_pos = app.ui.cursor_char_idx();
                    let before: String = chars[..app.ui.file_popup_start_pos].iter().collect();
                    let after: String = if cursor_pos < chars.len() {
                        chars[cursor_pos..].iter().collect()
                    } else {
                        String::new()
                    };
                    let replacement = format!("@file:{}", app.ui.file_popup_filter);
                    let new_cursor = before.chars().count() + replacement.chars().count();
                    app.ui
                        .set_input_text(&format!("{}{}{}", before, replacement, after), new_cursor);
                    app.ui.file_popup_selected = 0;
                } else {
                    // 文件：entry 已包含完整相对路径，直接补全
                    complete_file_mention(app, &entry);
                    app.ui.file_popup_active = false;
                }
                PopupResult::Handled
            } else {
                // filtered 为空时，关闭弹窗，让 Enter 继续处理（发送消息）
                app.ui.file_popup_active = false;
                PopupResult::PassThrough
            }
        }
        KeyCode::Esc => {
            app.ui.file_popup_active = false;
            PopupResult::Handled
        }
        KeyCode::Backspace => {
            app.ui.input_buffer.backspace();
            let cursor_pos = app.ui.cursor_char_idx();
            // @file: 占 6 个字符
            let prefix_end = app.ui.file_popup_start_pos + 6;
            if cursor_pos < prefix_end {
                app.ui.file_popup_active = false;
            } else {
                update_file_filter(app);
            }
            PopupResult::Handled
        }
        KeyCode::Char(c) => {
            if c == ' ' {
                app.ui.file_popup_active = false;
                PopupResult::PassThrough
            } else {
                app.ui.input_buffer.insert_char(c);
                update_file_filter(app);
                PopupResult::Handled
            }
        }
        _ => PopupResult::PassThrough,
    }
}

/// 处理技能补全弹窗
///
/// **性能注意**：`get_filtered_skill_names()` 开销较小，但为保持一致性仍延迟求值。
/// 若将来 skill 列表变大，此模式同样避免不必要的计算。
pub(super) fn handle_skill_popup(app: &mut ChatApp, key: KeyEvent) -> PopupResult {
    let filtered;
    match key.code {
        KeyCode::Up => {
            filtered = get_filtered_skill_names(app);
            move_up(&mut app.ui.skill_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Down => {
            filtered = get_filtered_skill_names(app);
            move_down(&mut app.ui.skill_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Tab | KeyCode::Enter => {
            filtered = get_filtered_skill_names(app);
            if !filtered.is_empty() {
                let sel = app.ui.skill_popup_selected.min(filtered.len() - 1);
                let entry = filtered[sel].clone();
                complete_skill_mention(app, &entry);
                app.ui.skill_popup_active = false;
                PopupResult::Handled
            } else {
                app.ui.skill_popup_active = false;
                PopupResult::PassThrough
            }
        }
        KeyCode::Esc => {
            app.ui.skill_popup_active = false;
            PopupResult::Handled
        }
        KeyCode::Backspace => {
            app.ui.input_buffer.backspace();
            let cursor_pos = app.ui.cursor_char_idx();
            // @skill: 占 7 个字符
            let prefix_end = app.ui.skill_popup_start_pos + 7;
            if cursor_pos < prefix_end {
                app.ui.skill_popup_active = false;
            } else {
                update_skill_filter(app);
            }
            PopupResult::Handled
        }
        KeyCode::Char(c) => {
            if c == ' ' {
                app.ui.skill_popup_active = false;
                PopupResult::PassThrough
            } else {
                app.ui.input_buffer.insert_char(c);
                update_skill_filter(app);
                PopupResult::Handled
            }
        }
        _ => PopupResult::PassThrough,
    }
}

/// 处理命令补全弹窗
///
/// **性能注意**：同 handle_skill_popup，延迟求值保持一致性。
pub(super) fn handle_command_popup(app: &mut ChatApp, key: KeyEvent) -> PopupResult {
    let filtered;
    match key.code {
        KeyCode::Up => {
            filtered = get_filtered_command_names(app);
            move_up(&mut app.ui.command_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Down => {
            filtered = get_filtered_command_names(app);
            move_down(&mut app.ui.command_popup_selected, filtered.len());
            PopupResult::Handled
        }
        KeyCode::Tab | KeyCode::Enter => {
            filtered = get_filtered_command_names(app);
            if !filtered.is_empty() {
                let sel = app.ui.command_popup_selected.min(filtered.len() - 1);
                let entry = filtered[sel].clone();
                complete_command_mention(app, &entry);
                app.ui.command_popup_active = false;
                PopupResult::Handled
            } else {
                app.ui.command_popup_active = false;
                PopupResult::PassThrough
            }
        }
        KeyCode::Esc => {
            app.ui.command_popup_active = false;
            PopupResult::Handled
        }
        KeyCode::Backspace => {
            app.ui.input_buffer.backspace();
            let cursor_pos = app.ui.cursor_char_idx();
            // @command: 占 9 个字符
            let prefix_end = app.ui.command_popup_start_pos + 9;
            if cursor_pos < prefix_end {
                app.ui.command_popup_active = false;
            } else {
                update_command_filter(app);
            }
            PopupResult::Handled
        }
        KeyCode::Char(c) => {
            if c == ' ' {
                app.ui.command_popup_active = false;
                PopupResult::PassThrough
            } else {
                app.ui.input_buffer.insert_char(c);
                update_command_filter(app);
                PopupResult::Handled
            }
        }
        _ => PopupResult::PassThrough,
    }
}
