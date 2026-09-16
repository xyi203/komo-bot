//! SSE 帧的拆分。
//!
//! 一条规则决定了这个文件的形状：**流没收到终止帧 = 可重试的失败，不是一个短回答**
//! （§6「参数未收齐时不能开始执行」）。所以解码器记着自己有没有见过 `data: [DONE]`，
//! 由调用方在流结束时检查——把"结束"和"完整"分开，是这里唯一要做对的事。

/// 从字节流里一段一段喂进来，吐出 `data:` 行的负载。
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    /// 见过终止帧了吗。
    terminated: bool,
}

/// 一帧的负载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// 一条 `data:` 负载（不是 `[DONE]`）。
    Data(String),
    /// `data: [DONE]`——协议规定的终止帧。
    Done,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段字节，取出这段里已经完整的帧。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut frames = Vec::new();
        // SSE 的事件以空行分隔；一个事件里可以有多条 `data:` 行，按协议要拼起来。
        while let Some((length, separator)) = find_event_boundary(&self.buffer) {
            let event: String = self.buffer[..length].to_string();
            self.buffer.drain(..length + separator);
            if let Some(frame) = decode_event(&event) {
                if frame == Frame::Done {
                    self.terminated = true;
                }
                frames.push(frame);
            }
        }
        frames
    }

    /// 见过 `[DONE]` 了吗。流结束时它是 `false`，这次回复就**没收齐**。
    pub fn terminated(&self) -> bool {
        self.terminated
    }

    /// 流结束了，缓冲区里还剩着没有以空行收尾的东西吗。
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

fn decode_event(event: &str) -> Option<Frame> {
    let mut data = String::new();
    for line in event.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        // `event:` / `id:` / `retry:` / 注释行对这个协议没有意义，丢掉。
    }
    if data.is_empty() {
        return None;
    }
    if data.trim() == "[DONE]" {
        return Some(Frame::Done);
    }
    Some(Frame::Data(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_arrive_whole_even_when_the_bytes_do_not() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: {\"a\":").is_empty(), "半帧不吐出来");
        let frames = decoder.push(b"1}\n\ndata: [DONE]\n\n");
        assert_eq!(frames, vec![Frame::Data("{\"a\":1}".into()), Frame::Done]);
        assert!(decoder.terminated());
    }

    #[test]
    fn a_stream_without_its_terminal_frame_says_so() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: {\"a\":1}\n\n");
        assert!(!decoder.terminated(), "没有 [DONE] 就是没收齐");
    }

    #[test]
    fn crlf_and_comment_lines_are_handled() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b": keep-alive\r\n\r\ndata: x\r\n\r\n");
        assert_eq!(frames, vec![Frame::Data("x".into())]);
    }

    #[test]
    fn a_multi_line_data_event_is_joined() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b"data: one\ndata: two\n\n");
        assert_eq!(frames, vec![Frame::Data("one\ntwo".into())]);
    }

    #[test]
    fn a_half_frame_left_in_the_buffer_is_visible() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: {\"a\":");
        assert!(decoder.has_trailing_bytes());
    }
}
