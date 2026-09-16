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
