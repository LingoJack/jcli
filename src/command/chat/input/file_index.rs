//! 项目文件索引：维护内存中的文件路径列表，支持后台监控自动更新。
//!
//! 解决 @ 弹窗和文件弹窗每帧渲染时触发 WalkBuilder 扫描导致的性能问题。
//!
//! 性能设计：
//! - 缓存以 `Arc<Vec<FileEntry>>` 快照持有，读方 clone Arc 为 O(1)，无深拷贝
//! - FileEntry 预存小写副本（全路径 + 文件名），模糊匹配时零 to_lowercase 分配
//! - 查询缓存：同一索引版本 + 同一过滤条件的重复查询（弹窗每帧渲染、上下键导航）直接复用结果

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::util::log::{write_error_log, write_info_log};
use ignore::WalkBuilder;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// 文件索引刷新间隔（秒）
const REFRESH_CHECK_INTERVAL_SECS: u64 = 1;

/// 文件名前缀匹配的评分加成（开头匹配优于包含匹配）
const NAME_PREFIX_SCORE_BONUS: i32 = -50;

/// 文件名包含匹配的评分加成
const NAME_CONTAINS_SCORE_BONUS: i32 = -20;

/// 代码类扩展名的评分加成
const CODE_EXT_SCORE_BONUS: i32 = -15;

/// 文档/配置类扩展名的评分加成
const DOC_EXT_SCORE_BONUS: i32 = -10;

/// 评分加成的代码类扩展名
const SCORE_BOOSTED_CODE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "swift",
];

/// 评分加成的文档/配置类扩展名
const SCORE_BOOSTED_DOC_EXTS: &[&str] = &["json", "yaml", "yml", "toml", "md"];

/// watcher 忽略的构建产物目录段（变化频繁，无需触发索引刷新）
const BUILD_DIR_SEGMENTS: &[&str] = &["target", "node_modules", "dist", "build"];

/// 索引条目：显示路径 + 预生成的小写副本（匹配与排序时零分配）
struct FileEntry {
    /// 显示用相对路径（目录以 '/' 结尾）
    display: String,
    /// display 的小写副本（排序与路径匹配用）
    lower: String,
    /// 文件名部分的小写副本（模糊匹配用）
    name_lower: String,
}

/// 查询缓存：索引版本 + 过滤条件 + 结果
struct SearchCache {
    version: u64,
    filter: String,
    max_results: usize,
    results: Vec<String>,
}

/// 项目文件索引：维护一个内存中的文件路径列表
pub struct FileIndex {
    /// 缓存的文件条目快照（读方 clone Arc 为 O(1)，写方整体替换）
    entries: Arc<Mutex<Arc<Vec<FileEntry>>>>,
    /// 缓存版本号（每次快照替换后 +1，用于查询缓存失效）
    version: Arc<AtomicU64>,
    /// 最近一次查询的缓存
    search_cache: Mutex<Option<SearchCache>>,
    /// 上次完整扫描时间
    #[allow(dead_code)]
    last_scan: Arc<Mutex<Instant>>,
    /// 后台文件监控线程的停止标记
    watch_stop: Arc<AtomicBool>,
    /// 缓存是否就绪（首次扫描完成）
    ready: Arc<AtomicBool>,
    /// 是否需要刷新（文件变化事件触发）
    needs_refresh: Arc<AtomicBool>,
    /// 文件监控器（非 blocking，生命周期与 FileIndex 绑定）
    watcher: Option<RecommendedWatcher>,
}

impl FileIndex {
    /// 创建并启动后台扫描 + 文件监控
    ///
    /// 首次扫描在后台线程执行，不阻塞主线程。
    /// 文件监控使用 notify crate 监听项目目录变化。
    pub fn new() -> Self {
        let entries: Arc<Mutex<Arc<Vec<FileEntry>>>> =
            Arc::new(Mutex::new(Arc::new(Vec::new())));
        let version = Arc::new(AtomicU64::new(0));
        let last_scan: Arc<Mutex<Instant>> = Arc::new(Mutex::new(Instant::now()));
        let watch_stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let needs_refresh = Arc::new(AtomicBool::new(true)); // 启动时触发首次扫描

        // 启动文件监控
        let needs_refresh_clone = Arc::clone(&needs_refresh);
        let watch_stop_clone = Arc::clone(&watch_stop);
        let watcher = Self::start_watcher(needs_refresh_clone, watch_stop_clone);

        // 启动后台扫描 + 刷新检查线程
        let entries_clone = Arc::clone(&entries);
        let version_clone = Arc::clone(&version);
        let last_scan_clone = Arc::clone(&last_scan);
        let ready_clone = Arc::clone(&ready);
        let needs_refresh_clone = Arc::clone(&needs_refresh);
        let watch_stop_clone = Arc::clone(&watch_stop);

        thread::spawn(move || {
            Self::background_scan_loop(
                entries_clone,
                version_clone,
                last_scan_clone,
                ready_clone,
                needs_refresh_clone,
                watch_stop_clone,
            );
        });

        Self {
            entries,
            version,
            search_cache: Mutex::new(None),
            last_scan,
            watch_stop,
            ready,
            needs_refresh,
            watcher,
        }
    }

    /// 启动文件监控器
    fn start_watcher(
        needs_refresh: Arc<AtomicBool>,
        watch_stop: Arc<AtomicBool>,
    ) -> Option<RecommendedWatcher> {
        // 创建事件通道
        let (tx, rx) = std::sync::mpsc::channel::<Result<Event, notify::Error>>();

        // 创建监控器
        let mut watcher: RecommendedWatcher = match Watcher::new(tx, notify::Config::default()) {
            Ok(w) => w,
            Err(e) => {
                write_error_log(
                    "[FileIndex::start_watcher]",
                    &format!("创建 watcher 失败: {}", e),
                );
                return None;
            }
        };

        // 监控当前工作目录
        let cwd = Path::new(".");
        if let Err(e) = watcher.watch(cwd, RecursiveMode::Recursive) {
            write_error_log(
                "[FileIndex::start_watcher]",
                &format!("启动监控失败: {}", e),
            );
            return None;
        }

        write_info_log("[FileIndex]", "文件监控已启动");

        // 启动事件处理线程
        thread::spawn(move || {
            while !watch_stop.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(event)) => {
                        // 只关注文件创建/删除/修改事件
                        match event.kind {
                            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => {
                                let should_refresh = event
                                    .paths
                                    .iter()
                                    .any(|p| !is_watch_ignored_path(p));
                                if should_refresh {
                                    needs_refresh.store(true, Ordering::Relaxed);
                                    write_info_log(
                                        "[FileIndex::watcher]",
                                        "检测到文件变化，标记需要刷新",
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(Err(e)) => {
                        write_error_log("[FileIndex::watcher]", &format!("监控事件错误: {}", e));
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // 超时继续等待
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        // 通道关闭，退出
                        break;
                    }
                }
            }
            write_info_log("[FileIndex::watcher]", "监控线程已退出");
        });

        Some(watcher)
    }

    /// 后台扫描循环：定期检查 needs_refresh 标记，触发重新扫描
    fn background_scan_loop(
        entries: Arc<Mutex<Arc<Vec<FileEntry>>>>,
        version: Arc<AtomicU64>,
        last_scan: Arc<Mutex<Instant>>,
        ready: Arc<AtomicBool>,
        needs_refresh: Arc<AtomicBool>,
        watch_stop: Arc<AtomicBool>,
    ) {
        write_info_log("[FileIndex::scan_loop]", "后台扫描线程已启动");

        while !watch_stop.load(Ordering::Relaxed) {
            // 检查是否需要刷新
            if needs_refresh.load(Ordering::Relaxed) {
                needs_refresh.store(false, Ordering::Relaxed);

                write_info_log("[FileIndex::scan_loop]", "开始扫描项目目录...");
                let start = Instant::now();

                // 执行扫描
                let new_files = Self::scan_files_internal();

                // 更新缓存快照并递增版本（先替换快照再 bump，保证查询缓存与新版本对应）
                let count = new_files.len();
                if let Ok(mut guard) = entries.lock() {
                    *guard = Arc::new(new_files);
                }
                version.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut guard) = last_scan.lock() {
                    *guard = Instant::now();
                }
                ready.store(true, Ordering::Relaxed);

                let elapsed = start.elapsed();
                write_info_log(
                    "[FileIndex::scan_loop]",
                    &format!(
                        "扫描完成，共 {} 个文件，耗时 {}ms",
                        count,
                        elapsed.as_millis()
                    ),
                );
            }

            // 定期检查
            thread::sleep(Duration::from_secs(REFRESH_CHECK_INTERVAL_SECS));
        }

        write_info_log("[FileIndex::scan_loop]", "后台扫描线程已退出");
    }

    /// 获取当前条目快照（clone Arc 为 O(1)，匹配在无锁快照上进行）
    fn snapshot(&self) -> Arc<Vec<FileEntry>> {
        self.entries
            .lock()
            .map(|guard| Arc::clone(&guard))
            .unwrap_or_else(|_| Arc::new(Vec::new()))
    }

    /// 执行目录扫描的核心逻辑（复用现有 WalkBuilder 配置）
    fn scan_files_internal() -> Vec<FileEntry> {
        let search_root = Path::new(".");
        let walker = WalkBuilder::new(search_root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .max_depth(Some(8))
            .build();

        let mut files: Vec<FileEntry> = Vec::new();
        for entry in walker.flatten() {
            let path = entry.path();
            // 跳过根目录本身
            if path == search_root {
                continue;
            }
            let rel = path
                .strip_prefix(search_root)
                .unwrap_or(path)
                .to_string_lossy();
            let rel_str = rel.as_ref();

            // 跳过隐藏路径段（除非以 . 开头的合法项目配置）
            if rel_str
                .split('/')
                .any(|seg| seg.starts_with('.') && seg != ".jcli")
            {
                continue;
            }

            // 跳过 .jcli/worktrees 目录（避免扫描大量 worktree 文件）
            if rel_str.starts_with(".jcli/worktrees/") {
                continue;
            }

            // 遍历时已携带 file_type，避免对每个条目再 stat 一次
            let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());

            let display = if is_dir {
                format!("{}/", rel_str)
            } else {
                rel_str.to_string()
            };

            let lower = display.to_lowercase();
            let name_lower = file_name_of(&display).to_lowercase();
            files.push(FileEntry {
                display,
                lower,
                name_lower,
            });
        }

        // 排序：目录优先，然后按名称排序
        files.sort_by(|a, b| {
            let a_dir = a.display.ends_with('/');
            let b_dir = b.display.ends_with('/');
            match (a_dir, b_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.lower.cmp(&b.lower),
            }
        });

        files
    }

    /// 获取所有缓存的文件路径（用于渲染，只读）
    pub fn files(&self) -> Vec<String> {
        self.snapshot().iter().map(|e| e.display.clone()).collect()
    }

    /// 缓存是否就绪
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// 按前缀过滤文件（目录导航模式）
    ///
    /// 当 filter 包含 `/` 时，提取目录部分和文件前缀，
    /// 从缓存中匹配该目录下的文件/子目录。
    #[allow(dead_code)]
    pub fn filter_by_prefix(&self, filter: &str) -> Vec<String> {
        let snapshot = self.snapshot();

        // 处理 ~ 路径展开（仅用于显示，不实际展开）
        let effective_filter = if filter.starts_with("~/") {
            // ~/ 路径无法从项目索引中匹配，返回空
            return Vec::new();
        } else {
            filter
        };

        // 如果 filter 包含 /，提取目录部分和前缀
        if let Some(last_slash) = effective_filter.rfind('/') {
            let dir_part = &effective_filter[..=last_slash];
            let prefix = &effective_filter[last_slash + 1..];
            let prefix_lower = prefix.to_lowercase();

            let mut entries: Vec<String> = Vec::new();
            for entry in snapshot.iter() {
                // 检查是否以 dir_part 开头
                if !entry.display.starts_with(dir_part) {
                    continue;
                }
                // 提取该目录下的名称
                let after_dir = &entry.display[dir_part.len()..];
                // 检查是否有更深层的路径（不匹配）
                if after_dir.contains('/') && !after_dir.ends_with('/') {
                    continue;
                }
                // 检查前缀匹配
                let name = if let Some(stripped) = after_dir.strip_suffix('/') {
                    stripped
                } else {
                    after_dir
                };
                if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                    continue;
                }
                entries.push(entry.display.clone());
            }
            entries.truncate(15);
            return entries;
        }

        // 无 / 时，直接前缀匹配文件名
        let filter_lower = effective_filter.to_lowercase();
        let mut entries: Vec<String> = Vec::new();
        for entry in snapshot.iter() {
            let name = file_name_of(&entry.display);
            if name.to_lowercase().starts_with(&filter_lower) {
                entries.push(entry.display.clone());
            }
        }
        entries.truncate(15);
        entries
    }

    /// 模糊搜索文件（内存快照匹配 + 查询缓存）
    ///
    /// 使用增强版模糊匹配：文件名匹配 + 评分排序。
    /// max_results 限制返回数量。
    pub fn fuzzy_search(&self, filter: &str, max_results: usize) -> Vec<String> {
        let version = self.version.load(Ordering::Relaxed);

        // 同一索引版本 + 同一过滤条件的重复查询直接复用
        //（弹窗每帧渲染、上下键导航都会以相同参数重查）
        if let Ok(guard) = self.search_cache.lock() {
            if let Some(hit) = guard.as_ref()
                && hit.version == version
                && hit.filter == filter
                && hit.max_results == max_results
            {
                return hit.results.clone();
            }
        }

        let snapshot = self.snapshot();

        let results: Vec<String> = if filter.is_empty() {
            // 返回前 max_results 个文件（已排序）
            snapshot
                .iter()
                .take(max_results)
                .map(|e| e.display.clone())
                .collect()
        } else {
            let filter_lower = filter.to_lowercase();

            let mut scored: Vec<(i32, usize)> = Vec::new();
            for (idx, entry) in snapshot.iter().enumerate() {
                let file_name = file_name_of(&entry.display);

                // 模糊匹配
                if let Some(score) = Self::fuzzy_match_enhanced(file_name, &filter_lower, entry) {
                    scored.push((score, idx));
                }
            }

            // 按分数排序
            scored.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| snapshot[a.1].lower.cmp(&snapshot[b.1].lower))
            });

            scored
                .into_iter()
                .take(max_results)
                .map(|(_, idx)| snapshot[idx].display.clone())
                .collect()
        };

        if let Ok(mut guard) = self.search_cache.lock() {
            *guard = Some(SearchCache {
                version,
                filter: filter.to_string(),
                max_results,
                results: results.clone(),
            });
        }

        results
    }

    /// 增强版模糊匹配：返回匹配分数（越小越好）
    fn fuzzy_match_enhanced(file_name: &str, filter_lower: &str, entry: &FileEntry) -> Option<i32> {
        let base_score = Self::fuzzy_match(&entry.name_lower, filter_lower)?;

        // 匹配位置加成：开头匹配更优
        let position_bonus = if entry.name_lower.starts_with(filter_lower) {
            NAME_PREFIX_SCORE_BONUS
        } else if entry.name_lower.contains(filter_lower) {
            NAME_CONTAINS_SCORE_BONUS
        } else {
            0
        };

        // 扩展名优先级：代码文件更优
        let ext_bonus = if let Some(ext) = Path::new(file_name).extension() {
            let ext_str = ext.to_string_lossy().to_lowercase();
            if SCORE_BOOSTED_CODE_EXTS.contains(&ext_str.as_str()) {
                CODE_EXT_SCORE_BONUS
            } else if SCORE_BOOSTED_DOC_EXTS.contains(&ext_str.as_str()) {
                DOC_EXT_SCORE_BONUS
            } else {
                0
            }
        } else {
            0
        };

        // 路径深度惩罚
        let depth = entry.display.matches('/').count() as i32;

        Some(base_score * 10 + depth + position_bonus + ext_bonus)
    }

    /// 模糊匹配：filter 的每个字符按顺序出现在 text 中即可匹配
    ///
    /// 入参均应为小写，逐字符流式比较，不产生中间分配
    fn fuzzy_match(text_lower: &str, filter_lower: &str) -> Option<i32> {
        if filter_lower.is_empty() {
            return Some(0);
        }

        let mut filter_chars = filter_lower.chars();
        let Some(mut fc) = filter_chars.next() else {
            return Some(0);
        };

        let mut score: i32 = 0;
        let mut last_match: Option<usize> = None;
        for (ti, tc) in text_lower.chars().enumerate() {
            if tc == fc {
                // 连续匹配加分（间距小更好）
                if let Some(lm) = last_match {
                    score += (ti - lm - 1) as i32;
                }
                last_match = Some(ti);

                match filter_chars.next() {
                    Some(next) => fc = next,
                    None => return Some(score),
                }
            }
        }
        None
    }

    /// 主动触发一次重新扫描（弹窗打开时调用）
    pub fn refresh(&self) {
        self.needs_refresh.store(true, Ordering::Relaxed);
    }

    /// 停止后台监控（ChatApp drop 时调用）
    pub fn shutdown(&self) {
        self.watch_stop.store(true, Ordering::Relaxed);
        write_info_log("[FileIndex]", "已标记停止，后台线程将退出");
    }
}

/// 提取显示路径的文件名部分（目录条目先去掉尾部 '/'）
fn file_name_of(display: &str) -> &str {
    let name = display.strip_suffix('/').unwrap_or(display);

    match name.rfind('/') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// 判断 watcher 事件路径是否应被忽略（.git、.jcli/worktrees、构建产物目录）
fn is_watch_ignored_path(p: &Path) -> bool {
    let path_str = p.to_string_lossy();

    if path_str.starts_with(".git/")
        || path_str.starts_with(".jcli/worktrees/")
        || path_str.contains("/.git/")
    {
        return true;
    }

    // 构建产物目录：按路径段匹配（事件路径可能带 "./" 前缀）
    p.iter().any(|seg| {
        let seg = seg.to_string_lossy();
        BUILD_DIR_SEGMENTS.contains(&seg.as_ref())
    })
}

impl Default for FileIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FileIndex {
    fn drop(&mut self) {
        self.shutdown();
        // 停止 watcher
        if let Some(ref mut watcher) = self.watcher {
            let _ = watcher.unwatch(Path::new("."));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FileIndex, file_name_of};

    #[test]
    fn fuzzy_match_basic() {
        assert_eq!(FileIndex::fuzzy_match("main.rs", "mai"), Some(0));
        assert_eq!(FileIndex::fuzzy_match("main.rs", "mrs"), Some(4)); // m..r..s, 间隔 4
        assert_eq!(FileIndex::fuzzy_match("main.rs", "xyz"), None);
    }

    #[test]
    fn fuzzy_match_empty_filter() {
        assert_eq!(FileIndex::fuzzy_match("anything", ""), Some(0));
    }

    #[test]
    fn file_name_of_basic() {
        assert_eq!(file_name_of("src/main.rs"), "main.rs");
        assert_eq!(file_name_of("src/lib/"), "lib");
        assert_eq!(file_name_of("README.md"), "README.md");
    }
}
