//! 覆写差异渲染
//!
//! 覆写已存在文件时生成旧内容与新内容的差异文本，随结果回喂给模型，
//! 使覆写具备可核对、可恢复的账本语义。
//!
//! ## 渲染分档
//!
//! 1. 常规：unified diff 未超 [`WRITE_DIFF_INLINE_MAX_BYTES`] 时整体直出
//! 2. 账本：超出直出预算时折叠新增侧（新内容与写入参数一致，模型侧零信息
//!    损失），完整保留删除侧——旧内容在覆写落盘后仅存于此，是恢复的唯一依据
//! 3. 截断：账本仍超 [`WRITE_DIFF_LEDGER_MAX_BYTES`] 时中段截断（对齐行边界）
//!    并标注省略行数，被截断部分不可从本结果恢复

use crate::config::{WRITE_DIFF_INLINE_MAX_BYTES, WRITE_DIFF_LEDGER_MAX_BYTES};
use crate::file::edit::backend::generate_unified_diff;

/// 账本模式头部说明模板：说明折叠规则与删除侧的恢复语义
const LEDGER_PREAMBLE: &str = "覆写全量差异（旧 {old_lines} 行 → 新 {new_lines} 行）：\
新增内容与本次写入参数一致（+ 行已折叠），\
以下 - 行为被替换的旧内容，是覆写后唯一留存的副本，如需恢复请据此重写";

/// 新增侧折叠占位模板：替换账本中每段连续 + 行
const PLUS_ELIDE_MARK: &str = "+ ⋯（{count} 行新增内容与本次写入参数一致，省略）";

/// 渲染覆写差异
///
/// 入参为覆写前的旧内容全文与本次写入的新内容全文；二者必然不同
///（一致性判定在调用方完成，相同内容走无变化短路，不进入本函数）。
pub(crate) fn render_overwrite_diff(old: &str, new: &str, display_path: &str) -> String {
    let full = generate_unified_diff(old, new, display_path, display_path);
    if full.len() <= WRITE_DIFF_INLINE_MAX_BYTES {
        return full;
    }

    let preamble = LEDGER_PREAMBLE
        .replace("{old_lines}", &old.lines().count().to_string())
        .replace("{new_lines}", &new.lines().count().to_string());
    let mut rendered = format!("{preamble}\n{}", elide_plus_runs(&full));
    if rendered.len() > WRITE_DIFF_LEDGER_MAX_BYTES {
        rendered = truncate_middle(&rendered);
    }
    rendered
}

/// 折叠 unified diff 中的连续 + 行
///
/// 新增内容是模型刚刚亲手写入的（参数原文在对话中），逐行重复是纯冗余；
/// 每段连续 + 行替换为单行占位标记，其余行（hunk 头 / - 行 / 上下文行）原样保留。
/// `+++` 文件头行不属于新增内容，不参与折叠。
fn elide_plus_runs(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len());
    let mut run: Vec<&str> = Vec::new();

    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            run.push(line);
        } else {
            flush_plus_run(&mut out, &mut run);
            out.push_str(line);
            out.push('\n');
        }
    }
    flush_plus_run(&mut out, &mut run);
    out
}

/// 输出并清空一段累计的 + 行
fn flush_plus_run(out: &mut String, run: &mut Vec<&str>) {
    if run.is_empty() {
        return;
    }
    let mark = PLUS_ELIDE_MARK.replace("{count}", &run.len().to_string());
    out.push_str(&mark);
    out.push('\n');
    run.clear();
}

/// 账本中段截断（对齐行边界）
///
/// 保留头尾两部分行，中段替换为省略标注（含省略行数与恢复指引），
/// 确保模型明确知道看到的旧内容不完整。头部预算占大头——文件开头的
/// 结构性内容（模块声明、导入、配置）比尾部更常用于判断与恢复。
fn truncate_middle(rendered: &str) -> String {
    let head_budget = WRITE_DIFF_LEDGER_MAX_BYTES * 4 / 5;
    let tail_budget = WRITE_DIFF_LEDGER_MAX_BYTES / 5;

    let lines: Vec<&str> = rendered.lines().collect();

    let mut head_end = 0usize;
    let mut used = 0usize;
    while head_end < lines.len() && used + lines[head_end].len() < head_budget {
        used += lines[head_end].len() + 1;
        head_end += 1;
    }

    let mut tail_start = lines.len();
    let mut used = 0usize;
    while tail_start > head_end && used + lines[tail_start - 1].len() < tail_budget {
        used += lines[tail_start - 1].len() + 1;
        tail_start -= 1;
    }

    let omitted = tail_start - head_end;
    let marker = format!(
        "⋯（中间 {omitted} 行已截断：被截断的旧内容无法从本结果恢复，\
如需完整恢复请依赖上下文中此前的读取记录或 git 历史）⋯"
    );

    let mut out = String::with_capacity(head_budget + tail_budget + marker.len());
    for line in &lines[..head_end] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&marker);
    out.push('\n');
    for line in &lines[tail_start..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_diff_renders_inline() {
        let diff =
            render_overwrite_diff("fn a() {}\nfn b() {}\n", "fn a() {}\nfn c() {}\n", "x.rs");
        assert!(diff.contains("-fn b() {}"));
        assert!(diff.contains("+fn c() {}"));
        assert!(!diff.contains("省略"));
    }

    #[test]
    fn full_rewrite_folds_plus_side_keeps_minus_side() {
        // 两侧各 600 行 × 30 字节，完整 diff 约 37KB，超过直出预算进入账本模式
        let old: String = (0..600)
            .map(|i| format!("old-line-{i:03}-aaaaaaaaaaaaaaaaaa\n"))
            .collect();
        let new: String = (0..600)
            .map(|i| format!("new-line-{i:03}-bbbbbbbbbbbbbbbbbb\n"))
            .collect();

        let diff = render_overwrite_diff(&old, &new, "big.rs");

        // 新增侧折叠：占位标记计数正确，且不出现任何 new 行原文
        assert!(diff.contains("+ ⋯（600 行新增内容与本次写入参数一致，省略）"));
        assert!(!diff.contains("new-line-300-"));
        // 删除侧完整保留：首尾行都在
        assert!(diff.contains("-old-line-000-"));
        assert!(diff.contains("-old-line-599-"));
        // 头部说明携带两侧行数
        assert!(diff.contains("旧 600 行 → 新 600 行"));
    }

    #[test]
    fn giant_ledger_truncates_middle_with_marker() {
        // 旧内容 3000 行 × 30 字节约 90KB，账本超预算进入中段截断
        let old: String = (0..3000)
            .map(|i| format!("old-line-{i:04}-aaaaaaaaaaaaaaaaaa\n"))
            .collect();
        let new = "completely different\n".to_string();

        let diff = render_overwrite_diff(&old, &new, "giant.rs");

        assert!(diff.contains("行已截断"));
        // 头尾保留、中段移除
        assert!(diff.contains("-old-line-0000-"));
        assert!(diff.contains("-old-line-2999-"));
        assert!(!diff.contains("-old-line-1500-"));
        // 截断后总量受预算约束（预留标注文案的少量余量）
        assert!(diff.len() < WRITE_DIFF_LEDGER_MAX_BYTES + 500);
    }

    #[test]
    fn mixed_change_keeps_minus_runs_unfolded() {
        // 交替散布的局部改动（非全量改写）累计超预算时同样进入账本：
        // 每段 + 行独立折叠，所有 - 行原样保留
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..200 {
            old.push_str("shared-context-line-here\n");
            old.push_str(&format!("old-{i:03}-aaaaaaaaaaaaaaaa\n"));
            new.push_str("shared-context-line-here\n");
            new.push_str(&format!("new-{i:03}-bbbbbbbbbbbbbbbb\n"));
        }

        let diff = render_overwrite_diff(&old, &new, "mix.rs");

        assert!(diff.contains("-old-000-"));
        assert!(diff.contains("-old-199-"));
        assert!(diff.contains("+ ⋯（1 行新增内容与本次写入参数一致，省略）"));
        assert!(!diff.contains("new-100-"));
        assert!(!diff.contains("行已截断"));
    }
}
