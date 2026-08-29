//! 文本循环检测器
//!
//! 检测流式文本内容的自相似度（重复），按严重程度升级处理。
//!
//! 两级判定策略：转写 / 改写类工作流会把既有内容再次输出（先抄全文、
//! 再输出删节版），末尾两窗口出现局部重叠（实测 0.5~0.7 区间），这不是循环；
//! 短周期复读时两窗口各自包含完整周期、近乎逐字重合（得分趋近 1.0），
//! 且持续多个检查点，才是循环。中位重叠仅警告供消费者参考，不中断输出。

use crate::loop_guard::detectors::text_self_similarity;
use crate::loop_guard::escalation::{self, DetectKind};
use crate::loop_guard::types::LoopSeverity;
use fuyao_api::LoopGuardConfig;

/// 文本循环检测结果
#[derive(Debug)]
pub(crate) struct TextDetectResult {
    /// 检测到的严重程度
    pub severity: LoopSeverity,
    /// 检测描述消息
    pub message: String,
}

/// 当前文本阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentPhase {
    Content,
    Reasoning,
}

/// 文本循环检测状态机
///
/// 累积流式文本内容，按间隔执行自相似度检测。
/// 每次检测返回 `TextDetectResult`，由协调层决定后续动作。
pub(crate) struct TextLoopGuard {
    /// 循环检测配置
    config: LoopGuardConfig,
    /// 当前轮次累积的流式文本内容
    pub(crate) accumulated_text: String,
    /// 连续超阈检查点计数：得分回落到警告线以下即清零，升级按重复事件独立进行
    warn_hits: usize,
    /// 上一次检查时的文本长度
    last_check_len: usize,
    /// 当前阶段：content 或 reasoning
    current_phase: Option<CurrentPhase>,
}

impl TextLoopGuard {
    pub fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            accumulated_text: String::new(),
            warn_hits: 0,
            last_check_len: 0,
            current_phase: None,
        }
    }

    /// 处理流式内容块，返回检测结果（可能为 None）
    ///
    /// 返回值语义：一次重复事件中首个超阈检查点返回 Warn（含重复率详情），
    /// 后续检查点处于观察态返回 None；仅当高位重叠持续足够检查点数后
    /// 才返回 Interrupt / Abort。
    pub fn handle_chunk(
        &mut self,
        content: Option<&str>,
        reasoning: Option<&str>,
        interrupt_count: usize,
    ) -> Option<TextDetectResult> {
        let content_str = content.unwrap_or("");
        let reasoning_str = reasoning.unwrap_or("");

        if content_str.is_empty() && reasoning_str.is_empty() {
            return None;
        }

        // 确定当前阶段，阶段切换则重置累积状态
        let current = if !reasoning_str.is_empty() {
            CurrentPhase::Reasoning
        } else {
            CurrentPhase::Content
        };

        if self.current_phase != Some(current) {
            self.accumulated_text.clear();
            self.last_check_len = 0;
            self.warn_hits = 0;
            self.current_phase = Some(current);
        }

        // 累加文本
        match current {
            CurrentPhase::Reasoning => self.accumulated_text.push_str(reasoning_str),
            CurrentPhase::Content => self.accumulated_text.push_str(content_str),
        }

        // 未达到检查间隔，跳过检测
        if self.accumulated_text.len() - self.last_check_len < self.config.streaming_check_interval
        {
            return None;
        }

        // 执行文本自相似度检测
        let score =
            text_self_similarity(&self.accumulated_text, self.config.streaming_window_ratio);

        self.last_check_len = self.accumulated_text.len();

        // 低于警告线：本次重复事件结束，连续命中计数清零
        let Some(score) = score.filter(|s| *s >= self.config.text_warn_threshold) else {
            self.warn_hits = 0;
            return None;
        };

        self.warn_hits += 1;

        // 中断档：逐字级重叠持续足够检查点数才允许中断输出
        if score >= self.config.text_interrupt_threshold
            && self.warn_hits >= self.config.text_interrupt_hits
        {
            let outcome = escalation::resolve(LoopSeverity::Interrupt, interrupt_count);
            return Some(TextDetectResult {
                severity: outcome.severity,
                message: escalation::interrupt_warning(DetectKind::Text, outcome.effective_count),
            });
        }

        // 事件内首个命中：发出警告（含重复率详情），不重复通知
        if self.warn_hits == 1 {
            return Some(TextDetectResult {
                severity: LoopSeverity::Warn,
                message: format!("内容重复率 {:.0}%", score * 100.0),
            });
        }

        // 后续命中持续观察，等待得分维持高位或回落
        tracing::debug!(hits = self.warn_hits, score, "文本重复持续观察中");
        None
    }

    /// 重置检测状态（轮次结束后调用）
    pub fn reset(&mut self) {
        self.accumulated_text.clear();
        self.warn_hits = 0;
        self.last_check_len = 0;
        self.current_phase = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(interval: usize) -> LoopGuardConfig {
        LoopGuardConfig {
            streaming_check_interval: interval,
            ..Default::default()
        }
    }

    /// 按 15 字一片流式喂入文本，收集全部检测结果
    fn stream_pieces(guard: &mut TextLoopGuard, text: &str) -> Vec<TextDetectResult> {
        let mut results = vec![];
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(15) {
            let piece: String = piece.iter().collect();
            if let Some(r) = guard.handle_chunk(None, Some(&piece), 0) {
                results.push(r);
            }
        }
        results
    }

    #[test]
    fn accumulates_text() {
        let mut guard = TextLoopGuard::new(make_config(10));
        guard.handle_chunk(Some("hello world"), None, 0);
        assert_eq!(guard.accumulated_text, "hello world");
    }

    #[test]
    fn phase_switch_resets() {
        let mut guard = TextLoopGuard::new(make_config(10));
        guard.handle_chunk(Some("content"), None, 0);
        assert_eq!(guard.current_phase, Some(CurrentPhase::Content));
        guard.handle_chunk(None, Some("reasoning"), 0);
        assert_eq!(guard.current_phase, Some(CurrentPhase::Reasoning));
        assert_eq!(guard.accumulated_text, "reasoning");
    }

    #[test]
    fn no_detection_below_interval() {
        let mut guard = TextLoopGuard::new(make_config(1000));
        let result = guard.handle_chunk(Some("短文本"), None, 0);
        assert!(result.is_none());
    }

    #[test]
    fn empty_chunk_returns_none() {
        let mut guard = TextLoopGuard::new(make_config(10));
        let result = guard.handle_chunk(Some(""), None, 0);
        assert!(result.is_none());
    }

    /// 短周期复读的完整升级链：首个命中 Warn → 中间命中静默观察 → 持续高位 Interrupt
    #[test]
    fn verbatim_loop_warns_then_interrupts() {
        let mut guard = TextLoopGuard::new(make_config(10));
        let text = "这是一段重复的内容".repeat(4);

        let r1 = guard.handle_chunk(Some(&text), None, 0).unwrap();
        assert_eq!(r1.severity, LoopSeverity::Warn);
        assert!(r1.message.contains("内容重复率"), "警告应携带重复率详情");

        let r2 = guard.handle_chunk(Some(&text), None, 0);
        assert!(r2.is_none(), "事件内第二次命中应静默观察");

        let r3 = guard.handle_chunk(Some(&text), None, 0).unwrap();
        assert_eq!(r3.severity, LoopSeverity::Interrupt);
    }

    /// 已有 3 次干预历史时，持续复读直接升级 Abort
    #[test]
    fn verbatim_loop_upgrades_to_abort() {
        let mut guard = TextLoopGuard::new(make_config(10));
        let text = "这是一段重复的内容".repeat(4);
        guard.handle_chunk(Some(&text), None, 3);
        guard.handle_chunk(Some(&text), None, 3);
        let r = guard.handle_chunk(Some(&text), None, 3).unwrap();
        assert_eq!(r.severity, LoopSeverity::Abort);
    }

    /// 得分回落警告线以下时连续命中计数清零，升级按事件独立进行
    #[test]
    fn low_score_resets_hit_counter() {
        let mut guard = TextLoopGuard::new(make_config(10));
        guard.warn_hits = 2;
        let novel = "这是一段与之前完全无关的新内容，主题、措辞、结构都没有任何重叠，用来把相似度拉回警告线以下。";
        assert!(guard.handle_chunk(Some(novel), None, 0).is_none());
        assert_eq!(guard.warn_hits, 0, "低于警告线的检查应清零连续命中计数");
    }

    /// 中断线从配置读取：调高到不可达后，持续复读也只警告不中断
    #[test]
    fn interrupt_threshold_comes_from_config() {
        let config = LoopGuardConfig {
            streaming_check_interval: 10,
            text_interrupt_threshold: 1.1,
            ..Default::default()
        };
        let mut guard = TextLoopGuard::new(config);
        let text = "这是一段重复的内容".repeat(4);
        for _ in 0..6 {
            if let Some(r) = guard.handle_chunk(Some(&text), None, 0) {
                assert_eq!(
                    r.severity,
                    LoopSeverity::Warn,
                    "中断线不可达时应只警告不中断"
                );
            }
        }
    }

    /// 连续命中数从配置读取：设为 1 时首个高位命中直接中断
    #[test]
    fn interrupt_hits_comes_from_config() {
        let config = LoopGuardConfig {
            streaming_check_interval: 10,
            text_interrupt_hits: 1,
            ..Default::default()
        };
        let mut guard = TextLoopGuard::new(config);
        let text = "这是一段重复的内容".repeat(4);
        let r = guard.handle_chunk(Some(&text), None, 0).unwrap();
        assert_eq!(r.severity, LoopSeverity::Interrupt);
    }

    // ===== 回归素材：「整理古籍摘录」任务的真实思考流结构 =====
    // 结构：引言 → 引文A全文 → 备注 → 引文B全文 → 备注 → 引文C全文（超限）
    //       → 删节讨论（引用C的首尾片段）→ 引文C删节版（重新输出）。
    // 转录后再删节重写会让末尾窗口出现局部重叠，此类起草流不应触发中断。
    const INTRO: &str = "格式先例已经确认完毕，接下来写摘录文件。三条摘录需要先逐条核对原文再转录，每条控制在四百字以内，超出就删节，删节时保留叙事框架和核心议论，去掉纯氛围描写。";
    const QUOTE_A: &str = "翰林院堂上不开中门，谓开则不利于掌院。癸未开四库全书馆，质郡王临视，司事者开之，掌院刘文正公、觉罗奉公相继逝。又门前甬道中有土成丸，误碎之必损翰林。癸未年雨水冲出一丸，吴云岩前辈之子误踹之，云岩寻卒。又元心阁西南隅，翰林有父母者不可设座，设则有刑。陆耳山前辈为学士时毅然坐之，卒丁外艰。至左角门久闭不启，启之则司事者获谴降，无敢试者，亦不知其果验否也。他部寺亦各有禁忌。礼部仪制司堂前甬道屏门旧不设木板，木板者以二木夹于阈，作坡形，使堂官乘车入其中，免旁绕也。钱樾田司空不听，寻有圜丘灯杆之事故。此等往往而验，然其理所在，则莫能明也。";
    const NOTE_A: &str = "这一条约三百四十字，在限额之内，可以整体收录。原文里有个别字存疑，照录不改，注释里说明即可。";
    const QUOTE_B: &str = "乌鲁木齐巡检所驻之地曰呼图壁，呼图译言鬼，呼图壁译言有鬼也。一贾人夜行，暗中见树下有人影，疑为鬼，呼问之。曰：吾日暮至此，畏鬼不敢前，待君同行为伴。遂同行，渐款洽。问君有急务冒寒夜行何故，曰：吾负友钱四千，闻其夫妇并病，恐饮药不继，故往偿之。其人却步树后曰：本欲祟君求小祭，今闻君言，君真义士，不敢犯。途中险仄皆预告之。俄新月微升，稍辨物色，谛视乃一无头人，栗却步，鬼亦随灭。";
    const NOTE_B: &str = "这条约二百五十字，篇幅合适，整体收录，不需要删节。";
    const QUOTE_C: &str = "日南防守营兵王成，姚安公之旧仆也，言乾隆辛酉夏夜坐高庙廊下纳凉，暗中见两人坐阶下，疑为盗，默伺其所往。时山阴会稽山西商人之贷钱者演戏酬神，锣鼓声未歇。一人曰：彼辈真乐矣，然机巧营削，恐造孽亦深。又一人曰：其间亦有差等。尝闻冥司判官论此事：凡候补官窘于滞留，饘粥不给，或赴远任而资斧难继，是不得已而借，其苦况不可殚述。若乘其急而多方勒掯，钳束之使进退无门，吞酸泣涕而立券，是罪与劫盗等，阳律不过笞杖，阴律应堕泥犁。至于性耽挥霍者，自度莅任之日可掊克小民以偿逋欠，遂滥用无度，千金到手辄尽，赊欠既多，索逋者踵至，有官可待而债不可逃，不得不饮恨为鱼肉，任其朘削。既积重难返，偿之不能，故先索重息以冀盈虚相抵。此在彼固情势所迫，在吾侪实孽由自作。阳官谳狱虽依律，神司或不深责。成闻是语，疑非人类。俄歌乐歇，二人并起，不待键启，已穿棂去。后闻道路喧传：是夜酒阑客散后，一人中暑暴卒。乃悟二鬼为勾摄之鬼也。";
    const NOTE_C: &str = "这条完整转录约四百六十字，超出四百字上限，需要删节。删节方案是去掉叙事框架和纯氛围描写，保留核心议论。";
    const DELIB: &str = "删节考虑：可以省去开头引言「日南防守营兵王成，姚安公之旧仆也，言乾隆辛酉夏夜坐高庙廊下纳凉，暗中见两人坐阶下，疑为盗，默伺其所往」，以及「锣鼓声未歇」这类氛围描写。中间第二类借贷者的铺陈可以压缩，但阴律阳律的对比必须保留。结尾「成闻是语，疑非人类」到「乃悟二鬼为勾摄之鬼也」是点题句，必须保留作为收束。整理后的删节版如下。";
    const QUOTE_C_TRIMMED: &str = "日南防守营兵王成言乾隆辛酉夏夜坐高庙廊下纳凉，暗中见两人坐阶下。时山阴会稽山西商人之贷钱者演戏酬神。一人曰：彼辈真乐矣，然机巧营削，恐造孽亦深。又一人曰：其间亦有差等。尝闻冥司判官论此事：凡候补官窘于滞留，或赴远任而资斧难继，是不得已而借。若乘其急而多方勒掯，使进退无门，吞酸立券，是罪与劫盗等，阳律不过笞杖，阴律应堕泥犁。至于性耽挥霍者，自度莅任之日可掊克小民以偿逋欠，遂滥用无度，赊欠既多，索逋者踵至，不得不饮恨为鱼肉，故先索重息以冀盈虚相抵。此在彼固情势所迫，在吾侪实孽由自作。成闻是语，疑非人类。俄歌乐歇，二人并起，不待键启，已穿棂去。后闻是夜酒阑客散后，一人中暑暴卒。乃悟二鬼为勾摄之鬼也。";

    /// 拼接完整起草场景文本
    fn drafting_scenario() -> String {
        format!(
            "{INTRO}{QUOTE_A}{NOTE_A}{QUOTE_B}{NOTE_B}{QUOTE_C}{NOTE_C}{DELIB}{QUOTE_C_TRIMMED}"
        )
    }

    /// 转录后删节重写的完整起草流：至多 Warn，绝不 Interrupt/Abort
    #[test]
    fn drafting_rewrite_never_interrupts() {
        let mut guard = TextLoopGuard::new(make_config(100));
        let results = stream_pieces(&mut guard, &drafting_scenario());
        assert!(
            results.iter().all(|r| r.severity == LoopSeverity::Warn),
            "起草流只允许警告，实际严重程度：{:?}",
            results.iter().map(|r| r.severity).collect::<Vec<_>>()
        );
    }

    /// 整段长引文紧接着重录一遍（复读周期远大于窗口）：两窗口落在不同段落，不构成逐字重合
    #[test]
    fn long_reemission_never_interrupts() {
        let mut guard = TextLoopGuard::new(make_config(100));
        let text = format!("{QUOTE_C}转录如下：{QUOTE_C}");
        let results = stream_pieces(&mut guard, &text);
        assert!(
            results.iter().all(|r| r.severity == LoopSeverity::Warn),
            "长周期重录只允许警告，实际严重程度：{:?}",
            results.iter().map(|r| r.severity).collect::<Vec<_>>()
        );
    }

    /// 持续短周期复读（真循环）：流式喂入应在少量检查点内升级到 Interrupt
    #[test]
    fn sustained_repetition_stream_interrupts() {
        let mut guard = TextLoopGuard::new(make_config(100));
        let text = format!(
            "先分析问题，然后逐项处理。{}",
            "这同一段内容被反复输出，模型已经卡住了。".repeat(30)
        );
        let results = stream_pieces(&mut guard, &text);
        assert!(
            results
                .iter()
                .any(|r| r.severity == LoopSeverity::Interrupt),
            "持续复读应触发中断，实际严重程度：{:?}",
            results.iter().map(|r| r.severity).collect::<Vec<_>>()
        );
    }
}
