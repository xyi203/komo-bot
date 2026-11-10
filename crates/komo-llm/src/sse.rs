//! SSE 帧的拆分。
//!
//! Responses API 的每一帧带一个 `event:` 名字和一行 `data:` 负载，**终止帧是
//! `response.completed`**（不是 `[DONE]`）。所以这个解码器只做协议无关的那一半：把字节
//! 流切成 `(事件名, 负载)`。"收齐了没有"由读帧的人判断——把"结束"和"完整"分开，是这里
//! 唯一要做对的事（§6：参数未收齐时不能开始执行）。

/// 从字节流里一段一段喂进来，吐出一帧一帧的事件。
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
}

/// 一帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` 行；有些实现只在负载的 `type` 字段里给名字，那时这里是 `None`。
    pub name: Option<String>,
    pub data: String,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段字节，取出这段里已经完整的帧。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut frames = Vec::new();
        // SSE 的事件以空行分隔；一个事件里可以有多条 `data:` 行，按协议要拼起来。
        while let Some((length, separator)) = find_event_boundary(&self.buffer) {
            let event: String = self.buffer[..length].to_string();
            self.buffer.drain(..length + separator);
            if let Some(frame) = decode_event(&event) {
                frames.push(frame);
            }
        }
        frames
    }

    /// 流结束了，缓冲区里还剩着没有以空行收尾的东西吗。诊断用：没有终止帧时，它区分
    /// 得开"断在一帧中间"和"最后一帧完整但没有终止事件"。
    pub fn has_trailing_bytes(&self) -> bool {
        !self.buffer.trim().is_empty()
    }
}

/// 返回 (事件正文长度, 分隔符长度)。
fn find_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|at| (at, 2));
    let crlf = buffer.find("\r\n\r\n").map(|at| (at, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) if a.0 <= b.0 => Some(a),
        (Some(_), Some(b)) => Some(b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn decode_event(event: &str) -> Option<SseEvent> {
    let mut name = None;
    let mut data = String::new();
    for line in event.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        // `id:` / `retry:` / 注释行对这个协议没有意义，丢掉。
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent { name, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent {
            name: Some(name.to_string()),
            data: data.to_string(),
        }
    }

    #[test]
    fn frames_arrive_whole_even_when_the_bytes_do_not() {
        let mut decoder = SseDecoder::new();
        assert!(
            decoder
                .push(b"event: response.output_text.delta\ndata: {\"delta\":")
                .is_empty(),
            "半帧不吐出来"
        );
        let frames = decoder.push("\"好\"}\n\nevent: response.completed\ndata: {}\n\n".as_bytes());
        assert_eq!(
            frames,
            vec![
                event("response.output_text.delta", "{\"delta\":\"好\"}"),
                event("response.completed", "{}"),
            ]
        );
    }

    #[test]
    fn a_frame_without_an_event_line_still_carries_its_payload() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b"data: {\"type\":\"response.completed\"}\n\n");
        assert_eq!(
            frames,
            vec![SseEvent {
                name: None,
                data: "{\"type\":\"response.completed\"}".into()
            }]
        );
    }

    #[test]
    fn crlf_and_comment_lines_are_handled() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b": keep-alive\r\n\r\nevent: error\r\ndata: x\r\n\r\n");
        assert_eq!(frames, vec![event("error", "x")]);
    }

    #[test]
    fn a_multi_line_data_event_is_joined() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b"event: e\ndata: one\ndata: two\n\n");
        assert_eq!(frames, vec![event("e", "one\ntwo")]);
    }

    #[test]
    fn a_trailing_done_frame_is_left_for_the_protocol_adapter() {
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decoder.push(b"data: [DONE]\n\n"),
            vec![SseEvent {
                name: None,
                data: "[DONE]".into(),
            }]
        );
    }

    #[test]
    fn a_half_frame_left_in_the_buffer_is_visible() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"event: response.completed\ndata: {\"a\":");
        assert!(decoder.has_trailing_bytes());
    }
}
