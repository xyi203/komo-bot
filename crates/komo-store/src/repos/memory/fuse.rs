//! 两条召回臂的排名融合（§9.4）。
//!
//! **按排名融合（RRF），不直接相加不同量纲的分数**——关键词臂给的是 IDF 加权命中数，
//! 向量臂给的是余弦相似度，两者的"1.0"不是同一件事，加起来的那个数没有意义。RRF 只看
//! 每条在各臂里排第几：`Σ 1/(K + rank)`。
//!
//! `K` 压低头名的优势：`K` 越大，两臂"都排中游"越容易赢过"一臂第一、另一臂没有"。60
//! 是原论文（Cormack 2009）的取值，也是此处的起点——它是可调参数，不是常识。

/// RRF 的平滑常数。
pub const RRF_K: f64 = 60.0;

/// 把若干条有序候选列表融成一条。`arms` 里每一项是一条**已经排好序**的 id 列表。
///
/// 返回按融合分降序、同分按 id 升序的 id 列表——同一次查询两遍答案一样，是能不能对着
/// 它写断言的前提。
pub fn reciprocal_rank_fusion(arms: &[Vec<String>]) -> Vec<String> {
    let mut scores: std::collections::BTreeMap<&str, f64> = Default::default();
    for arm in arms {
        for (rank, id) in arm.iter().enumerate() {
            *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (RRF_K + (rank + 1) as f64);
        }
    }
    let mut ranked: Vec<(&str, f64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(b.0))
    });
    ranked.into_iter().map(|(id, _)| id.to_string()).collect()
}

/// 余弦相似度。两边都不归一化——空间是不是归一化的由 `EmbeddingSpace` 说，这里不假设。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a <= 0.0 || norm_b <= 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// `memory_vectors.vector` 的小端 f32 BLOB 解码。长度对不上就是**结构错误的向量**，
/// 不接受（§9.5）。
pub fn decode_vector(bytes: &[u8], dimensions: usize) -> Option<Vec<f32>> {
    if bytes.len() != dimensions * 4 || dimensions == 0 {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect(),
    )
}

/// 小端 f32 BLOB 编码。与 [`decode_vector`] 是一对。
pub fn encode_vector(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    /// 两臂都认的那一条赢过只有一臂认的头名——这正是"按排名融合"要的效果。
    #[test]
    fn a_hit_on_both_arms_outranks_a_first_place_on_one() {
        let keyword = ids(&["a", "b"]);
        let vector = ids(&["c", "b"]);
        assert_eq!(
            reciprocal_rank_fusion(&[keyword, vector]),
            ids(&["b", "a", "c"])
        );
    }

    /// 只有一条臂时融合是恒等的（顺序不动）。
    #[test]
    fn one_arm_alone_keeps_its_own_order() {
        assert_eq!(
            reciprocal_rank_fusion(&[ids(&["x", "y", "z"])]),
            ids(&["x", "y", "z"])
        );
    }

    /// 同分按 id：同一次查询两遍答案一样。
    #[test]
    fn ties_break_on_the_id_so_two_runs_agree() {
        let once = reciprocal_rank_fusion(&[ids(&["b"]), ids(&["a"])]);
        let twice = reciprocal_rank_fusion(&[ids(&["b"]), ids(&["a"])]);
        assert_eq!(once, twice);
        assert_eq!(once, ids(&["a", "b"]));
    }

    #[test]
    fn cosine_is_one_for_the_same_direction_and_zero_for_a_zero_vector() {
        assert!((cosine(&[1.0, 0.0], &[3.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 1.0]), 0.0, "维度不同不比");
    }

    /// 截断的 BLOB 是结构错误，不是"少几个分量"（§9.5）。
    #[test]
    fn a_truncated_blob_decodes_to_nothing() {
        let bytes = encode_vector(&[1.0, 2.0, 3.0]);
        assert_eq!(decode_vector(&bytes, 3), Some(vec![1.0, 2.0, 3.0]));
        assert_eq!(decode_vector(&bytes[..7], 3), None);
        assert_eq!(decode_vector(&bytes, 4), None);
    }
}
