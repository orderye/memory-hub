//! 算法 D：候选假设边际判定（设计规范 17.4 D）

/// 边际判定输出结果
#[derive(Debug, Clone, PartialEq)]
pub struct MarginResult {
    /// Top-1 与 Top-2 置信度差距
    pub margin: f32,
    /// 是否存在显著歧义/模棱两可
    pub ambiguous: bool,
    /// 指令补充建议
    pub guidance_note: Option<&'static str>,
}

/// 对同轮候选假设的置信度执行边际判定
///
/// 规则：
/// - margin = c_top1 - c_top2
/// - 若 margin < margin_delta (默认 0.15)，标记 ambiguous = true，
///   并要求助理不要武断硬选，改为追问或使用委婉模糊表达。
pub fn check_margin(confidences: &[f32], margin_delta: f32) -> MarginResult {
    if confidences.is_empty() {
        return MarginResult {
            margin: 0.0,
            ambiguous: false,
            guidance_note: None,
        };
    }

    if confidences.len() == 1 {
        return MarginResult {
            margin: confidences[0],
            ambiguous: false,
            guidance_note: None,
        };
    }

    let mut sorted = confidences.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

    let top1 = sorted[0];
    let top2 = sorted[1];
    let margin = (top1 - top2).max(0.0);

    if margin < margin_delta {
        MarginResult {
            margin,
            ambiguous: true,
            guidance_note: Some("意图假设边际差距过小，请以追问或模糊化表达为主，切勿预设立场"),
        }
    } else {
        MarginResult {
            margin,
            ambiguous: false,
            guidance_note: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_margin_check() {
        // 差异明显
        let res_clear = check_margin(&[0.85, 0.50, 0.30], 0.15);
        assert!(!res_clear.ambiguous);
        assert!((res_clear.margin - 0.35).abs() < 1e-4);

        // 边际过小 (0.72 - 0.70 = 0.02 < 0.15)
        let res_ambiguous = check_margin(&[0.72, 0.70, 0.20], 0.15);
        assert!(res_ambiguous.ambiguous);
        assert!(res_ambiguous.guidance_note.is_some());
    }
}
