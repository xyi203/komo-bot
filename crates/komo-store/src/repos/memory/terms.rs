//! 关键词索引的分词（§9.4）。
//!
//! Turso 的 MVCC 下建不出 FTS 索引（§8.2 实测），所以分词挪到**索引时**。索引侧与查询
//! 侧用的是**同一个函数**：两边分词规则不同，命中率就是一件没人说得清的事。

/// 把一段文本切成检索用的 token（§9.4）。
///
/// 规则只有两条，它们合起来是"两字中文查询天然命中"的原因：
///
/// - **CJK 连续字符切 bigram**——"空调"本身就是一个 bigram，不需要子串后备。
/// - **ASCII 切单词并小写化**。
///
/// 另有一条例外：**落单的虚字不成 token**（[`LONE_STOPWORDS`]）。"devops-mcp-prod 的
/// issue_get_status" 里两边是空格的"的"会单独切出来；库小的时候它只在一两条里出现，
/// IDF 接近满分，和一个真关键词一起就能凑过 `min_keyword_score`（2026-11-03 实测：
/// "工单" + "的" = 1.152，把无关的工单参数经验召了回来）。只丢落单的：bigram 里的
/// "的"照常保留，单个实字（"猫"）照常是 token。
///
/// 索引侧和查询侧用的是**同一个函数**：两边分词规则不同，命中率就是一件没人说得清的
/// 事。
pub fn lexical_terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();

    fn flush_word(word: &mut String, out: &mut Vec<String>) {
        if !word.is_empty() {
            out.push(std::mem::take(word));
        }
    }
    fn flush_cjk(cjk: &mut Vec<char>, out: &mut Vec<String>) {
        match cjk.len() {
            0 => {}
            // 单个 CJK 字符成不了 bigram，但它是用户真的可能打出来的查询。
            1 if LONE_STOPWORDS.contains(&cjk[0]) => {}
            1 => out.push(cjk[0].to_string()),
            _ => {
                for pair in cjk.windows(2) {
                    out.push(pair.iter().collect());
                }
            }
        }
        cjk.clear();
    }

    for ch in text.chars() {
        if is_cjk(ch) {
            flush_word(&mut word, &mut out);
            cjk.push(ch);
        } else if ch.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            word.extend(ch.to_lowercase());
        } else {
            flush_word(&mut word, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_cjk(&mut cjk, &mut out);

    out.sort();
    out.dedup();
    out
}

/// 落单时不成 token 的中文虚字：结构助词、语气词、常见介词连词与系动词。只管单字，
/// 两字以上的连续汉字仍全部切 bigram。
const LONE_STOPWORDS: &[char] = &[
    '的', '地', '得', '了', '着', '过', '是', '在', '和', '与', '及', '或', '也', '都', '就', '把',
    '被', '给', '对', '从', '向', '为', '以', '而', '之', '其', '吗', '呢', '吧', '啊', '呀', '嘛',
    '个', '这', '那', '有', '我', '你', '他', '她', '它',
];

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF      // 假名
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x4E00..=0x9FFF    // 基本区
        | 0xF900..=0xFAFF    // 兼容
        | 0x20000..=0x2FA1F  // 扩展 B+
        | 0xAC00..=0xD7AF) // 谚文
}

/// `memory_terms.terms` 的写法：token 用空格连接，**首尾也带空格**，于是
/// `instr(terms, ' tok ')` 只会命中整个 token。
pub fn terms_column(text: &str) -> (String, usize) {
    let tokens = lexical_terms(text);
    let count = tokens.len();
    if tokens.is_empty() {
        return (String::from(" "), 0);
    }
    (format!(" {} ", tokens.join(" ")), count)
}

#[cfg(test)]
mod tests {
    use super::{lexical_terms, terms_column};

    #[test]
    fn cjk_runs_become_bigrams_and_ascii_becomes_lowercase_words() {
        assert_eq!(lexical_terms("空调"), vec!["空调".to_string()]);
        assert_eq!(
            lexical_terms("客厅空调"),
            vec!["厅空".to_string(), "客厅".to_string(), "空调".to_string()]
        );
        assert_eq!(
            lexical_terms("Cargo TEST cargo"),
            vec!["cargo".to_string(), "test".to_string()]
        );
    }

    #[test]
    fn a_lone_cjk_character_is_still_a_token() {
        assert_eq!(lexical_terms("猫"), vec!["猫".to_string()]);
    }

    #[test]
    fn a_lone_function_character_is_not_a_token_but_bigrams_keep_it() {
        // 2026-11-03：两边是空格的"的"单独成词，靠高 IDF 帮无关条目凑过关键词门槛。
        assert_eq!(
            lexical_terms("devops-mcp-prod 的 issue_get_status"),
            vec![
                "devops".to_string(),
                "get".to_string(),
                "issue".to_string(),
                "mcp".to_string(),
                "prod".to_string(),
                "status".to_string(),
            ]
        );
        assert_eq!(lexical_terms("的"), Vec::<String>::new());
        // 连在实词里的"的"照常进 bigram。
        assert_eq!(
            lexical_terms("工单的"),
            vec!["单的".to_string(), "工单".to_string()]
        );
    }

    #[test]
    fn the_terms_column_wraps_tokens_in_spaces_so_instr_matches_whole_tokens() {
        let (terms, count) = terms_column("cargo test");
        assert_eq!(terms, " cargo test ");
        assert_eq!(count, 2);
        assert!(terms.contains(" cargo "));
        // ' car ' 不是一个 token，所以它不该命中。
        assert!(!terms.contains(" car "));
    }

    #[test]
    fn an_empty_text_still_produces_a_column_instr_can_run_against() {
        let (terms, count) = terms_column("   ");
        assert_eq!(terms, " ");
        assert_eq!(count, 0);
    }
}
