//! 没有 codemode 沙箱的平台：探测直接失败，codemode 不注册（`docs/codemode.md` §4）。

use std::path::Path;

use super::Paths;

#[derive(Debug, Clone)]
pub(super) enum Jail {}

impl Jail {
    pub(super) fn new(_paths: &Paths) -> Result<Jail, String> {
        Err("这个平台没有 codemode 沙箱（docs/codemode.md §4：只有 macOS 与 Linux）".into())
    }

    pub(super) fn command(&self, _interpreter: &Path) -> tokio::process::Command {
        match *self {}
    }
}
